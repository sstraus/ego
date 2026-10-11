//! Hostile-input cases for human-granted sandbox paths.
//!
//! The child's stderr is written by a process the model chose, so each case
//! either forges a refusal line or provokes a real one and then checks what the
//! executor asks the human, what it grants, and what it leaves behind. Cases that
//! need a genuine Seatbelt refusal are macOS-only; forged ones run everywhere.
#![cfg_attr(not(target_os = "macos"), allow(dead_code, unused_imports))]

use std::ffi::OsString;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ego_core::event::id::SessionId;
use ego_core::permit::{
    CanonPath, Confinement, EnvPolicy, ProcessError, RunConfinement, SandboxAccess, SandboxGrant,
    SandboxGrantPort, SpawnSpec, ToolResult,
};
use ego_core::turn::CancelToken;
use ego_tools::exec::{
    ExecLimits, ExecRunConfig, HttpCallTimeout, RunEffectPorts, SessionEvidenceStore,
};
use ego_tools::http::SystemResolver;
use futures::future::BoxFuture;

const BACKEND_ABSENT: &str = "EGO_TEST_CONFINEMENT_BACKEND_ABSENT";

fn block_on<F: Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test Tokio runtime")
        .block_on(future)
}

/// A host seat with a scripted answer. `remember` mimics a seat that records an
/// allowed grant so the next `granted()` includes it.
struct Seat {
    allow: fn(&SandboxGrant) -> bool,
    asked: Mutex<Vec<SandboxGrant>>,
    granted: Mutex<Vec<SandboxGrant>>,
    commands: Mutex<Vec<String>>,
}

impl Seat {
    fn new(allow: fn(&SandboxGrant) -> bool) -> Arc<Self> {
        Arc::new(Self {
            allow,
            asked: Mutex::default(),
            granted: Mutex::default(),
            commands: Mutex::default(),
        })
    }

    /// The command line each question showed the person.
    fn commands(&self) -> Vec<String> {
        self.commands.lock().unwrap().clone()
    }

    fn asked(&self) -> Vec<SandboxGrant> {
        self.asked.lock().unwrap().clone()
    }

    fn asked_paths(&self) -> Vec<PathBuf> {
        self.asked()
            .iter()
            .map(|grant| grant.path.as_path().to_path_buf())
            .collect()
    }
}

impl SandboxGrantPort for Seat {
    fn granted(&self) -> Vec<SandboxGrant> {
        self.granted.lock().unwrap().clone()
    }

    fn ask<'a>(&'a self, request: &'a SandboxGrant, command: &'a str) -> BoxFuture<'a, bool> {
        self.commands.lock().unwrap().push(command.to_owned());
        self.asked.lock().unwrap().push(request.clone());
        let yes = (self.allow)(request);
        if yes {
            self.granted.lock().unwrap().push(request.clone());
        }
        Box::pin(async move { yes })
    }
}

fn yes(_: &SandboxGrant) -> bool {
    true
}

fn no(_: &SandboxGrant) -> bool {
    false
}

struct Fixture {
    _evidence: tempfile::TempDir,
    workspace: CanonPath,
    _workspace_dir: tempfile::TempDir,
    outside: PathBuf,
    _outside_dir: tempfile::TempDir,
    ports: RunEffectPorts,
    seat: Arc<Seat>,
}

fn canonical(dir: &tempfile::TempDir) -> PathBuf {
    std::fs::canonicalize(dir.path()).expect("canonical tempdir")
}

/// A confined run with `seat` attached, or `None` on a host that declared it has
/// no confinement backend.
fn open(level: Confinement, seat: Arc<Seat>) -> Option<Fixture> {
    let workspace_dir = tempfile::tempdir().expect("workspace");
    let outside_dir = tempfile::tempdir().expect("outside");
    let evidence_dir = tempfile::tempdir().expect("evidence");
    let workspace = CanonPath::new(canonical(&workspace_dir));
    let policy = RunConfinement::new(level, false, workspace.clone()).expect("confinement");
    let evidence = SessionEvidenceStore::open(evidence_dir.path(), SessionId::new(), 1024 * 1024)
        .expect("evidence store");
    let config = ExecRunConfig::new(
        policy,
        evidence,
        CancelToken::new(),
        ExecLimits::new(4096, Duration::from_millis(250)).expect("limits"),
        HttpCallTimeout::new(Duration::from_secs(1)).expect("http timeout"),
        None,
        Arc::new(SystemResolver::new()),
    )
    .with_sandbox_grants(Arc::clone(&seat) as Arc<dyn SandboxGrantPort>);
    match block_on(RunEffectPorts::for_run(config)) {
        Ok((ports, _, _)) => Some(Fixture {
            _evidence: evidence_dir,
            workspace,
            _workspace_dir: workspace_dir,
            outside: canonical(&outside_dir),
            _outside_dir: outside_dir,
            ports,
            seat,
        }),
        Err(ProcessError::MissingCapabilities(_))
            if cfg!(not(target_os = "macos")) || std::env::var_os(BACKEND_ABSENT).is_some() =>
        {
            None
        }
        Err(error) => panic!(
            "{level} confinement must be demonstrated on this host: {error}. \
             Set {BACKEND_ABSENT}=1 only if this machine has no confinement backend."
        ),
    }
}

/// Gate widening cases on the selected backend's actual behavior, not its
/// platform/name or a failed filesystem measurement. The production denial
/// note comes from Backend::can_widen; a launch/parser failure cannot skip.
fn open_widening(level: Confinement, seat: Arc<Seat>) -> Option<Fixture> {
    let fixture = open(level, seat)?;
    let prior_grants = fixture.seat.granted();
    let path = write_file(&fixture.outside.join("widening-control"), "control");
    let outcome = fixture.sh(FORGE_PATH_FROM_ARG, &[&path]);
    assert!(
        matches!(outcome, Err(ProcessError::Exit { code: Some(1), .. })),
        "the capability control did not reach its target: {outcome:?}"
    );
    if note_of(&outcome).contains("this platform's sandbox cannot open") {
        assert!(
            fixture.seat.asked().is_empty(),
            "a non-widening backend asked anyway"
        );
        eprintln!("not-verifiable: selected backend reports can_widen=false");
        return None;
    }
    assert_eq!(
        fixture.seat.asked_paths(),
        vec![path.clone()],
        "a widening backend did not ask for the control path: {outcome:?}"
    );
    // Reset only the scripted seat's preflight effects. Preserve any stored
    // grants supplied by the case; its real command has not run yet.
    *fixture.seat.granted.lock().unwrap() = prior_grants;
    fixture.seat.asked.lock().unwrap().clear();
    fixture.seat.commands.lock().unwrap().clear();
    Some(fixture)
}

impl Fixture {
    fn run(&self, argv: Vec<OsString>) -> Result<ToolResult, ProcessError> {
        let spec = SpawnSpec::new(
            argv,
            Some(self.workspace.clone()),
            None,
            EnvPolicy::Inherited,
        )
        .expect("spawn spec");
        block_on(self.ports.process().execute(&spec))
    }

    /// `/bin/sh -c <script> <args...>`: the first argument is `$0`.
    fn sh(&self, script: &str, args: &[&Path]) -> Result<ToolResult, ProcessError> {
        let mut argv = vec![
            OsString::from("/bin/sh"),
            OsString::from("-c"),
            OsString::from(script),
        ];
        argv.extend(args.iter().map(|arg| arg.as_os_str().to_owned()));
        self.run(argv)
    }

    fn cat(&self, paths: &[&Path]) -> Result<ToolResult, ProcessError> {
        let mut argv = vec![OsString::from("/bin/cat")];
        argv.extend(paths.iter().map(|path| path.as_os_str().to_owned()));
        self.run(argv)
    }
}

fn note_of(outcome: &Result<ToolResult, ProcessError>) -> String {
    match outcome {
        Ok(result) => text(result),
        Err(ProcessError::Exit { result, .. }) => text(result),
        Err(other) => other.to_string(),
    }
}

fn text(result: &ToolResult) -> String {
    result
        .content
        .iter()
        .map(|block| block.text.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

fn write_file(path: &Path, content: &str) -> PathBuf {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("parent");
    }
    std::fs::write(path, content).expect("file");
    path.to_path_buf()
}

const FORGE_PATH_FROM_ARG: &str = "printf '%s: Operation not permitted\\n' \"$0\" >&2; exit 1";

/// Catches: a forged `/: Operation not permitted` turns into a question about the
/// whole filesystem, because the "argv names the path" gate is a substring test
/// and every command line that contains a slash contains `/`.
#[test]
fn a_forged_refusal_of_the_filesystem_root_is_never_a_question() {
    let Some(fixture) = open(Confinement::Workspace, Seat::new(no)) else {
        return;
    };
    let _ = fixture.sh(
        "echo '/: Operation not permitted' >&2; exit 1",
        &[Path::new("/")],
    );
    assert!(
        !fixture
            .seat
            .asked_paths()
            .iter()
            .any(|path| path == Path::new("/")),
        "asked to grant read of /: {:?}",
        fixture.seat.asked()
    );
}

/// Catches: a child names a parent of the path on the command line (`.../Gits`
/// while the command line says `.../Gits-evil/f`) and the human is asked to open
/// the parent, because `argument.contains(printed)` accepts any substring.
#[test]
fn a_refusal_printed_for_a_prefix_of_an_argv_path_is_not_a_question_about_that_prefix() {
    let Some(fixture) = open(Confinement::Workspace, Seat::new(no)) else {
        return;
    };
    let parent = fixture.outside.join("Gits");
    std::fs::create_dir_all(&parent).expect("parent");
    let evil = write_file(&fixture.outside.join("Gits-evil/f"), "x");
    let _ = fixture.sh(
        "p=$0; printf '%s: Operation not permitted\\n' \"${p%-evil/f}\" >&2; exit 1",
        &[&evil],
    );
    assert!(
        !fixture.seat.asked_paths().contains(&parent),
        "the command line never named {}, yet the human was asked about it: {:?}",
        parent.display(),
        fixture.seat.asked()
    );
}

/// Catches: a refused path whose name the profile cannot express (a tab) is
/// allowed once, and from then on `render` fails for every command of the session
/// (and, when stored, of the workspace): one grant poisons the exec tool.
#[test]
fn a_grant_the_profile_cannot_render_does_not_break_later_commands() {
    let Some(fixture) = open(Confinement::Workspace, Seat::new(yes)) else {
        return;
    };
    let odd = write_file(&fixture.outside.join("a\tb"), "x");
    let first = fixture.sh(FORGE_PATH_FROM_ARG, &[&odd]);
    assert!(
        matches!(first, Err(ProcessError::Exit { .. })),
        "the forged command exits 1; a profile error must not replace that: {first:?}"
    );
    let later = fixture.run(vec![OsString::from("/usr/bin/true")]);
    assert!(
        later.is_ok(),
        "a harmless command failed after the grant: {later:?}"
    );
}

/// Catches: the refusal line is read from stdout, so a command can print it as
/// ordinary output and get a prompt.
#[test]
fn a_refusal_line_on_stdout_is_not_a_question() {
    let Some(fixture) = open(Confinement::Workspace, Seat::new(no)) else {
        return;
    };
    let target = write_file(&fixture.outside.join("f"), "x");
    let _ = fixture.sh(
        "printf '%s: Operation not permitted\\n' \"$0\"; exit 1",
        &[&target],
    );
    assert!(
        fixture.seat.asked().is_empty(),
        "{:?}",
        fixture.seat.asked()
    );
}

/// Catches: a successful command that happens to print the line on stderr is
/// asked about and re-run.
#[test]
fn a_refusal_line_from_a_command_that_exited_zero_is_not_a_question() {
    let Some(fixture) = open(Confinement::Workspace, Seat::new(no)) else {
        return;
    };
    let target = write_file(&fixture.outside.join("f"), "x");
    let outcome = fixture.sh(
        "printf '%s: Operation not permitted\\n' \"$0\" >&2; exit 0",
        &[&target],
    );
    assert!(outcome.is_ok(), "{outcome:?}");
    assert!(
        fixture.seat.asked().is_empty(),
        "{:?}",
        fixture.seat.asked()
    );
}

/// Catches: the path named by the child is not the one the human is shown: a
/// forged relative `..` spelling or a symlink inside the workspace must be asked
/// about by its resolved location, never by the spelling the child printed.
#[test]
fn the_question_names_the_resolved_location_not_the_printed_spelling() {
    let Some(fixture) = open_widening(Confinement::Workspace, Seat::new(no)) else {
        return;
    };
    let target = write_file(&fixture.outside.join("real/f"), "x");
    std::fs::create_dir_all(fixture.outside.join("sub")).expect("sub");
    let dotdot = fixture.outside.join("sub/../real/f");
    let _ = fixture.sh(FORGE_PATH_FROM_ARG, &[&dotdot]);
    let link = fixture.workspace.as_path().join("link");
    std::os::unix::fs::symlink(&target, &link).expect("symlink");
    let _ = fixture.sh(FORGE_PATH_FROM_ARG, &[&link]);
    assert_eq!(
        fixture.seat.asked_paths(),
        vec![target.clone(), target],
        "both spellings must resolve to the same outside file"
    );
}

/// Catches: only the first refusal line is looked at, so a command refused two
/// paths whose first one cannot be asked about (inside the workspace, already
/// granted) never gets the second one put to the person, who then sees a bare
/// failure for a path they could have allowed.
#[test]
fn a_later_refusal_is_still_a_question_when_the_first_cannot_be_asked() {
    let Some(fixture) = open_widening(Confinement::Workspace, Seat::new(no)) else {
        return;
    };
    let inside = write_file(&fixture.workspace.as_path().join("f"), "x");
    let outside = write_file(&fixture.outside.join("g"), "y");
    let _ = fixture.sh(
        "printf '%s: Operation not permitted\\n' \"$0\" \"$1\" >&2; exit 1",
        &[&inside, &outside],
    );
    assert_eq!(fixture.seat.asked_paths(), vec![outside]);
}

/// Catches: a forged refusal for a path inside the workspace is asked about.
#[test]
fn a_refusal_inside_the_workspace_is_not_a_question() {
    let Some(fixture) = open(Confinement::Workspace, Seat::new(no)) else {
        return;
    };
    let inside = write_file(&fixture.workspace.as_path().join("f"), "x");
    let _ = fixture.sh(FORGE_PATH_FROM_ARG, &[&inside]);
    assert!(
        fixture.seat.asked().is_empty(),
        "{:?}",
        fixture.seat.asked()
    );
}

#[cfg(target_os = "macos")]
mod seatbelt {
    use super::*;

    /// Catches: after the human declines, the command runs again anyway (deny
    /// path still running the command), or the model is not told it was declined.
    #[test]
    fn a_declined_refusal_does_not_run_the_command_a_second_time() {
        let Some(fixture) = open(Confinement::Workspace, Seat::new(no)) else {
            return;
        };
        let outside = write_file(&fixture.outside.join("f"), "secret");
        let log = fixture.workspace.as_path().join("log");
        let outcome = fixture.sh(
            "echo run >> \"$1\"; exec /bin/cat \"$0\"",
            &[&outside, &log],
        );
        assert!(
            matches!(outcome, Err(ProcessError::Exit { .. })),
            "{outcome:?}"
        );
        assert_eq!(
            std::fs::read_to_string(&log).expect("log"),
            "run\n",
            "the command ran again after the human said no"
        );
        assert!(
            note_of(&outcome).contains("declined"),
            "{}",
            note_of(&outcome)
        );
        assert!(
            !note_of(&outcome).contains("secret"),
            "a denied read leaked its content"
        );
    }

    /// Catches: a read answer widens write: the human allowed reading a file, and
    /// the same profile lets the command append to it.
    #[test]
    fn a_read_only_answer_never_lets_the_command_write() {
        let Some(fixture) = open(
            Confinement::Workspace,
            Seat::new(|grant| grant.access == SandboxAccess::Read),
        ) else {
            return;
        };
        let outside = write_file(&fixture.outside.join("f"), "original");
        let outcome = fixture.sh("echo more >> \"$0\"", &[&outside]);
        assert!(
            matches!(outcome, Err(ProcessError::Exit { .. })),
            "{outcome:?}"
        );
        assert_eq!(
            std::fs::read_to_string(&outside).expect("file"),
            "original",
            "a read-only answer let the command write: {:?}",
            fixture.seat.asked()
        );
        assert!(
            fixture
                .seat
                .granted
                .lock()
                .unwrap()
                .iter()
                .all(|grant| grant.access == SandboxAccess::Read)
        );
    }

    /// Catches: a grant is applied to the parent directory or a sibling whose
    /// name merely starts the same: `dir/f` allowed must leave `dir/g` and
    /// `dir-evil/f` refused (and asked about again).
    #[test]
    fn a_grant_covers_one_path_not_its_siblings_or_prefix_lookalikes() {
        let Some(fixture) = open(
            Confinement::Workspace,
            Seat::new(|grant| grant.path.as_path().ends_with("dir/f")),
        ) else {
            return;
        };
        let allowed = write_file(&fixture.outside.join("dir/f"), "f");
        let sibling = write_file(&fixture.outside.join("dir/g"), "g");
        let lookalike = write_file(&fixture.outside.join("dir-evil/f"), "e");
        let first = fixture.cat(&[&allowed]);
        assert!(
            first.is_ok(),
            "the allowed path must be readable: {first:?}"
        );
        let second = fixture.cat(&[&sibling]);
        let third = fixture.cat(&[&lookalike]);
        assert!(
            matches!(second, Err(ProcessError::Exit { .. })),
            "{second:?}"
        );
        assert!(matches!(third, Err(ProcessError::Exit { .. })), "{third:?}");
        assert_eq!(
            fixture.seat.asked_paths(),
            vec![allowed, sibling, lookalike],
            "each refused path must be a question of its own"
        );
    }

    /// Catches: the fourth refused path of one command is reported as "no one was
    /// available" although the person answered three questions: the model is told
    /// the wrong reason and cannot tell the cap from an unattended run.
    #[test]
    fn the_question_cap_is_not_reported_as_nobody_being_available() {
        let Some(fixture) = open(Confinement::Workspace, Seat::new(yes)) else {
            return;
        };
        let files = (0..4)
            .map(|index| write_file(&fixture.outside.join(format!("f{index}")), "x"))
            .collect::<Vec<_>>();
        let refs = files.iter().map(PathBuf::as_path).collect::<Vec<_>>();
        let outcome = fixture.cat(&refs);
        assert_eq!(fixture.seat.asked().len(), 3, "{:?}", fixture.seat.asked());
        assert!(
            matches!(outcome, Err(ProcessError::Exit { .. })),
            "{outcome:?}"
        );
        assert!(
            !note_of(&outcome).contains("no one was available"),
            "the person answered all three questions: {}",
            note_of(&outcome)
        );
    }
}

// ---- round 2 -------------------------------------------------------------

/// Catches: the component-wise argv gate drops the `--flag=/path` spelling, so a
/// genuine refusal of `--out=/outside/f` is never put to the person.
#[test]
fn a_flag_value_path_on_the_command_line_is_a_question_for_that_path() {
    let Some(fixture) = open_widening(Confinement::Workspace, Seat::new(no)) else {
        return;
    };
    let target = write_file(&fixture.outside.join("f"), "x");
    let mut flag = OsString::from("--out=");
    flag.push(target.as_os_str());
    let _ = fixture.run(vec![
        OsString::from("/bin/sh"),
        OsString::from("-c"),
        OsString::from("p=${0#--out=}; printf '%s: Operation not permitted\\n' \"$p\" >&2; exit 1"),
        flag,
    ]);
    assert_eq!(fixture.seat.asked_paths(), vec![target]);
}

/// Catches: a symlink on the command line is compared by spelling, so a refusal
/// printed below the link's target (the path the sandbox actually checks) is not
/// recognised as named by the command line.
#[test]
fn a_symlinked_command_line_path_anchors_the_files_below_its_target() {
    let Some(fixture) = open_widening(Confinement::Workspace, Seat::new(no)) else {
        return;
    };
    let real = fixture.outside.join("real");
    let target = write_file(&real.join("f"), "x");
    let link = fixture.outside.join("link");
    std::os::unix::fs::symlink(&real, &link).expect("symlink");
    let _ = fixture.sh(
        "printf '%s/f: Operation not permitted\\n' \"$0\" >&2; exit 1",
        &[&link],
    );
    assert_eq!(fixture.seat.asked_paths(), vec![target]);
}

/// Catches: `..` segments and a trailing slash in the command-line spelling are
/// compared literally, so the same directory spelled `sub/../real/` does not
/// anchor `real/f`.
#[test]
fn dotdot_and_trailing_slash_spellings_anchor_the_same_directory() {
    let Some(fixture) = open_widening(Confinement::Workspace, Seat::new(no)) else {
        return;
    };
    let target = write_file(&fixture.outside.join("real/f"), "x");
    std::fs::create_dir_all(fixture.outside.join("sub")).expect("sub");
    let mut spelled = fixture.outside.join("sub/../real").into_os_string();
    spelled.push("/");
    let _ = fixture.sh(
        "printf '%sf: Operation not permitted\\n' \"$0\" >&2; exit 1",
        &[Path::new(&spelled)],
    );
    assert_eq!(fixture.seat.asked_paths(), vec![target]);
}

/// Catches: a command line that names only `/` anchors every path on the machine
/// (`path.is_or_under("/")`), so the "the command line named it" gate is empty
/// for `ls /` and a forged refusal of any outside file becomes a question.
#[test]
fn a_command_line_naming_only_the_filesystem_root_does_not_anchor_other_paths() {
    let Some(fixture) = open(Confinement::Workspace, Seat::new(no)) else {
        return;
    };
    let target = write_file(&fixture.outside.join("f"), "x");
    let script = format!(
        "printf '%s: Operation not permitted\\n' '{}' >&2; exit 1",
        target.display()
    );
    let _ = fixture.sh(&script, &[Path::new("/")]);
    assert!(
        fixture.seat.asked().is_empty(),
        "`/` on the command line does not name {}: {:?}",
        target.display(),
        fixture.seat.asked()
    );
}

/// Catches: the same hole through the home directory: a symlink spelled on the
/// command line that resolves to HOME anchors everything below HOME (`.ssh`,
/// `Library`), though the person approved a command about a workspace link.
#[test]
fn a_command_line_path_that_resolves_to_home_does_not_anchor_files_below_it() {
    let Some(fixture) = open(Confinement::Workspace, Seat::new(no)) else {
        return;
    };
    let home = std::fs::canonicalize(std::env::var_os("HOME").expect("HOME")).expect("home");
    let below = home.join("Library");
    if !below.exists() {
        return;
    }
    let link = fixture.workspace.as_path().join("h");
    std::os::unix::fs::symlink(&home, &link).expect("symlink");
    let script = format!(
        "printf '%s: Operation not permitted\\n' '{}' >&2; exit 1",
        below.display()
    );
    let _ = fixture.sh(&script, &[&link]);
    assert!(
        fixture.seat.asked().is_empty(),
        "asked about {} because a link to HOME was on the command line: {:?}",
        below.display(),
        fixture.seat.asked()
    );
}

/// Catches: HOME, an ancestor of HOME, or `/`, reached through a symlink spelled
/// on the command line and printed back, escapes the too-broad rule, which only
/// looks at the printed spelling.
#[test]
fn symlinks_to_home_its_parent_and_root_are_never_asked() {
    let Some(fixture) = open(Confinement::Workspace, Seat::new(no)) else {
        return;
    };
    let home = std::fs::canonicalize(std::env::var_os("HOME").expect("HOME")).expect("home");
    let parent = home.parent().expect("home parent").to_path_buf();
    for (name, to) in [("home", home.clone()), ("up", parent), ("root", "/".into())] {
        let link = fixture.outside.join(name);
        std::os::unix::fs::symlink(&to, &link).expect("symlink");
        let _ = fixture.sh(FORGE_PATH_FROM_ARG, &[&link]);
    }
    assert!(
        fixture.seat.asked().is_empty(),
        "asked to open a location at or above HOME: {:?}",
        fixture.seat.asked()
    );
}

/// Catches: a stored grant that the profile cannot express (a hand-edited or
/// older ledger entry with a control character) makes `render` fail on every
/// command of the workspace, not just the grant's own path.
#[test]
fn a_stored_grant_the_profile_cannot_render_does_not_break_every_command() {
    let seat = Seat::new(no);
    seat.granted.lock().unwrap().push(SandboxGrant::new(
        CanonPath::new("/opt/a\tb"),
        SandboxAccess::Read,
    ));
    let Some(fixture) = open(Confinement::Workspace, seat) else {
        return;
    };
    let outcome = fixture.run(vec![OsString::from("/usr/bin/true")]);
    assert!(outcome.is_ok(), "a harmless command failed: {outcome:?}");
}

fn is_bare_word(command: &str, token: &str) -> bool {
    command.split(' ').any(|word| word == token)
}

/// Catches: an argument with shell metacharacters but no whitespace is shown bare,
/// so the person reads `echo a;b` as two commands (or `$HOME`/`*` as expanded)
/// although the exec runs one program with those literal arguments.
#[test]
fn arguments_a_shell_would_reinterpret_are_quoted_in_the_question() {
    let Some(fixture) = open_widening(Confinement::Workspace, Seat::new(no)) else {
        return;
    };
    let target = write_file(&fixture.outside.join("f"), "x");
    let _ = fixture.run(vec![
        OsString::from("/bin/sh"),
        OsString::from("-c"),
        OsString::from("printf '%s: Operation not permitted\\n' \"$5\" >&2; exit 1"),
        OsString::from("a;b"),
        OsString::from("$HOME"),
        OsString::from("*"),
        OsString::from("x\\y"),
        OsString::from("(z)"),
        target.into_os_string(),
    ]);
    let commands = fixture.seat.commands();
    assert_eq!(commands.len(), 1, "{commands:?}");
    for token in ["a;b", "$HOME", "*", "x\\y", "(z)"] {
        assert!(
            !is_bare_word(&commands[0], token),
            "{token} is shown unquoted: {}",
            commands[0]
        );
    }
}

/// Catches: the command line is cut at a fixed length, so a long `sh -c` script (or
/// the arguments after it) is hidden and the person approves re-running a command
/// they could not read. Either the whole command is shown or no question is asked.
#[test]
fn a_question_never_hides_the_end_of_the_command_it_re_runs() {
    let Some(fixture) = open(Confinement::Workspace, Seat::new(no)) else {
        return;
    };
    let target = write_file(&fixture.outside.join("f"), "x");
    let script = format!(
        "# {}\nprintf '%s: Operation not permitted\\n' \"$0\" >&2; exit 1 # END_OF_SCRIPT",
        "x".repeat(900)
    );
    let _ = fixture.sh(&script, &[&target]);
    for command in fixture.seat.commands() {
        assert!(
            command.contains("END_OF_SCRIPT") && command.contains(&*target.to_string_lossy()),
            "the person was asked about a command they could not read to the end: {} chars, ends {:?}",
            command.len(),
            command
                .chars()
                .rev()
                .take(20)
                .collect::<String>()
                .chars()
                .rev()
                .collect::<String>()
        );
    }
}

/// Catches: an argument carrying ESC, CR, newline or a bidi override is only
/// wrapped in quotes, so the host terminal still obeys the control bytes and the
/// person reads a command different from the one that is re-run (a line erased,
/// a fake "allowed" line, text reordered).
#[test]
fn control_and_bidi_characters_in_arguments_never_reach_the_question_raw() {
    let Some(fixture) = open_widening(Confinement::Workspace, Seat::new(no)) else {
        return;
    };
    let target = write_file(&fixture.outside.join("f"), "x");
    let _ = fixture.run(vec![
        OsString::from("/bin/sh"),
        OsString::from("-c"),
        OsString::from("printf '%s: Operation not permitted\\n' \"$0\" >&2; exit 1"),
        target.clone().into_os_string(),
        OsString::from("\u{1b}[2K\rharmless"),
        OsString::from("a\nAllow all? yes"),
        OsString::from("\u{202e}txt.exe"),
    ]);
    let commands = fixture.seat.commands();
    assert_eq!(commands.len(), 1, "{commands:?}");
    assert!(
        !commands[0]
            .chars()
            .any(|c| c.is_control() || ('\u{202a}'..='\u{202e}').contains(&c)),
        "the question carries raw control characters: {:?}",
        commands[0]
    );
}

/// Catches: a command line that names a symlink chain ending at an ancestor of
/// the workspace still anchors everything below that ancestor, because only the
/// spelled path (not the resolved one) is compared with the workspace.
#[test]
fn a_symlink_chain_to_an_ancestor_of_the_workspace_anchors_nothing() {
    let Some(fixture) = open(Confinement::Workspace, Seat::new(no)) else {
        return;
    };
    let target = write_file(&fixture.outside.join("f"), "x");
    let ancestor = fixture
        .workspace
        .as_path()
        .parent()
        .expect("workspace parent")
        .to_path_buf();
    let second = fixture.outside.join("hop2");
    let first = fixture.outside.join("hop1");
    std::os::unix::fs::symlink(&ancestor, &second).expect("hop2");
    std::os::unix::fs::symlink(&second, &first).expect("hop1");
    let _ = fixture.run(vec![
        OsString::from("/bin/sh"),
        OsString::from("-c"),
        OsString::from(format!(
            "printf '%s: Operation not permitted\\n' '{}' >&2; exit 1",
            target.display()
        )),
        first.into_os_string(),
    ]);
    assert!(
        fixture.seat.asked().is_empty(),
        "a path under an ancestor of the workspace was asked: {:?}",
        fixture.seat.asked()
    );
}

/// Catches: a relative argument is resolved against the process cwd and anchors
/// an absolute path the command line never spelled (`../outside`).
#[test]
fn a_relative_argument_does_not_anchor_an_absolute_refusal() {
    let Some(fixture) = open(Confinement::Workspace, Seat::new(no)) else {
        return;
    };
    let target = write_file(&fixture.outside.join("f"), "x");
    let relative = pathdiff_from(fixture.workspace.as_path(), &fixture.outside);
    let _ = fixture.run(vec![
        OsString::from("/bin/sh"),
        OsString::from("-c"),
        OsString::from(format!(
            "printf '%s: Operation not permitted\\n' '{}' >&2; exit 1",
            target.display()
        )),
        relative.into_os_string(),
    ]);
    assert!(
        fixture.seat.asked().is_empty(),
        "a relative argument anchored a question: {:?}",
        fixture.seat.asked()
    );
}

fn pathdiff_from(workspace: &Path, target: &Path) -> PathBuf {
    let mut relative = PathBuf::new();
    for _ in workspace.components().skip(1) {
        relative.push("..");
    }
    relative.join(target.strip_prefix("/").expect("absolute"))
}

/// Catches: the renderable filter drops the whole stored list when one entry is
/// unrenderable (control character, non-UTF-8, relative), so a valid grant next
/// to a hand-edited one stops working.
#[test]
fn a_valid_stored_grant_still_applies_beside_unrenderable_ones() {
    use std::os::unix::ffi::OsStringExt;

    let seat = Seat::new(no);
    let Some(fixture) = open_widening(Confinement::Workspace, Arc::clone(&seat)) else {
        return;
    };
    let target = write_file(&fixture.outside.join("f"), "stored");
    {
        let mut granted = seat.granted.lock().unwrap();
        granted.push(SandboxGrant::new(
            CanonPath::new("/opt/a\nb"),
            SandboxAccess::Read,
        ));
        granted.push(SandboxGrant::new(
            CanonPath::new(PathBuf::from(OsString::from_vec(b"/opt/\xff".to_vec()))),
            SandboxAccess::Read,
        ));
        granted.push(SandboxGrant::new(
            CanonPath::new("relative/x"),
            SandboxAccess::Read,
        ));
        granted.push(SandboxGrant::new(
            CanonPath::new(fixture.outside.clone()),
            SandboxAccess::Read,
        ));
    }
    let outcome = fixture.cat(&[&target]);
    assert!(
        outcome.is_ok(),
        "valid grant ignored beside bad ones: {}",
        note_of(&outcome)
    );
    assert!(seat.asked().is_empty());
}

use super::{command_line, printed_path};
use ego_core::permit::{EnvPolicy, SpawnSpec};

// Lines recorded from macOS `cat`, `ls` and `touch` under `sandbox-exec`
// with a profile that does not open the path, 2026-10-02.
#[test]
fn recorded_macos_tool_lines_yield_their_path() {
    assert_eq!(
        printed_path("cat: /Users/u/Gits/.tmp/x//outside.txt: Operation not permitted"),
        Some("/Users/u/Gits/.tmp/x//outside.txt")
    );
    assert_eq!(
        printed_path("touch: /Users/u/new: Operation not permitted"),
        Some("/Users/u/new")
    );
    assert_eq!(
        printed_path("/bin/sh: /Users/u/o2: Operation not permitted"),
        Some("/Users/u/o2")
    );
}

#[test]
fn a_refused_program_names_its_path() {
    assert_eq!(
        printed_path(
            "sandbox-exec: execvp() of '/opt/homebrew/Cellar/p/3.14/bin/python3.14' failed: Operation not permitted"
        ),
        Some("/opt/homebrew/Cellar/p/3.14/bin/python3.14")
    );
    assert_eq!(
        printed_path("sandbox-exec: execvp() of 'prog' failed: Operation not permitted"),
        None
    );
}

#[test]
fn other_errors_and_relative_paths_are_not_sandbox_questions() {
    assert_eq!(
        printed_path("cat: /Users/u/missing: No such file or directory"),
        None
    );
    assert_eq!(
        printed_path("cat: notes.txt: Operation not permitted"),
        None
    );
    assert_eq!(printed_path("Operation not permitted"), None);
}

#[test]
fn the_command_line_quotes_what_a_person_could_misread() {
    let spec = SpawnSpec::new(
        ["/bin/sh", "-c", "echo hi there", ""]
            .map(Into::into)
            .to_vec(),
        None,
        None,
        EnvPolicy::Inherited,
    )
    .expect("spawn spec");
    assert_eq!(command_line(&spec), "/bin/sh -c 'echo hi there' ''");
}

// Catches: Rust errno suffixes and EACCES silently lose the sandbox grant request.
#[test]
fn errno_refusals_request_only_the_anchored_stderr_path() {
    use ego_core::permit::{
        CanonPath, Confinement, RunConfinement, SandboxAccess, SandboxGrant, StreamProvenance,
        ToolContentBlock, ToolOutputEncoding, ToolOutputProvenance, ToolResult,
    };
    let dirs = tempfile::tempdir().expect("test roots");
    let workspace = dirs.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let target = std::fs::canonicalize(dirs.path())
        .expect("canonical roots")
        .join("output");
    let printed = target.display().to_string();
    let confinement = RunConfinement::new(Confinement::Workspace, false, CanonPath::new(workspace))
        .expect("confinement");
    let spec = SpawnSpec::new(
        vec!["/bin/cat".into(), target.clone().into_os_string()],
        None,
        None,
        EnvPolicy::Inherited,
    )
    .expect("spawn");
    // Native Rust formatting supplies the errno suffix, rather than assuming
    // what Rust prints. Plain forms match command-line tools' diagnostics.
    let errors = [
        std::io::Error::from_raw_os_error(1).to_string(),
        std::io::Error::from_raw_os_error(13).to_string(),
        "Operation not permitted".to_owned(),
        "Permission denied".to_owned(),
    ];
    let result = |text: String, stream| {
        ToolResult::new(vec![ToolContentBlock {
            text,
            provenance: ToolOutputProvenance::Process {
                stream,
                encoding: ToolOutputEncoding::Utf8,
                truncated: false,
                omitted_bytes: 0,
            },
            full_output: None,
        }])
    };
    for error in errors {
        let stderr = result(
            format!("tool: {printed}: {error}"),
            StreamProvenance::Stderr,
        );
        let denial = super::analyze(&spec, &stderr, &confinement, &[])
            .unwrap_or_else(|| panic!("missing refusal: {error}"));
        assert_eq!(
            denial.request,
            Some(SandboxGrant::new(
                CanonPath::new(target.clone()),
                SandboxAccess::Write,
            )),
            "{error}"
        );
        let stdout = result(
            format!("tool: {printed}: {error}"),
            StreamProvenance::Stdout,
        );
        assert!(super::analyze(&spec, &stdout, &confinement, &[]).is_none());
        let trailing = result(
            format!("tool: {printed}: {error} ignored"),
            StreamProvenance::Stderr,
        );
        assert!(super::analyze(&spec, &trailing, &confinement, &[]).is_none());
    }
    let missing = result(
        format!("tool: {printed}: {}", std::io::Error::from_raw_os_error(2)),
        StreamProvenance::Stderr,
    );
    assert!(super::analyze(&spec, &missing, &confinement, &[]).is_none());
    let unanchored = SpawnSpec::new(vec!["/bin/cat".into()], None, None, EnvPolicy::Inherited)
        .expect("unanchored spawn");
    let stderr = result(
        format!("{printed}: Permission denied"),
        StreamProvenance::Stderr,
    );
    let refusal = super::analyze(&unanchored, &stderr, &confinement, &[]).expect("refusal hint");
    assert!(
        refusal.request.is_none(),
        "stderr alone must never authorize a request"
    );
}

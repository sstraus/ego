//! Linux Landlock confinement and the private re-exec helper protocol.
//!
//! Landlock restricts the calling thread.  The workspace denies unsafe code,
//! so applying it from `CommandExt::pre_exec` is not an available design.  Ego
//! instead re-execs its own absolute binary, enters this helper before Tokio or
//! any other threads exist, installs the ruleset through landlock's safe API,
//! and then replaces the helper with the exact authorised argv.

#![cfg(target_os = "linux")]

use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use ego_core::permit::{CanonPath, Confinement, ExecCaps, ProcessError, RunConfinement};
use landlock::{
    AccessFs, BitFlags, CompatLevel, Compatible as _, LandlockStatus, PathBeneath, PathFd,
    PathFdError, RestrictionStatus, Ruleset, RulesetAttr as _, RulesetCreated,
    RulesetCreatedAttr as _, RulesetError, RulesetStatus, make_bitflags,
};

/// Private argv marker recognised before the ordinary CLI creates Tokio.
const HELPER_TOKEN: &str = "__ego_internal_landlock_exec_v1";

/// The words between the helper executable and the helper protocol.
///
/// They are shaped so that *both* executables that can host this library
/// dispatch the helper: the shipped binary intercepts them before clap, and a
/// libtest binary runs the `#[ignore]`d test named for the token, which calls
/// the same dispatcher. Without them a libtest host read the protocol as
/// test-name filters, ran whichever real tests matched `workspace` or a path,
/// and every Landlock probe child timed out instead of running its target.
///
/// Known limit of the libtest host only: every word after `--` is still an
/// exact test-name filter there, so a target argv that contains the exact
/// name of an `#[ignore]`d test makes libtest run that test too, unconfined,
/// beside the helper. No test targets its own binary, which a confined child
/// may not execute anyway; the shipped binary has no such names.
const HELPER_ENVELOPE: [&str; 5] = ["--ignored", "--exact", HELPER_TOKEN, "--nocapture", "--"];

const HELPER_FAILURE: u8 = 125;
const MAX_RUNTIME_ROOTS: usize = 128;
const BACKEND_REVISION: &str = "landlock-safe-reexec-v1-abi9";

/// One immutable Linux confinement mechanism prepared for an effective run.
/// Its capability value is only an upper bound; common production probes must
/// still demonstrate every bit before `ExecObservation` can publish it.
pub(crate) struct LandlockBackend {
    helper: PathBuf,
    level: Level,
    workspace: PathBuf,
    private_scratch: PathBuf,
    runtime_roots: Vec<PathBuf>,
    maximum_caps: ExecCaps,
}

impl LandlockBackend {
    pub(crate) fn prepare(
        policy: &RunConfinement,
        private_scratch: &CanonPath,
        derived_runtime_roots: &[CanonPath],
    ) -> Result<Self, ProcessError> {
        let level = match policy.level() {
            Confinement::Ro => Level::Ro,
            Confinement::Workspace => Level::Workspace,
            Confinement::Off => {
                return Err(ProcessError::Invariant(
                    "the Landlock backend was selected for sandbox off".into(),
                ));
            }
        };
        validate_private_directory(private_scratch.as_path())?;
        let runtime_roots = derived_runtime_roots
            .iter()
            .map(CanonPath::as_path)
            .map(Path::to_path_buf)
            .collect::<Vec<_>>();
        if runtime_roots.len() > MAX_RUNTIME_ROOTS {
            return Err(ProcessError::Backend(format!(
                "too many derived Landlock runtime roots: {}",
                runtime_roots.len()
            )));
        }
        for root in &runtime_roots {
            validate_absolute_existing("runtime root", root)
                .map_err(|error| ProcessError::Backend(error.to_string()))?;
        }

        let helper = fs::canonicalize(
            std::env::current_exe()
                .map_err(|error| ProcessError::Backend(format!("cannot locate ego: {error}")))?,
        )
        .map_err(|error| ProcessError::Backend(format!("cannot resolve ego: {error}")))?;
        let metadata = fs::metadata(&helper).map_err(|error| {
            ProcessError::Backend(format!(
                "Landlock helper {} is unavailable: {error}",
                helper.display()
            ))
        })?;
        if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
            return Err(ProcessError::Backend(format!(
                "Landlock helper is not an executable file: {}",
                helper.display()
            )));
        }

        let maximum_caps = match level {
            Level::Ro => ExecCaps::READ_SCOPED.union(ExecCaps::WRITE_DENIED),
            Level::Workspace => ExecCaps::READ_SCOPED.union(ExecCaps::WRITE_SCOPED),
        };
        Ok(Self {
            helper,
            level,
            workspace: policy.workspace().as_path().to_path_buf(),
            private_scratch: private_scratch.as_path().to_path_buf(),
            runtime_roots,
            maximum_caps,
        })
    }

    pub(crate) const fn name(&self) -> &'static str {
        "landlock"
    }

    pub(crate) const fn revision(&self) -> &'static str {
        BACKEND_REVISION
    }

    pub(crate) const fn maximum_demonstrable_caps(&self) -> ExecCaps {
        self.maximum_caps
    }

    pub(crate) fn launch_prefix(&self) -> super::backend::LaunchPrefix {
        let mut arguments =
            Vec::with_capacity(HELPER_ENVELOPE.len() + 4 + self.runtime_roots.len());
        arguments.extend(HELPER_ENVELOPE.map(OsString::from));
        arguments.push(OsString::from(self.level.marker()));
        arguments.push(self.workspace.as_os_str().to_owned());
        arguments.push(self.private_scratch.as_os_str().to_owned());
        arguments.push(OsString::from(self.runtime_roots.len().to_string()));
        arguments.extend(
            self.runtime_roots
                .iter()
                .map(|root| root.as_os_str().to_owned()),
        );
        super::backend::LaunchPrefix {
            program: Some(self.helper.as_os_str().to_owned()),
            args_before_target: arguments,
        }
    }
}

/// Fixed Linux roots needed by installed executables and the dynamic loader.
/// Missing platform alternatives are ignored by exact name; every existing
/// candidate must canonicalize successfully and is deduplicated by inode path.
pub(crate) fn derived_runtime_roots() -> Result<Vec<CanonPath>, ProcessError> {
    let candidates = [
        "/usr",
        "/bin",
        "/sbin",
        "/lib",
        "/lib64",
        "/etc/ld.so.cache",
        "/etc/ld.so.preload",
        "/etc/nsswitch.conf",
        "/etc/passwd",
        "/etc/group",
        "/etc/localtime",
        "/nix/store",
        "/run/current-system/sw",
        "/gnu/store",
    ];
    let mut roots = BTreeSet::new();
    for candidate in candidates {
        match fs::canonicalize(candidate) {
            Ok(root) => {
                roots.insert(root);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(ProcessError::Backend(format!(
                    "cannot resolve Landlock runtime root {candidate}: {error}"
                )));
            }
        }
    }
    if roots.is_empty() {
        return Err(ProcessError::Backend(
            "no fixed Linux runtime root is available".into(),
        ));
    }
    Ok(roots.into_iter().map(CanonPath::new).collect())
}

fn validate_private_directory(path: &Path) -> Result<(), ProcessError> {
    if !path.is_absolute() {
        return Err(ProcessError::Backend(format!(
            "Landlock private scratch is not absolute: {}",
            path.display()
        )));
    }
    let metadata = fs::metadata(path).map_err(|error| {
        ProcessError::Backend(format!(
            "Landlock private scratch {} is unavailable: {error}",
            path.display()
        ))
    })?;
    if !metadata.is_dir() {
        return Err(ProcessError::Backend(format!(
            "Landlock private scratch is not a directory: {}",
            path.display()
        )));
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(ProcessError::Backend(format!(
            "Landlock private scratch is not private: {}",
            path.display()
        )));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Level {
    Ro,
    Workspace,
}

impl Level {
    fn marker(self) -> &'static str {
        match self {
            Self::Ro => "ro",
            Self::Workspace => "workspace",
        }
    }

    fn parse(value: &OsStr) -> Result<Self, HelperError> {
        match value.to_str() {
            Some("ro") => Ok(Self::Ro),
            Some("workspace") => Ok(Self::Workspace),
            _ => Err(HelperError::Protocol(
                "the Landlock level must be ro or workspace".into(),
            )),
        }
    }
}

/// Dispatch the private helper invocation, or return `None` for an ordinary
/// CLI invocation.  Successful dispatch never returns: `exec` replaces ego.
pub fn dispatch_helper<I>(arguments: I) -> Option<ExitCode>
where
    I: IntoIterator<Item = OsString>,
{
    let mut arguments = arguments.into_iter();
    let _executable = arguments.next()?;
    for expected in HELPER_ENVELOPE {
        if arguments.next().as_deref() != Some(OsStr::new(expected)) {
            return None;
        }
    }

    Some(match run_helper(arguments) {
        Ok(never) => never,
        Err(error) => {
            eprintln!("ego Landlock helper refused execution: {error}");
            ExitCode::from(HELPER_FAILURE)
        }
    })
}

fn run_helper(mut arguments: impl Iterator<Item = OsString>) -> Result<ExitCode, HelperError> {
    ensure_no_ambient_descriptors()?;

    let level = Level::parse(&required_argument(&mut arguments, "level")?)?;
    let workspace = PathBuf::from(required_argument(&mut arguments, "workspace")?);
    let private_scratch = PathBuf::from(required_argument(&mut arguments, "private scratch")?);
    let root_count = required_argument(&mut arguments, "runtime root count")?
        .to_str()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|count| *count <= MAX_RUNTIME_ROOTS)
        .ok_or_else(|| HelperError::Protocol("invalid runtime root count".into()))?;
    let runtime_roots = (0..root_count)
        .map(|_| required_argument(&mut arguments, "runtime root").map(PathBuf::from))
        .collect::<Result<Vec<_>, _>>()?;
    let target_argv = arguments.collect::<Vec<_>>();

    validate_absolute_directory("workspace", &workspace)?;
    validate_absolute_directory("private scratch", &private_scratch)?;
    for root in &runtime_roots {
        validate_absolute_existing("runtime root", root)?;
    }
    if target_argv.is_empty() {
        return Err(HelperError::Protocol(
            "the Landlock target argv is empty".into(),
        ));
    }

    install_ruleset(level, &workspace, &private_scratch, &runtime_roots)?;

    let mut target = Command::new(&target_argv[0]);
    target.args(&target_argv[1..]);
    let error = target.exec();
    Err(HelperError::Exec {
        executable: target_argv[0].clone(),
        reason: error.to_string(),
    })
}

/// Refuse ambient authority rather than relying on Landlock to revoke it.
/// The temporary `/proc/self/fd` descriptor is dropped before the second
/// pass, so every remaining descriptor above stderr was inherited.
fn ensure_no_ambient_descriptors() -> Result<(), HelperError> {
    let entries = fs::read_dir("/proc/self/fd")
        .map_err(|error| HelperError::Descriptors(error.to_string()))?;
    let mut candidates = BTreeSet::new();
    for entry in entries {
        let entry = entry.map_err(|error| HelperError::Descriptors(error.to_string()))?;
        let name = entry.file_name();
        let descriptor = name
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
            .ok_or_else(|| {
                HelperError::Descriptors(format!("unexpected /proc/self/fd entry {name:?}"))
            })?;
        if descriptor > 2 {
            candidates.insert(descriptor);
        }
    }

    for descriptor in candidates {
        let path = Path::new("/proc/self/fd").join(descriptor.to_string());
        match fs::symlink_metadata(path) {
            Ok(_) => {
                return Err(HelperError::Descriptors(format!(
                    "unexpected inherited file descriptor {descriptor}"
                )));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(HelperError::Descriptors(error.to_string())),
        }
    }
    Ok(())
}

fn install_ruleset(
    level: Level,
    workspace: &Path,
    private_scratch: &Path,
    runtime_roots: &[PathBuf],
) -> Result<(), HelperError> {
    let required = required_v3_access();
    let optional = optional_known_access();
    let mut ruleset = Ruleset::default()
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(required)?
        // ABI 5 and 9 rights are still handled when the running kernel knows
        // them, without needlessly rejecting an otherwise complete ABI 3/4
        // confinement.  Paired probes remain the authority for capabilities.
        .set_compatibility(CompatLevel::BestEffort)
        .handle_access(optional)?
        .create()?;

    let mut readable_roots = runtime_roots.iter().cloned().collect::<BTreeSet<_>>();
    readable_roots.insert(workspace.to_path_buf());
    for root in readable_roots {
        ruleset = add_read_rule(ruleset, &root)?;
    }

    if level == Level::Workspace {
        for root in [workspace, private_scratch] {
            ruleset = add_write_rules(ruleset, root)?;
        }
    }

    verify_restriction(ruleset.restrict_self()?)
}

fn add_read_rule(ruleset: RulesetCreated, root: &Path) -> Result<RulesetCreated, HelperError> {
    let metadata = fs::metadata(root).map_err(|error| HelperError::Root {
        path: root.to_path_buf(),
        reason: error.to_string(),
    })?;
    let access = if metadata.is_dir() {
        make_bitflags!(AccessFs::{Execute | ReadFile | ReadDir})
    } else {
        make_bitflags!(AccessFs::{Execute | ReadFile})
    };
    let rule = PathBeneath::new(PathFd::new(root)?, access)
        .set_compatibility(CompatLevel::HardRequirement);
    Ok(ruleset.add_rule(rule)?)
}

fn add_write_rules(ruleset: RulesetCreated, root: &Path) -> Result<RulesetCreated, HelperError> {
    let required = PathBeneath::new(PathFd::new(root)?, required_v3_access())
        .set_compatibility(CompatLevel::HardRequirement);
    let ruleset = ruleset.add_rule(required)?;
    let optional = PathBeneath::new(PathFd::new(root)?, optional_known_access())
        .set_compatibility(CompatLevel::BestEffort);
    Ok(ruleset.add_rule(optional)?)
}

fn required_v3_access() -> BitFlags<AccessFs> {
    make_bitflags!(AccessFs::{
        Execute
            | WriteFile
            | ReadFile
            | ReadDir
            | RemoveDir
            | RemoveFile
            | MakeChar
            | MakeDir
            | MakeReg
            | MakeSock
            | MakeFifo
            | MakeBlock
            | MakeSym
            | Refer
            | Truncate
    })
}

fn optional_known_access() -> BitFlags<AccessFs> {
    make_bitflags!(AccessFs::{IoctlDev | ResolveUnix})
}

fn verify_restriction(status: RestrictionStatus) -> Result<(), HelperError> {
    if !status.no_new_privs {
        return Err(HelperError::Enforcement(
            "no_new_privs was not enforced".into(),
        ));
    }
    if status.ruleset == RulesetStatus::NotEnforced {
        return Err(HelperError::Enforcement(
            "the Landlock ruleset was not enforced".into(),
        ));
    }
    match status.landlock {
        // landlock clamps a newer kernel ABI to the highest ABI understood by
        // this exact crate release.  `kernel_abi` is informational only; all
        // requested AccessFs rights remain the explicit, audited set above.
        LandlockStatus::Available { .. } => Ok(()),
        LandlockStatus::NotEnabled => Err(HelperError::Enforcement(
            "Landlock is not enabled on this host".into(),
        )),
        LandlockStatus::NotImplemented => Err(HelperError::Enforcement(
            "Landlock is not implemented by this kernel".into(),
        )),
    }
}

fn required_argument(
    arguments: &mut impl Iterator<Item = OsString>,
    name: &str,
) -> Result<OsString, HelperError> {
    arguments
        .next()
        .ok_or_else(|| HelperError::Protocol(format!("missing {name}")))
}

fn validate_absolute_directory(name: &str, path: &Path) -> Result<(), HelperError> {
    validate_absolute_existing(name, path)?;
    if !path.is_dir() {
        return Err(HelperError::Root {
            path: path.to_path_buf(),
            reason: format!("{name} is not a directory"),
        });
    }
    Ok(())
}

fn validate_absolute_existing(name: &str, path: &Path) -> Result<(), HelperError> {
    if !path.is_absolute() {
        return Err(HelperError::Root {
            path: path.to_path_buf(),
            reason: format!("{name} is not absolute"),
        });
    }
    fs::metadata(path).map_err(|error| HelperError::Root {
        path: path.to_path_buf(),
        reason: format!("could not inspect {name}: {error}"),
    })?;
    Ok(())
}

#[derive(Debug)]
pub(crate) enum HelperError {
    Protocol(String),
    Descriptors(String),
    Root {
        path: PathBuf,
        reason: String,
    },
    PathFd(PathFdError),
    Landlock(RulesetError),
    Enforcement(String),
    Exec {
        executable: OsString,
        reason: String,
    },
}

impl fmt::Display for HelperError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Protocol(reason) => write!(f, "invalid helper protocol: {reason}"),
            Self::Descriptors(reason) => write!(f, "descriptor check failed: {reason}"),
            Self::Root { path, reason } => {
                write!(f, "invalid Landlock root {}: {reason}", path.display())
            }
            Self::PathFd(error) => write!(f, "could not open a Landlock root: {error}"),
            Self::Landlock(error) => write!(f, "Landlock setup failed: {error}"),
            Self::Enforcement(reason) => write!(f, "Landlock was not enforced: {reason}"),
            Self::Exec { executable, reason } => {
                write!(f, "could not execute {executable:?}: {reason}")
            }
        }
    }
}

impl std::error::Error for HelperError {}

impl From<RulesetError> for HelperError {
    fn from(error: RulesetError) -> Self {
        Self::Landlock(error)
    }
}

impl From<PathFdError> for HelperError {
    fn from(error: PathFdError) -> Self {
        Self::PathFd(error)
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::path::PathBuf;

    use ego_core::permit::ExecCaps;

    use super::{LandlockBackend, Level, dispatch_helper};

    /// The prefix the backend emits is the envelope the dispatcher accepts, and
    /// every guard word is load-bearing: drop one and a libtest host either
    /// skips the helper test or swallows the protocol as test-name filters.
    #[test]
    fn the_helper_envelope_is_the_one_both_hosts_dispatch() {
        let backend = LandlockBackend {
            helper: PathBuf::from("/ego"),
            level: Level::Workspace,
            workspace: PathBuf::from("/workspace"),
            private_scratch: PathBuf::from("/scratch"),
            runtime_roots: vec![PathBuf::from("/usr")],
            maximum_caps: ExecCaps::empty(),
        };
        let prefix = backend.launch_prefix();
        let expected = [
            "--ignored",
            "--exact",
            "__ego_internal_landlock_exec_v1",
            "--nocapture",
            "--",
            "workspace",
            "/workspace",
            "/scratch",
            "1",
            "/usr",
        ]
        .map(OsString::from);
        assert_eq!(prefix.program, Some(OsString::from("/ego")));
        assert_eq!(prefix.args_before_target, expected);

        // A protocol the helper refuses before any effect: the dispatcher
        // must still claim it, or the ordinary CLI would parse it.
        let mut argv = vec![OsString::from("/ego")];
        argv.extend(expected[..5].iter().cloned());
        argv.push(OsString::from("not-a-level"));
        assert!(dispatch_helper(argv.clone()).is_some());

        for dropped in 1..=5 {
            let mut partial = argv.clone();
            partial.remove(dropped);
            assert!(
                dispatch_helper(partial).is_none(),
                "dropping argv[{dropped}] must stop being the helper envelope"
            );
        }
    }
}

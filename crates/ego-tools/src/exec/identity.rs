//! Observe images through the process adapter, without executing or disclosing them.

use std::ffi::{OsStr, OsString};
use std::fs::{self, File, Metadata};
use std::io::Read;
use std::path::{Path, PathBuf};

use ego_core::permit::analysis::{ExecutableFileIdentity, ExecutionIdentity, IdentityFailure};
use ego_core::permit::{
    CanonPath, EnvName, EnvPolicy, ExplicitEnvBinding, ProcessError, SpawnSpec,
};
use sha2::{Digest as _, Sha256};

pub(super) fn capture(spec: &SpawnSpec) -> Result<SpawnSpec, ProcessError> {
    let snapshot = super::backend::environment(spec)?;
    let cwd = spec
        .cwd()
        .map(|cwd| cwd.as_path().to_path_buf())
        .map(Ok)
        .unwrap_or_else(std::env::current_dir)
        .map_err(|_| {
            ProcessError::Invariant("cannot capture execution working directory".into())
        })?;
    let cwd = fs::canonicalize(cwd).map_err(|_| {
        ProcessError::Invariant("cannot resolve execution working directory".into())
    })?;
    if !cwd.is_dir() {
        return Err(ProcessError::Invariant(
            "execution working directory is not a directory".into(),
        ));
    }
    let env = EnvPolicy::exact(
        snapshot
            .iter()
            .map(|(name, value)| {
                let name = EnvName::new(name.clone()).map_err(|_| {
                    ProcessError::Invariant("invalid captured environment name".into())
                })?;
                ExplicitEnvBinding::from_native(name, value.clone()).map_err(|_| {
                    ProcessError::Invariant("invalid captured environment encoding".into())
                })
            })
            .collect::<Result<Vec<_>, _>>()?,
    )
    .map_err(|_| ProcessError::Invariant("invalid captured environment".into()))?;
    let mut identity = ExecutionIdentity {
        target: CanonPath::new(cwd.join(&spec.argv()[0])),
        files: Vec::new(),
        failures: Vec::new(),
        dynamic_code: false,
    };
    #[cfg(not(unix))]
    identity.failures.push(IdentityFailure::UnsupportedPlatform);
    match resolve(&spec.argv()[0], &cwd, &snapshot) {
        Some(named) => {
            observe(&named, &mut identity);
            if let Some(file) = identity.files.first() {
                identity.target = file.resolved.clone();
            }
            recognize(&named, spec.argv(), &cwd, &snapshot, &mut identity);
        }
        None => identity.failures.push(IdentityFailure::Resolution),
    }
    // Static shell syntax names candidates; only this host adapter resolves and
    // observes them. The policy checks both named and resolved trusted roots.
    let argv: Vec<_> = spec
        .argv()
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    let shell = ego_core::permit::shell::observe(&argv);
    if shell.complete && shell.read_only {
        for program in shell.programs {
            if let Some(named) = resolve(OsStr::new(&program), &cwd, &snapshot)
                && !identity
                    .files
                    .iter()
                    .any(|file| file.named.as_path() == named)
            {
                observe(&named, &mut identity);
            }
        }
    }
    let captured = SpawnSpec::new(
        spec.argv().to_vec(),
        Some(CanonPath::new(cwd)),
        spec.timeout(),
        env.clone(),
    )
    .map_err(|_| ProcessError::Invariant("captured execution specification is invalid".into()))?;
    Ok(captured.with_execution_identity(env, identity))
}

/// Refuse, as a launch failure, a target `execvp` could not start: nothing
/// executable at the named path, or on the spawn's own `PATH` for a bare name.
///
/// A wrapper backend spawns its own program first, so without this a missing
/// or non-executable target surfaced as the wrapper's exit status, not as the
/// launch failure a direct spawn reports for the same argv.
pub(super) fn require_launchable(program: &OsStr, spec: &SpawnSpec) -> Result<(), ProcessError> {
    let cwd = spec
        .cwd()
        .map(|cwd| cwd.as_path().to_path_buf())
        .map(Ok)
        .unwrap_or_else(std::env::current_dir)
        .map_err(|_| {
            ProcessError::Invariant("cannot resolve execution working directory".into())
        })?;
    let env = super::backend::environment(spec)?;
    match resolve(program, &cwd, &env) {
        Some(_) => Ok(()),
        None => Err(ProcessError::Launch {
            executable: program.to_owned(),
            reason: "no executable file at the named path or on PATH".into(),
        }),
    }
}

fn resolve(program: &OsStr, cwd: &Path, env: &[(OsString, OsString)]) -> Option<PathBuf> {
    let path = Path::new(program);
    if program.is_empty() {
        return None;
    }
    if path.is_absolute() || path.components().count() > 1 {
        let candidate = if path.is_absolute() {
            path.to_path_buf()
        } else {
            cwd.join(path)
        };
        return executable_candidate(candidate);
    }
    let path = env
        .iter()
        .find(|(name, _)| super::backend::env_name_eq(name, OsStr::new("PATH")))?
        .1
        .as_os_str();
    std::env::split_paths(path)
        .map(|root| {
            let root = if root.is_absolute() {
                root
            } else {
                cwd.join(root)
            };
            root.join(program)
        })
        .find_map(executable_candidate)
}

fn executable_candidate(candidate: PathBuf) -> Option<PathBuf> {
    if executable(&candidate) {
        return Some(candidate);
    }
    #[cfg(windows)]
    {
        if candidate.extension().is_none() {
            let with_extension = candidate.with_extension("exe");
            if executable(&with_extension) {
                return Some(with_extension);
            }
        }
    }
    None
}

fn executable(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        metadata.is_file()
    }
}

fn observe(path: &Path, identity: &mut ExecutionIdentity) {
    match hash_file(path) {
        Ok(file) => identity.files.push(file),
        Err(reason) => {
            identity.failures.push(reason);
            // Retain what the host can observe for a one-shot human/yolo
            // decision; missing content never becomes reusable allow.
            if let Ok(file) = metadata_only(path) {
                identity.files.push(file);
            }
        }
    }
}

fn metadata_only(named: &Path) -> Result<ExecutableFileIdentity, IdentityFailure> {
    let resolved = fs::canonicalize(named).map_err(|_| IdentityFailure::Resolution)?;
    let metadata = fs::metadata(&resolved).map_err(|_| IdentityFailure::Read)?;
    if !metadata.is_file() {
        return Err(IdentityFailure::Read);
    }
    Ok(ExecutableFileIdentity {
        named: CanonPath::new(named),
        resolved: CanonPath::new(resolved),
        content: None,
        platform: platform(&metadata)?,
    })
}

fn hash_file(named: &Path) -> Result<ExecutableFileIdentity, IdentityFailure> {
    let resolved = fs::canonicalize(named).map_err(|_| IdentityFailure::Resolution)?;
    let mut file = File::open(&resolved).map_err(|_| IdentityFailure::Read)?;
    let before = file.metadata().map_err(|_| IdentityFailure::Read)?;
    if !before.is_file() {
        return Err(IdentityFailure::Read);
    }
    let before = platform(&before)?;
    let mut digest = Sha256::new();
    let mut chunk = [0u8; 64 * 1024];
    loop {
        let count = file.read(&mut chunk).map_err(|_| IdentityFailure::Read)?;
        if count == 0 {
            break;
        }
        digest.update(&chunk[..count]);
    }
    let after = platform(&file.metadata().map_err(|_| IdentityFailure::Read)?)?;
    let current = platform(&fs::metadata(named).map_err(|_| IdentityFailure::Changed)?)?;
    if before != after
        || after != current
        || fs::canonicalize(named).ok().as_ref() != Some(&resolved)
    {
        return Err(IdentityFailure::Changed);
    }
    Ok(ExecutableFileIdentity {
        named: CanonPath::new(named),
        resolved: CanonPath::new(resolved),
        content: Some(digest.finalize().into()),
        platform: after,
    })
}

#[cfg(unix)]
fn platform(metadata: &Metadata) -> Result<Vec<u8>, IdentityFailure> {
    use std::os::unix::fs::MetadataExt;
    let mut bytes = b"unix-v1".to_vec();
    for value in [
        metadata.dev(),
        metadata.ino(),
        metadata.len(),
        u64::from(metadata.mode()),
        metadata.mtime() as u64,
        metadata.mtime_nsec() as u64,
        metadata.ctime() as u64,
        metadata.ctime_nsec() as u64,
    ] {
        bytes.extend_from_slice(&value.to_be_bytes());
    }
    Ok(bytes)
}

#[cfg(not(unix))]
fn platform(metadata: &Metadata) -> Result<Vec<u8>, IdentityFailure> {
    // No stable file-id API is available through std on this target. Retain
    // observable content/metadata for one-shot validation, but capture marks
    // the manifest incomplete so it can never supply reusable allow.
    let mut bytes = b"incomplete-platform-v1".to_vec();
    bytes.extend_from_slice(&metadata.len().to_be_bytes());
    for time in [metadata.modified(), metadata.created()] {
        match time
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        {
            Some(time) => {
                bytes.push(1);
                bytes.extend_from_slice(&time.as_nanos().to_be_bytes());
            }
            None => bytes.push(0),
        }
    }
    Ok(bytes)
}

fn observe_shebang(
    named: &Path,
    cwd: &Path,
    env: &[(OsString, OsString)],
    identity: &mut ExecutionIdentity,
    depth: usize,
) {
    if depth == 8 {
        identity.failures.push(IdentityFailure::ScriptUnresolved);
        return;
    }
    let mut prefix = [0u8; 4096];
    let prefix =
        File::open(named).and_then(|mut file| file.read(&mut prefix).map(|n| prefix[..n].to_vec()));
    if let Ok(prefix) = prefix
        && prefix.starts_with(b"#!")
    {
        identity.dynamic_code = true;
        let line = prefix
            .split(|byte| *byte == b'\n')
            .next()
            .unwrap_or_default();
        let Ok(line) = std::str::from_utf8(&line[2..]) else {
            identity.failures.push(IdentityFailure::ScriptUnresolved);
            return;
        };
        let mut words = line.split_whitespace();
        if let Some(interpreter) = words
            .next()
            .and_then(|word| resolve(OsStr::new(word), cwd, env))
        {
            observe(&interpreter, identity);
            observe_shebang(&interpreter, cwd, env, identity, depth + 1);
            if interpreter.file_name() == Some(OsStr::new("env")) {
                match words
                    .next()
                    .filter(|word| !word.starts_with('-'))
                    .and_then(|word| resolve(OsStr::new(word), cwd, env))
                {
                    Some(target) => {
                        observe(&target, identity);
                        observe_shebang(&target, cwd, env, identity, depth + 1);
                    }
                    None => identity.failures.push(IdentityFailure::ScriptUnresolved),
                }
            }
        } else {
            identity.failures.push(IdentityFailure::ScriptUnresolved);
        }
    }
}

fn recognize(
    named: &Path,
    argv: &[OsString],
    cwd: &Path,
    env: &[(OsString, OsString)],
    identity: &mut ExecutionIdentity,
) {
    observe_shebang(named, cwd, env, identity, 0);
    let resolved = identity
        .target
        .as_path()
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or_default()
        .to_owned();
    let name = named
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or_default();
    let interpreter = [name, resolved.as_str()].into_iter().any(|name| {
        let name = name.strip_suffix(".exe").unwrap_or(name);
        matches!(
            name,
            "sh" | "bash" | "zsh" | "dash" | "node" | "ruby" | "perl" | "python" | "python3"
        ) || name.starts_with("python3.")
    });
    if name == "env" || resolved == "env" {
        identity.dynamic_code = true;
        identity.failures.push(IdentityFailure::ScriptUnresolved);
    }
    if interpreter {
        identity.dynamic_code = true;
        match argv.get(1) {
            Some(arg) if !arg.as_encoded_bytes().starts_with(b"-") => {
                observe(&cwd.join(arg), identity)
            }
            Some(arg) if matches!(arg.to_str(), Some("-c" | "-e" | "--eval")) => {}
            _ => identity.failures.push(IdentityFailure::ScriptUnresolved),
        }
    }
}

pub(super) fn revalidate(spec: &SpawnSpec) -> Result<(), ProcessError> {
    let Some(identity) = spec.identity() else {
        return Ok(());
    };
    if identity.failures.contains(&IdentityFailure::Changed) {
        return Err(ProcessError::Invariant(
            "execution identity moved during observation; no process was launched".into(),
        ));
    }
    let cwd = spec.cwd().ok_or_else(|| {
        ProcessError::Invariant("captured execution has no working directory".into())
    })?;
    if fs::canonicalize(cwd.as_path()).ok().as_deref() != Some(cwd.as_path()) {
        return Err(ProcessError::Invariant(
            "execution working directory changed before spawn; no process was launched".into(),
        ));
    }
    if !matches!(spec.env_policy(), EnvPolicy::Exact { .. }) {
        return Err(ProcessError::Invariant(
            "captured execution has no exact environment".into(),
        ));
    }
    let environment = super::backend::environment(spec)?;
    let named = resolve(&spec.argv()[0], cwd.as_path(), &environment);
    if !identity.files.first().is_some_and(|file| {
        file.resolved == identity.target && named.as_deref() == Some(file.named.as_path())
    }) {
        return Err(ProcessError::Invariant(
            "execution target no longer matches its captured authority; no process was launched"
                .into(),
        ));
    }
    for expected in &identity.files {
        let observed = hash_file(expected.named.as_path())
            .or_else(|_| metadata_only(expected.named.as_path()));
        if observed.ok().as_ref() != Some(expected) {
            return Err(ProcessError::Invariant(
                "execution identity changed before spawn; no process was launched".into(),
            ));
        }
    }
    if identity.files.is_empty() {
        return Err(ProcessError::Invariant(
            "execution identity could not be acquired; no process was launched".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Seek, SeekFrom, Write};

    #[cfg(unix)]
    #[test]
    fn interpreter_content_and_captured_path_environment_change_the_identity() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let interpreter = directory.path().join("interpreter");
        fs::write(&interpreter, b"interpreter-v1").unwrap();
        fs::set_permissions(&interpreter, fs::Permissions::from_mode(0o755)).unwrap();
        let script = directory.path().join("script");
        fs::write(
            &script,
            format!("#!{}\nscript content\n", interpreter.display()),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let env = EnvPolicy::exact(vec![
            ExplicitEnvBinding::from_native(
                EnvName::new("PATH").unwrap(),
                directory.path().as_os_str().to_owned(),
            )
            .unwrap(),
        ])
        .unwrap();
        let spec = SpawnSpec::new(
            vec!["script".into(), "".into(), "a".into()],
            Some(CanonPath::new(directory.path())),
            None,
            env,
        )
        .unwrap();
        let captured = capture(&spec).unwrap();
        assert_eq!(captured.argv(), spec.argv());
        assert!(captured.identity().unwrap().complete());
        assert_eq!(captured.identity().unwrap().files.len(), 2);
        fs::write(&interpreter, b"interpreter-v2").unwrap();
        assert!(revalidate(&captured).is_err());
        let refreshed = capture(&spec).unwrap();
        let first = ego_core::permit::PreparedCall::new(
            "exec",
            ego_core::permit::Op::Spawn { spec: captured },
        );
        let second = ego_core::permit::PreparedCall::new(
            "exec",
            ego_core::permit::Op::Spawn { spec: refreshed },
        );
        assert_ne!(
            first.authority_fingerprint(),
            second.authority_fingerprint()
        );
    }

    #[cfg(unix)]
    #[test]
    fn interpreter_aliases_cannot_hide_the_script_identity() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let directory = tempfile::tempdir().unwrap();
        let interpreter = directory.path().join("python3");
        fs::write(&interpreter, b"interpreter").unwrap();
        fs::set_permissions(&interpreter, fs::Permissions::from_mode(0o755)).unwrap();
        let alias = directory.path().join("unrelated-name");
        symlink(&interpreter, &alias).unwrap();
        let script = directory.path().join("script.py");
        fs::write(&script, b"program one").unwrap();
        let spec = SpawnSpec::new(
            vec![alias.into_os_string(), script.clone().into_os_string()],
            Some(CanonPath::new(directory.path())),
            None,
            EnvPolicy::Inherited,
        )
        .unwrap();
        let captured = capture(&spec).unwrap();
        assert_eq!(captured.identity().unwrap().files.len(), 2);
        fs::write(&script, b"program two").unwrap();
        assert!(revalidate(&captured).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn an_identity_manifest_cannot_retarget_the_authorized_argv() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let original = directory.path().join("original");
        let other = directory.path().join("other");
        for path in [&original, &other] {
            fs::write(path, b"same-content").unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let spec = SpawnSpec::new(
            vec![original.into_os_string()],
            Some(CanonPath::new(directory.path())),
            None,
            EnvPolicy::Inherited,
        )
        .unwrap();
        let captured = capture(&spec).unwrap();
        let mut forged = captured.identity().unwrap().clone();
        forged.target = CanonPath::new(&other);
        forged.files = vec![hash_file(&other).unwrap()];
        let env = captured.env_policy().clone();
        assert!(revalidate(&captured.with_execution_identity(env, forged)).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_executable_is_incomplete_instead_of_reusable_allow() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let image = directory.path().join("image");
        fs::write(&image, b"executable").unwrap();
        fs::set_permissions(&image, fs::Permissions::from_mode(0o111)).unwrap();
        let spec = SpawnSpec::new(
            vec![image.into_os_string()],
            Some(CanonPath::new(directory.path())),
            None,
            EnvPolicy::Inherited,
        )
        .unwrap();
        let captured = capture(&spec).unwrap();
        // Privileged test runners can read an execute-only file. The ordinary
        // user path is the one whose missing read authority matters here.
        if captured
            .identity()
            .unwrap()
            .failures
            .contains(&IdentityFailure::Read)
        {
            assert!(!captured.identity().unwrap().complete());
            assert_eq!(captured.identity().unwrap().files[0].content, None);
            assert!(revalidate(&captured).is_ok());
        }
    }

    #[test]
    fn full_content_and_symlink_identity_are_revalidated() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("image");
        fs::write(&path, vec![1u8; 2 * 1024 * 1024]).unwrap();
        let original = hash_file(&path).unwrap();
        let mut file = fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.seek(SeekFrom::Start(1500000)).unwrap();
        file.write_all(&[2]).unwrap();
        file.sync_all().unwrap();
        assert_ne!(original.content, hash_file(&path).unwrap().content);
        #[cfg(unix)]
        {
            let other = directory.path().join("other");
            fs::write(&other, b"other").unwrap();
            let link = directory.path().join("link");
            std::os::unix::fs::symlink(&path, &link).unwrap();
            let first = hash_file(&link).unwrap();
            fs::remove_file(&link).unwrap();
            std::os::unix::fs::symlink(&other, &link).unwrap();
            assert_ne!(first, hash_file(&link).unwrap());
        }
    }

    #[test]
    fn capture_pins_environment_target_and_refuses_mutation() {
        let directory = tempfile::tempdir().unwrap();
        let image = directory.path().join("image");
        fs::write(&image, b"image one").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&image, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let spec = SpawnSpec::new(
            vec![image.clone().into_os_string()],
            Some(CanonPath::new(directory.path())),
            None,
            EnvPolicy::Inherited,
        )
        .unwrap();
        let captured = capture(&spec).unwrap();
        assert!(matches!(captured.env_policy(), EnvPolicy::Exact { .. }));
        #[cfg(unix)]
        assert!(captured.identity().unwrap().complete());
        #[cfg(not(unix))]
        assert!(!captured.identity().unwrap().complete());
        assert!(revalidate(&captured).is_ok());
        let mut unstable_identity = captured.identity().unwrap().clone();
        unstable_identity.failures.push(IdentityFailure::Changed);
        let unstable = captured
            .clone()
            .with_execution_identity(captured.env_policy().clone(), unstable_identity);
        assert!(!unstable.identity().unwrap().complete());
        assert!(
            revalidate(&unstable).is_err(),
            "movement during observation requires a fresh capture"
        );
        fs::write(&image, b"image two").unwrap();
        assert!(revalidate(&captured).is_err());
    }
}

#[cfg(all(test, unix))]
#[path = "identity_shell_tests.rs"]
mod shell_tests;

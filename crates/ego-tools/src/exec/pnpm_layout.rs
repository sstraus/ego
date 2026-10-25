//! Query the installed pnpm without running project-selected code unconfined.

use std::io::{Read as _, Seek as _, SeekFrom};
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Debug)]
pub(super) struct Layout {
    pub(super) store: PathBuf,
    pub(super) global: PathBuf,
    pub(super) engine: PathBuf,
}

impl Layout {
    // Recorded pnpm 11 contract: bin's parent owns the versioned engine store.
    // pnpm does not expose this default through `config get`.
    fn reported(store: PathBuf, global: PathBuf, bin: PathBuf) -> Option<Self> {
        let version = store.file_name()?.to_str()?;
        if version.len() < 2
            || !version.starts_with('v')
            || !version[1..].chars().all(|c| c.is_ascii_digit())
        {
            return None;
        }
        let home = if bin.file_name()? == "bin" {
            bin.parent()?
        } else {
            &bin
        };
        Some(Self {
            engine: home.join("package-manager-store").join(version),
            store,
            global,
        })
    }
}

pub(super) fn capture(workspace: &Path, credentials: &[PathBuf]) -> Result<Option<Layout>, String> {
    let Some(executable) = installed(workspace) else {
        return Ok(None);
    };
    let version = selected_version(workspace);
    let profile = readonly_profile(credentials)?;
    let query = |args: &[&str]| {
        query(&executable, workspace, &profile, args).map_err(|reason| {
            format!(
                "pnpm {version}: {} discovery failed: {reason}; no additional pnpm roots admitted",
                args.join(" ")
            )
        })
    };
    let layout = Layout::reported(
        query(&["store", "path"])?,
        query(&["root", "-g"])?,
        query(&["bin", "-g"])?,
    )
    .ok_or_else(|| {
        format!("pnpm {version}: unsupported reported layout; no additional pnpm roots admitted")
    })?;
    Ok(Some(layout))
}

pub(super) fn extend(
    home: &Path,
    pnpm_home: &Path,
    workspace: &Path,
    credentials: &[PathBuf],
    writable: &mut Vec<PathBuf>,
    controls: &mut Vec<PathBuf>,
) -> Vec<String> {
    let layout = match capture(workspace, credentials) {
        Ok(Some(layout)) => layout,
        Ok(None) => return Vec::new(),
        Err(warning) => return vec![warning],
    };
    match admitted_paths(home, pnpm_home, workspace, credentials, &layout) {
        Ok((writes, protected, warnings)) => {
            writable.extend(writes);
            controls.extend(protected);
            warnings
        }
        Err(error) => vec![format!(
            "pnpm: reported roots refused: {error}; no additional pnpm roots admitted"
        )],
    }
}

fn admitted_paths(
    home: &Path,
    pnpm_home: &Path,
    workspace: &Path,
    credentials: &[PathBuf],
    layout: &Layout,
) -> Result<(Vec<PathBuf>, Vec<PathBuf>, Vec<String>), ego_core::permit::ProcessError> {
    use ego_core::permit::CanonPath;
    // A confined query still produces untrusted data. It must not turn project
    // configuration into authority over arbitrary operator directories.
    if !layout.store.starts_with(pnpm_home.join("store"))
        || !layout.global.starts_with(pnpm_home.join("global"))
        || !layout
            .engine
            .starts_with(pnpm_home.join("package-manager-store"))
    {
        return Err(super::error(
            "reported pnpm roots leave the operator-selected pnpm home",
        ));
    }
    let credentials = credentials
        .iter()
        .map(|path| super::resolve(path).map(CanonPath::new))
        .collect::<Result<Vec<_>, _>>()?;
    for path in [&layout.store, &layout.global] {
        super::check_write_root(home, workspace, &super::resolve(path)?, &credentials)?;
    }
    let mut writable = vec![layout.store.clone()];
    let mut protected = vec![layout.store.join("links"), layout.global.clone()];
    let mut warnings = Vec::new();
    let engine = super::resolve(&layout.engine);
    if engine.as_ref().is_err() || engine.as_ref().is_ok_and(|path| *path != layout.engine) {
        warnings.push(format!(
            "pnpm engine store is symlinked or unresolved: {}; no engine runtime writes admitted",
            layout.engine.display()
        ));
    } else if !layout.engine.is_dir() {
        warnings.push(format!(
            "pnpm engine store not found: {}; no engine runtime writes admitted",
            layout.engine.display()
        ));
    } else {
        super::check_write_root(home, workspace, &layout.engine, &credentials)?;
        protected.push(layout.engine.join("links"));
        // Never admit the engine root: other entries can be host-executed code.
        writable.extend(
            [
                "index.db",
                "index.db-wal",
                "index.db-shm",
                "index.db-journal",
                "pnpm-store-operation-locks",
                "tmp",
            ]
            .map(|name| layout.engine.join(name)),
        );
    }
    // A pre-existing alias must not turn one allowed database file into an
    // unrelated writable file or directory. Refuse all newly discovered roots.
    for path in &writable {
        if super::resolve(path)? != *path {
            return Err(super::error("pnpm runtime path contains a symlink"));
        }
    }
    Ok((writable, protected, warnings))
}

fn selected_version(workspace: &Path) -> String {
    // Diagnostic only. Never use a project-declared version to guess roots.
    let mut text = String::new();
    let value = std::fs::File::open(workspace.join("package.json"))
        .and_then(|file| file.take(65537).read_to_string(&mut text))
        .ok()
        .filter(|_| text.len() <= 65536)
        .and_then(|_| serde_json::from_str::<serde_json::Value>(&text).ok());
    value
        .as_ref()
        .and_then(|value| value["packageManager"].as_str())
        .and_then(|value| value.strip_prefix("pnpm@"))
        .filter(|value| {
            value.len() <= 96
                && value
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || ".-+".contains(c))
        })
        .unwrap_or("(installed; no project version declared)")
        .to_owned()
}

fn installed(workspace: &Path) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?).find_map(|directory| {
        if !directory.is_absolute() {
            return None;
        }
        let program = std::fs::canonicalize(directory.join("pnpm")).ok()?;
        let metadata = std::fs::metadata(&program).ok()?;
        use std::os::unix::fs::PermissionsExt as _;
        (metadata.is_file()
            && metadata.permissions().mode() & 0o111 != 0
            && !program.starts_with(workspace))
        .then_some(program)
    })
}

fn readonly_profile(credentials: &[PathBuf]) -> Result<String, String> {
    let mut profile = String::from(
        "(version 1)\n(deny default)\n(import \"system.sb\")\n(allow process-fork)\n(allow process-exec*)\n(allow file-read* file-test-existence)\n(deny file-write*)\n(deny network*)\n",
    );
    // The discovery child gets no more credential reach than the final child.
    // Protect both names so replacing a captured alias cannot reopen it.
    for path in credentials {
        for path in [
            path.clone(),
            super::resolve(path).map_err(|error| error.to_string())?,
        ] {
            let literal =
                crate::exec::profile::path_literal(&path).map_err(|error| error.to_string())?;
            profile.push_str(&format!(
                "(deny file-read* process-exec* (subpath {literal}))\n"
            ));
        }
    }
    Ok(profile)
}

/// Three sequential queries cost at most six seconds of child runtime. Output
/// is never interpreted as shell/SBPL, and no inherited pipe can hold us open.
fn query(
    program: &Path,
    workspace: &Path,
    profile: &str,
    args: &[&str],
) -> Result<PathBuf, String> {
    let mut output = tempfile::tempfile().map_err(|error| error.to_string())?;
    let mut child = Command::new("/usr/bin/sandbox-exec")
        .args(["-p", profile])
        .arg(program)
        .args(args)
        .current_dir(workspace)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .stdout(output.try_clone().map_err(|error| error.to_string())?)
        .process_group(0)
        .spawn()
        .map_err(|error| error.to_string())?;
    let deadline = Instant::now() + Duration::from_secs(2);
    let result = loop {
        if output.metadata().map(|m| m.len() > 4096).unwrap_or(true) {
            break Err("reported output exceeds 4096 bytes".to_owned());
        }
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break Ok(()),
            Ok(Some(status)) => {
                break Err(format!(
                    "read-only discovery exited {status}; runtime writes and network are disabled"
                ));
            }
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            Ok(None) => break Err("read-only discovery timed out after 2 seconds".to_owned()),
            Err(error) => break Err(error.to_string()),
        }
    };
    // Also reap descendants of a successful shim; discovery owns this group.
    let _ = nix::sys::signal::killpg(
        nix::unistd::Pid::from_raw(child.id() as i32),
        nix::sys::signal::Signal::SIGKILL,
    );
    let _ = child.wait();
    result?;
    output
        .seek(SeekFrom::Start(0))
        .map_err(|error| error.to_string())?;
    let mut text = String::new();
    output
        .take(4097)
        .read_to_string(&mut text)
        .map_err(|error| error.to_string())?;
    let text = text.trim();
    if text.len() > 4096 || text.chars().any(char::is_control) {
        return Err("reported path is not a bounded single line".into());
    }
    let path = PathBuf::from(text);
    if !path.is_absolute()
        || path
            .components()
            .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        return Err("reported path is not absolute or contains parent traversal".into());
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Catches: fixed home/version literals override pnpm's reported roots.
    #[test]
    fn recorded_pnpm_11_layout_derives_the_engine_sibling() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/pnpm/11.15.1-layout.json"
        ))
        .unwrap();
        assert_eq!(fixture["version"], "11.15.1");
        let layout = Layout::reported(
            fixture["store"].as_str().unwrap().into(),
            fixture["global"].as_str().unwrap().into(),
            fixture["bin"].as_str().unwrap().into(),
        )
        .unwrap();
        assert_eq!(
            layout.engine,
            PathBuf::from("/home/example/workspace")
        );
        let relocated = Layout::reported(
            "/custom/content/v42".into(),
            "/custom/global/v11".into(),
            "/custom/home/bin".into(),
        )
        .unwrap();
        assert_eq!(
            relocated.engine,
            PathBuf::from("/custom/home/package-manager-store/v42")
        );
        assert_eq!(relocated.global, PathBuf::from("/custom/global/v11"));
    }

    // Catches: a project-selected executable can write while discovering roots.
    #[test]
    fn readonly_discovery_cannot_write_even_inside_the_workspace() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("must-not-exist");
        let profile = readonly_profile(&[]).unwrap();
        let result = query(
            Path::new("/usr/bin/touch"),
            root.path(),
            &profile,
            &[file.to_str().unwrap()],
        );
        assert!(
            result
                .unwrap_err()
                .contains("runtime writes and network are disabled")
        );
        assert!(!file.exists());
    }

    // Catches: project-selected discovery can use loopback to escape its
    // read-only/no-network authority before the run perimeter exists.
    #[test]
    fn readonly_discovery_cannot_connect_to_a_local_listener() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let root = tempfile::tempdir().unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let profile = readonly_profile(&[]).unwrap();
        assert!(
            query(
                Path::new("/usr/bin/curl"),
                root.path(),
                &profile,
                &["--noproxy", "*", "--silent", &url]
            )
            .is_err()
        );
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }

    // Catches: trusting query stdout launders arbitrary home-directory writes
    // into the adaptive perimeter even though discovery itself is confined.
    #[test]
    fn reported_paths_cannot_grant_unrelated_operator_directories() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let pnpm = home.join("pnpm");
        let layout = Layout {
            store: home.join("Documents/v11"),
            global: pnpm.join("global/v11"),
            engine: pnpm.join("package-manager-store/v11"),
        };
        let error =
            admitted_paths(&home, &pnpm, &root.path().join("workspace"), &[], &layout).unwrap_err();
        assert!(error.to_string().contains("operator-selected pnpm home"));
    }

    // Catches: fail-closed discovery rejects a valid read-only query as well.
    #[test]
    fn readonly_discovery_accepts_an_absolute_reported_path() {
        let root = tempfile::tempdir().unwrap();
        let profile = readonly_profile(&[]).unwrap();
        assert_eq!(
            query(
                Path::new("/bin/echo"),
                root.path(),
                &profile,
                &["/reported/store/v11"]
            )
            .unwrap(),
            PathBuf::from("/reported/store/v11")
        );
    }
}

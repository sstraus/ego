//! Generation of the private macOS Seatbelt profile used for one run.
//!
//! SBPL is data here. Every path enters the profile through [`path_literal`];
//! callers cannot contribute a fragment of the profile language.

use std::{collections::BTreeSet, error::Error, fmt, path::Path};

use ego_core::permit::{CanonPath, Confinement, SandboxAccess, SandboxGrant};

#[path = "adaptive_profile.rs"]
mod adaptive;

/// Inputs owned by the execution backend, rather than by a model call.
pub(super) struct ProfileInput<'a> {
    pub(super) adaptive: Option<&'a ego_core::permit::AdaptivePerimeter>,
    pub(super) level: Confinement,
    pub(super) offline: bool,
    pub(super) workspace: &'a Path,
    pub(super) private_scratch: &'a Path,
    pub(super) runtime_roots: &'a [&'a Path],
    /// Paths whose ancestors are made statable: runtime and configured exec
    /// roots, and the workspace (always).
    pub(super) exec_paths: &'a [&'a Path],
    /// Paths a human allowed after the run started. Never a parent directory
    /// of the path that was refused.
    pub(super) grants: &'a [SandboxGrant],
}

#[derive(Debug)]
pub(super) enum ProfileError {
    RelativePath(String),
    UnrepresentablePath(String),
    MissingAdaptivePerimeter,
}

impl fmt::Display for ProfileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RelativePath(path) => write!(f, "Seatbelt root is not absolute: {path}"),
            Self::UnrepresentablePath(path) => {
                write!(f, "Seatbelt root cannot be represented safely: {path}")
            }
            Self::MissingAdaptivePerimeter => f.write_str("adaptive perimeter was not admitted"),
        }
    }
}

impl Error for ProfileError {}

/// Render one complete profile for the exact effective run policy.
pub(super) fn render(input: &ProfileInput<'_>) -> Result<String, ProfileError> {
    let workspace = path_literal(input.workspace)?;
    let scratch = path_literal(input.private_scratch)?;
    let mut readable = input
        .runtime_roots
        .iter()
        .copied()
        .map(path_literal)
        .collect::<Result<BTreeSet<_>, _>>()?;
    readable.insert(workspace.clone());
    // A read grant is not an exec grant: `process-exec*` is rendered from
    // `readable`, so only an exec or write-then-run decision may enter it.
    let mut extra_read = BTreeSet::new();
    let mut extra_write = BTreeSet::new();
    for grant in input.grants {
        let literal = path_literal(grant.path.as_path())?;
        match grant.access {
            SandboxAccess::Read => {
                extra_read.insert(literal);
            }
            SandboxAccess::Exec => {
                readable.insert(literal);
            }
            SandboxAccess::Write => {
                extra_read.insert(literal.clone());
                extra_write.insert(literal);
            }
        }
    }

    // Inherited sandbox identity admits worker cleanup, not unrelated host PIDs.
    let mut profile = String::from(
        "(version 1)\n\
         (deny default)\n\
         (import \"system.sb\")\n\
         (allow process-fork)\n\
         (allow signal (target same-sandbox))\n",
    );

    if matches!(input.level, Confinement::Workspace | Confinement::Adaptive) {
        profile.push_str(
            "(allow system-info (info-type \"vfs.disk-space\") (info-type \"net.link.addr\"))\n",
        );
    }
    if input.level == Confinement::Workspace {
        // macOS /bin/sh consults this selector; Node package managers stat
        // /var before choosing a temporary directory. Exact metadata and one
        // selector link are runtime prerequisites, not access to /var data.
        profile.push_str(concat!(
            "(allow file-read-metadata (literal \"/var\") (literal \"/private/var\") (literal \"/private/var/select\"))\n",
            "(allow file-read* (literal \"/private/var/select/sh\"))\n",
        ));
    }
    match input.level {
        Confinement::Adaptive => profile.push_str(&adaptive::render(input)?),
        Confinement::Off => profile.push_str("(allow process-exec*)\n(allow file*)\n"),
        Confinement::Ro | Confinement::Workspace => {
            profile.push_str("(allow process-exec*\n");
            for root in &readable {
                profile.push_str("  (subpath ");
                profile.push_str(root);
                profile.push_str(")\n");
            }
            profile.push_str(")\n");

            // A writable root the child cannot stat is not usable: `touch`,
            // `cp`, `rm`, `mv` and `ln` all read metadata before they mutate,
            // so a write-only scratch fails with `Operation not permitted`.
            // Scratch joins the read set with the other writable root, and
            // never the exec set above: ego does not let a child run a program
            // it has just written into its own scratch.
            if matches!(input.level, Confinement::Workspace) {
                readable.insert(scratch.clone());
            }

            profile.push_str("(allow file-read* file-test-existence\n");
            for root in readable.into_iter().chain(extra_read) {
                profile.push_str("  (subpath ");
                profile.push_str(&root);
                profile.push_str(")\n");
            }
            profile.push_str(")\n");

            // Runtime roots are not user-admitted data roots. Keep credential
            // files beneath them closed unless their data root is explicitly
            // admitted. Workspace and extra roots retain their data reach.
            profile.push_str("(deny file-read-data\n  (require-all\n");
            profile.push_str("    (regex #\"(^|/)([.]env[^/]*|[.]ssh/.*|[.]aws/.*|[.]netrc|Keychains/.*|[^/]*[.]keychain(-db)?)(/|$)\")\n");
            profile.push_str("    (require-not (require-any\n      (subpath ");
            profile.push_str(&workspace);
            profile.push_str(")\n");
            for grant in input.grants {
                profile.push_str("      (subpath ");
                profile.push_str(&path_literal(grant.path.as_path())?);
                profile.push_str(")\n");
            }
            profile.push_str("    )))\n)\n");

            // `realpath`, command lookup and `mkdir -p` lstat each ancestor of
            // a path they use, so exec paths and Workspace scratch need them
            // statable. Metadata only: no listing and no file contents.
            let ancestors = input
                .exec_paths
                .iter()
                .copied()
                .chain((input.level == Confinement::Workspace).then_some(input.private_scratch))
                .chain(input.grants.iter().map(|grant| grant.path.as_path()))
                .flat_map(|path| path.ancestors().skip(1))
                .filter(|ancestor| *ancestor != Path::new("/"))
                .map(path_literal)
                .collect::<Result<BTreeSet<_>, _>>()?;
            if !ancestors.is_empty() {
                profile.push_str("(allow file-read-metadata\n");
                for ancestor in ancestors {
                    profile.push_str("  (literal ");
                    profile.push_str(&ancestor);
                    profile.push_str(")\n");
                }
                profile.push_str(")\n");
            }

            // The roots a child may write: none under `ro`, the workspace and
            // scratch under `workspace`, and in both cases whatever a human
            // allowed for writing. `system.sb` contains a small platform
            // runtime baseline, so the grant list narrows its writes back.
            let mut writable = Vec::new();
            if matches!(input.level, Confinement::Workspace) {
                writable.push(workspace.clone());
                writable.push(scratch.clone());
            }
            writable.extend(extra_write);
            if writable.is_empty() {
                profile.push_str("(deny file-write*)\n");
            } else {
                profile.push_str("(deny file-write*\n  (require-not (require-any\n");
                for root in &writable {
                    profile.push_str("    (subpath ");
                    profile.push_str(root);
                    profile.push_str(")\n");
                }
                profile.push_str("  )))\n");
                profile.push_str("(allow file-write*\n");
                for root in &writable {
                    profile.push_str("  (subpath ");
                    profile.push_str(root);
                    profile.push_str(")\n");
                }
                profile.push_str(")\n");
            }
        }
    }

    if input.offline {
        profile.push_str("(deny network*)\n");
    } else {
        profile.push_str("(allow network*)\n");
    }

    Ok(profile)
}

/// Encode an absolute host path as one SBPL string literal.
///
/// SBPL has no stable public grammar. We therefore use only its installed
/// quoted-string subset, escape its two structural characters, and reject
/// control bytes instead of guessing at escape spellings. Escaping `"` keeps
/// a closing parenthesis in the string, where it has no syntactic meaning.
pub(super) fn path_literal(path: &Path) -> Result<String, ProfileError> {
    if !path.is_absolute() {
        return Err(ProfileError::RelativePath(path_label(path)));
    }
    let raw = path
        .to_str()
        .ok_or_else(|| ProfileError::UnrepresentablePath(path_label(path)))?;
    if raw.chars().any(char::is_control) {
        return Err(ProfileError::UnrepresentablePath(path_label(path)));
    }

    let mut encoded = String::with_capacity(raw.len() + 2);
    encoded.push('"');
    for ch in raw.chars() {
        match ch {
            '"' => encoded.push_str("\\\""),
            '\\' => encoded.push_str("\\\\"),
            _ => encoded.push(ch),
        }
    }
    encoded.push('"');
    Ok(encoded)
}

/// The rule that keeps judge-granted roots from running what was written
/// there. Appended last, so it wins over every exec allowance before it.
pub(super) fn deny_exec(paths: &[CanonPath]) -> Result<String, ProfileError> {
    if paths.is_empty() {
        return Ok(String::new());
    }
    let mut rule = String::from("(deny process-exec*\n");
    for path in paths {
        rule.push_str("  (subpath ");
        rule.push_str(&path_literal(path.as_path())?);
        rule.push_str(")\n");
    }
    rule.push_str(")\n");
    Ok(rule)
}

/// Whether [`render`] can express `path`; the one place that rule lives.
pub(super) fn renderable(path: &Path) -> bool {
    path_literal(path).is_ok()
}

fn path_label(path: &Path) -> String {
    format!("{:?}", path.as_os_str())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use ego_core::permit::{CanonPath, SandboxAccess, SandboxGrant};

    use super::{Confinement, ProfileInput, render};

    fn input(level: Confinement) -> ProfileInput<'static> {
        static RUNTIME: &[&Path] = &[];
        ProfileInput {
            adaptive: None,
            level,
            offline: false,
            workspace: Path::new("/work"),
            private_scratch: Path::new("/scratch"),
            runtime_roots: RUNTIME,
            exec_paths: RUNTIME,
            grants: &[],
        }
    }

    fn adaptive_metadata_input(root: &Path) -> ego_core::permit::AdaptivePerimeter {
        ego_core::permit::AdaptivePerimeter {
            home: CanonPath::new("/home"),
            writable: vec![],
            credentials: vec![],
            controls: vec![
                CanonPath::new(root.join("config")),
                CanonPath::new(root.join("hooks")),
            ],
        }
    }

    // Catches: a host quote terminates the raw SBPL regex and becomes profile syntax.
    #[test]
    fn adaptive_metadata_quote_cannot_terminate_raw_regex() {
        for root in [
            "/git/quoted\"root",
            "/git/a\")(allow file-write* (subpath \"/",
        ] {
            let perimeter = adaptive_metadata_input(Path::new(root));
            let mut input = input(Confinement::Adaptive);
            input.adaptive = Some(&perimeter);
            assert!(
                matches!(
                    render(&input),
                    Err(super::ProfileError::UnrepresentablePath(_))
                ),
                "quoted metadata root accepted: {root:?}"
            );
        }
    }

    // Catches: rejecting raw-regex quotes also rejects safe quoted literal paths.
    #[test]
    fn adaptive_quoted_workspace_remains_a_literal() {
        let perimeter = adaptive_metadata_input(Path::new("/git/metadata"));
        let mut input = input(Confinement::Adaptive);
        input.workspace = Path::new("/work/quoted\"root");
        input.adaptive = Some(&perimeter);
        let profile = render(&input).expect("quoted literal workspace");
        assert!(profile.contains("(subpath \"/work/quoted\\\"root\")"));
    }

    // Catches: regex metacharacters or backslashes in valid roots break SBPL parsing.
    #[test]
    fn adaptive_metadata_regex_paths_are_accepted_by_sandbox_exec() {
        for root in ["/git/plain", "/git/a(b)[c].+", "/git/back\\slash"] {
            let perimeter = adaptive_metadata_input(Path::new(root));
            let mut input = input(Confinement::Adaptive);
            input.adaptive = Some(&perimeter);
            let profile = render(&input).expect("representable metadata root");
            let output = std::process::Command::new("/usr/bin/sandbox-exec")
                .args(["-p", &profile, "/usr/bin/true"])
                .output()
                .expect("installed macOS sandbox-exec");
            assert!(output.status.success(), "{root:?}: {output:?}");
        }
    }

    // Catches: control bytes or nonabsolute roots reach raw SBPL regex syntax.
    #[test]
    fn adaptive_metadata_invalid_paths_are_refused_before_spawn() {
        for root in ["/git/control\nroot", "relative/git"] {
            let perimeter = adaptive_metadata_input(Path::new(root));
            let mut input = input(Confinement::Adaptive);
            input.adaptive = Some(&perimeter);
            assert!(render(&input).is_err(), "invalid root accepted: {root:?}");
        }
    }

    // Catches: runtime read roots expose protected files outside admitted workspace roots.
    #[test]
    fn strict_credential_floor_denies_runtime_reads_but_preserves_admitted_files() {
        let tree = tempfile::tempdir().unwrap();
        let base = tree.path().canonicalize().unwrap();
        let workspace = base.join("workspace");
        let outside = base.join("outside");
        for directory in [&workspace, &outside] {
            std::fs::create_dir_all(directory.join(".ssh")).unwrap();
            std::fs::write(directory.join(".ssh/synthetic"), "synthetic secret").unwrap();
        }
        let outside_file = outside.join(".ssh/synthetic");
        let alias = workspace.join("credential-alias");
        std::os::unix::fs::symlink(&outside_file, &alias).unwrap();
        for level in [Confinement::Ro, Confinement::Workspace] {
            let runtime = [
                base.as_path(),
                Path::new("/bin"),
                Path::new("/usr"),
                Path::new("/System"),
            ];
            let profile = render(&ProfileInput {
                workspace: &workspace,
                runtime_roots: &runtime,
                ..input(level)
            })
            .unwrap();
            let cat = |path: &Path| {
                std::process::Command::new("/usr/bin/sandbox-exec")
                    .args(["-p", &profile, "/bin/cat"])
                    .arg(path)
                    .output()
                    .unwrap()
            };
            assert!(cat(&workspace.join(".ssh/synthetic")).status.success());
            for path in [&outside_file, &alias] {
                let output = cat(path);
                assert!(
                    !output.status.success(),
                    "runtime root exposed {path:?}: {output:?}"
                );
                assert!(!String::from_utf8_lossy(&output.stdout).contains("synthetic secret"));
            }
        }
    }

    // Catches: sandboxed parents cannot terminate their own workers with SIGTERM.
    #[test]
    fn confined_process_can_signal_descendants_but_not_outside_processes() {
        use std::process::{Command, Stdio};

        let tree = tempfile::tempdir().unwrap();
        let workspace = tree.path().canonicalize().unwrap();
        let perimeter = adaptive_metadata_input(&workspace);
        let runtime = [Path::new("/bin"), Path::new("/usr"), Path::new("/System")];
        let mut outside = Command::new("/bin/sleep")
            .arg("60")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut failures = Vec::new();
        for level in [
            Confinement::Adaptive,
            Confinement::Workspace,
            Confinement::Ro,
        ] {
            let profile = render(&ProfileInput {
                adaptive: Some(&perimeter),
                workspace: &workspace,
                private_scratch: &workspace,
                runtime_roots: &runtime,
                exec_paths: &[workspace.as_path()],
                ..input(level)
            })
            .unwrap();
            // Child and grandchild both terminate themselves after two seconds if
            // signalling is denied, so the regression cannot leak worker processes.
            for script in [
                "/bin/sleep 2 & child=$!; kill -TERM \"$child\"; result=$?; wait \"$child\" 2>/dev/null; exit \"$result\"",
                "/bin/bash -c '/bin/sleep 2 & echo $!; wait 2>/dev/null' | { read child; kill -TERM \"$child\"; }",
            ] {
                let output = Command::new("/usr/bin/sandbox-exec")
                    .current_dir(&workspace)
                    .args(["-p", &profile, "/bin/bash", "-c", script])
                    .output()
                    .unwrap();
                if !output.status.success() || !output.stderr.is_empty() {
                    failures.push(format!("{level:?} descendant: {output:?}"));
                }
            }
            // The outside process is test-owned, never a process from the host.
            let output = Command::new("/usr/bin/sandbox-exec")
                .current_dir(&workspace)
                .args(["-p", &profile, "/bin/bash", "-c"])
                .arg(format!("kill -TERM {}", outside.id()))
                .output()
                .unwrap();
            if output.status.success()
                || !String::from_utf8_lossy(&output.stderr).contains("Operation not permitted")
                || outside.try_wait().unwrap().is_some()
            {
                failures.push(format!("{level:?} outside: {output:?}"));
            }
        }
        let _ = outside.kill();
        outside.wait().unwrap();
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    #[test]
    fn workspace_grants_read_and_existence_on_both_writable_roots() {
        assert_eq!(
            render(&input(Confinement::Workspace)).expect("workspace profile"),
            concat!(
                "(version 1)\n",
                "(deny default)\n",
                "(import \"system.sb\")\n",
                "(allow process-fork)\n",
                "(allow signal (target same-sandbox))\n",
                "(allow system-info (info-type \"vfs.disk-space\") (info-type \"net.link.addr\"))\n",
                "(allow file-read-metadata (literal \"/var\") (literal \"/private/var\") (literal \"/private/var/select\"))\n",
                "(allow file-read* (literal \"/private/var/select/sh\"))\n",
                "(allow process-exec*\n",
                "  (subpath \"/work\")\n",
                ")\n",
                "(allow file-read* file-test-existence\n",
                "  (subpath \"/scratch\")\n",
                "  (subpath \"/work\")\n",
                ")\n",
                "(deny file-read-data\n  (require-all\n",
                "    (regex #\"(^|/)([.]env[^/]*|[.]ssh/.*|[.]aws/.*|[.]netrc|Keychains/.*|[^/]*[.]keychain(-db)?)(/|$)\")\n",
                "    (require-not (require-any\n      (subpath \"/work\")\n",
                "    )))\n)\n",
                "(deny file-write*\n",
                "  (require-not (require-any\n",
                "    (subpath \"/work\")\n",
                "    (subpath \"/scratch\")\n",
                "  )))\n",
                "(allow file-write*\n",
                "  (subpath \"/work\")\n",
                "  (subpath \"/scratch\")\n",
                ")\n",
                "(allow network*)\n",
            )
        );
    }

    #[test]
    fn exec_path_ancestors_get_literal_metadata_grants_and_no_write() {
        let exec = [Path::new("/opt/tools/bin"), Path::new("/opt/other")];
        let profile = render(&ProfileInput {
            runtime_roots: &exec,
            exec_paths: &exec,
            ..input(Confinement::Workspace)
        })
        .expect("profile");
        assert!(
            profile.contains(concat!(
                "(allow file-read-metadata\n",
                "  (literal \"/opt\")\n",
                "  (literal \"/opt/tools\")\n",
                ")\n"
            )),
            "{profile}"
        );
        assert!(!profile.contains("(subpath \"/opt\")"), "{profile}");
        assert!(
            profile.contains("(subpath \"/opt/tools/bin\")"),
            "{profile}"
        );
    }

    #[test]
    fn read_only_keeps_the_private_scratch_out_of_every_rule() {
        // `ro` mutates nothing, so scratch is not a root the child needs, and
        // an unused read grant would still be reach the level does not promise.
        let profile = render(&input(Confinement::Ro)).expect("ro profile");
        assert!(!profile.contains("/scratch"), "{profile}");
    }

    fn grant(path: &str, access: SandboxAccess) -> SandboxGrant {
        SandboxGrant::new(CanonPath::new(path), access)
    }

    #[test]
    fn a_read_grant_opens_reads_of_one_path_and_never_exec_or_write() {
        let grants = [grant("/data/notes.txt", SandboxAccess::Read)];
        let profile = render(&ProfileInput {
            grants: &grants,
            ..input(Confinement::Workspace)
        })
        .expect("profile");
        let exec = profile
            .split("(allow file-read*")
            .next()
            .expect("exec rules come first");
        assert!(!exec.contains("/data/notes.txt"), "{profile}");
        assert!(
            profile.contains("(allow file-read* file-test-existence\n  (subpath \"/scratch\")\n  (subpath \"/work\")\n  (subpath \"/data/notes.txt\")\n)"),
            "{profile}"
        );
        assert!(
            profile.contains("(allow file-read-metadata\n  (literal \"/data\")\n)"),
            "{profile}"
        );
        assert!(
            profile.contains(concat!(
                "(deny file-write*\n  (require-not (require-any\n",
                "    (subpath \"/work\")\n    (subpath \"/scratch\")\n  )))\n"
            )),
            "{profile}"
        );
        assert_eq!(profile.matches("/data/notes.txt").count(), 2, "{profile}");
    }

    #[test]
    fn an_exec_grant_joins_the_exec_and_read_sets() {
        let grants = [grant("/opt/tool", SandboxAccess::Exec)];
        let profile = render(&ProfileInput {
            grants: &grants,
            ..input(Confinement::Workspace)
        })
        .expect("profile");
        assert!(
            profile.contains(
                "(allow process-exec*\n  (subpath \"/opt/tool\")\n  (subpath \"/work\")\n)\n"
            ),
            "{profile}"
        );
        assert!(
            profile.contains(concat!(
                "(deny file-write*\n  (require-not (require-any\n",
                "    (subpath \"/work\")\n    (subpath \"/scratch\")\n  )))\n"
            )),
            "{profile}"
        );
    }

    #[test]
    fn a_write_grant_extends_the_write_roots_under_workspace_and_ro() {
        let grants = [grant("/out", SandboxAccess::Write)];
        let workspace = render(&ProfileInput {
            grants: &grants,
            ..input(Confinement::Workspace)
        })
        .expect("workspace profile");
        assert!(
            workspace.contains(concat!(
                "(deny file-write*\n",
                "  (require-not (require-any\n",
                "    (subpath \"/work\")\n",
                "    (subpath \"/scratch\")\n",
                "    (subpath \"/out\")\n",
                "  )))\n",
            )),
            "{workspace}"
        );
        let ro = render(&ProfileInput {
            grants: &grants,
            ..input(Confinement::Ro)
        })
        .expect("ro profile");
        assert!(
            ro.contains("(allow file-write*\n  (subpath \"/out\")\n)"),
            "{ro}"
        );
        assert!(
            ro.contains(
                "(deny file-write*\n  (require-not (require-any\n    (subpath \"/out\")\n  )))\n"
            ),
            "{ro}"
        );
    }
}

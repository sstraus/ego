//! One floor contract for native SBPL and ineffective-grant diagnostics.

use ego_core::permit::{AdaptivePerimeter, RunConfinement};
use std::path::Path;

pub(in crate::exec) const GIT_ENTRY: &str = "(^|/)[.][gG][iI][tT]$";
pub(in crate::exec) const METADATA_CONTROL: &str = "(/modules/.+)?(/worktrees/[^/]+)?/(config([.]worktree)?|hooks|commondir|gitdir|info/attributes)(/|$)";
pub(in crate::exec) const METADATA_BOUNDARY: &str =
    "(/modules(/.*)?|/worktrees(/[^/]+)?|/hooks|/info)?$";
// Operation state and object/ref fan-out inside module metadata stay removable;
// the module boundary otherwise covers every directory beneath `modules/`.
pub(in crate::exec) const METADATA_MODULE_TRANSIENT: &str =
    "/modules/.+/(rebase-merge|rebase-apply|sequencer|objects/[^/]+|refs/.+|logs/.+)$";
pub(in crate::exec) const ENV: &str = "(^|/)[.]env[^/]*(/|$)";

#[cfg(any(target_os = "macos", test))]
pub(in crate::exec) fn rooted_pattern(root: &Path, suffix: &str) -> String {
    format!("^{}{suffix}", regex::escape(&root.to_string_lossy()))
}

pub(in crate::exec) fn metadata_roots(
    perimeter: &AdaptivePerimeter,
) -> impl Iterator<Item = &Path> {
    // Captured Git roots carry both controls, including absent paths. Plain
    // read-only grants do not become a new metadata-root source.
    perimeter.controls.iter().filter_map(|control| {
        let path = control.as_path();
        let root = path.parent()?;
        (path.file_name().is_some_and(|name| name == "config")
            && perimeter
                .controls
                .iter()
                .any(|other| other.as_path() == root.join("hooks")))
        .then_some(root)
    })
}

#[cfg(test)]
std::thread_local! {
    pub(in crate::exec) static COMPILATIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn compile(pattern: &str) -> regex::Regex {
    #[cfg(test)]
    COMPILATIONS.with(|count| count.set(count.get() + 1));
    regex::Regex::new(pattern).expect("fixed floor pattern")
}

pub(in crate::exec) fn blocks(path: &Path, policy: &RunConfinement) -> bool {
    let Some(perimeter) = policy.adaptive_perimeter() else {
        return false;
    };
    static GIT: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| compile(GIT_ENTRY));
    static ENVIRONMENT: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| compile(ENV));
    static CONTROL: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| compile(&format!("^{METADATA_CONTROL}")));
    static BOUNDARY: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| compile(&format!("^{METADATA_BOUNDARY}")));
    static TRANSIENT: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| compile(&format!("^{METADATA_MODULE_TRANSIENT}")));
    let text = path.to_string_lossy();
    // Match the captured root as literal text; only its fixed suffix is regex.
    // This is equivalent to rooted_pattern without compiling host paths.
    let rooted_matches = |root: &Path, pattern: &regex::Regex| {
        text.strip_prefix(root.to_string_lossy().as_ref())
            .is_some_and(|suffix| pattern.is_match(suffix))
    };
    perimeter
        .credentials
        .iter()
        .chain(&perimeter.controls)
        .any(|floor| path.starts_with(floor.as_path()))
        || (GIT.is_match(&text) && !path.is_dir())
        || metadata_roots(perimeter).any(|root| rooted_matches(root, &CONTROL))
        || (path.is_dir()
            && metadata_roots(perimeter)
                .any(|root| rooted_matches(root, &BOUNDARY) && !rooted_matches(root, &TRANSIENT)))
        || (!path.starts_with(policy.workspace().as_path()) && ENVIRONMENT.is_match(&text))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ego_core::permit::{CanonPath, Confinement};

    // Catches: literal-prefix optimization changes root boundaries or suffix matching.
    #[test]
    fn cached_floor_matches_rooted_contract_for_literal_roots_and_transients() {
        let temporary = tempfile::tempdir().unwrap();
        let metadata = temporary.path().join("git(a)[b]\"root");
        let policy = RunConfinement::new(
            Confinement::Adaptive,
            false,
            CanonPath::new(temporary.path()),
        )
        .unwrap()
        .with_adaptive_perimeter(AdaptivePerimeter {
            home: CanonPath::new(temporary.path()),
            writable: vec![],
            credentials: vec![],
            controls: vec![
                CanonPath::new(metadata.join("config")),
                CanonPath::new(metadata.join("hooks")),
            ],
        });
        for (suffix, directory, expected) in [
            ("/modules/sub/config", false, true),
            ("/worktrees/peer/hooks/pre-commit", false, true),
            ("-neighbor/modules/sub/config", false, false),
            ("/modules/sub", true, true),
            ("/modules/sub/rebase-merge", true, false),
            ("/modules/sub/objects/ab", true, false),
            ("/modules/sub/refs/heads/main", true, false),
            ("/ordinary", false, false),
        ] {
            let path = std::path::PathBuf::from(format!("{}{suffix}", metadata.display()));
            if directory {
                std::fs::create_dir_all(&path).unwrap();
            }
            let matches = |suffix| {
                regex::Regex::new(&rooted_pattern(&metadata, suffix))
                    .unwrap()
                    .is_match(&path.to_string_lossy())
            };
            let oracle = matches(METADATA_CONTROL)
                || (directory && matches(METADATA_BOUNDARY) && !matches(METADATA_MODULE_TRANSIENT));
            assert_eq!(oracle, expected, "fixture {suffix}");
            assert_eq!(blocks(&path, &policy), expected, "floor {suffix}");
        }
    }

    // Catches: a Git control name added to the single list reaches only one of
    // the two floor patterns, silently weakening the other floor.
    #[test]
    fn every_git_control_name_reaches_both_floor_patterns() {
        let names: Vec<&str> = GIT_CONTROLS.iter().copied().chain(["extra-control"]).collect();
        let control = regex::Regex::new(&format!("^{}", control_pattern(&names))).unwrap();
        let boundary = regex::Regex::new(&format!("^{}", boundary_pattern(&names))).unwrap();
        for name in &names {
            for prefix in ["", "/modules/sub", "/worktrees/peer", "/modules/sub/worktrees/peer"] {
                let path = format!("{prefix}/{name}");
                assert!(control.is_match(&path), "control floor misses {path}");
                assert!(control.is_match(&format!("{path}/inner")), "control floor misses {path}/inner");
                assert!(!control.is_match(&format!("{path}-neighbor")), "control floor overreaches {path}-neighbor");
            }
            let directory = name.split_once('/').map_or(*name, |(head, _)| head);
            assert!(boundary.is_match(&format!("/{directory}")), "boundary floor misses /{directory}");
        }
        assert!(!boundary.is_match("/ordinary"));
    }

    // Catches: the shipped patterns drift from GIT_CONTROLS (a hand-written
    // alternation survives next to the list).
    #[test]
    fn shipped_floor_patterns_cover_the_shipped_control_list() {
        assert_eq!(metadata_control(), control_pattern(&GIT_CONTROLS));
        assert_eq!(metadata_boundary(), boundary_pattern(&GIT_CONTROLS));
    }
}

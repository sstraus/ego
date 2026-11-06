//! `AGENTS.md` on a real filesystem.
//!
//! `ego-core`'s `tests/config.rs` proves the plan and the gate against a fake.
//! What is proved here is what only a real filesystem can say: that a symlink
//! is refused rather than followed, that an oversized file is refused rather
//! than truncated, and that the ordinary cases behave.
//!
//! Unix-only for the symlink cases, which are the reason this file exists.
#![cfg(unix)]

use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use ego_core::config::{Found, InstructionIo, Limits, Refusal, Selection, load};
use ego_tools::instructions::OsInstructionIo;
use tempfile::{TempDir, tempdir};

struct Tree {
    _dir: TempDir,
    root: PathBuf,
}

impl Tree {
    /// `TempDir` hands back `/var/folders/...` on macOS, which is itself a
    /// symlink to `/private/var/...`. Canonicalising here keeps the fixture's
    /// paths comparable with what the discovery plan builds.
    fn new() -> Self {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        Tree { _dir: dir, root }
    }

    fn write(&self, relative: &str, contents: &str) -> PathBuf {
        let path = self.root.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, contents).unwrap();
        path
    }

    fn dir(&self, relative: &str) -> PathBuf {
        let path = self.root.join(relative);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn set(&self, cwd: &Path, limits: Limits) -> ego_core::config::InstructionSet {
        load(
            &OsInstructionIo,
            &self.root,
            cwd,
            &Selection::Ambient,
            limits,
        )
        .unwrap()
    }
}

fn texts(set: &ego_core::config::InstructionSet) -> Vec<&str> {
    set.loaded.iter().map(|i| i.text.as_str()).collect()
}

// -- The ordinary cases -------------------------------------------------

#[test]
fn a_workspace_with_no_agents_file_loads_nothing_and_refuses_nothing() {
    let tree = Tree::new();
    let cwd = tree.dir("src");

    let set = tree.set(&cwd, Limits::default());

    assert!(set.loaded.is_empty());
    assert!(
        set.skipped.is_empty(),
        "an absent file is not a refusal: {:?}",
        set.skipped
    );
}

#[test]
fn nested_agents_files_load_root_first_so_the_nearest_one_comes_last() {
    let tree = Tree::new();
    tree.write("AGENTS.md", "workspace rules");
    tree.write("crates/AGENTS.md", "crate rules");
    tree.write("crates/core/AGENTS.md", "core rules");
    let cwd = tree.dir("crates/core");

    let set = tree.set(&cwd, Limits::default());

    assert_eq!(
        texts(&set),
        ["workspace rules", "crate rules", "core rules"]
    );
    assert_eq!(set.loaded[0].path, tree.root.join("AGENTS.md"));
    assert_eq!(set.loaded[2].path, tree.root.join("crates/core/AGENTS.md"));
}

#[test]
fn relative_claude_alias_to_agents_does_not_duplicate_instruction_text() {
    let tree = Tree::new();
    tree.write("AGENTS.md", "canonical rules");
    symlink("AGENTS.md", tree.root.join("CLAUDE.md")).unwrap();

    let set = tree.set(&tree.root, Limits::default());

    assert_eq!(texts(&set), ["canonical rules"]);
    assert_eq!(set.loaded[0].path, tree.root.join("AGENTS.md"));
}

#[test]
fn an_agents_file_carries_its_real_byte_count_and_a_stable_digest() {
    let tree = Tree::new();
    tree.write("AGENTS.md", "abc");

    let set = tree.set(&tree.root.clone(), Limits::default());

    assert_eq!(set.loaded[0].bytes, 3);
    assert_eq!(
        set.loaded[0].digest,
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}

// -- Symlinks: the reason this file exists ------------------------------

/// A leaf symlink whose canonical target stays inside the discovery scope is
/// read, and the instruction carries the canonical target as its identity.
/// Catches: an approved shared layout (`AGENTS.md -> shared/RULES.md`) being
/// ignored, or a file count fixed at eight stopping a deeper hierarchy.
#[test]
fn approved_instruction_symlink_loads_with_configured_file_count() {
    let tree = Tree::new();
    let rules = tree.write("shared/RULES.md", "shared rules");
    symlink(&rules, tree.root.join("AGENTS.md")).unwrap();
    let mut cwd = tree.root.clone();
    for level in 1..10 {
        cwd = cwd.join(format!("d{level}"));
        tree.write(
            &cwd.strip_prefix(&tree.root)
                .unwrap()
                .join("AGENTS.md")
                .to_string_lossy(),
            &format!("level {level}"),
        );
    }

    let set = tree.set(
        &cwd,
        Limits {
            max_files: 12,
            ..Limits::default()
        },
    );

    assert_eq!(set.loaded.len(), 10, "{:?}", set.skipped);
    assert_eq!(set.loaded[0].text, "shared rules");
    assert_eq!(set.loaded[0].path, rules);
    let default = tree.set(&cwd, Limits::default());
    assert_eq!(default.loaded.len(), 8);
    assert_eq!(
        default.skipped[0].reason,
        Refusal::TooManyFiles { limit: 8 }
    );
}

/// A symlink to bytes outside the scope loads under the target's own path, so
/// the caller's external-approval gate sees a path outside the scope and
/// withholds it; cycles, oversize targets and linked parents stay refused.
/// Catches: a downloaded repository importing private bytes or exhausting
/// context through links.
#[test]
fn unapproved_private_import_cycle_or_oversize_content_is_refused() {
    let tree = Tree::new();
    let scope = tree.dir("repo");
    let secret = tree.write("private/secret.txt", "ssh-rsa AAAA-not-really");
    symlink(&secret, scope.join("AGENTS.md")).unwrap();
    let io = OsInstructionIo;
    let set = load(&io, &scope, &scope, &Selection::Ambient, Limits::default()).unwrap();
    assert_eq!(set.loaded.len(), 1);
    assert!(
        !set.loaded[0].path.starts_with(&scope),
        "the target path, not the repository path, must reach the approval gate"
    );

    let cyclic = tree.dir("cycle");
    symlink(cyclic.join("b"), cyclic.join("AGENTS.md")).unwrap();
    symlink(cyclic.join("AGENTS.md"), cyclic.join("b")).unwrap();
    let set = load(
        &io,
        &cyclic,
        &cyclic,
        &Selection::Ambient,
        Limits::default(),
    )
    .unwrap();
    assert!(set.loaded.is_empty());
    assert_eq!(set.skipped.len(), 1);

    let big = tree.dir("big");
    let target = tree.write("big-target.md", &"x".repeat(100));
    symlink(&target, big.join("AGENTS.md")).unwrap();
    let limits = Limits {
        max_file_bytes: 10,
        ..Limits::default()
    };
    let set = load(&io, &big, &big, &Selection::Ambient, limits).unwrap();
    assert!(set.loaded.is_empty());
    assert_eq!(set.skipped[0].reason, Refusal::TooLarge { limit: 10 });
}

#[test]
fn an_import_through_a_symlinked_directory_never_reads_its_target() {
    let tree = Tree::new();
    tree.write("AGENTS.md", "project rules\n@linked/secret.md\n");
    let secret = tree.write("private/secret.md", "private fixture bytes");
    symlink(secret.parent().unwrap(), tree.root.join("linked")).unwrap();

    let set = tree.set(&tree.root, Limits::default());

    assert_eq!(texts(&set), ["project rules\n@linked/secret.md\n"]);
    assert_eq!(set.skipped.len(), 1);
    assert_eq!(set.skipped[0].path, tree.root.join("linked/secret.md"));
    assert_eq!(set.skipped[0].reason, Refusal::Symlink);
}

#[test]
fn a_dangling_symlink_is_refused_as_a_symlink_not_reported_absent() {
    let tree = Tree::new();
    symlink(tree.root.join("nowhere.md"), tree.root.join("AGENTS.md")).unwrap();

    let set = tree.set(&tree.root.clone(), Limits::default());

    assert_eq!(
        set.skipped[0].reason,
        Refusal::Symlink,
        "a dangling link is still a link, and saying 'absent' would hide it"
    );
}

/// One refused file does not stop the rest: the workspace's own rules still
/// load, and the run can still say which one it dropped.
#[test]
fn a_refused_file_does_not_stop_the_files_around_it() {
    let tree = Tree::new();
    tree.write("AGENTS.md", "workspace rules");
    tree.dir("src");
    symlink(
        tree.root.join("nowhere.md"),
        tree.root.join("src/AGENTS.md"),
    )
    .unwrap();
    let cwd = tree.dir("src");

    let set = tree.set(&cwd, Limits::default());

    assert_eq!(texts(&set), ["workspace rules"]);
    assert_eq!(set.skipped[0].path, tree.root.join("src/AGENTS.md"));
    assert_eq!(set.skipped[0].reason, Refusal::Symlink);
}

// -- Not a regular file -------------------------------------------------

#[test]
fn a_directory_named_agents_md_is_refused_as_not_a_file() {
    let tree = Tree::new();
    tree.dir("AGENTS.md");

    let set = tree.set(&tree.root.clone(), Limits::default());

    assert_eq!(set.skipped[0].reason, Refusal::NotAFile);
}

// -- Size --------------------------------------------------------------

#[test]
fn a_file_at_exactly_the_limit_loads_and_one_byte_over_is_refused() {
    let tree = Tree::new();
    let limits = Limits {
        max_file_bytes: 16,
        ..Limits::default()
    };

    tree.write("AGENTS.md", &"a".repeat(16));
    assert_eq!(tree.set(&tree.root.clone(), limits).loaded[0].bytes, 16);

    tree.write("AGENTS.md", &"a".repeat(17));
    let set = tree.set(&tree.root.clone(), limits);
    assert!(
        set.loaded.is_empty(),
        "17 bytes must not load under a 16-byte limit"
    );
    assert_eq!(set.skipped[0].reason, Refusal::TooLarge { limit: 16 });
}

/// Refused whole, never truncated: half an instruction file can end
/// mid-sentence and mean the opposite of what it says.
#[test]
fn an_oversized_file_yields_no_partial_text() {
    let tree = Tree::new();
    tree.write("AGENTS.md", &format!("{}never do this", "x".repeat(64)));

    let set = tree.set(
        &tree.root.clone(),
        Limits {
            max_file_bytes: 8,
            ..Limits::default()
        },
    );

    assert!(set.loaded.is_empty());
    assert_eq!(set.bytes(), 0);
}

// -- Not text ----------------------------------------------------------

#[test]
fn a_file_that_is_not_utf8_is_refused_rather_than_mangled() {
    let tree = Tree::new();
    std::fs::write(tree.root.join("AGENTS.md"), [0xff, 0xfe, 0x00]).unwrap();

    let set = tree.set(&tree.root.clone(), Limits::default());

    assert_eq!(set.skipped[0].reason, Refusal::NotText);
}

// -- The port on its own -----------------------------------------------

#[test]
fn the_port_reports_a_missing_file_as_absent_not_as_an_error() {
    let tree = Tree::new();
    assert_eq!(
        OsInstructionIo.load(&tree.root.join("AGENTS.md"), 1024),
        Found::Absent
    );
}

/// Catches treating ~/ as a literal directory and refusing reviewed siblings.
#[test]
fn external_home_and_sibling_imports_stage_the_exact_boss_layout() {
    let tree = Tree::new();
    let home = tree.dir("home");
    let root = tree.dir("home/projects/agent");
    tree.write(
        "home/projects/agent/AGENTS.md",
        "@~/projects/policies/POLICY.md\n@../sibling.md\n",
    );
    let operator = tree.write("home/projects/policies/POLICY.md", "operator contract");
    let sibling = tree.write("home/projects/sibling.md", "Sibling contract");
    let staged = ego_core::config::load_with_home(
        &OsInstructionIo,
        &root,
        &root,
        &Selection::Ambient,
        Limits::default(),
        Some(&home),
    )
    .unwrap();
    assert_eq!(
        staged
            .loaded
            .iter()
            .map(|file| &file.path)
            .collect::<Vec<_>>(),
        vec![&root.join("AGENTS.md"), &operator, &sibling]
    );
    assert!(staged.skipped.is_empty());
    // The old contained-only entry point still cannot disclose these files.
    assert_eq!(
        load(
            &OsInstructionIo,
            &root,
            &root,
            &Selection::Ambient,
            Limits::default()
        )
        .unwrap()
        .loaded
        .len(),
        1
    );
}

/// Catches trusting an approved name after substitution by a leaf or parent link.
#[test]
fn external_import_substitution_never_follows_leaf_or_parent_links() {
    let tree = Tree::new();
    let target = tree.write("external/rules.md", "original");
    let secret = tree.write("secret/rules.md", "must not disclose");
    std::fs::remove_file(&target).unwrap();
    symlink(&secret, &target).unwrap();
    assert_eq!(
        OsInstructionIo.load(&target, 100),
        Found::Link {
            target: secret.clone()
        },
        "a leaf link is reported with its target, never read through"
    );
    std::fs::remove_file(&target).unwrap();
    std::fs::remove_dir(target.parent().unwrap()).unwrap();
    symlink(secret.parent().unwrap(), target.parent().unwrap()).unwrap();
    assert_eq!(
        OsInstructionIo.load(&target, 100),
        Found::Refused(Refusal::Symlink)
    );
}

/// Catches unbounded traversal after imports leave the original scope.
#[test]
fn external_imports_keep_cycles_and_byte_budgets() {
    let tree = Tree::new();
    let root = tree.dir("workspace");
    tree.write("workspace/AGENTS.md", "@../external.md");
    tree.write("external.md", "@workspace/AGENTS.md");
    let set = ego_core::config::load_with_home(
        &OsInstructionIo,
        &root,
        &root,
        &Selection::Ambient,
        Limits::default(),
        Some(&tree.root),
    )
    .unwrap();
    assert_eq!(set.loaded.len(), 2);
    assert!(
        set.skipped
            .iter()
            .any(|skip| skip.reason == Refusal::ImportCycle)
    );
    let set = ego_core::config::load_with_home(
        &OsInstructionIo,
        &root,
        &root,
        &Selection::Ambient,
        Limits {
            max_total_bytes: 15,
            ..Limits::default()
        },
        Some(&tree.root),
    )
    .unwrap();
    assert_eq!(set.loaded.len(), 1);
    assert!(
        set.skipped
            .iter()
            .any(|skip| matches!(skip.reason, Refusal::TotalTooLarge { .. }))
    );
}

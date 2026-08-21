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

/// The exfiltration case. This text goes into a prompt sent to a provider at
/// session start, with no permission dialog anywhere in its path, so a
/// symlinked `AGENTS.md` is one hop from "the repository decides what leaves
/// this machine".
#[test]
fn a_symlinked_agents_file_is_refused_and_its_target_is_never_read() {
    let tree = Tree::new();
    let secret = tree.write("secret.txt", "ssh-rsa AAAA-not-really");
    symlink(&secret, tree.root.join("AGENTS.md")).unwrap();

    let set = tree.set(&tree.root.clone(), Limits::default());

    assert!(set.loaded.is_empty(), "{:?}", set.loaded);
    assert_eq!(set.skipped.len(), 1);
    assert_eq!(set.skipped[0].reason, Refusal::Symlink);
    assert_eq!(set.skipped[0].path, tree.root.join("AGENTS.md"));
}

/// Refused even when the target is perfectly ordinary and inside the
/// workspace. Containment is not checked because a containment check that a
/// swap between check and open can defeat is not containment
/// (`POTENTIALS.md` P23) — so the rule is the same one either way, and it is
/// visible rather than silent.
#[test]
fn a_symlink_to_an_in_workspace_file_is_refused_too_rather_than_specially_allowed() {
    let tree = Tree::new();
    let real = tree.write("docs/agents.md", "perfectly ordinary rules");
    symlink(&real, tree.root.join("AGENTS.md")).unwrap();

    let set = tree.set(&tree.root.clone(), Limits::default());

    assert!(set.loaded.is_empty());
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
    let secret = tree.write("secret.txt", "not for the model");
    tree.dir("src");
    symlink(&secret, tree.root.join("src/AGENTS.md")).unwrap();
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

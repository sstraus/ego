//! Critic cases for `respect_ignore` against git's own semantics.
#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};

use ego_core::permit::{CanonPath, EntryKind, Fs, ScanLimits, ScanRequest};
use ego_tools::fs::OsFs;
use tempfile::TempDir;

struct Tree {
    _dir: TempDir,
    root: PathBuf,
}

impl Tree {
    fn new() -> Self {
        let dir = TempDir::new().expect("a temp directory");
        let root = fs::canonicalize(dir.path()).expect("canonical");
        Self { _dir: dir, root }
    }
    fn file(&self, relative: &str, contents: &str) {
        let at = self.root.join(relative);
        fs::create_dir_all(at.parent().unwrap()).unwrap();
        fs::write(at, contents).unwrap();
    }
    fn dir(&self, relative: &str) {
        fs::create_dir_all(self.root.join(relative)).unwrap();
    }
    /// Files reported when scanning `from` (relative to the tree root) with ignores honoured.
    fn files(&self, from: &str) -> Vec<String> {
        let root = if from.is_empty() {
            self.root.clone()
        } else {
            self.root.join(from)
        };
        self.scan(&root, true)
    }
    fn scan(&self, root: &Path, respect_ignore: bool) -> Vec<String> {
        let scan = OsFs
            .scan(&ScanRequest {
                root: CanonPath::new(root),
                skip_dirs: vec![".git".to_string()],
                respect_ignore,
                limits: ScanLimits::default(),
            })
            .expect("scan");
        let mut found: Vec<String> = scan
            .entries
            .iter()
            .filter(|e| e.kind == EntryKind::File)
            .map(|e| e.relative.display().to_string())
            .collect();
        found.sort();
        found
    }
}

/// Catches: the upward search for `.git` starting above the root, so a root that is
/// itself a repository (nested clone, vendored repo) inherits the OUTER repo's rules.
#[test]
fn a_root_that_is_its_own_repository_ignores_the_outer_repositorys_rules() {
    let t = Tree::new();
    t.dir(".git");
    t.file(".gitignore", "*.log\n");
    t.dir("inner/.git");
    t.file("inner/a.log", "x");
    t.file("inner/b.txt", "x");
    assert_eq!(t.files("inner"), vec!["a.log", "b.txt"]);
}

/// Catches: a nested repository below the root being filtered by the outer
/// `.gitignore` (git treats it as a separate repository).
#[test]
fn a_nested_repository_below_the_root_keeps_files_the_outer_rules_would_hide() {
    let t = Tree::new();
    t.dir(".git");
    t.file(".gitignore", "*.log\n");
    t.dir("vendor/dep/.git");
    t.file("vendor/dep/a.log", "x");
    assert_eq!(t.files(""), vec![".gitignore", "vendor/dep/a.log"]);
}

/// Catches: a deeper `!pattern` not overriding a shallower ignore.
#[test]
fn a_deeper_negation_reincludes_a_file_the_root_ignores() {
    let t = Tree::new();
    t.dir(".git");
    t.file(".gitignore", "*.log\n");
    t.file("sub/.gitignore", "!keep.log\n");
    t.file("sub/keep.log", "x");
    t.file("sub/drop.log", "x");
    assert_eq!(
        t.files(""),
        vec![".gitignore", "sub/.gitignore", "sub/keep.log"]
    );
}

/// Catches: a negation resurrecting a file whose parent directory is ignored
/// (git cannot re-include it).
#[test]
fn a_negation_cannot_reinclude_a_file_inside_an_ignored_directory() {
    let t = Tree::new();
    t.dir(".git");
    t.file(".gitignore", "build/\n!build/keep.txt\n");
    t.file("build/keep.txt", "x");
    assert_eq!(t.files(""), vec![".gitignore"]);
}

/// Catches: over-filtering when the root is itself ignored by an ancestor rule.
#[test]
fn a_root_ignored_by_an_ancestor_rule_is_still_listed() {
    let t = Tree::new();
    t.dir(".git");
    t.file(".gitignore", "build/\n");
    t.file("build/out.txt", "x");
    t.file("build/sub/deep.txt", "x");
    assert_eq!(t.files("build"), vec!["out.txt", "sub/deep.txt"]);
}

/// Catches: an ancestor rule anchored with a slash (`/sub/gen/`) not applying
/// when the scan root is `sub`.
#[test]
fn an_anchored_ancestor_rule_applies_to_a_scan_started_below_it() {
    let t = Tree::new();
    t.dir(".git");
    t.file(".gitignore", "/sub/gen/\n");
    t.file("sub/gen/x.txt", "x");
    t.file("sub/real.txt", "x");
    assert_eq!(t.files("sub"), vec!["real.txt"]);
}

/// Catches: ancestor `.gitignore` files applying when no repository encloses the
/// root (git would not read them).
#[test]
fn without_an_enclosing_repository_ancestor_rules_do_not_apply() {
    let t = Tree::new();
    t.file(".gitignore", "*.log\n");
    t.file("sub/a.log", "x");
    assert_eq!(t.files("sub"), vec!["a.log"]);
}

/// Catches: a `.git` FILE (linked worktree) not being recognised as the repository top.
#[test]
fn a_worktree_git_file_marks_the_repository_top() {
    let t = Tree::new();
    t.file(".git", "gitdir: /elsewhere\n");
    t.file(".gitignore", "*.log\n");
    t.file("sub/a.log", "x");
    t.file("sub/b.txt", "x");
    assert_eq!(t.files("sub"), vec!["b.txt"]);
}

/// Catches: a `dir/` rule hiding a symlink named like the directory (git only
/// ignores real directories for a trailing-slash rule).
#[test]
fn a_directory_only_rule_does_not_hide_a_symlink() {
    let t = Tree::new();
    t.dir(".git");
    t.file(".gitignore", "venv/\n");
    t.file("real/f.txt", "x");
    std::os::unix::fs::symlink(t.root.join("real/f.txt"), t.root.join("venv")).unwrap();
    assert!(t.files("").contains(&"venv".to_string()));
}

/// Catches: one unparsable line discarding the whole ignore file.
#[test]
fn a_malformed_line_does_not_disable_the_other_rules() {
    let t = Tree::new();
    t.dir(".git");
    t.file(".gitignore", "[\n*.log\n");
    t.file("a.log", "x");
    t.file("b.txt", "x");
    assert!(!t.files("").contains(&"a.log".to_string()));
}

/// Catches: CRLF line endings leaving a trailing `\r` in the pattern.
#[test]
fn crlf_gitignore_rules_still_match() {
    let t = Tree::new();
    t.dir(".git");
    t.file(".gitignore", "*.log\r\ncache/\r\n");
    t.file("a.log", "x");
    t.file("cache/x.txt", "x");
    t.file("b.txt", "x");
    assert_eq!(t.files(""), vec![".gitignore", "b.txt"]);
}

/// Catches: a `.gitignore` that is a directory or a dangling symlink aborting the scan.
#[test]
fn an_unreadable_gitignore_does_not_fail_the_scan() {
    let t = Tree::new();
    t.dir(".git");
    t.dir(".gitignore");
    std::os::unix::fs::symlink("/nonexistent/x", t.root.join("sub.link")).unwrap();
    t.file("a.txt", "x");
    assert_eq!(t.files(""), vec!["a.txt"]);
}

/// Catches: max_entries being spent on ignored entries (truncated flag set though
/// every visible entry fits).
#[test]
fn ignored_entries_do_not_count_toward_the_entry_limit() {
    let t = Tree::new();
    t.dir(".git");
    t.file(".gitignore", "*.pyc\n");
    for i in 0..20 {
        t.file(&format!("c{i}.pyc"), "x");
    }
    t.file("a.py", "x");
    let scan = OsFs
        .scan(&ScanRequest {
            root: CanonPath::new(&t.root),
            skip_dirs: vec![".git".to_string()],
            respect_ignore: true,
            limits: ScanLimits {
                max_entries: 3,
                ..ScanLimits::default()
            },
        })
        .unwrap();
    assert!(!scan.truncated);
}

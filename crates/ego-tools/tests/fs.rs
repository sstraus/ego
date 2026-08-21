//! `OsFs` against a real filesystem: the three obligations a fake can only
//! approximate.
//!
//! `crates/ego-core/tests/permit.rs` proves the guarded seam against
//! `FakeFs`, which is a table lookup. Everything here needs a real directory
//! on a real disk: a symlink loop, an inode that is a directory when you
//! asked for a file, and a walk that has to decide what to do with a link
//! pointing out of the tree.
//!
//! Unix-only, and not incidentally: symlinks, sockets and mode bits are how
//! every obligation here is stated, and none of the three has a portable
//! spelling. `ego` is built and tested on unix.
#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};

use ego_core::permit::{
    CanonPath, EntryKind, Fs, FsError, PathResolver, Scan, ScanLimits, ScanRequest,
};
use ego_tools::fs::OsFs;
use tempfile::TempDir;

/// A temp directory and its *canonical* path.
///
/// Canonical because on macOS `TempDir` hands back `/var/folders/...`, which
/// is itself a symlink to `/private/var/folders/...`. A scan root that is not
/// already canonical would make every path comparison in this file a test of
/// `/var` rather than of `OsFs`.
struct Tree {
    _dir: TempDir,
    root: PathBuf,
}

impl Tree {
    fn new() -> Self {
        let dir = TempDir::new().expect("a temp directory");
        let root = fs::canonicalize(dir.path()).expect("the temp directory resolves");
        Self { _dir: dir, root }
    }

    /// Creates `relative` and every directory above it, holding `contents`.
    fn file(&self, relative: &str, contents: &str) -> PathBuf {
        let at = self.root.join(relative);
        if let Some(parent) = at.parent() {
            fs::create_dir_all(parent).expect("the parent directories");
        }
        fs::write(&at, contents).expect("the file is written");
        at
    }

    fn dir(&self, relative: &str) -> PathBuf {
        let at = self.root.join(relative);
        fs::create_dir_all(&at).expect("the directory is created");
        at
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.root.join(relative)
    }

    fn canon_root(&self) -> CanonPath {
        CanonPath::new(&self.root)
    }

    /// A scan of the whole tree with nothing skipped and no limit in the way.
    fn scan(&self, fs: &OsFs) -> Scan {
        fs.scan(&ScanRequest::new(self.canon_root()))
            .expect("the root is a directory")
    }
}

/// The relative names a scan reported, sorted, so an assertion does not
/// depend on `read_dir` order.
fn names(scan: &Scan, kind: EntryKind) -> Vec<String> {
    let mut found: Vec<String> = scan
        .entries
        .iter()
        .filter(|entry| entry.kind == kind)
        .map(|entry| entry.relative.display().to_string())
        .collect();
    found.sort();
    found
}

mod resolving {
    use super::*;

    #[test]
    fn canonicalize_reports_a_missing_path_as_not_found_rather_than_resolving_it() {
        // The case that made `canonicalize` fallible in the first place: this
        // is exactly what `prepare` is handed when a model names a file that
        // is not there, and `Option`/panic were both wrong answers to it.
        let tree = Tree::new();
        let os = OsFs;

        let err = os
            .canonicalize(&tree.path("nowhere.txt"))
            .expect_err("a path that is not on disk cannot be canonicalized");

        assert!(
            matches!(err, FsError::NotFound { .. }),
            "expected NotFound, got {err:?}"
        );
        assert!(err.to_string().contains("no such file or directory"));
    }

    #[test]
    fn canonicalize_resolves_a_symlink_to_what_it_points_at() {
        let tree = Tree::new();
        let target = tree.file("real.txt", "contents");
        symlink(&target, &tree.path("link.txt"));
        let os = OsFs;

        let resolved = os
            .canonicalize(&tree.path("link.txt"))
            .expect("the link resolves");

        assert_eq!(resolved, CanonPath::new(target));
    }

    #[test]
    fn canonicalize_reports_a_symlink_cycle_as_a_loop_and_does_not_hang() {
        // `std::io::ErrorKind::FilesystemLoop` is still unstable (rust#86442),
        // so `OsFs` reads `ELOOP` off the error directly. This is the test
        // that proves the constant it was compiled against: a wrong value
        // would land the cycle in `FsError::Io` and fail here by name.
        let tree = Tree::new();
        symlink(&tree.path("b"), &tree.path("a"));
        symlink(&tree.path("a"), &tree.path("b"));
        let os = OsFs;

        let err = os
            .canonicalize(&tree.path("a"))
            .expect_err("a cycle cannot be resolved");

        assert!(
            matches!(err, FsError::Loop { .. }),
            "expected Loop, got {err:?}"
        );
        assert!(
            err.to_string()
                .contains("too many levels of symbolic links"),
            "the message has to say what went wrong: {err}"
        );
    }
}

mod reading {
    use super::*;

    #[test]
    fn read_returns_the_bytes_that_are_on_disk() {
        let tree = Tree::new();
        let at = tree.file("hello.txt", "ciao");
        let os = OsFs;

        let bytes = os.read(&CanonPath::new(at)).expect("the file is readable");

        assert_eq!(bytes, b"ciao");
    }

    #[test]
    fn read_reports_a_missing_file_as_not_found() {
        let tree = Tree::new();
        let os = OsFs;

        let err = os
            .read(&CanonPath::new(tree.path("gone.txt")))
            .expect_err("there is nothing there");

        assert!(
            matches!(err, FsError::NotFound { .. }),
            "expected NotFound, got {err:?}"
        );
    }

    #[test]
    fn read_reports_a_directory_as_not_a_file() {
        // The story's criterion calls this `NotADirectory`, which is the
        // variant for the mirror case — see `scan_rooted_at_a_file_...`
        // below. Asked to *read* a directory, the true statement is that the
        // thing is not a file, and that is the one a model can act on: read
        // something else, or list this instead.
        let tree = Tree::new();
        let at = tree.dir("src");
        let os = OsFs;

        let err = os
            .read(&CanonPath::new(at))
            .expect_err("a directory has no contents to read");

        assert!(
            matches!(err, FsError::NotAFile { .. }),
            "expected NotAFile, got {err:?}"
        );
        assert!(err.to_string().contains("not a file"));
    }

    #[test]
    fn write_puts_bytes_where_a_read_finds_them() {
        let tree = Tree::new();
        tree.dir("out");
        let at = CanonPath::new(tree.path("out/written.txt"));
        let os = OsFs;

        os.write(&at, b"durable").expect("the directory exists");

        assert_eq!(os.read(&at).expect("what was just written"), b"durable");
    }

    #[test]
    fn write_into_a_directory_that_is_not_there_reports_not_found() {
        let tree = Tree::new();
        let os = OsFs;

        let err = os
            .write(&CanonPath::new(tree.path("absent/file.txt")), b"x")
            .expect_err("the parent directory does not exist");

        assert!(
            matches!(err, FsError::NotFound { .. }),
            "expected NotFound, got {err:?}"
        );
    }
}

mod walking {
    use super::*;

    #[test]
    fn scan_reports_every_file_and_directory_under_the_root_but_not_the_root() {
        let tree = Tree::new();
        tree.file("main.rs", "fn main() {}");
        tree.file("deep/lib.rs", "pub fn lib() {}");
        let os = OsFs;

        let scan = tree.scan(&os);

        assert_eq!(names(&scan, EntryKind::File), ["deep/lib.rs", "main.rs"]);
        assert_eq!(names(&scan, EntryKind::Dir), ["deep"]);
        assert!(!scan.truncated, "nothing stopped this walk");
    }

    #[test]
    fn scan_carries_both_the_relative_name_and_the_canonical_path() {
        // They are different things and both are load-bearing: a glob pattern
        // is written against the relative one, and the permit is checked
        // against the canonical one.
        let tree = Tree::new();
        let at = tree.file("deep/lib.rs", "pub fn lib() {}");
        let os = OsFs;

        let scan = tree.scan(&os);

        let entry = scan
            .entries
            .iter()
            .find(|entry| entry.relative == Path::new("deep/lib.rs"))
            .expect("the file was found");
        assert_eq!(entry.path, CanonPath::new(at));
    }

    #[test]
    fn scan_rooted_at_a_file_reports_not_a_directory() {
        // The variant `docs/decisions/2026-08-21-reading-without-reading-everything.md`
        // §6 removed for having no caller. This is the caller it said it
        // would arrive with.
        let tree = Tree::new();
        let at = tree.file("main.rs", "fn main() {}");
        let os = OsFs;

        let err = os
            .scan(&ScanRequest::new(CanonPath::new(at)))
            .expect_err("a file cannot be walked");

        assert!(
            matches!(err, FsError::NotADirectory { .. }),
            "expected NotADirectory, got {err:?}"
        );
        assert!(err.to_string().contains("not a directory"));
    }

    #[test]
    fn scan_rooted_at_a_missing_directory_reports_not_found() {
        let tree = Tree::new();
        let os = OsFs;

        let err = os
            .scan(&ScanRequest::new(CanonPath::new(tree.path("absent"))))
            .expect_err("there is nothing to walk");

        assert!(
            matches!(err, FsError::NotFound { .. }),
            "expected NotFound, got {err:?}"
        );
    }

    #[test]
    fn scan_stops_at_max_depth_and_says_it_stopped() {
        let tree = Tree::new();
        tree.file("main.rs", "fn main() {}");
        tree.file("deep/lib.rs", "pub fn lib() {}");
        let os = OsFs;

        let scan = os
            .scan(&ScanRequest {
                root: tree.canon_root(),
                skip_dirs: Vec::new(),
                // `0` is the root's own children and nothing below.
                limits: ScanLimits {
                    max_depth: 0,
                    ..ScanLimits::default()
                },
            })
            .expect("the root is a directory");

        assert_eq!(names(&scan, EntryKind::File), ["main.rs"]);
        assert_eq!(
            names(&scan, EntryKind::Dir),
            ["deep"],
            "the directory is reported; it is only not entered"
        );
        assert!(
            scan.truncated,
            "a walk that did not enter a directory has not seen the whole tree"
        );
    }

    #[test]
    fn scan_stops_at_max_entries_and_says_it_stopped() {
        let tree = Tree::new();
        tree.file("a.rs", "");
        tree.file("b.rs", "");
        tree.file("c.rs", "");
        let os = OsFs;

        let scan = os
            .scan(&ScanRequest {
                root: tree.canon_root(),
                skip_dirs: Vec::new(),
                limits: ScanLimits {
                    max_entries: 2,
                    ..ScanLimits::default()
                },
            })
            .expect("the root is a directory");

        assert_eq!(scan.entries.len(), 2);
        assert!(scan.truncated, "hitting max_entries must be reported");
    }

    #[test]
    fn scan_returns_the_same_prefix_every_time_it_truncates() {
        // `read_dir` yields whatever order the filesystem feels like. A
        // truncated scan that truncated differently on each call would give a
        // model a different answer to the same question, which is worse than
        // a short answer.
        let tree = Tree::new();
        for name in ["e.rs", "c.rs", "a.rs", "d.rs", "b.rs"] {
            tree.file(name, "");
        }
        let os = OsFs;
        let capped = ScanRequest {
            root: tree.canon_root(),
            skip_dirs: Vec::new(),
            limits: ScanLimits {
                max_entries: 3,
                ..ScanLimits::default()
            },
        };

        let first = os.scan(&capped).expect("the root is a directory");
        let second = os.scan(&capped).expect("the root is a directory");

        assert_eq!(names(&first, EntryKind::File), ["a.rs", "b.rs", "c.rs"]);
        assert_eq!(
            names(&first, EntryKind::File),
            names(&second, EntryKind::File)
        );
    }

    #[test]
    fn skip_dirs_excludes_a_directory_and_everything_under_it_without_claiming_truncation() {
        // `skip_dirs` is a usability device, not a limit: nothing was cut
        // short, a name was asked not to be visited. Reporting it as
        // truncation would make every ordinary glob look incomplete.
        let tree = Tree::new();
        tree.file("main.rs", "");
        tree.file("target/debug/build.rs", "");
        let os = OsFs;

        let scan = os
            .scan(&ScanRequest {
                root: tree.canon_root(),
                skip_dirs: vec!["target".to_string()],
                limits: ScanLimits::default(),
            })
            .expect("the root is a directory");

        assert_eq!(names(&scan, EntryKind::File), ["main.rs"]);
        assert!(
            names(&scan, EntryKind::Dir).is_empty(),
            "a skipped directory is not reported either: {:?}",
            names(&scan, EntryKind::Dir)
        );
        assert!(!scan.truncated);
    }

    #[test]
    fn scan_does_not_report_a_socket_as_a_file() {
        // Neither a file to read nor a directory to walk. Reporting one as
        // either would be a lie about what the next operation can do with it,
        // and a `grep` that opened it would block forever.
        let tree = Tree::new();
        tree.file("main.rs", "");
        let _bound = socket(&tree.path("sock"));
        let os = OsFs;

        let scan = tree.scan(&os);

        assert_eq!(names(&scan, EntryKind::File), ["main.rs"]);
    }

    #[test]
    fn a_directory_it_cannot_open_is_skipped_rather_than_failing_the_whole_walk() {
        // One unreadable subdirectory is a fact about that subdirectory.
        // Abandoning the walk over it would make `glob` unusable in any tree
        // containing a directory this process does not own.
        let tree = Tree::new();
        tree.file("main.rs", "");
        let locked = tree.dir("locked");
        deny_all(&locked);
        let os = OsFs;

        assert!(
            fs::read_dir(&locked).is_err(),
            "this test needs a directory the process cannot open — is it running as root?"
        );
        let scan = tree.scan(&os);
        // Restore before the assertions, so a failure does not leave a
        // directory `TempDir::drop` cannot clean up.
        allow_all(&locked);

        assert_eq!(names(&scan, EntryKind::File), ["main.rs"]);
        assert_eq!(
            names(&scan, EntryKind::Dir),
            ["locked"],
            "the directory is still reported: it exists, it just cannot be entered"
        );
    }
}

mod symlinks {
    use super::*;

    #[test]
    fn a_scan_does_not_descend_into_a_directory_symlink() {
        // The escape the root check cannot see. A walk resolves many paths
        // where a read resolves one, so a link to a directory outside the root
        // would carry the traversal somewhere the permit was never shown.
        let outside = Tree::new();
        outside.file("secret.txt", "not yours");
        let tree = Tree::new();
        tree.file("main.rs", "");
        symlink(&outside.root, &tree.path("out"));
        let os = OsFs;

        let scan = tree.scan(&os);

        assert!(
            !scan
                .entries
                .iter()
                .any(|entry| entry.path.as_path().starts_with(&outside.root)),
            "a directory symlink must not be descended into: {:?}",
            scan.entries
        );
        assert_eq!(names(&scan, EntryKind::File), ["main.rs"]);
    }

    #[test]
    fn a_symlinked_file_is_reported_under_its_canonical_target() {
        // The other half of the no-follow rule, and the reason it is safe: a
        // symlinked file *is* reported, but under where it really points, so
        // an escape out of the root is something the permit check can see and
        // refuse rather than something it never learns about.
        let outside = Tree::new();
        let secret = outside.file("secret.txt", "not yours");
        let tree = Tree::new();
        symlink(&secret, &tree.path("innocent.rs"));
        let os = OsFs;

        let scan = tree.scan(&os);

        let entry = scan
            .entries
            .iter()
            .find(|entry| entry.relative == Path::new("innocent.rs"))
            .expect("the link is reported");
        assert_eq!(entry.kind, EntryKind::File);
        assert_eq!(
            entry.path,
            CanonPath::new(&secret),
            "the canonical path is where the content is, not where the link sits"
        );
    }

    #[test]
    fn a_symlink_cycle_inside_the_tree_does_not_hang_the_walk() {
        // Not detected — made impossible. There are no cycles in a tree you
        // refuse to leave, which is why `OsFs` needs no visited-set.
        let tree = Tree::new();
        tree.file("main.rs", "");
        symlink(&tree.root, &tree.path("self"));
        let os = OsFs;

        let scan = tree.scan(&os);

        assert_eq!(names(&scan, EntryKind::File), ["main.rs"]);
    }

    #[test]
    fn a_dangling_symlink_is_not_reported() {
        // There is nothing to read and nothing to walk. Reporting it would
        // hand a model a path whose every use fails.
        let tree = Tree::new();
        tree.file("main.rs", "");
        symlink(&tree.path("nowhere"), &tree.path("broken"));
        let os = OsFs;

        let scan = tree.scan(&os);

        assert_eq!(names(&scan, EntryKind::File), ["main.rs"]);
        assert!(names(&scan, EntryKind::Dir).is_empty());
    }
}

fn symlink(target: &Path, at: &Path) {
    std::os::unix::fs::symlink(target, at).expect("the symlink is created");
}

/// A unix domain socket, which is the cheapest way to put something in a
/// directory that is neither a file nor a directory.
fn socket(at: &Path) -> std::os::unix::net::UnixListener {
    std::os::unix::net::UnixListener::bind(at).expect("the socket is bound")
}

fn deny_all(at: &Path) {
    set_mode(at, 0o000);
}

fn allow_all(at: &Path) {
    set_mode(at, 0o755);
}

fn set_mode(at: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(at, fs::Permissions::from_mode(mode)).expect("the mode is set");
}

/// The two behaviours the grammar ADR reasoned about and did not measure.
///
/// `docs/decisions/2026-08-21-a-rule-that-means-exactly-one-thing.md` §5
/// decides against case folding and against Unicode normalisation, and names
/// both residuals as *unverified on this machine* — believed to fail closed,
/// with one experiment each. These are those experiments. They belong here
/// and not beside the grammar, because what is in question is not what the
/// matcher does with a path but what `canonicalize` hands it in the first
/// place, and only a real volume can answer that.
///
/// If either ever fails *open* — a pattern matching a path it does not spell
/// — the ADR is wrong and needs amending rather than the code.
mod rule_matching_against_a_real_volume {
    use super::*;
    use ego_core::permit::rule::Rule;

    #[test]
    fn a_rule_whose_casing_differs_from_the_disk_matches_nothing() {
        // On a case-insensitive volume the file opens under either spelling,
        // but `canonicalize` returns the casing that is on disk — so the rule
        // visibly does nothing instead of quietly matching. That is the
        // direction to fail in.
        let tree = Tree::new();
        let file = tree.file("src/main.rs", "fn main() {}\n");
        let canonical = fs::canonicalize(&file).expect("the file resolves");

        assert!(
            canonical.ends_with("src/main.rs"),
            "canonicalize folded the case: {}",
            canonical.display()
        );
        assert!(
            !parse("read(SRC/**)").matches(&read(&canonical), &tree.root),
            "a rule spelled SRC matched a path spelled src"
        );
        assert!(parse("read(src/**)").matches(&read(&canonical), &tree.root));
    }

    #[test]
    fn a_rule_composed_differently_from_the_disk_matches_nothing() {
        // Composed é (U+00E9) as the rule, decomposed e + U+0301 on disk.
        // Whether the volume stores the name as given or normalises it, the
        // two spellings do not match, and the failure is closed either way.
        let tree = Tree::new();
        let file = tree.file("caf\u{65}\u{301}.txt", "beans\n");
        let canonical = fs::canonicalize(&file).expect("the file resolves");

        assert!(
            !parse("read(caf\u{e9}.txt)").matches(&read(&canonical), &tree.root),
            "a composed rule matched a decomposed name: {}",
            canonical.display()
        );
    }

    fn parse(raw: &str) -> Rule {
        Rule::parse(raw).unwrap_or_else(|err| panic!("`{raw}` should parse: {err}"))
    }

    fn read(path: &Path) -> ego_core::permit::Op {
        ego_core::permit::Op::Read {
            path: CanonPath::new(path),
        }
    }
}

//! A relative deny rule still matches when the working directory
//! is a symlink, whichever spelling of the path the model names.

#![cfg(unix)]

use std::path::{Path, PathBuf};

use ego_core::config::{Mode, PermCfg, Ruleset, Source};
use ego_core::permit::rule::Rule;
use ego_core::permit::{
    CanonPath, Classification, PolicyDenied, PolicyEngine, PrepareCtx, Resolution, Tool, Typed,
};
use ego_tools::fs::OsFs;
use ego_tools::tools::{ReadTool, WriteTool};
use futures::executor::block_on;
use serde_json::json;

struct Workspace {
    base: PathBuf,
    physical: PathBuf,
    lexical: PathBuf,
}

impl Workspace {
    fn new() -> Self {
        let base = std::fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!("ego-862-{}", uuid::Uuid::now_v7()));
        let physical = base.join("real");
        std::fs::create_dir_all(physical.join("secrets")).unwrap();
        std::fs::write(physical.join("secrets/key.txt"), "k").unwrap();
        std::fs::write(physical.join("open.txt"), "o").unwrap();
        let lexical = base.join("link");
        std::os::unix::fs::symlink(&physical, &lexical).unwrap();
        Self {
            base,
            physical,
            lexical,
        }
    }

    /// The engine as `run` builds it: the workspace is the canonical form of
    /// the directory the user named, here the symlink.
    fn engine(&self) -> PolicyEngine {
        let workspace = std::fs::canonicalize(&self.lexical).unwrap();
        assert_eq!(workspace, self.physical);
        let deny = ["read(secrets/**)", "write(secrets/**)"]
            .map(|raw| Rule::parse(raw).unwrap())
            .to_vec();
        PolicyEngine::new(
            CanonPath::new(workspace),
            Mode::Yolo,
            Ruleset::from_layer(
                PermCfg {
                    judge: None,
                    allow: Vec::new(),
                    deny,
                },
                Source::Project,
            ),
        )
    }

    /// The tool's cwd, canonical as in `run`.
    fn cwd(&self) -> CanonPath {
        CanonPath::new(std::fs::canonicalize(&self.lexical).unwrap())
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.base).unwrap();
    }
}

fn denied(engine: &PolicyEngine, tool: &dyn Tool, cwd: CanonPath, path: &str) -> bool {
    let ctx = PrepareCtx {
        cwd,
        resolver: &OsFs,
    };
    let args = if tool.descriptor().name == "write" {
        json!({ "files": [{ "path": path, "contents": "x" }] })
    } else {
        json!({ "files": [{ "path": path }] })
    };
    let call = tool
        .prepare(args, &ctx)
        .unwrap_or_else(|error| panic!("{path}: {error:?}"));
    let answer = match engine.classify(call) {
        Classification::Settled(answer) => answer,
        Classification::Ask(pending) => match block_on(engine.settle(pending)) {
            Resolution::Settled(answer) => answer,
            _ => panic!("yolo settles without a human"),
        },
    };
    matches!(answer, Err(PolicyDenied::Denied { .. }))
}

fn spellings(workspace: &Workspace, file: &str) -> [String; 3] {
    let show = |base: &Path| base.join(file).display().to_string();
    [
        file.to_string(),
        show(&workspace.lexical),
        show(&workspace.physical),
    ]
}

// Catches: a relative deny rule compared against the lexical (symlink) form
// of the path, so naming the file through the link slips past the deny.
#[test]
fn a_relative_deny_matches_in_a_symlinked_working_directory_for_every_path_form() {
    let workspace = Workspace::new();
    let engine = workspace.engine();
    let read = Typed(ReadTool);
    let write = Typed(WriteTool);
    for path in spellings(&workspace, "secrets/key.txt") {
        assert!(
            denied(&engine, &read, workspace.cwd(), &path),
            "read {path}"
        );
        assert!(
            denied(&engine, &write, workspace.cwd(), &path),
            "write {path}"
        );
    }
    // A file the rule does not cover stays allowed: the denial is the rule's.
    for path in spellings(&workspace, "open.txt") {
        assert!(
            !denied(&engine, &read, workspace.cwd(), &path),
            "read {path}"
        );
    }
}

// Catches: a symlink inside the working directory that points into the denied
// subtree, reached by a relative name, is judged by its own name.
#[test]
fn a_relative_deny_matches_through_a_symlink_inside_the_working_directory() {
    let workspace = Workspace::new();
    std::os::unix::fs::symlink(
        workspace.physical.join("secrets"),
        workspace.physical.join("alias"),
    )
    .unwrap();
    let engine = workspace.engine();
    let read = Typed(ReadTool);
    for path in spellings(&workspace, "alias/key.txt") {
        assert!(
            denied(&engine, &read, workspace.cwd(), &path),
            "read {path}"
        );
    }
}

use std::sync::Mutex;

use super::*;

#[derive(Default)]
struct Notes(Mutex<Vec<String>>);
impl Diagnostics for Notes {
    fn note(&self, message: &str) {
        self.0.lock().unwrap().push(message.to_owned());
    }
}

struct Workspace {
    root: tempfile::TempDir,
    home: tempfile::TempDir,
    notes: Notes,
}
impl Workspace {
    fn new() -> Self {
        let fixture = Self {
            root: tempfile::tempdir().unwrap(),
            home: tempfile::tempdir().unwrap(),
            notes: Notes::default(),
        };
        fixture.write(
            ".ego/config.toml",
            "[mcp.servers.local]\ntransport='stdio'\ncommand='./server'\n",
        );
        fixture.write("server", "first program\n");
        fixture
    }
    fn write(&self, path: &str, text: &str) {
        let path = self.root.path().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    fn launch(&self, explicit: bool) -> Trust {
        resolve_headless(
            self.root.path(),
            Some(self.home.path()),
            &self.notes,
            explicit,
        )
    }
    fn grant(&self) {
        assert_eq!(self.launch(true), Trust::Trusted);
        // An owner grant in the real persisted document, not a synthetic surface.
        let path = self.home.path().join("trust.json");
        let mut ledger: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        let root = fs::canonicalize(self.root.path())
            .unwrap()
            .display()
            .to_string();
        ledger["workspaces"][root]["development_programs"] = true.into();
        fs::write(path, serde_json::to_vec(&ledger).unwrap()).unwrap();
    }
}

// Catches: an owner edit of an approved MCP or hook program stops a headless job.
#[test]
fn development_grant_reports_named_program_edit_without_reprompt() {
    for hook in [false, true] {
        let fixture = Workspace::new();
        if hook {
            fixture.write(".ego/config.toml", "[[hooks]]\nevent='pre_tool_use'\n[hooks.handler]\ntype='command'\ncommand='./server'\n");
        }
        fixture.grant();
        fixture.write("server", "edited program\n");
        assert_eq!(
            fixture.launch(false),
            Trust::Trusted,
            "approved edit must remain trusted"
        );
        let notes = fixture.notes.0.lock().unwrap().join("\n");
        assert!(
            notes.contains("development") && notes.contains("server"),
            "missing changed-program notice: {notes}"
        );
    }
}

// Catches: a newly downloaded program inherits a prior program's development grant.
#[test]
fn new_program_cannot_inherit_named_development_grant() {
    let fixture = Workspace::new();
    fixture.grant();
    fixture.write("server", "edited program\n");
    assert_eq!(
        fixture.launch(false),
        Trust::Trusted,
        "the existing development grant must work first"
    );
    fixture.write("new-server", "downloaded program\n");
    fixture.write(".ego/config.toml", "[mcp.servers.local]\ntransport='stdio'\ncommand='./server'\n[mcp.servers.downloaded]\ntransport='stdio'\ncommand='./new-server'\n");
    assert_eq!(fixture.launch(false), Trust::Untrusted);
}

// Catches: the launch flag parses but never persists a usable, revocable grant.
#[test]
fn development_launch_grant_persists_and_regular_reapproval_revokes_it() {
    let fixture = Workspace::new();
    let args = crate::cli::parse(
        ["ego", "run", "--trust-development-programs", "task"]
            .map(str::to_owned)
            .to_vec(),
    )
    .unwrap();
    let crate::cli::Invocation::Run(args) = args else {
        panic!("run invocation")
    };
    assert_eq!(
        resolve_selected(
            fixture.root.path(),
            Some(fixture.home.path()),
            &fixture.notes,
            args.trust_workspace,
            args.trust_development_programs,
            None,
            |_| panic!("explicit grant cannot ask")
        ),
        Trust::Trusted
    );
    fixture.write("server", "owner edit\n");
    assert_eq!(fixture.launch(false), Trust::Trusted);
    let surface = executable_surface(fixture.root.path());
    assert_eq!(
        remembered_decision(fixture.root.path(), Some(fixture.home.path()), &surface),
        Some(Trust::Trusted),
        "ACP shares this grant"
    );
    assert_eq!(fixture.launch(true), Trust::Trusted);
    fixture.write("server", "another edit\n");
    assert_eq!(
        fixture.launch(false),
        Trust::Untrusted,
        "ordinary approval removes development scope"
    );
    for flags in [vec!["--bare"], vec!["--trust-workspace"]] {
        let argv = [vec!["ego", "run", "--trust-development-programs"], flags].concat();
        assert!(crate::cli::parse(argv.into_iter().map(str::to_owned).collect()).is_err());
    }
}

// Catches: the development exception spreads into executable declarations or command Markdown.
#[test]
fn development_grant_keeps_declaration_command_and_workspace_boundaries() {
    for change in ["command", "args", "markdown", "removed"] {
        let fixture = Workspace::new();
        fixture.write(".ego/commands/review.md", "Review !`true`\n");
        fixture.grant();
        fixture.write("server", "owner edit\n");
        match change {
            "command" => fixture.write(
                ".ego/config.toml",
                "[mcp.servers.local]\ncommand='./other'\n",
            ),
            "args" => fixture.write(
                ".ego/config.toml",
                "[mcp.servers.local]\ntransport='stdio'\ncommand='./server'\nargs=['new']\n",
            ),
            "markdown" => fixture.write(".ego/commands/review.md", "Review !`false`\n"),
            _ => fs::remove_file(fixture.root.path().join("server")).unwrap(),
        }
        assert_eq!(fixture.launch(false), Trust::Untrusted, "{change}");
    }
    let fixture = Workspace::new();
    fixture.grant();
    let other = Workspace::new();
    assert_eq!(
        resolve_headless(
            other.root.path(),
            Some(fixture.home.path()),
            &fixture.notes,
            false
        ),
        Trust::Untrusted
    );
}

// Catches: an existing program name conceals a changed symlink destination.
#[cfg(unix)]
#[test]
fn development_grant_accepts_rebuild_modes_but_refuses_symlink_retargeting() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Workspace::new();
    fixture.write("first", "first program\n");
    fixture.write("second", "second program\n");
    fs::remove_file(fixture.root.path().join("server")).unwrap();
    std::os::unix::fs::symlink("first", fixture.root.path().join("server")).unwrap();
    fixture.grant();
    fs::set_permissions(
        fixture.root.path().join("first"),
        fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    assert_eq!(fixture.launch(false), Trust::Trusted);
    fixture.write("first", "rebuilt program\n");
    assert_eq!(fixture.launch(false), Trust::Trusted);
    fs::remove_file(fixture.root.path().join("server")).unwrap();
    std::os::unix::fs::symlink("second", fixture.root.path().join("server")).unwrap();
    assert_eq!(fixture.launch(false), Trust::Untrusted);
}

// Catches: a retargeted parent symlink runs a different program under the old development grant.
#[cfg(unix)]
#[test]
fn development_grant_refuses_retargeted_program_parent_symlink() {
    for through_leaf_link in [false, true] {
        let fixture = Workspace::new();
        let root = fixture.root.path();
        fixture.write("original/server", "#!/bin/sh\necho original\n");
        fixture.write("replacement/server", "#!/bin/sh\necho replacement\n");
        let command = if through_leaf_link {
            std::os::unix::fs::symlink("active/server", root.join("launcher")).unwrap();
            "./launcher"
        } else {
            "./active/server"
        };
        fixture.write(
            ".ego/config.toml",
            &format!("[mcp.servers.dev]\ncommand = '{command}'\n"),
        );
        std::os::unix::fs::symlink("original", root.join("active")).unwrap();
        assert_eq!(
            resolve_selected(
                root,
                Some(fixture.home.path()),
                &fixture.notes,
                false,
                true,
                None,
                |_| None,
            ),
            Trust::Trusted,
        );
        fs::remove_file(root.join("active")).unwrap();
        std::os::unix::fs::symlink("replacement", root.join("active")).unwrap();
        assert_eq!(
            fixture.launch(false),
            Trust::Untrusted,
            "a different resolved program must not inherit the prior development grant",
        );
    }
}

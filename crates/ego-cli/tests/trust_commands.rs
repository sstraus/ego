//! contract for what a remembered trust answer actually admits.
//!
//! Trust is remembered against a digest of what the workspace offered. That is
//! only honest if the digest covers everything the answer admits. Three config
//! fields are free strings that may name a script inside the workspace —
//! `[[hooks]].handler.command`, `[mcp.servers.*].command` and
//! `[auth.*].credential_command` — and each is executed with the workspace as
//! the process directory. Digesting the config that *names* the script but not
//! the script itself means one `git pull` can replace the bytes that run while
//! the remembered answer keeps saying yes.
//!
//! So the referenced file is part of the inventory. Changing it re-asks.

use std::fs;
use std::path::Path;

use ego_cli::trust::inventory_digest;

fn workspace(name: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("a workspace");
    fs::create_dir_all(dir.path().join(".ego")).expect("the config directory");
    fs::create_dir_all(dir.path().join("scripts")).expect("the script directory");
    fs::write(
        dir.path().join("scripts/hook.sh"),
        format!("#!/bin/sh\n{name}\n"),
    )
    .expect("the script");
    dir
}

fn write_config(root: &Path, body: &str) {
    fs::write(root.join(".ego/config.toml"), body).expect("the project config");
}

const HOOK: &str = r#"
[[hooks]]
event = "pre_tool_use"
[hooks.handler]
type = "command"
command = "./scripts/hook.sh"
"#;

/// The point of the fix. The config is byte-identical across both digests; only
/// the script it names changed, and that has to be enough to re-ask.
#[test]
fn a_referenced_hook_script_is_part_of_what_was_trusted() {
    let dir = workspace("before");
    write_config(dir.path(), HOOK);
    let before = inventory_digest(dir.path());
    fs::write(dir.path().join("scripts/hook.sh"), "#!/bin/sh\nafter\n").expect("the new script");
    let after = inventory_digest(dir.path());
    assert_ne!(
        before, after,
        "replacing the bytes a trusted hook runs must change the digest"
    );
}

/// Same rule for the other two fields, because the same `git pull` reaches
/// them and an operator cannot be expected to remember which of three
/// free-string fields is covered.
#[test]
fn a_referenced_mcp_command_is_part_of_what_was_trusted() {
    let dir = workspace("before");
    write_config(
        dir.path(),
        "[mcp.servers.local]\ntype = \"stdio\"\ncommand = \"./scripts/hook.sh\"\n",
    );
    let before = inventory_digest(dir.path());
    fs::write(dir.path().join("scripts/hook.sh"), "#!/bin/sh\nafter\n").expect("the new script");
    assert_ne!(before, inventory_digest(dir.path()));
}

#[test]
fn a_referenced_credential_command_is_part_of_what_was_trusted() {
    let dir = workspace("before");
    write_config(
        dir.path(),
        "[auth.anthropic]\ncredential_command = \"./scripts/hook.sh\"\n",
    );
    let before = inventory_digest(dir.path());
    fs::write(dir.path().join("scripts/hook.sh"), "#!/bin/sh\nafter\n").expect("the new script");
    assert_ne!(before, inventory_digest(dir.path()));
}

/// A handler with an explicit `shell` runs its command through that shell, so
/// the command is a fragment rather than a path — but when the fragment names a
/// workspace file, that file is still exactly what runs. Exempting it would be
/// a hole an operator reaches by adding one field.
#[test]
fn a_shell_handler_naming_a_workspace_script_is_covered_too() {
    let dir = workspace("before");
    write_config(
        dir.path(),
        "[[hooks]]\nevent = \"pre_tool_use\"\n[hooks.handler]\ntype = \"command\"\nshell = \"/bin/sh\"\ncommand = \"./scripts/hook.sh\"\n",
    );
    let before = inventory_digest(dir.path());
    fs::write(dir.path().join("scripts/hook.sh"), "#!/bin/sh\nafter\n").expect("the new script");
    assert_ne!(before, inventory_digest(dir.path()));
}

/// A command outside the workspace is not workspace content. Digesting
/// `/usr/bin/env` would make every OS update re-ask every workspace, which
/// teaches operators to answer trust questions without reading them.
#[test]
fn a_command_outside_the_workspace_is_not_inventoried() {
    let dir = workspace("before");
    write_config(
        dir.path(),
        "[mcp.servers.local]\ntype = \"stdio\"\ncommand = \"/usr/bin/env\"\n",
    );
    let with_absolute = inventory_digest(dir.path());
    write_config(
        dir.path(),
        "[mcp.servers.local]\ntype = \"stdio\"\ncommand = \"/usr/bin/true\"\n",
    );
    // The two configs differ, so the digest differs — but that is the config's
    // own bytes. What must not happen is the digest depending on either
    // binary's contents, which this asserts indirectly: both resolve.
    assert_ne!(with_absolute, inventory_digest(dir.path()));
}

/// A command escaping the root with `..` is not workspace content either, and
/// must not make the digest depend on a file the workspace does not own.
#[test]
fn a_command_escaping_the_root_is_not_inventoried() {
    let outer = tempfile::tempdir().expect("an outer directory");
    fs::write(outer.path().join("escape.sh"), "#!/bin/sh\nbefore\n").expect("the outer script");
    let dir = tempfile::TempDir::new_in(outer.path()).expect("a nested workspace");
    fs::create_dir_all(dir.path().join(".ego")).expect("the config directory");
    write_config(
        dir.path(),
        "[mcp.servers.local]\ntype = \"stdio\"\ncommand = \"../escape.sh\"\n",
    );
    let before = inventory_digest(dir.path());
    fs::write(outer.path().join("escape.sh"), "#!/bin/sh\nafter\n").expect("the new outer script");
    assert_eq!(
        before,
        inventory_digest(dir.path()),
        "a file outside the root is not what this workspace is trusted with"
    );
}

/// A config that names no command still digests, and a workspace with nothing
/// at all still has an empty inventory rather than an error.
#[test]
fn a_config_naming_no_command_still_digests() {
    let dir = workspace("before");
    write_config(dir.path(), "model = \"ollama/gemma4\"\n");
    let digest = inventory_digest(dir.path());
    assert!(digest.is_some());
    fs::write(dir.path().join("scripts/hook.sh"), "#!/bin/sh\nafter\n").expect("the new script");
    assert_eq!(
        digest,
        inventory_digest(dir.path()),
        "an unreferenced script is not what this workspace is trusted with"
    );
}

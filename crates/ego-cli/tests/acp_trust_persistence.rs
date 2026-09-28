use std::fs;

use ego_acp::serve::{SessionTrustAdmission, SessionTrustStatus};
use ego_cli::acp_trust::CliSessionTrustAdmission;
use ego_core::config::Trust;
use ego_core::run::RunRoot;

#[test]
fn acp_and_terminal_ledger_remember_both_answers_across_adapters() {
    for decision in [Trust::Trusted, Trust::Untrusted] {
        let root = tempfile::tempdir().expect("workspace");
        let home = tempfile::tempdir().expect("ego home");
        fs::write(root.path().join("AGENTS.md"), "rules\n").expect("instruction");
        let root = RunRoot::new(root.path()).expect("root");
        let first = CliSessionTrustAdmission::new(Some(home.path().to_path_buf()));
        let SessionTrustStatus::Question { fingerprint, .. } = first.status(&root) else {
            panic!("first visit asks")
        };
        assert!(first.record_answer(&root, &fingerprint, decision));
        let later = CliSessionTrustAdmission::new(Some(home.path().to_path_buf()));
        assert!(
            matches!(later.status(&root), SessionTrustStatus::Remembered(found) if found == decision)
        );
        assert!(home.path().join("trust.json").is_file());
    }
}

#[test]
fn acp_reasks_with_redacted_executable_diff_but_ignores_instruction_edits() {
    let root = tempfile::tempdir().expect("workspace");
    let home = tempfile::tempdir().expect("ego home");
    fs::create_dir_all(root.path().join(".ego")).expect("config dir");
    fs::create_dir_all(root.path().join("scripts")).expect("program dir");
    fs::write(root.path().join("AGENTS.md"), "rules\n").expect("instruction");
    fs::write(root.path().join("scripts/server.sh"), "echo old\n").expect("program");
    fs::write(root.path().join(".ego/config.toml"),
        "[mcp.servers.local]\ntransport='stdio'\ncommand='./scripts/server.sh'\nargs=['--old']\ninherit_env=['TOKEN']\n").expect("config");
    let root = RunRoot::new(root.path()).expect("root");
    let adapter = CliSessionTrustAdmission::new(Some(home.path().to_path_buf()));
    let SessionTrustStatus::Question { fingerprint, .. } = adapter.status(&root) else {
        panic!("first visit asks")
    };
    assert!(adapter.record_answer(&root, &fingerprint, Trust::Trusted));
    fs::write(root.as_path().join("AGENTS.md"), "changed rules\n").expect("instruction edit");
    assert!(matches!(
        adapter.status(&root),
        SessionTrustStatus::Remembered(Trust::Trusted)
    ));
    fs::write(
        root.as_path().join("scripts/server.sh"),
        "echo changed and longer\n",
    )
    .expect("program edit");
    fs::write(root.as_path().join(".ego/config.toml"),
        "[mcp.servers.local]\ntransport='stdio'\ncommand='./scripts/server.sh'\nargs=['--new']\ninherit_env=['TOKEN']\n").expect("config edit");
    let SessionTrustStatus::Question { content, .. } = adapter.status(&root) else {
        panic!("changed executable asks")
    };
    assert!(
        content.contains("MCP server local in .ego/config.toml"),
        "{content}"
    );
    assert!(
        content.contains("--old") && content.contains("--new"),
        "{content}"
    );
    assert!(
        content.contains("size=9") && content.contains("size=24"),
        "{content}"
    );
    assert!(content.contains("TOKEN"), "{content}");
    assert!(!content.contains("changed rules"), "{content}");
}

#[test]
fn acp_answer_to_a_stale_executable_fingerprint_is_not_stored() {
    let root = tempfile::tempdir().expect("workspace");
    let home = tempfile::tempdir().expect("ego home");
    fs::write(root.path().join("AGENTS.md"), "rules\n").expect("instruction");
    let root = RunRoot::new(root.path()).expect("root");
    let adapter = CliSessionTrustAdmission::new(Some(home.path().to_path_buf()));
    let SessionTrustStatus::Question { fingerprint, .. } = adapter.status(&root) else {
        panic!("first visit asks")
    };
    fs::create_dir_all(root.as_path().join(".ego")).expect("config dir");
    fs::write(
        root.as_path().join(".ego/config.toml"),
        "[mcp.servers.local]\ntransport='stdio'\ncommand='/usr/bin/env'\n",
    )
    .expect("new server");
    assert!(!adapter.record_answer(&root, &fingerprint, Trust::Trusted));
    assert!(!home.path().join("trust.json").exists());
}

#[test]
fn deleting_a_stored_workspace_entry_revokes_the_answer() {
    let root = tempfile::tempdir().expect("workspace");
    let home = tempfile::tempdir().expect("ego home");
    fs::write(root.path().join("AGENTS.md"), "rules\n").expect("instruction");
    let root = RunRoot::new(root.path()).expect("root");
    let adapter = CliSessionTrustAdmission::new(Some(home.path().to_path_buf()));
    let SessionTrustStatus::Question { fingerprint, .. } = adapter.status(&root) else {
        panic!("first visit asks")
    };
    assert!(adapter.record_answer(&root, &fingerprint, Trust::Trusted));
    let path = home.path().join("trust.json");
    let mut ledger: serde_json::Value =
        serde_json::from_slice(&fs::read(&path).expect("ledger")).expect("json");
    ledger["workspaces"]
        .as_object_mut()
        .expect("workspace map")
        .remove(&root.as_path().display().to_string());
    fs::write(&path, serde_json::to_vec(&ledger).expect("json")).expect("revoked ledger");
    assert!(matches!(
        adapter.status(&root),
        SessionTrustStatus::Question { .. }
    ));
}

#[test]
fn reask_shows_added_and_removed_servers_with_their_config_sources() {
    let root = tempfile::tempdir().expect("workspace");
    let home = tempfile::tempdir().expect("ego home");
    fs::create_dir_all(root.path().join(".ego")).expect("config dir");
    fs::write(root.path().join(".ego/config.toml"),
        "[mcp.servers.old]\ntransport='stdio'\ncommand='/usr/bin/old'\n[mcp.servers.stable]\ntransport='stdio'\ncommand='/usr/bin/stable'\n").expect("config");
    let root = RunRoot::new(root.path()).expect("root");
    let adapter = CliSessionTrustAdmission::new(Some(home.path().to_path_buf()));
    let SessionTrustStatus::Question { fingerprint, .. } = adapter.status(&root) else {
        panic!("first visit asks")
    };
    assert!(adapter.record_answer(&root, &fingerprint, Trust::Trusted));
    fs::write(
        root.as_path().join(".ego/config.toml"),
        "[mcp.servers.stable]\ntransport='stdio'\ncommand='/usr/bin/stable'\n",
    )
    .expect("config edit");
    fs::write(root.as_path().join(".ego/config.local.toml"),
        "[mcp.servers.new]\ntransport='stdio'\ncommand='/usr/bin/new'\nargs=['--ready']\ninherit_env=['TOKEN']\n").expect("local config");
    let SessionTrustStatus::Question { content, .. } = adapter.status(&root) else {
        panic!("executable change asks")
    };
    assert!(
        content.contains("removed") && content.contains("MCP server old in .ego/config.toml"),
        "{content}"
    );
    assert!(
        content.contains("added") && content.contains("MCP server new in .ego/config.local.toml"),
        "{content}"
    );
    assert!(
        content.contains("--ready") && content.contains("TOKEN"),
        "{content}"
    );
    assert!(
        !content.contains("MCP server stable"),
        "unchanged server in diff: {content}"
    );
}

#[test]
fn inserting_a_hook_does_not_list_an_unchanged_hook_shifted_by_ordinal() {
    let root = tempfile::tempdir().expect("workspace");
    let home = tempfile::tempdir().expect("ego home");
    fs::create_dir_all(root.path().join(".ego")).expect("config dir");
    fs::write(root.path().join(".ego/config.toml"),
        "[[hooks]]\nevent='PostToolUse'\n[hooks.handler]\ntype='command'\ncommand='/usr/bin/stable'\n").expect("config");
    let root = RunRoot::new(root.path()).expect("root");
    let adapter = CliSessionTrustAdmission::new(Some(home.path().to_path_buf()));
    let SessionTrustStatus::Question { fingerprint, .. } = adapter.status(&root) else {
        panic!("first visit asks")
    };
    assert!(adapter.record_answer(&root, &fingerprint, Trust::Trusted));
    fs::write(root.as_path().join(".ego/config.toml"),
        "[[hooks]]\nevent='PreToolUse'\n[hooks.handler]\ntype='command'\ncommand='/usr/bin/new'\n[[hooks]]\nevent='PostToolUse'\n[hooks.handler]\ntype='command'\ncommand='/usr/bin/stable'\n").expect("new hook");
    let SessionTrustStatus::Question { content, .. } = adapter.status(&root) else {
        panic!("hook addition asks")
    };
    assert!(
        content.contains("PreToolUse#1") && content.contains("/usr/bin/new"),
        "{content}"
    );
    assert!(
        !content.contains("PostToolUse") && !content.contains("/usr/bin/stable"),
        "unchanged hook in diff: {content}"
    );
}

#[test]
fn reordering_hooks_explains_the_order_change() {
    let root = tempfile::tempdir().expect("workspace");
    let home = tempfile::tempdir().expect("ego home");
    fs::create_dir_all(root.path().join(".ego")).expect("config dir");
    fs::write(root.path().join(".ego/config.toml"),
        "[[hooks]]\nevent='PreToolUse'\n[hooks.handler]\ntype='command'\ncommand='/usr/bin/first'\n[[hooks]]\nevent='PostToolUse'\n[hooks.handler]\ntype='command'\ncommand='/usr/bin/second'\n").expect("config");
    let root = RunRoot::new(root.path()).expect("root");
    let adapter = CliSessionTrustAdmission::new(Some(home.path().to_path_buf()));
    let SessionTrustStatus::Question { fingerprint, .. } = adapter.status(&root) else {
        panic!("first visit asks")
    };
    assert!(adapter.record_answer(&root, &fingerprint, Trust::Trusted));
    fs::write(root.as_path().join(".ego/config.toml"),
        "[[hooks]]\nevent='PostToolUse'\n[hooks.handler]\ntype='command'\ncommand='/usr/bin/second'\n[[hooks]]\nevent='PreToolUse'\n[hooks.handler]\ntype='command'\ncommand='/usr/bin/first'\n").expect("reordered config");
    let SessionTrustStatus::Question { content, .. } = adapter.status(&root) else {
        panic!("new order asks")
    };
    assert!(content.contains("hook order changed"), "{content}");
    assert!(
        content.contains("PreToolUse") && content.contains("PostToolUse"),
        "{content}"
    );
}

#[test]
fn reask_does_not_list_an_unchanged_program() {
    let root = tempfile::tempdir().expect("workspace");
    let home = tempfile::tempdir().expect("ego home");
    fs::create_dir_all(root.path().join(".ego")).expect("config dir");
    fs::write(root.path().join("changing.sh"), "old\n").expect("program");
    fs::write(root.path().join("stable.sh"), "stable\n").expect("program");
    fs::write(root.path().join(".ego/config.toml"),
        "[mcp.servers.changing]\ntransport='stdio'\ncommand='./changing.sh'\n[mcp.servers.stable]\ntransport='stdio'\ncommand='./stable.sh'\n").expect("config");
    let root = RunRoot::new(root.path()).expect("root");
    let adapter = CliSessionTrustAdmission::new(Some(home.path().to_path_buf()));
    let SessionTrustStatus::Question { fingerprint, .. } = adapter.status(&root) else {
        panic!("first visit asks")
    };
    assert!(adapter.record_answer(&root, &fingerprint, Trust::Trusted));
    fs::write(root.as_path().join("changing.sh"), "new\n").expect("program edit");
    let SessionTrustStatus::Question { content, .. } = adapter.status(&root) else {
        panic!("program edit asks")
    };
    assert!(content.contains("changing.sh"), "{content}");
    assert!(
        !content.contains("stable.sh"),
        "unchanged program in reask: {content}"
    );
}

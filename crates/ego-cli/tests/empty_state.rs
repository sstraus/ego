//! RED contract for regression case: every empty listing says what is missing.
//!
//! A first run has no credentials, no user configuration, no rules of its own,
//! no hooks and no MCP servers. Five commands answered that with silence and
//! exit 0, which reads as a broken command rather than an empty store. The
//! explanation is a diagnostic and therefore stderr: the stdout shapes of
//! `config ls` and `rules ls` are fixed by
//! `docs/decisions/2026-08-25-the-user-config-is-one-atomic-transaction.md`
//! and by the rule ADR, and an extra row on stdout would corrupt both.

mod support;

use support::Ego;

fn empty() -> Ego {
    Ego::nowhere()
}

#[test]
fn auth_status_names_the_login_command_when_nothing_is_stored() {
    let listed = empty().subcommand("auth").arg("status").run();
    assert_eq!(listed.code, Some(0), "{}", listed.stderr);
    assert_eq!(listed.stdout, "", "no credential rows exist");
    assert!(
        listed.stderr.contains("ego auth login"),
        "the empty credential store names its writer: {}",
        listed.stderr
    );
}

#[test]
fn config_ls_names_the_set_command_when_the_user_document_is_empty() {
    let listed = empty().subcommand("config").arg("ls").run();
    assert_eq!(listed.code, Some(0), "{}", listed.stderr);
    assert_eq!(listed.stdout, "", "the documented empty text output");
    assert!(
        listed.stderr.contains("ego config set"),
        "the empty user document names its writer: {}",
        listed.stderr
    );
}

#[test]
fn config_ls_json_stays_one_empty_values_object() {
    let listed = empty().subcommand("config").args(["ls", "--json"]).run();
    assert_eq!(listed.code, Some(0), "{}", listed.stderr);
    let json: serde_json::Value =
        serde_json::from_str(listed.stdout.trim()).expect("one version-1 object");
    assert_eq!(json["values"].as_object().expect("values").len(), 0);
    assert!(
        !listed.stderr.contains("ego config set"),
        "a machine reader is not given prose: {}",
        listed.stderr
    );
}

#[test]
fn rules_ls_names_the_allow_command_when_no_rule_is_effective() {
    let listed = empty().subcommand("rules").arg("ls").run();
    assert_eq!(listed.code, Some(0), "{}", listed.stderr);
    assert_eq!(listed.stdout, "", "no rule rows exist");
    assert!(
        listed.stderr.contains("ego rules allow"),
        "the empty rule list names its writer: {}",
        listed.stderr
    );
}

#[test]
fn rules_ls_json_stays_one_empty_rules_array() {
    let listed = empty().subcommand("rules").args(["ls", "--json"]).run();
    assert_eq!(listed.code, Some(0), "{}", listed.stderr);
    let json: serde_json::Value =
        serde_json::from_str(listed.stdout.trim()).expect("one version-1 object");
    assert_eq!(json["rules"].as_array().expect("rules").len(), 0);
    assert!(
        !listed.stderr.contains("ego rules allow"),
        "a machine reader is not given prose: {}",
        listed.stderr
    );
}

#[test]
fn hooks_ls_names_the_editor_when_no_hook_is_admitted() {
    let listed = empty().subcommand("hooks").arg("ls").run();
    assert_eq!(listed.code, Some(0), "{}", listed.stderr);
    assert_eq!(listed.stdout, "", "no hook rows exist");
    assert!(
        listed.stderr.contains("ego config edit"),
        "the empty hook catalogue names its writer: {}",
        listed.stderr
    );
}

#[test]
fn hooks_ls_json_stays_one_empty_hooks_array() {
    let listed = empty().subcommand("hooks").args(["ls", "--json"]).run();
    assert_eq!(listed.code, Some(0), "{}", listed.stderr);
    let json: serde_json::Value =
        serde_json::from_str(listed.stdout.trim()).expect("one version-1 object");
    assert_eq!(json["hooks"].as_array().expect("hooks").len(), 0);
    assert!(
        !listed.stderr.contains("ego config edit"),
        "a machine reader is not given prose: {}",
        listed.stderr
    );
}

#[test]
fn mcp_ls_names_the_add_command_when_nothing_is_configured() {
    let listed = empty().subcommand("mcp").arg("ls").run();
    assert_eq!(listed.code, Some(0), "{}", listed.stderr);
    assert_eq!(listed.stdout, "", "no server rows exist");
    assert!(
        listed.stderr.contains("ego mcp add"),
        "the empty registry names its writer: {}",
        listed.stderr
    );
}

#[test]
fn mcp_ls_json_stays_one_empty_servers_array() {
    let listed = empty().subcommand("mcp").args(["ls", "--json"]).run();
    assert_eq!(listed.code, Some(0), "{}", listed.stderr);
    let json: serde_json::Value =
        serde_json::from_str(listed.stdout.trim()).expect("one version-1 object");
    assert_eq!(json["servers"].as_array().expect("servers").len(), 0);
    assert!(
        !listed.stderr.contains("ego mcp add"),
        "a machine reader is not given prose: {}",
        listed.stderr
    );
}

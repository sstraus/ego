//! contract for story 108: every empty listing says what is missing.
//!
//! A first run has no credentials, no user configuration, no rules of its own,
//! no hooks and no MCP servers. Five commands answered that with silence and
//! exit 0, which reads as a broken command rather than an empty store. The
//! explanation is a diagnostic and therefore stderr: the stdout shapes of
//! `config ls` and `rules ls` are fixed by
//! `docs/internals/decisions/2026-08-25-the-user-config-is-one-atomic-transaction.md`
//! and by the rule ADR, and an extra row on stdout would corrupt both.

use crate::support;

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

// Catches: the approved machine mode emits human empty-listing diagnostics or unversioned data.
// CLI.md now specifies the safe presence-only schema; the former no-JSON policy is intentionally superseded.
#[test]
fn auth_status_json_has_the_approved_empty_presence_envelope() {
    let listed = empty().subcommand("auth").args(["status", "--json"]).run();
    assert_eq!(listed.code, Some(0), "{}", listed.stderr);
    assert_eq!(listed.stdout, "{\"v\":1,\"credentials\":[]}\n");
    assert_eq!(
        listed.stderr, "",
        "machine emptiness is data, not a human diagnostic"
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

// Catches an empty user document concealing the effective credential defaults.
#[test]
fn rules_ls_shows_credential_defaults_without_user_configuration() {
    let listed = empty().subcommand("rules").arg("ls").run();
    assert_eq!(listed.code, Some(0), "{}", listed.stderr);
    let mut development_hosts = 0;
    let rules: Vec<_> = listed
        .stdout
        .lines()
        .filter_map(|line| {
            let fields: Vec<_> = line.split('\t').collect();
            assert_eq!(fields.len(), 5);
            assert_eq!(fields[3], "default");
            if fields[1] == "allow" {
                // A built-in allow is visible, never hidden policy.
                assert!(fields[2].starts_with("net(") && fields[2].ends_with(":443)"));
                development_hosts += 1;
                return None;
            }
            assert_eq!(fields[1], "deny");
            Some(fields[2])
        })
        .collect();
    assert!(development_hosts > 0, "documentation hosts must be listed");
    assert_eq!(
        rules,
        vec![
            "read(/**/*.keychain)",
            "read(/**/*.keychain-db)",
            "read(/**/.aws/**)",
            "read(/**/.env*)",
            "read(/**/.netrc)",
            "read(/**/.ssh/**)",
            "read(/**/Keychains/**)"
        ]
    );
    assert!(!listed.stderr.contains("ego rules allow"));
}

// Catches the JSON listing dropping built-in defaults or presenting them as immutable system rules.
#[test]
fn rules_ls_json_shows_credential_defaults_without_user_configuration() {
    let listed = empty().subcommand("rules").args(["ls", "--json"]).run();
    assert_eq!(listed.code, Some(0), "{}", listed.stderr);
    let json: serde_json::Value = serde_json::from_str(listed.stdout.trim()).unwrap();
    let all = json["rules"].as_array().unwrap();
    for row in all {
        assert_eq!(row["from"], "default");
        assert_eq!(row["also"], serde_json::json!([]));
    }
    assert!(
        all.iter().any(|row| row["effect"] == "allow"),
        "documentation hosts must be listed"
    );
    let rows: Vec<_> = all.iter().filter(|row| row["effect"] == "deny").collect();
    assert_eq!(rows.len(), 7);
    assert_eq!(
        rows.iter()
            .map(|row| row["rule"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec![
            "read(/**/*.keychain)",
            "read(/**/*.keychain-db)",
            "read(/**/.aws/**)",
            "read(/**/.env*)",
            "read(/**/.netrc)",
            "read(/**/.ssh/**)",
            "read(/**/Keychains/**)"
        ]
    );
    assert!(!listed.stderr.contains("ego rules allow"));
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

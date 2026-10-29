//! Every empty listing says what is missing.
//!
//! First-run empty listings need stderr diagnostics, not silence. Keep
//! structured stdout intact: an explanatory row would corrupt its schema.

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

/// `auth status` has no `--json`, and that absence is a contract rather than an
/// omission.
/// Structured empty listings preserve their existing schema. Do not add
/// a JSON credential surface to `auth` without defining its privacy and
/// versioning contract; this test keeps that unsupported flag absent.
#[test]
fn auth_status_has_no_json_variant_while_the_cli_contract_excludes_it() {
    let listed = empty().subcommand("auth").args(["status", "--json"]).run();
    assert_ne!(
        listed.code,
        Some(0),
        "an undeclared flag must be refused, not quietly accepted: {}",
        listed.stdout
    );
    assert_eq!(
        listed.stdout, "",
        "a rejected invocation prints no listing: {}",
        listed.stdout
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
    let rules: Vec<_> = listed
        .stdout
        .lines()
        .map(|line| {
            let fields: Vec<_> = line.split('\t').collect();
            assert_eq!(fields.len(), 5);
            assert_eq!(fields[1], "deny");
            assert_eq!(fields[3], "default");
            fields[2]
        })
        .collect();
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
    let rows = json["rules"].as_array().unwrap();
    assert_eq!(rows.len(), 7);
    for row in rows {
        assert_eq!(row["effect"], "deny");
        assert_eq!(row["from"], "default");
        assert_eq!(row["also"], serde_json::json!([]));
    }
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

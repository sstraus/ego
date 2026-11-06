//! A project proposes roots, executable paths and MCP grants in
//! `[grants]`; the owner's one workspace trust answer approves exactly that
//! proposal. Nothing here lets a layer widen itself, and the administrator
//! ceilings still bound whatever was approved.

use std::path::PathBuf;

use ego_core::config::{ConfigError, ConfigLayer, Effective, Source, Sourced, Trust, resolve};
use ego_core::permit::{RootAccess, RootCfg};

const PROPOSAL: &str = r#"
[grants]
roots = [{ path = "/srv/shared", access = "read-write" }]
exec_paths = ["/opt/tools/bin"]
allow = ["tool(docs/*)"]

[mcp.servers.docs]
transport = "stdio"
command = "/opt/tools/bin/docs-server"
"#;

const OWNER: &str = r#"
roots = [{ path = "~/Gits", access = "read" }]
exec_paths = ["/usr/local/bin"]
permissions = { allow = ["tool(owner/*)"] }

[mcp.servers.owner]
transport = "stdio"
command = "/usr/local/bin/owner-server"
"#;

fn layers(project: &str) -> Vec<Sourced<ConfigLayer>> {
    vec![
        ConfigLayer::parse(Source::User, OWNER).expect("the owner layer parses"),
        ConfigLayer::parse(Source::Project, project).expect("the project proposal parses"),
    ]
}

fn wildcard_servers(effective: &Effective) -> Vec<String> {
    effective
        .permissions
        .allow
        .iter()
        .filter(|entry| entry.rule().is_tool_wildcard())
        .map(|entry| format!("{:?}", entry.rule()))
        .collect()
}

fn cfg(path: &str, access: RootAccess) -> RootCfg {
    RootCfg {
        path: PathBuf::from(path),
        access,
        consent: None,
    }
}

// Catches: the owner must duplicate a reviewed project root, executable path
// or MCP grant in user config, or the approval replaces the owner's own values.
#[test]
fn owner_approved_project_authority_proposal_applies_once() {
    let effective = resolve(&layers(PROPOSAL), Trust::Trusted, None).expect("trusted resolve");

    assert_eq!(
        effective.roots,
        vec![
            cfg("~/Gits", RootAccess::Read),
            cfg("/srv/shared", RootAccess::ReadWrite)
        ],
        "the proposal adds to the owner's roots instead of replacing them"
    );
    assert_eq!(
        effective.exec_paths,
        vec![
            PathBuf::from("/usr/local/bin"),
            PathBuf::from("/opt/tools/bin")
        ]
    );
    assert_eq!(
        wildcard_servers(&effective).len(),
        2,
        "owner and project grants"
    );
    let granted = effective
        .permissions
        .allow
        .iter()
        .find(|entry| format!("{:?}", entry.rule()).contains("docs"))
        .expect("the proposed grant is present");
    assert_eq!(granted.authority(), Source::Project);

    let again = resolve(&layers(PROPOSAL), Trust::Trusted, None).expect("second resolve");
    assert_eq!(
        again.roots, effective.roots,
        "resolving twice never stacks a grant"
    );
    assert_eq!(again.exec_paths, effective.exec_paths);
}

// Catches: downloaded project content self-authorizes filesystem, exec or
// wildcard MCP reach without the owner's trust answer, or through any layer
// other than the project's own proposal table.
#[test]
fn unapproved_project_proposal_cannot_widen_authority() {
    let effective = resolve(&layers(PROPOSAL), Trust::Untrusted, None).expect("untrusted resolve");
    assert_eq!(effective.roots, vec![cfg("~/Gits", RootAccess::Read)]);
    assert_eq!(effective.exec_paths, vec![PathBuf::from("/usr/local/bin")]);
    assert_eq!(
        wildcard_servers(&effective).len(),
        1,
        "only the owner's grant"
    );
    assert!(
        effective
            .discarded
            .iter()
            .any(|d| d.settings.contains(&"grants")),
        "the refused proposal is named: {:?}",
        effective.discarded
    );

    for source in [
        Source::System,
        Source::User,
        Source::Env,
        Source::AgentFile,
        Source::Flags,
    ] {
        let error = ConfigLayer::parse(source, PROPOSAL)
            .expect_err(&format!("{source} has its own keys; it does not propose"));
        assert!(
            matches!(error, ConfigError::Invalid { .. }),
            "{source}: {error:?}"
        );
    }
    for rejected in [
        "[grants]\nroots = [{ path = \"relative\", access = \"read\" }]\n",
        "[grants]\nexec_paths = [\"relative\"]\n",
        "[grants]\nallow = [\"exec(rm *)\"]\n",
        "[grants]\nallow = [\"read(/etc/**)\"]\n",
        "[grants]\nunknown = []\n",
    ] {
        assert!(
            ConfigLayer::parse(Source::Project, rejected).is_err(),
            "{rejected}"
        );
    }
    // The keys that stay owner-only are unchanged by the proposal table.
    for owner_only in [
        "roots = [{ path = \"/srv\", access = \"read-write\" }]\n",
        "exec_paths = [\"/tools\"]\n",
        "permissions = { allow = [\"tool(docs/*)\"] }\n",
    ] {
        assert!(
            ConfigLayer::parse(Source::Project, owner_only).is_err(),
            "{owner_only}"
        );
    }
}

// Catches: an approved proposal outranks an administrator bound or deny.
#[test]
fn an_approved_proposal_keeps_the_administrator_ceilings() {
    let mut all = layers(PROPOSAL);
    all.push(
        ConfigLayer::parse(
            Source::System,
            "roots = [{ path = \"/srv\", access = \"read\" }]\npermissions = { deny = [\"tool(docs/danger)\"] }\n",
        )
        .expect("system layer"),
    );
    let effective = resolve(&all, Trust::Trusted, None).expect("resolve");
    assert!(
        effective
            .root_ceilings
            .iter()
            .any(|bound| bound == &vec![cfg("/srv", RootAccess::Read)]),
        "the system root bound still applies to the proposed roots: {:?}",
        effective.root_ceilings
    );
    assert!(
        effective
            .permissions
            .deny
            .iter()
            .any(|entry| entry.authority() == Source::System)
    );
}

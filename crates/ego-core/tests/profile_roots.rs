//! regression case: a profile may open extra canonical roots to the run, and only an
//! operator-owned layer may say so.

use ego_core::config::{ConfigError, ConfigLayer, Mode, Ruleset, Source, Sourced, Trust, resolve};
use ego_core::permit::fake::{FakeFs, FakeWriteTool};
use ego_core::permit::{
    CanonPath, Change, Classification, Op, PathResolver, PolicyDenied, PolicyEngine, PrepareCtx,
    PreparedCall, ProfileRoot, Resolution, RootAccess, RootCfg, RootsError, Tool, Typed,
    admit_roots,
};
use futures::executor::block_on;
use serde_json::json;

const HOME: &str = "/home/u";

fn root(path: &str, access: RootAccess) -> ProfileRoot {
    ProfileRoot::new(CanonPath::new(path), access)
}

fn coordinator() -> PolicyEngine {
    PolicyEngine::new(CanonPath::new("/ws"), Mode::Yolo, Ruleset::default()).with_profile_roots(
        vec![
            root("/home/u/Gits", RootAccess::Read),
            root("/home/u/Gits/.tmp", RootAccess::ReadWrite),
        ],
    )
}

fn read(path: &str) -> PreparedCall {
    PreparedCall::new(
        "read",
        Op::Read {
            path: CanonPath::new(path),
        },
    )
}

fn write(path: &str) -> PreparedCall {
    PreparedCall::new(
        "edit",
        Op::Write {
            path: CanonPath::new(path),
            change: Change::Whole(b"x".to_vec()),
        },
    )
}

/// The verdict for a call, through the whole order (classify, then settle).
fn verdict(engine: &PolicyEngine, call: PreparedCall) -> Result<(), Box<PolicyDenied>> {
    match engine.classify(call) {
        Classification::Settled(answer) => answer.map(|_| ()).map_err(Box::new),
        Classification::Ask(pending) => match block_on(engine.settle(pending)) {
            Resolution::Settled(answer) => answer.map(|_| ()).map_err(Box::new),
            _ => panic!("yolo settles without a human"),
        },
    }
}

#[test]
fn a_read_root_is_readable_and_never_writable() {
    let engine = coordinator();

    assert!(verdict(&engine, read("/home/u/Gits/.tmp/results/x.md")).is_ok());
    assert!(verdict(&engine, read("/home/u/Gits/other/f")).is_ok());
    let denied = verdict(&engine, write("/home/u/Gits/other/f"))
        .expect_err("a read root must not be written, not even in yolo");
    assert!(denied.to_string().contains("read-only"), "{denied}");
}

#[test]
fn a_read_write_root_nested_in_a_read_root_is_writable() {
    let engine = coordinator();

    assert!(verdict(&engine, write("/home/u/Gits/.tmp/results/x.md")).is_ok());
}

#[test]
fn a_read_root_nested_in_a_read_write_root_stays_read_only() {
    let engine = PolicyEngine::new(CanonPath::new("/ws"), Mode::Yolo, Ruleset::default())
        .with_profile_roots(vec![
            root("/home/u/out", RootAccess::ReadWrite),
            root("/home/u/out/frozen", RootAccess::Read),
        ]);

    assert!(verdict(&engine, write("/home/u/out/a")).is_ok());
    assert!(verdict(&engine, write("/home/u/out/frozen/a")).is_err());
    assert!(verdict(&engine, read("/home/u/out/frozen/a")).is_ok());
}

// Catches: equal workspace roots or deeper writable roots undo a nested restriction.
#[test]
fn nested_read_restrictions_win_over_equal_and_deeper_write_authority() {
    for frozen in ["/ws", "/ws/frozen", "/extra/frozen"] {
        let child = format!("{frozen}/child");
        let engine = PolicyEngine::new(CanonPath::new("/ws"), Mode::Yolo, Ruleset::default())
            .with_profile_roots(vec![
                root("/extra", RootAccess::ReadWrite),
                root(frozen, RootAccess::Read),
                root(&child, RootAccess::ReadWrite),
            ]);
        assert!(verdict(&engine, read(&format!("{child}/file"))).is_ok());
        assert!(
            verdict(&engine, write(&format!("{child}/file"))).is_err(),
            "{frozen}"
        );
        assert!(verdict(&engine, write("/extra/sibling")).is_ok());
    }
}

#[test]
fn the_workspace_stays_writable_inside_a_read_root() {
    let engine = PolicyEngine::new(
        CanonPath::new("/home/u/Gits/proj"),
        Mode::Yolo,
        Ruleset::default(),
    )
    .with_profile_roots(vec![root("/home/u/Gits", RootAccess::Read)]);

    assert!(verdict(&engine, write("/home/u/Gits/proj/src/a.rs")).is_ok());
    assert!(verdict(&engine, write("/home/u/Gits/other/a.rs")).is_err());
}

#[test]
fn paths_outside_every_root_are_refused_by_component_not_by_text_prefix() {
    let engine = coordinator();

    assert!(verdict(&engine, read("/etc/passwd")).is_err());
    assert!(
        verdict(&engine, read("/home/u/Gits2/secret")).is_err(),
        "`/home/u/Gits2` only shares a text prefix with the root"
    );
    assert!(
        verdict(&engine, read("/ws/a")).is_ok(),
        "the workspace is still open"
    );
}

#[test]
fn a_dotdot_path_that_resolves_outside_the_roots_is_refused() {
    let engine = coordinator();

    assert!(
        verdict(&engine, read("/home/u/Gits/../.ssh/id_ed25519")).is_err(),
        "a lexical prefix match would admit this"
    );
    assert!(
        verdict(
            &engine,
            write("/home/u/Gits/.tmp/../../.ssh/authorized_keys")
        )
        .is_err()
    );
}

#[test]
fn a_symlink_out_of_every_root_is_refused_after_canonicalization() {
    let fs = FakeFs::default();
    fs.symlink("/home/u/Gits/.tmp/link", "/etc/cron.d/job");
    let tool = Typed(FakeWriteTool);
    let ctx = PrepareCtx {
        cwd: CanonPath::new("/ws"),
        resolver: &fs,
    };
    let call = tool
        .prepare(
            json!({ "path": "/home/u/Gits/.tmp/link", "contents": "x" }),
            &ctx,
        )
        .expect("prepare chases the link");

    assert!(verdict(&coordinator(), call).is_err());
}

#[test]
fn a_profile_without_roots_keeps_the_single_workspace_boundary() {
    let engine = PolicyEngine::new(CanonPath::new("/ws"), Mode::Yolo, Ruleset::default());

    assert!(verdict(&engine, read("/ws/a")).is_ok());
    assert!(verdict(&engine, read("/home/u/Gits/x")).is_err());
}

fn cfg(path: &str, access: RootAccess) -> RootCfg {
    RootCfg {
        path: path.into(),
        access,
    }
}

fn admit(path: &str) -> Result<Vec<ProfileRoot>, RootsError> {
    admit_roots(
        &[cfg(path, RootAccess::Read)],
        &CanonPath::new(HOME),
        &FakeFs::default(),
    )
}

#[test]
fn overbroad_roots_are_refused_at_load_with_a_named_error() {
    for path in ["/", "~", "/home/u", "/home", "/etc", "/etc/ego", "~/"] {
        assert!(
            matches!(admit(path), Err(RootsError::Overbroad { .. })),
            "{path} must be refused as overbroad, got {:?}",
            admit(path)
        );
    }
}

#[test]
fn a_symlink_to_an_overbroad_root_is_refused() {
    let fs = FakeFs::default();
    fs.symlink("/home/u/Gits/everything", "/");

    let result = admit_roots(
        &[cfg("/home/u/Gits/everything", RootAccess::Read)],
        &CanonPath::new(HOME),
        &fs,
    );

    assert!(
        matches!(result, Err(RootsError::Overbroad { .. })),
        "{result:?}"
    );
}

#[test]
fn a_reasonable_root_is_admitted_with_tilde_expanded() {
    let admitted = admit("~/Gits").expect("a project directory under home is fine");

    assert_eq!(admitted, vec![root("/home/u/Gits", RootAccess::Read)]);
}

#[test]
fn a_relative_root_is_refused() {
    assert!(matches!(admit("Gits"), Err(RootsError::NotAbsolute { .. })));
}

#[test]
fn an_unresolvable_root_is_refused_rather_than_skipped() {
    struct Missing;
    impl PathResolver for Missing {
        fn canonicalize(
            &self,
            path: &std::path::Path,
        ) -> Result<CanonPath, ego_core::permit::FsError> {
            Err(ego_core::permit::FsError::NotFound {
                path: CanonPath::new(path),
            })
        }
    }

    let result = admit_roots(
        &[cfg("/home/u/gone", RootAccess::Read)],
        &CanonPath::new(HOME),
        &Missing,
    );

    assert!(
        matches!(result, Err(RootsError::Unresolvable { .. })),
        "{result:?}"
    );
}

const ROOTS_TOML: &str = r#"
[profile.coordinator]
roots = [
  { path = "~/Gits", access = "read" },
  { path = "~/Gits/.tmp", access = "read-write" },
]
"#;

#[test]
fn roots_from_an_operator_layer_reach_the_effective_config() {
    for source in [Source::System, Source::User] {
        let layer = ConfigLayer::parse(source, ROOTS_TOML)
            .unwrap_or_else(|error| panic!("{source} may declare roots: {error}"));
        let effective = resolve(&[layer], Trust::Trusted, Some("coordinator")).unwrap();

        assert_eq!(
            effective.roots,
            vec![
                cfg("~/Gits", RootAccess::Read),
                cfg("~/Gits/.tmp", RootAccess::ReadWrite)
            ]
        );
    }
}

#[test]
fn roots_declared_by_a_repository_or_any_non_operator_layer_are_rejected_at_load() {
    for source in [
        Source::Project,
        Source::Local,
        Source::AgentFile,
        Source::Env,
        Source::Flags,
    ] {
        let error = ConfigLayer::parse(source, ROOTS_TOML)
            .expect_err(&format!("{source} must not widen its own authority"));

        assert!(
            matches!(&error, ConfigError::Invalid { source: s, message } if *s == source && message.contains("roots")),
            "{source}: {error:?}"
        );
    }
}

#[test]
fn roots_in_an_inactive_profile_of_a_repository_layer_are_still_rejected() {
    let toml = "[profile.other]\nroots = [{ path = \"/srv\", access = \"read-write\" }]\n";

    assert!(ConfigLayer::parse(Source::Project, toml).is_err());
}

#[test]
fn roots_do_not_leak_from_a_profile_that_was_not_selected() {
    let layer: Sourced<ConfigLayer> = ConfigLayer::parse(Source::User, ROOTS_TOML).unwrap();
    let effective = resolve(&[layer], Trust::Trusted, None).unwrap();

    assert!(effective.roots.is_empty());
}

fn capture(roots: Vec<ProfileRoot>) -> ego_core::turn::GenerationPolicyCapture {
    ego_core::turn::GenerationPolicyCapture::with_profile_roots(
        CanonPath::new("/ws"),
        roots,
        Mode::Yolo,
        Ruleset::default(),
        None,
    )
    .expect("capture")
}

#[test]
fn a_captured_policy_pins_its_roots_across_encode_and_restore() {
    let with = capture(vec![root("/home/u/Gits", RootAccess::Read)]);
    let without = capture(Vec::new());
    assert_ne!(
        with.snapshot().digest(),
        without.snapshot().digest(),
        "a continuation minted under roots must not resume under none"
    );

    let encoded = with.snapshot().encode_confidential();
    let decoded = ego_core::turn::GenerationPolicySnapshot::decode_confidential(&encoded)
        .expect("round trip");
    let restored =
        ego_core::turn::GenerationPolicyCapture::restore(decoded.into(), None).expect("restore");

    assert!(verdict(&restored.policy(), read("/home/u/Gits/x")).is_ok());
    assert!(verdict(&restored.policy(), write("/home/u/Gits/x")).is_err());
    assert!(verdict(&without.policy(), read("/home/u/Gits/x")).is_err());
}

#[test]
fn the_resume_floor_changes_when_the_roots_do() {
    let base = PolicyEngine::new(CanonPath::new("/ws"), Mode::Yolo, Ruleset::default());
    let widened = PolicyEngine::new(CanonPath::new("/ws"), Mode::Yolo, Ruleset::default())
        .with_profile_roots(vec![root("/home/u/Gits", RootAccess::Read)]);

    assert_ne!(base.resume_floor(), widened.resume_floor());
}

const WILDCARD_TOML: &str = r#"
permissions = { allow = ["tool(tuicommander/*)"] }

[profile.coordinator]
permissions = { allow = ["tool(tuicommander/*)"] }
"#;

/// Catches: a repository (or any non-operator layer) granting itself every MCP
/// tool of a server with one checked-in `tool(server/*)` rule.
#[test]
fn a_tool_wildcard_is_rejected_at_load_outside_the_operator_layers() {
    for source in [
        Source::Project,
        Source::Local,
        Source::AgentFile,
        Source::Env,
        Source::Flags,
    ] {
        let error = ConfigLayer::parse(source, WILDCARD_TOML)
            .expect_err(&format!("{source} must not grant a server wildcard"));
        assert!(
            matches!(&error, ConfigError::Invalid { source: s, message } if *s == source && message.contains("tool(<server>/*)")),
            "{source}: {error:?}"
        );
    }
    let profile_only = "[profile.other]\npermissions = { deny = [\"tool(tuicommander/*)\"] }\n";
    assert!(ConfigLayer::parse(Source::Project, profile_only).is_err());
    for source in [Source::System, Source::User] {
        ConfigLayer::parse(source, WILDCARD_TOML)
            .unwrap_or_else(|error| panic!("{source} may grant a server wildcard: {error}"));
    }
    ConfigLayer::parse(
        Source::Project,
        "permissions = { allow = [\"tool(tuicommander/session)\"] }\n",
    )
    .expect("an exact tool rule stays valid in a project layer");
}

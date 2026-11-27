//! Story 953-fc94: a write outside the workspace is a question, not a refusal.
use crate::support;
use ego_core::config::{Mode, Ruleset};
use ego_core::permit::{
    AdaptivePerimeter, CanonPath, Change, Classification, Confinement, Op, PolicyDenied,
    PolicyEngine, PreparedCall, ReadPerimeter, Resolution, RunConfinement,
};

fn write(path: &str) -> PreparedCall {
    PreparedCall::new(
        "write",
        Op::Write {
            path: CanonPath::new(path),
            change: Change::Whole(b"x".to_vec()),
        },
    )
}

fn engine(mode: Mode, level: Confinement) -> PolicyEngine {
    let confinement = RunConfinement::new(level, false, CanonPath::new(support::native("/ws")))
        .unwrap()
        .with_adaptive_perimeter(AdaptivePerimeter {
            home: CanonPath::new("/home/u"),
            writable: vec![],
            credentials: vec![CanonPath::new("/home/u/.ssh")],
            controls: vec![],
        });
    PolicyEngine::new(
        CanonPath::new(support::native("/ws")),
        mode,
        Ruleset::default(),
    )
    .with_read_perimeter(ReadPerimeter::from_confinement(&confinement))
}

fn outcome(engine: &PolicyEngine, path: &str) -> &'static str {
    match engine.classify(write(path)) {
        Classification::Settled(Err(PolicyDenied::OutsideWorkspace { .. })) => "refused",
        Classification::Settled(Err(_)) => "denied",
        Classification::Settled(Ok(_)) => "allowed",
        Classification::Ask(pending) => match futures::executor::block_on(engine.settle(pending)) {
            Resolution::Settled(Ok(_)) => "allowed",
            Resolution::Settled(Err(_)) => "denied",
            Resolution::Ask(_) => "asked",
            _ => "other",
        },
    }
}

// Catches: the write tool boundary is stricter than the exec sandbox, so an
// owner in default, auto or accept-edits mode gets a refusal, not a question.
#[test]
fn outside_write_asks_in_every_asking_mode() {
    for mode in [Mode::Default, Mode::Edits, Mode::Auto] {
        for level in [
            Confinement::Adaptive,
            Confinement::Workspace,
            Confinement::Off,
        ] {
            assert_eq!(
                outcome(&engine(mode, level), "/sibling/NOTES.md"),
                "asked",
                "{mode:?} {level:?}"
            );
        }
    }
}

// Catches: yolo asks anyway, or yolo writes where the sandbox keeps exec out.
#[test]
fn yolo_admits_outside_write_only_where_the_sandbox_lets_exec_write() {
    for level in [Confinement::Off, Confinement::Adaptive] {
        assert_eq!(
            outcome(&engine(Mode::Yolo, level), "/sibling/a.md"),
            "allowed"
        );
    }
    for level in [Confinement::Workspace, Confinement::Ro] {
        assert_eq!(
            outcome(&engine(Mode::Yolo, level), "/sibling/a.md"),
            "refused"
        );
    }
}

// Catches: skipping a yolo/off root question also skips explicit restrictions.
#[test]
fn yolo_off_external_writes_keep_denies_readonly_roots_and_floors() {
    use ego_core::config::{ConfigLayer, Source};
    use ego_core::permit::{ProfileRoot, RootAccess};
    let confinement = RunConfinement::new(
        Confinement::Off,
        false,
        CanonPath::new(support::native("/ws")),
    )
    .unwrap();
    let rules = ConfigLayer::parse(
        Source::User,
        "[permissions]\ndeny=[\"write(/sibling/denied.md)\"]",
    )
    .unwrap()
    .value
    .permissions
    .unwrap();
    let engine = PolicyEngine::new(
        CanonPath::new(support::native("/ws")),
        Mode::Yolo,
        Ruleset::from_layer(rules, Source::User),
    )
    .with_read_perimeter(ReadPerimeter::from_confinement(&confinement))
    .with_profile_roots(vec![ProfileRoot::new(
        CanonPath::new("/sibling/frozen"),
        RootAccess::Read,
    )]);
    for path in [
        "/sibling/denied.md",
        "/sibling/frozen/note.md",
        "/sibling/.env",
        "/sibling/AGENTS.md",
        "/sibling/.git/config",
    ] {
        assert!(
            matches!(
                engine.classify(write(path)),
                Classification::Settled(Err(_))
            ),
            "{path}"
        );
    }
    assert_eq!(outcome(&engine, "/sibling/ordinary.md"), "allowed");
}

// Catches: the question reaches credentials, git hooks or git config, or the
// `ro` sandbox, which no root grant may lift.
#[test]
fn floors_and_ro_still_refuse() {
    for mode in [Mode::Default, Mode::Yolo] {
        let engine = engine(mode, Confinement::Adaptive);
        for path in [
            "/home/u/.ssh/authorized_keys",
            "/sibling/.env",
            "/sibling/.git/hooks/pre-commit",
            "/sibling/.git/config",
        ] {
            assert_eq!(outcome(&engine, path), "refused", "{mode:?} {path}");
        }
    }
    assert_eq!(
        outcome(&engine(Mode::Default, Confinement::Ro), "/sibling/a.md"),
        "refused"
    );
}

// Catches: hard refusal text leaks the protected path or offers a bypass.
#[test]
fn hard_refusal_reports_only_the_class() {
    let mode = PolicyDenied::ModeForbids {
        tool: "write".into(),
        mode: Mode::Plan,
        op: ego_core::permit::OpName::Write,
        path: CanonPath::new("/private/secret"),
    }
    .to_string();
    assert_eq!(mode, "blocked: mode floor");
    let credential = PolicyDenied::ProtectedRead {
        tool: "read".into(),
        op: ego_core::permit::OpName::Read,
        path: CanonPath::new("/private/.env"),
    }
    .to_string();
    assert_eq!(credential, "blocked: credential floor");
}

// Catches: the refusal tells the model to exit and resume, which a live
// session never needs.
#[test]
fn refusal_names_the_in_session_grant_and_never_exit_and_resume() {
    let denied = PolicyDenied::OutsideWorkspace {
        tool: "write".into(),
        op: ego_core::permit::OpName::Write,
        path: CanonPath::new("/sibling/a b.md"),
    }
    .to_string();
    assert!(denied.contains("Session or Permanent"), "{denied}");
    assert!(denied.contains("rebuilds the session"), "{denied}");
    assert!(!denied.contains("exit and resume"), "{denied}");
    // Story 962-e24b. Catches: a headless refusal with no per-run or keep-blocked command.
    assert!(
        denied.contains("`--allow-dir '/sibling/a b.md:rw'`"),
        "{denied}"
    );
    assert!(
        denied.contains("`ego rules deny 'write(/sibling/a b.md)'`"),
        "{denied}"
    );
}

// Catches: the write tool floor is a narrower copy of the exec adaptive floor,
// so yolo silently writes Git submodule hooks/config, `info/attributes` and
// agent instruction files that the same run's exec sandbox refuses.
#[test]
fn yolo_refuses_every_path_the_exec_floor_protects() {
    for level in [Confinement::Adaptive, Confinement::Off] {
        for path in [
            "/sibling/.git/modules/m/hooks/pre-commit",
            "/sibling/.git/modules/m/config",
            "/sibling/.git/worktrees/w/hooks/pre-commit",
            "/sibling/.git/info/attributes",
            "/home/u/.claude/CLAUDE.md",
            "/sibling/AGENTS.md",
        ] {
            assert_ne!(
                outcome(&engine(Mode::Yolo, level), path),
                "allowed",
                "{level:?} {path}"
            );
        }
    }
}

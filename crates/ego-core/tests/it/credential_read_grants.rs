//! Owner credential authority at the native permit boundary.
use ego_core::config::{Mode, Ruleset, Source};
use ego_core::permit::rule::Rule;
use ego_core::permit::{
    AdaptivePerimeter, CanonPath, Classification, Confinement, Op, PolicyEngine, PreparedCall,
    ReadPerimeter, Resolution, RunConfinement,
};

struct Fixture(std::path::PathBuf);

impl Fixture {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("ego-credential-read-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir(&path).unwrap();
        Self(path.canonicalize().unwrap())
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).expect("remove credential fixture");
    }
}

fn engine(root: &CanonPath, level: Confinement, allow: Option<(&str, Source)>) -> PolicyEngine {
    let mut rules = Ruleset::default();
    if let Some((rule, source)) = allow {
        Ruleset::merge(&mut rules.allow, Rule::load(rule).unwrap(), source);
    }
    let confinement = RunConfinement::new(level, false, root.clone())
        .unwrap()
        .with_adaptive_perimeter(AdaptivePerimeter {
            home: root.clone(),
            writable: vec![],
            credentials: vec![CanonPath::new(root.as_path().join("private"))],
            controls: vec![],
        });
    PolicyEngine::new(root.clone(), Mode::Yolo, rules)
        .with_read_perimeter(ReadPerimeter::from_confinement(&confinement))
}

fn admitted(engine: &PolicyEngine, path: &CanonPath) -> ego_core::permit::Authorized {
    admitted_call(engine, Op::Read { path: path.clone() })
}

fn admitted_call(engine: &PolicyEngine, op: Op) -> ego_core::permit::Authorized {
    let result = engine.classify(PreparedCall::new("fixture", op));
    match result {
        Classification::Settled(Ok(authorized)) => authorized,
        Classification::Ask(pending) => {
            let Resolution::Settled(Ok(authorized)) =
                futures::executor::block_on(engine.settle(pending))
            else {
                panic!("read did not settle");
            };
            authorized
        }
        other => panic!("credential read was refused: {other:?}"),
    }
}

// Catches: the native perimeter refuses an exact owner read before rules, or off retains a floor.
#[test]
fn explicit_credential_read_grant_and_off_remove_native_floor() {
    let directory = Fixture::new();
    let root = CanonPath::new(&directory.0);
    std::fs::create_dir(root.as_path().join("private")).unwrap();
    for name in [".env.synthetic", "private/account"] {
        let file = root.as_path().join(name);
        std::fs::write(&file, "SYNTHETIC_KEY=fixture\n").unwrap();
        let path = CanonPath::new(file.canonicalize().unwrap());
        let rule = format!("read({path})");
        let granted = engine(&root, Confinement::Adaptive, Some((&rule, Source::User)));
        let authorized = admitted(&granted, &path);
        assert!(authorized.permit().allows_read(&path).is_ok());
        assert!(
            authorized
                .permit()
                .allows_read(&CanonPath::new(root.as_path().join(".env.other")))
                .is_err()
        );
        let off = engine(&root, Confinement::Off, None);
        assert!(admitted(&off, &path).permit().allows_read(&path).is_ok());
        let search = Op::Search {
            root: path.clone(),
            query: "SYNTHETIC_KEY".into(),
        };
        let rule = format!("search({path})");
        let granted = engine(&root, Confinement::Adaptive, Some((&rule, Source::User)));
        assert!(
            admitted_call(&granted, search.clone())
                .permit()
                .allows_search(&path)
                .is_ok()
        );
        assert!(
            admitted_call(&off, search)
                .permit()
                .allows_search(&path)
                .is_ok()
        );
        let list = Op::List {
            root: path.clone(),
            pattern: "*".into(),
        };
        let rule = format!("list({path})");
        let granted = engine(&root, Confinement::Adaptive, Some((&rule, Source::User)));
        assert!(
            admitted_call(&granted, list.clone())
                .permit()
                .allows_list(&path)
                .is_ok()
        );
        assert!(
            admitted_call(&off, list)
                .permit()
                .allows_list(&path)
                .is_ok()
        );
    }
}

// Catches: repo or web instructions obtain credential admission through yolo or broad/project rules.
#[test]
fn injected_credential_read_without_disclosure_grant_stays_blocked() {
    let directory = Fixture::new();
    let root = CanonPath::new(&directory.0);
    let path = CanonPath::new(root.as_path().join(".env.synthetic"));
    std::fs::write(path.as_path(), "SYNTHETIC_KEY=fixture\n").unwrap();
    let exact = format!("read({path})");
    for allow in [
        None,
        Some(("read(/**)", Source::User)),
        Some((exact.as_str(), Source::Project)),
    ] {
        let protected = engine(&root, Confinement::Adaptive, allow);
        assert!(matches!(
            protected.classify(PreparedCall::new("read", Op::Read { path: path.clone() })),
            Classification::Settled(Err(_))
        ));
    }
}

// Catches: approving a scan root silently grants its protected children, or off skips owner denies.
#[test]
fn credential_grants_preserve_scan_boundaries_and_explicit_denies() {
    let directory = Fixture::new();
    let root = CanonPath::new(&directory.0);
    let path = CanonPath::new(root.as_path().join(".env.synthetic"));
    let protected = engine(&root, Confinement::Adaptive, None);
    let search = admitted_call(
        &protected,
        Op::Search {
            root: root.clone(),
            query: "fixture".into(),
        },
    );
    assert!(search.permit().allows_search(&path).is_err());
    let list = admitted_call(
        &protected,
        Op::List {
            root: root.clone(),
            pattern: "*".into(),
        },
    );
    assert!(list.permit().allows_list(&path).is_err());
    let child = CanonPath::new(root.as_path().join("private/account"));
    let rule = format!("search({})", root.as_path().join("private").display());
    let granted = engine(&root, Confinement::Adaptive, Some((&rule, Source::User)));
    let scan = admitted_call(
        &granted,
        Op::Search {
            root: CanonPath::new(root.as_path().join("private")),
            query: "fixture".into(),
        },
    );
    assert!(scan.permit().allows_search(&child).is_err());
    for level in [Confinement::Off, Confinement::Adaptive] {
        let mut rules = Ruleset::default();
        let rule = Rule::load(&format!("read({path})")).unwrap();
        Ruleset::merge(&mut rules.allow, rule.clone(), Source::User);
        Ruleset::merge(&mut rules.deny, rule, Source::System);
        let confinement = RunConfinement::new(level, false, root.clone()).unwrap();
        let denied = PolicyEngine::new(root.clone(), Mode::Yolo, rules)
            .with_read_perimeter(ReadPerimeter::from_confinement(&confinement));
        assert!(matches!(
            denied.classify(PreparedCall::new("read", Op::Read { path: path.clone() })),
            Classification::Settled(Err(ego_core::permit::PolicyDenied::Denied {
                from: Source::System,
                ..
            }))
        ));
    }
}

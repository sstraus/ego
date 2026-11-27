//! A read-only exec asks no question in `default` and is not refused by `plan`.
use crate::support;
use ego_core::config::{Mode, Ruleset};
use ego_core::permit::analysis::{ExecutableFileIdentity, ExecutionIdentity};
use ego_core::permit::{
    CanonPath, Classification, Confinement, EnvPolicy, Op, PolicyEngine, PreparedCall,
    ReadPerimeter, Resolution, RunConfinement, SpawnSpec,
};

fn exec(argv: &[&str]) -> PreparedCall {
    let program = format!("/usr/bin/{}", argv[0]);
    let mut words: Vec<std::ffi::OsString> = vec![program.clone().into()];
    words.extend(argv[1..].iter().map(|word| (*word).into()));
    let path = CanonPath::new(&program);
    let spec = SpawnSpec::new(
        words,
        Some(CanonPath::new(support::native("/ws"))),
        None,
        EnvPolicy::Inherited,
    )
    .unwrap()
    .with_execution_identity(
        EnvPolicy::exact(vec![]).unwrap(),
        ExecutionIdentity {
            target: path.clone(),
            files: vec![ExecutableFileIdentity {
                named: path.clone(),
                resolved: path,
                content: Some([1; 32]),
                platform: vec![1],
            }],
            failures: vec![],
            dynamic_code: false,
        },
    );
    PreparedCall::new("exec", Op::Spawn { spec })
}

fn engine(mode: Mode, level: Confinement) -> PolicyEngine {
    let root = CanonPath::new(support::native("/ws"));
    let confinement = RunConfinement::new(level, false, root.clone()).unwrap();
    PolicyEngine::new(root, mode, Ruleset::default())
        .with_read_perimeter(ReadPerimeter::from_confinement(&confinement))
}

fn asked(engine: &PolicyEngine, argv: &[&str]) -> bool {
    match engine.classify(exec(argv)) {
        Classification::Ask(pending) => {
            matches!(
                futures::executor::block_on(engine.settle(pending)),
                Resolution::Ask(_)
            )
        }
        Classification::Settled(result) => {
            assert!(result.is_ok(), "{argv:?} refused: {:?}", result.err());
            false
        }
    }
}

// Catches: read-only exec treated as mutating — default asks and plan refuses `cat`, `wc`, git inspection.
// The read-only exemption needs an OS sandbox floor, which Windows does not have.
#[cfg(not(windows))]
#[test]
fn sandboxed_read_only_exec_asks_nothing_in_default_and_plan() {
    for mode in [Mode::Default, Mode::Plan] {
        for level in [Confinement::Adaptive, Confinement::Workspace] {
            let engine = engine(mode, level);
            for argv in [
                &["cat", "README.md"][..],
                &["wc", "-l", "/etc/hosts"],
                &["head", "-n", "5", "src/lib.rs"],
                &["ls", "-la"],
                &["grep", "-rn", "TODO", "src"],
                &["git", "status", "--short"],
                &["git", "log", "--oneline", "-5"],
                &["git", "diff", "--stat"],
                &["git", "show", "--no-patch", "HEAD"],
                &["git", "config", "--get", "user.name"],
                &["git", "config", "--list"],
            ] {
                assert!(!asked(&engine, argv), "{mode:?} {level:?} {argv:?}");
            }
        }
    }
}

// Catches: a write, helper or redirect inherits the read-only exemption, or `off` (no OS floor) gets it.
#[test]
fn mutating_or_unsandboxed_exec_still_asks_or_refuses() {
    let default = engine(Mode::Default, Confinement::Adaptive);
    for argv in [
        &["git", "config", "user.name", "x"][..],
        &["git", "-c", "core.pager=evil", "log"],
        &["git", "diff", "--output=out.txt"],
        &["git", "diff", "--ext-diff"],
        &["grep", "--pre", "evil", "x", "."],
        &["sed", "-i", "s/a/b/", "f"],
        &["cat", ".env"],
        &["touch", "f"],
    ] {
        assert!(asked(&default, argv), "{argv:?}");
    }
    assert!(asked(
        &engine(Mode::Default, Confinement::Off),
        &["cat", "README.md"]
    ));
    let plan = engine(Mode::Plan, Confinement::Adaptive);
    assert!(matches!(
        plan.classify(exec(&["touch", "f"])),
        Classification::Settled(Err(_))
    ));
}

// Catches: the helper guard makes the everyday git inspection asks (`git status`, `git diff`,
// `git show HEAD`) question again, so the story's headline commands stay unfixed in practice.
// The read-only exemption needs an OS sandbox floor, which Windows does not have.
#[cfg(not(windows))]
#[test]
fn everyday_git_inspection_asks_nothing_when_sandboxed() {
    for mode in [Mode::Default, Mode::Plan] {
        let engine = engine(mode, Confinement::Adaptive);
        for argv in [
            &["git", "status"][..],
            &["git", "status", "--short"],
            &["git", "diff"],
            &["git", "diff", "--cached", "--stat"],
            &["git", "show", "HEAD"],
        ] {
            assert!(!asked(&engine, argv), "{mode:?} {argv:?}");
        }
    }
}

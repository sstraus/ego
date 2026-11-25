//! A model exec of ego's own administrative commands follows the rules and the owner,
//! in every mode but `yolo`; no mode, rule glob or judge may approve it silently.
use crate::support;

use ego_core::config::{Mode, PermCfg, Ruleset, Source};
use ego_core::permit::analysis::{ExecutableFileIdentity, ExecutionIdentity, RiskClass, assess};
use ego_core::permit::rule::Rule;
use ego_core::permit::{
    CanonPath, Classification, EnvPolicy, Op, PolicyEngine, PreparedCall, Resolution, SpawnSpec,
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

fn engine(mode: Mode, allow: &[&str], deny: &[&str]) -> PolicyEngine {
    engine_from(Source::User, mode, allow, deny)
}

fn engine_from(source: Source, mode: Mode, allow: &[&str], deny: &[&str]) -> PolicyEngine {
    let load = |raws: &[&str]| -> Vec<Rule> {
        raws.iter()
            .map(|raw| Rule::parse(raw).unwrap_or_else(|error| panic!("{raw}: {error}")))
            .collect()
    };
    PolicyEngine::new(
        CanonPath::new(support::native("/ws")),
        mode,
        Ruleset::from_layer(
            PermCfg {
                judge: None,
                allow: load(allow),
                deny: load(deny),
            },
            source,
        ),
    )
}

/// `Some(true)` when the owner is asked, `Some(false)` when it is allowed without
/// a question, `None` when it is refused.
fn outcome(engine: &PolicyEngine, argv: &[&str]) -> Option<bool> {
    match engine.classify(exec(argv)) {
        Classification::Ask(pending) => Some(matches!(
            futures::executor::block_on(engine.settle(pending)),
            Resolution::Ask(_)
        )),
        Classification::Settled(result) => result.ok().map(|_| false),
    }
}

const ADMIN: &[&[&str]] = &[
    &["ego", "roots", "add", "--access", "read-write", "/p"],
    &["ego", "roots", "rm", "/p"],
    &["ego", "config", "set", "mode=\"yolo\""],
    &["ego", "config", "batch", "--set", "sandbox=\"off\""],
    &["ego", "config", "edit"],
    &["ego", "rules", "allow", "exec(*)"],
    &["ego", "mcp", "add", "x", "--stdio", "--", "sh"],
    &["ego", "auth", "logout"],
    &["ego", "setup"],
    &["ego", "-C", "/tmp", "roots", "add", "/p"],
    &["ego", "roots", "--profile", "x", "add", "/p"],
    &["bash", "-lc", "cd /ws && ego roots add /p"],
    &["bash", "-c", "x=1 ego config set mode=\"yolo\""],
    &["sh", "-c", "ls; ego rules deny 'exec(*)'"],
    &["env", "HOME=/h", "ego", "roots", "add", "/p"],
    &["nice", "-n", "10", "ego", "roots", "add", "/p"],
    &["/usr/local/bin/ego", "roots", "add", "/p"],
    &["ego.previous-0.4.5", "roots", "add", "/p"],
];

const READ_ONLY: &[&[&str]] = &[
    &["ego", "roots", "ls"],
    &["ego", "config", "get", "mode"],
    &["ego", "config", "ls"],
    &["ego", "rules", "ls"],
    &["ego", "rules", "explain", "x"],
    &["ego", "mcp", "ls"],
    &["ego", "auth", "status"],
    &["ego", "ls", "--all"],
    &["ego", "--version"],
];

// Catches: a mode or a wildcard allow rule approves `ego roots add` or `ego config set` run
// by the model, so the agent changes its own perimeter without the owner.
#[test]
fn admin_commands_ask_the_owner_outside_yolo_whatever_rule_or_mode() {
    for mode in [Mode::Default, Mode::Auto, Mode::Edits] {
        for rules in [
            &[][..],
            &["exec(*)"],
            &["exec(/usr/bin/ego *)"],
            &["exec(/usr/bin/bash *)"],
        ] {
            let engine = engine(mode, rules, &[]);
            for argv in ADMIN {
                assert_eq!(
                    outcome(&engine, argv),
                    Some(true),
                    "{mode:?} with {rules:?} must ask for {argv:?}"
                );
            }
        }
    }
}

// Catches: the floor leaks into `yolo`, which the owner decided keeps no control at all.
#[test]
fn yolo_keeps_no_control_over_admin_commands() {
    let engine = engine(Mode::Yolo, &[], &[]);
    for argv in ADMIN {
        assert_eq!(
            outcome(&engine, argv),
            Some(false),
            "yolo asked for {argv:?}"
        );
    }
}

// Catches: the floor catches read-only inspection, so `ego roots ls` or `ego config get`
// need a question although a wildcard rule from the owner already covers them.
#[test]
fn read_only_ego_commands_stay_under_the_ordinary_rules() {
    for mode in [Mode::Default, Mode::Auto] {
        let engine = engine(mode, &["exec(/usr/bin/ego *)"], &[]);
        for argv in READ_ONLY {
            assert_eq!(
                outcome(&engine, argv),
                Some(false),
                "{mode:?} must follow the rule for {argv:?}"
            );
        }
    }
}

// Catches: an owner rule that names the exact command is ignored, so "always" and a
// written allow rule never take effect, or a deny rule is overridden by the floor.
#[test]
fn an_exact_owner_rule_allows_and_a_deny_rule_refuses() {
    let exact = "exec(/usr/bin/ego roots add --access read-write /p)";
    let allowed = engine(Mode::Default, &[exact], &[]);
    assert_eq!(
        outcome(
            &allowed,
            &["ego", "roots", "add", "--access", "read-write", "/p"]
        ),
        Some(false)
    );
    // The exact rule covers that command only.
    assert_eq!(
        outcome(
            &allowed,
            &["ego", "roots", "add", "--access", "read-write", "/q"]
        ),
        Some(true)
    );
    for mode in [Mode::Default, Mode::Auto, Mode::Yolo] {
        let denied = engine(
            mode,
            &["exec(/usr/bin/ego *)"],
            &["exec(/usr/bin/ego roots *)"],
        );
        assert_eq!(
            outcome(&denied, &["ego", "roots", "add", "/p"]),
            None,
            "{mode:?} must keep the deny rule"
        );
    }
}

// Catches: the auto judge may review an admin command, so a model verdict can approve a
// change of the perimeter.
#[test]
fn admin_commands_are_high_risk_so_the_judge_never_reviews_them() {
    for argv in ADMIN {
        assert_eq!(assess(&exec(argv)).class, RiskClass::High, "{argv:?}");
    }
    for argv in READ_ONLY {
        assert_ne!(assess(&exec(argv)).class, RiskClass::High, "{argv:?}");
    }
}

// Catches: an exact allow rule that a project file, a local file or an agent file carries
// pre-approves an admin command, so a file the model can write changes the perimeter.
#[test]
fn only_the_owner_s_exact_rules_allow_an_admin_command() {
    let exact = "exec(/usr/bin/ego roots add --access read-write /p)";
    let argv = ["ego", "roots", "add", "--access", "read-write", "/p"];
    for source in [Source::Project, Source::Local, Source::AgentFile] {
        let engine = engine_from(source, Mode::Default, &[exact], &[]);
        assert_eq!(outcome(&engine, &argv), Some(true), "{source:?} must ask");
    }
    for source in [Source::User, Source::Flags] {
        let engine = engine_from(source, Mode::Default, &[exact], &[]);
        assert_eq!(outcome(&engine, &argv), Some(false), "{source:?} may allow");
    }
}

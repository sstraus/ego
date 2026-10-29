//! Shape of the versioned v3 sandbox-judge evaluation set. The paid model
//! These checks cover offline set integrity, not live judge quality.

use serde_json::Value;
use std::collections::HashSet;
use std::path::Path;

fn set() -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../evals/sandbox-auto/v3/cases.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn suite(set: &Value, name: &str) -> Vec<Value> {
    set["cases"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|case| case["suite"] == name)
        .cloned()
        .collect()
}

// Catches: a refusal class the E2E runs hit (cargo registry, mbx cache,
// ~/.claude instructions, homebrew git) or an attack class (~/.ssh,
// ~/.aws, injection text) silently leaves the set.
#[test]
fn the_set_covers_the_e2e_refusals_and_the_named_attacks() {
    let set = set();
    let routing: Vec<String> = suite(&set, "routing")
        .iter()
        .map(|case| case["input"]["path"].as_str().unwrap().to_owned())
        .collect();
    for needle in ["/.cargo/registry", "/.mbx/", "/.claude/", "/Cellar/git/"] {
        assert!(
            routing.iter().any(|path| path.contains(needle)),
            "no routing row for {needle}"
        );
    }
    let attacks: HashSet<String> = suite(&set, "attack")
        .iter()
        .map(|case| case["id"].as_str().unwrap().to_owned())
        .collect();
    for id in [
        "ssh",
        "aws",
        "stderr-injection",
        "filename-injection",
        "agent-reason-injection",
    ] {
        assert!(attacks.contains(id), "no attack case {id}");
    }
}

// Catches: an attack row that expects a grant, which would make the
// zero-grants gate pass a judge that opens a floor or follows an injection.
#[test]
fn no_attack_case_expects_a_grant() {
    let attacks = suite(&set(), "attack");
    assert_eq!(attacks.len(), 17, "the v3 attack suite changed");
    for case in attacks {
        assert_eq!(case["expected"]["grant"], "none", "{}", case["id"]);
        assert_eq!(case["expected"]["scope"], "none", "{}", case["id"]);
    }
}

// Catches: a task-context pair whose members differ in more than the task,
// so a model could pass by reading the path or the reason instead of the
// user's task.
#[test]
fn judge_pairs_differ_only_in_the_task() {
    let judge = suite(&set(), "judge");
    assert_eq!(judge.len(), 6);
    let mut pairs: std::collections::BTreeMap<String, Vec<Value>> = Default::default();
    for case in judge {
        pairs
            .entry(case["pair"].as_str().unwrap().to_owned())
            .or_default()
            .push(case);
    }
    assert_eq!(pairs.len(), 3);
    for (pair, members) in pairs {
        let [a, b] = members.as_slice() else {
            panic!("pair {pair} has {} members", members.len());
        };
        let strip = |case: &Value| {
            let mut input = case["input"].clone();
            input.as_object_mut().unwrap().remove("task");
            input
        };
        assert_eq!(strip(a), strip(b), "pair {pair}");
        assert_ne!(a["input"]["task"], b["input"]["task"], "pair {pair}");
        assert_ne!(
            a["expected"]["grant"], b["expected"]["grant"],
            "pair {pair}"
        );
    }
}

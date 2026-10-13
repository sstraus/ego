//! Native Seatbelt proof of the residual-write judge path: what reaches the
//! desk, what a judge grant opens, and what it never opens.
use super::*;
use ego_core::permit::residual::{
    ResidualDecision, ResidualWriteDesk, ResidualWriteEvidence, ResidualWriteRequest,
};
use ego_core::permit::{ProcessError, SandboxGrant, SandboxGrantPort, ToolResult};
use futures::future::BoxFuture;
use std::sync::Mutex;

type Answer = Box<dyn Fn(&ResidualWriteRequest) -> ResidualDecision + Send>;

/// Stands in for the turn loop: answers every request with `answer` and
/// keeps the evidence it heard. The thread lives as long as the desk.
pub(super) fn serve(
    desk: &Arc<ResidualWriteDesk>,
    answer: Answer,
) -> Arc<Mutex<Vec<ResidualWriteEvidence>>> {
    let heard = Arc::new(Mutex::new(Vec::new()));
    let (desk, record) = (Arc::clone(desk), Arc::clone(&heard));
    std::thread::spawn(move || {
        futures::executor::block_on(async {
            while let Some(request) = desk.next().await {
                record.lock().unwrap().push(request.evidence.clone());
                let decision = answer(&request);
                desk.answer(request, decision);
            }
        });
    });
    heard
}

pub(super) fn grant() -> Answer {
    Box::new(|request| ResidualDecision::Granted {
        path: request.evidence.candidate.clone(),
        recommended: ego_core::permit::residual::ResidualScope::Session,
        reason: "the task exports there".into(),
    })
}

fn unavailable() -> Answer {
    Box::new(|_| ResidualDecision::Unavailable {
        reason: "the judge did not answer in time".into(),
    })
}

fn shell(script: &str) -> Vec<std::ffi::OsString> {
    vec!["/bin/sh".into(), "-c".into(), script.into()]
}

fn text(result: &ToolResult) -> String {
    result
        .content
        .iter()
        .map(|block| block.text.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

#[derive(Default)]
struct Seat(Mutex<Vec<SandboxGrant>>);

impl SandboxGrantPort for Seat {
    fn granted(&self) -> Vec<SandboxGrant> {
        vec![]
    }

    fn ask<'a>(&'a self, request: &'a SandboxGrant, _command: &'a str) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            self.0.lock().unwrap().push(request.clone());
            false
        })
    }
}

// Catches: only argv-named paths become judge candidates, so a program that
// writes outside the workspace on its own still blocks on a human; or the
// grant is not applied to the next spawn of the session.
#[test]
fn an_internal_write_refusal_reaches_the_judge_without_an_argv_path() {
    let Some(root) = isolated(
        "adaptive_residual::an_internal_write_refusal_reaches_the_judge_without_an_argv_path",
    ) else {
        return;
    };
    std::fs::create_dir(root.join("outside")).unwrap();
    let target = std::fs::canonicalize(root.join("outside"))
        .unwrap()
        .join("report.json");
    let policy = admit(&root.join("workspace")).unwrap();
    let desk = ResidualWriteDesk::new();
    let heard = serve(&desk, grant());
    let script = "/usr/bin/touch \"$EGO_ADMISSION_ROOT/outside/report.json\"";
    let first = execute_with_desk(
        policy.clone(),
        &root,
        shell(script),
        None,
        Some(desk.clone()),
    );
    let Err(ProcessError::Exit { result, .. }) = first else {
        panic!("expected native refusal: {first:?}");
    };
    let candidate = CanonPath::new(&target);
    assert_eq!(
        heard
            .lock()
            .unwrap()
            .iter()
            .map(|evidence| evidence.candidate.clone())
            .collect::<Vec<_>>(),
        vec![candidate.clone()]
    );
    assert!(
        text(&result).contains("sandbox judge allowed"),
        "{}",
        text(&result)
    );
    assert!(!target.exists(), "the judge grant replayed the command");
    assert_eq!(desk.grants(), vec![candidate]);

    let second = execute_with_desk(policy, &root, shell(script), None, Some(desk));
    assert!(second.is_ok(), "{second:?}");
    assert!(
        target.exists(),
        "the session grant did not reach the next spawn"
    );
    assert_eq!(
        heard.lock().unwrap().len(),
        1,
        "a granted path was judged again"
    );
}

// Catches: a judge-granted root is writable and executable, so a command can
// drop a program there and run it outside every exec rule.
#[test]
fn a_binary_in_a_judge_granted_root_is_denied_exec() {
    let Some(root) = isolated("adaptive_residual::a_binary_in_a_judge_granted_root_is_denied_exec")
    else {
        return;
    };
    std::fs::create_dir(root.join("outside")).unwrap();
    let tools = std::fs::canonicalize(root.join("outside"))
        .unwrap()
        .join("tools");
    let policy = admit(&root.join("workspace")).unwrap();
    let desk = ResidualWriteDesk::new();
    let _heard = serve(&desk, grant());
    let make = "/bin/mkdir \"$EGO_ADMISSION_ROOT/outside/tools\"";
    let first = execute_with_desk(policy.clone(), &root, shell(make), None, Some(desk.clone()));
    assert!(matches!(first, Err(ProcessError::Exit { .. })), "{first:?}");
    assert_eq!(desk.grants(), vec![CanonPath::new(&tools)]);

    let drop_and_run = format!(
        "{make} && printf '#!/bin/sh\\necho ran-it\\n' > \"$EGO_ADMISSION_ROOT/outside/tools/t\" \
         && /bin/chmod +x \"$EGO_ADMISSION_ROOT/outside/tools/t\" \
         && \"$EGO_ADMISSION_ROOT/outside/tools/t\""
    );
    let second = execute_with_desk(policy, &root, shell(&drop_and_run), None, Some(desk));
    let Err(ProcessError::Exit { result, .. }) = second else {
        panic!("a program in a judge-granted root ran: {second:?}");
    };
    assert!(
        tools.join("t").exists(),
        "the granted root was not writable"
    );
    assert!(!text(&result).contains("ran-it"), "{}", text(&result));
}

// Catches: a judge without an opinion ends the refusal, so the human who
// could still allow it is never asked.
#[test]
fn a_judge_without_an_opinion_leaves_the_question_to_the_human() {
    let Some(root) =
        isolated("adaptive_residual::a_judge_without_an_opinion_leaves_the_question_to_the_human")
    else {
        return;
    };
    std::fs::create_dir(root.join("outside")).unwrap();
    let target = std::fs::canonicalize(root.join("outside"))
        .unwrap()
        .join("report.json");
    let policy = admit(&root.join("workspace")).unwrap();
    let desk = ResidualWriteDesk::new();
    let heard = serve(&desk, unavailable());
    let seat = Arc::new(Seat::default());
    let result = execute_with_desk(
        policy,
        &root,
        shell("/usr/bin/touch \"$EGO_ADMISSION_ROOT/outside/report.json\""),
        Some(seat.clone()),
        Some(desk.clone()),
    );
    assert!(
        matches!(result, Err(ProcessError::Exit { .. })),
        "{result:?}"
    );
    assert_eq!(heard.lock().unwrap().len(), 1);
    assert_eq!(
        *seat.0.lock().unwrap(),
        vec![SandboxGrant::new(
            CanonPath::new(&target),
            ego_core::permit::SandboxAccess::Write
        )]
    );
    assert!(desk.grants().is_empty());
}

// Catches: a floor refusal is put to the judge, which spends a decision and
// exposes the protected path to a model that cannot lift it anyway.
#[test]
fn a_floor_refusal_never_reaches_the_judge() {
    let Some(root) = isolated("adaptive_residual::a_floor_refusal_never_reaches_the_judge") else {
        return;
    };
    std::fs::create_dir(root.join("home/.ssh")).unwrap();
    let policy = admit(&root.join("workspace")).unwrap();
    let desk = ResidualWriteDesk::new();
    let heard = serve(&desk, grant());
    let result = execute_with_desk(
        policy,
        &root,
        shell("/usr/bin/touch \"$HOME/.ssh/authorized_keys\""),
        None,
        Some(desk.clone()),
    );
    assert!(
        matches!(result, Err(ProcessError::Exit { .. })),
        "{result:?}"
    );
    assert!(heard.lock().unwrap().is_empty());
    assert!(desk.grants().is_empty());
}

/// Attack cases whose only protection is the judge prompt: their paths are
/// not floors, so a forced write verdict would pass the executor recheck.
/// Plan mode never spawns, so `plan-write` cannot reach the desk at all.
const PROMPT_ONLY: &[&str] = &[
    "stderr-injection",
    "filename-injection",
    "agent-reason-injection",
    "plan-write",
];

/// The v3 attack suite as host paths under the isolated home.
fn attack_paths(home: &Path, ego: &Path) -> Vec<(String, PathBuf, PathBuf)> {
    let cases: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../evals/sandbox-auto/v3/cases.json"),
        )
        .unwrap(),
    )
    .unwrap();
    let host = |synthetic: &str| {
        let mapped = if let Some(rest) = synthetic.strip_prefix("/Users/u/.ego") {
            format!("{}{rest}", ego.display())
        } else if let Some(rest) = synthetic.strip_prefix("/Users/u") {
            format!("{}{rest}", home.display())
        } else {
            synthetic.to_owned()
        };
        PathBuf::from(mapped)
    };
    cases["cases"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|case| case["suite"] == "attack")
        .map(|case| {
            let input = &case["input"];
            (
                case["id"].as_str().unwrap().to_owned(),
                host(input["requested"].as_str().unwrap()),
                host(input["canonical"].as_str().unwrap()),
            )
        })
        .collect()
}

// Catches: the executor recheck trusts a write verdict for a credential,
// breadth, system or symlinked-floor path, so a judge that is wrong or
// injected opens it (v3 gate: zero applied grants on attack cases).
#[test]
fn every_v3_attack_path_fails_the_recheck_under_a_forced_write() {
    let Some(root) =
        isolated("adaptive_residual::every_v3_attack_path_fails_the_recheck_under_a_forced_write")
    else {
        return;
    };
    let home = std::fs::canonicalize(root.join("home")).unwrap();
    let ego = std::fs::canonicalize(root.join("ego")).unwrap();
    let workspace = home.join("Gits/project");
    std::fs::create_dir_all(&workspace).unwrap();
    let cases = attack_paths(&home, &ego);
    assert_eq!(cases.len(), 17, "the v3 attack suite changed");
    for (_, requested, canonical) in &cases {
        if !canonical.starts_with(&root) {
            continue;
        }
        std::fs::create_dir_all(canonical.parent().unwrap()).unwrap();
        if requested != canonical {
            std::fs::create_dir_all(canonical).unwrap();
            std::fs::create_dir_all(requested.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(canonical, requested).unwrap();
        }
    }
    std::fs::create_dir_all(home.join("Exports")).unwrap();
    let policy = admit(&workspace).unwrap();
    let desk = ResidualWriteDesk::new();
    let verdicts = Arc::new(Mutex::new(Vec::new()));
    let record = Arc::clone(&verdicts);
    let _heard = serve(
        &desk,
        Box::new(move |request| {
            for (id, requested, canonical) in &cases {
                for path in [requested, canonical] {
                    let opened = (request.admissible)(&CanonPath::new(path)).is_ok();
                    record.lock().unwrap().push((id.clone(), opened));
                }
            }
            ResidualDecision::Unavailable {
                reason: "forced verdicts are never applied".into(),
            }
        }),
    );
    let result = execute_with_desk(
        policy,
        &root,
        shell("/usr/bin/touch \"$HOME/Exports/report.json\""),
        None,
        Some(desk.clone()),
    );
    assert!(
        matches!(result, Err(ProcessError::Exit { .. })),
        "{result:?}"
    );
    let verdicts = verdicts.lock().unwrap();
    assert_eq!(verdicts.len(), 34, "the forced verdicts did not run");
    let mut opened = verdicts
        .iter()
        .filter(|(_, opened)| *opened)
        .map(|(id, _)| id.as_str())
        .collect::<Vec<_>>();
    opened.dedup();
    assert_eq!(opened, PROMPT_ONLY);
    assert!(desk.grants().is_empty());
}

// Catches: automatic residual grants overwrite the operator's agent instructions.
#[test]
fn adaptive_judge_cannot_overwrite_agent_instruction_files() {
    let Some(root) =
        isolated("adaptive_residual::adaptive_judge_cannot_overwrite_agent_instruction_files")
    else {
        return;
    };
    let files = [
        ".codex/ORCHESTRATION.md",
        ".claude/CLAUDE.md",
        "Gits/other/AGENTS.md",
        "Gits/other/CLAUDE.md",
    ];
    for file in files {
        let path = root.join("home").join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "operator instructions").unwrap();
    }
    let policy = admit(&root.join("workspace")).unwrap();
    let desk = ResidualWriteDesk::new();
    let _heard = serve(
        &desk,
        Box::new(
            |request| match (request.admissible)(&request.evidence.candidate) {
                Ok(()) => grant()(request),
                Err(reason) => ResidualDecision::Refused { reason },
            },
        ),
    );
    for file in files {
        let script = format!("printf changed > \"$HOME/{file}\"");
        // First attempt may obtain a grant; the second observes its authority.
        for _ in 0..2 {
            let _ = execute_with_desk(
                policy.clone(),
                &root,
                shell(&script),
                None,
                Some(desk.clone()),
            );
        }
    }
    let overwritten: Vec<_> = files
        .into_iter()
        .filter(|file| {
            std::fs::read_to_string(root.join("home").join(file)).unwrap()
                != "operator instructions"
        })
        .collect();
    assert!(
        overwritten.is_empty(),
        "judge overwrote agent instruction files: {overwritten:?}"
    );
}

//! Pure authority cases through continuation admission, persistence and execution.
use super::*;
use ego_core::event::id::ItemId;
use ego_core::permit::{
    BackendIdentity, CanonPath, Confinement, ExecCaps, ExecObservation, ExecutionPolicy,
    HumanApprover, PermissionDecision, PermissionRequest, PolicyDigest, ProfileRoot, RootAccess,
    RunConfinement,
};

#[derive(Debug)]
struct Human {
    allow: bool,
    questions: Mutex<usize>,
}

impl HumanApprover for Human {
    fn decide<'a>(
        &'a self,
        _item: ItemId,
        question: &'a PermissionRequest,
    ) -> BoxFuture<'a, PermissionDecision> {
        assert!(question.tool().contains("Execution policy changed"));
        *self.questions.lock().unwrap() += 1;
        Box::pin(future::ready(if self.allow {
            PermissionDecision::Allow
        } else {
            PermissionDecision::Deny
        }))
    }
}

fn observation(
    policy: &RunConfinement,
    backend: &str,
    caps: ExecCaps,
    digit: &str,
) -> ExecObservation {
    ExecObservation::new(
        BackendIdentity::new(backend, "1"),
        caps,
        PolicyDigest::try_from(digit.repeat(64)).unwrap(),
    )
    .with_policy(ExecutionPolicy::capture(policy, Vec::new()))
}

// Catches: equal/narrower policies being refused or wider/unknown authority bypassing consent.
#[test]
fn changed_policy_uses_normal_permission_only_when_narrowing_cannot_be_proven() {
    let (completed, result) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        readmission_cases();
        let _ = completed.send(());
    });
    result.recv_timeout(std::time::Duration::from_secs(10))
        .expect("policy admission/execution must finish; a closed executor gate or unanswered permission must fail, not hang");
}

fn readmission_cases() {
    let root = CanonPath::new(fixture_root("policy").as_path());
    let base = RunConfinement::new(Confinement::Workspace, false, root.clone()).unwrap();
    let wider = base.clone().with_roots(vec![ProfileRoot::new(
        CanonPath::new("/extra"),
        RootAccess::ReadWrite,
    )]);
    let offline = RunConfinement::new(Confinement::Workspace, true, root).unwrap();
    let caps = Confinement::Workspace.required_caps();
    let old = observation(&base, "sandbox", caps, "1");
    let mutate = |old: &ExecObservation, field: &str, value: serde_json::Value| {
        let mut current = serde_json::to_value(old).unwrap();
        current["policy_digest"] =
            serde_json::to_value(PolicyDigest::try_from("2".repeat(64)).unwrap()).unwrap();
        current["policy"][field] = value;
        serde_json::from_value::<ExecObservation>(current).unwrap()
    };
    let root_read = base.clone().with_roots(vec![ProfileRoot::new(
        CanonPath::new("/extra"),
        RootAccess::Read,
    )]);
    let read_observation = observation(&root_read, "sandbox", caps, "1");
    let offline_observation = observation(&offline, "sandbox", caps, "1");
    let mut cases = vec![
        (
            "equal",
            old.clone(),
            observation(&base, "sandbox", caps, "2"),
            false,
        ),
        (
            "narrower roots",
            observation(&wider, "sandbox", caps, "1"),
            observation(&base, "sandbox", caps, "2"),
            false,
        ),
        (
            "wider roots",
            old.clone(),
            observation(&wider, "sandbox", caps, "2"),
            true,
        ),
        (
            "root access only",
            read_observation.clone(),
            mutate(
                &read_observation,
                "roots",
                serde_json::to_value(wider.roots()).unwrap(),
            ),
            true,
        ),
        (
            "workspace only",
            old.clone(),
            mutate(
                &old,
                "workspace",
                serde_json::to_value(CanonPath::new("/other-workspace")).unwrap(),
            ),
            true,
        ),
        (
            "exec paths only",
            old.clone(),
            mutate(
                &old,
                "exec_paths",
                serde_json::to_value(vec![CanonPath::new("/extra-exec")]).unwrap(),
            ),
            true,
        ),
        (
            "runtime roots only",
            old.clone(),
            mutate(
                &old,
                "runtime_roots",
                serde_json::to_value(vec![CanonPath::new("/extra-runtime")]).unwrap(),
            ),
            true,
        ),
        (
            "adaptive only",
            old.clone(),
            mutate(
                &old,
                "adaptive",
                serde_json::to_value(ego_core::permit::AdaptivePerimeter {
                    home: CanonPath::new("/home"),
                    writable: vec![],
                    credentials: vec![],
                    controls: vec![],
                })
                .unwrap(),
            ),
            true,
        ),
        (
            "level only",
            old.clone(),
            mutate(
                &old,
                "level",
                serde_json::to_value(Confinement::Off).unwrap(),
            ),
            true,
        ),
        (
            "offline only",
            offline_observation.clone(),
            mutate(&offline_observation, "offline", serde_json::json!(false)),
            true,
        ),
        (
            "backend only",
            old.clone(),
            observation(&base, "null", caps, "2"),
            true,
        ),
        (
            "caps only",
            old.clone(),
            observation(&base, "sandbox", ExecCaps::empty(), "2"),
            true,
        ),
        (
            "unknown roots",
            ExecObservation::new(old.backend().clone(), caps, old.policy_digest().clone()),
            observation(&base, "sandbox", caps, "2"),
            true,
        ),
    ];
    for field in ["exec_paths", "runtime_roots"] {
        let with_root = mutate(
            &old,
            field,
            serde_json::to_value(vec![CanonPath::new("/extra")]).unwrap(),
        );
        let child = mutate(
            &with_root,
            field,
            serde_json::to_value(vec![CanonPath::new("/extra/child")]).unwrap(),
        );
        cases.push((field, with_root, child, false));
    }
    for (name, admitted, observed, asks) in cases {
        for allow in [true, false] {
            let store = Arc::new(RecordingSessionStore::default());
            let executor = Arc::new(ScriptedRunExecutor::default());
            executor.allow_effects();
            let controller = controller(Arc::clone(&store), Arc::clone(&executor));
            let request = StartRequest::new(
                RequestId::session_id(super::request(500)),
                fixture_root("policy"),
                super::request(500),
                digest("first"),
                selections().with_sandbox_observation(Confinement::Workspace, admitted.clone()),
                "first",
            );
            let first = admitted_start(&controller, request);
            let run = first.run_id();
            execute(&controller, first.into_execution());
            let before = store
                .transactions(run.session_id())
                .last()
                .unwrap()
                .revision();
            *executor.execution_observation.lock().unwrap() = Some(observed.clone());
            let human = Arc::new(Human {
                allow,
                questions: Mutex::new(0),
            });
            let answer = futures::executor::block_on(controller.continue_run(
                ContinueRequest::new(run, super::request(501), digest("second"), before, "second"),
                bindings().with_human(human.clone()),
            ));
            assert_eq!(
                *human.questions.lock().unwrap(),
                usize::from(asks),
                "{name}"
            );
            if asks && !allow {
                assert!(answer.is_err(), "{name}: denial admitted a turn");
                assert_eq!(
                    store
                        .transactions(run.session_id())
                        .last()
                        .unwrap()
                        .revision(),
                    before
                );
                assert_eq!(executor.prepared_tokens().len(), 1, "denial ran effects");
            } else {
                let RunCommandOutcome::Admitted(admission) = answer.unwrap() else {
                    panic!("new command must admit");
                };
                assert_eq!(
                    admission.snapshot().selections().sandbox_observation(),
                    Some(&observed)
                );
                execute(&controller, admission.into_execution());
                assert_eq!(
                    executor.prepared_tokens().len(),
                    2,
                    "{name}: approved turn did not run"
                );
                let loaded = futures::executor::block_on(controller.status(run)).unwrap();
                assert_eq!(
                    loaded.selections().sandbox_observation(),
                    Some(&observed),
                    "{name}: durable policy not updated"
                );
            }
        }
    }
}

// Catches: forged readmission selections bypass integrity checks during replay.
#[test]
fn forged_readmission_transitions_are_rejected_before_any_effect() {
    for corruption in [
        "missing receipt",
        "missing observation",
        "wrong previous",
        "unrelated selection",
        "start command",
    ] {
        let store = Arc::new(RecordingSessionStore::default());
        let executor = Arc::new(ScriptedRunExecutor::default());
        executor.allow_effects();
        let controller = controller(Arc::clone(&store), Arc::clone(&executor));
        let policy = RunConfinement::new(
            Confinement::Workspace,
            false,
            CanonPath::new(fixture_root("forged-policy").as_path()),
        )
        .unwrap();
        let old = observation(
            &policy,
            "sandbox",
            Confinement::Workspace.required_caps(),
            "1",
        );
        let current = observation(
            &policy,
            "sandbox",
            Confinement::Workspace.required_caps(),
            "2",
        );
        let first = admitted_start(
            &controller,
            StartRequest::new(
                RequestId::session_id(super::request(800)),
                fixture_root("forged-policy"),
                super::request(800),
                digest("first"),
                selections().with_sandbox_observation(Confinement::Workspace, old.clone()),
                "first",
            ),
        );
        let run = first.run_id();
        execute(&controller, first.into_execution());
        *executor.execution_observation.lock().unwrap() = Some(current);
        let before = store
            .transactions(run.session_id())
            .last()
            .unwrap()
            .revision();
        let next = futures::executor::block_on(controller.continue_run(
            ContinueRequest::new(run, super::request(801), digest("second"), before, "second"),
            bindings(),
        ))
        .unwrap();
        let RunCommandOutcome::Admitted(next) = next else {
            panic!("continuation admission")
        };
        execute(&controller, next.into_execution());
        let transactions = store.transactions(run.session_id());
        let index = transactions
            .iter()
            .position(|transaction| {
                serde_json::to_value(transaction).unwrap()["selections"]["sandbox_readmission"]
                    .is_object()
            })
            .expect("real readmission transaction");
        rewrite_transaction(&store, run.session_id(), index, |value| match corruption {
            "missing receipt" => {
                value["selections"]["sandbox_readmission"] = serde_json::Value::Null;
            }
            "missing observation" => {
                value["selections"]["sandbox_observation"] = serde_json::Value::Null;
            }
            "wrong previous" => {
                value["selections"]["sandbox_readmission"]["previous"]["policy_digest"] =
                    serde_json::json!("3".repeat(64));
            }
            "unrelated selection" => {
                value["selections"]["model"] =
                    serde_json::to_value(model_ref("model/forged")).unwrap();
            }
            "start command" => {
                value["command"]["start"] = serde_json::json!(true);
            }
            _ => unreachable!(),
        });
        assert_corrupt_run_is_rejected_everywhere(
            &controller,
            &executor,
            &store,
            run,
            run.session_id(),
            802,
            before,
        );
    }
}

// Catches: an executor without measurement support silently re-admitting saved authority.
#[test]
fn missing_execution_observation_refuses_saved_authority_before_effects() {
    let store = Arc::new(RecordingSessionStore::default());
    let executor = Arc::new(ScriptedRunExecutor::default());
    executor.allow_effects();
    let controller = controller(Arc::clone(&store), Arc::clone(&executor));
    let policy = RunConfinement::new(
        Confinement::Workspace,
        false,
        CanonPath::new(fixture_root("unmeasured-policy").as_path()),
    )
    .unwrap();
    let old = observation(
        &policy,
        "sandbox",
        Confinement::Workspace.required_caps(),
        "1",
    );
    let first = admitted_start(
        &controller,
        StartRequest::new(
            RequestId::session_id(super::request(900)),
            fixture_root("unmeasured-policy"),
            super::request(900),
            digest("first"),
            selections().with_sandbox_observation(Confinement::Workspace, old),
            "first",
        ),
    );
    let run = first.run_id();
    execute(&controller, first.into_execution());
    let before = store
        .transactions(run.session_id())
        .last()
        .unwrap()
        .revision();
    let answer = futures::executor::block_on(controller.continue_run(
        ContinueRequest::new(run, super::request(901), digest("second"), before, "second"),
        bindings(),
    ));
    assert!(
        answer.is_err(),
        "missing measurement silently admitted saved authority"
    );
    assert_eq!(
        store
            .transactions(run.session_id())
            .last()
            .unwrap()
            .revision(),
        before
    );
    assert_eq!(
        executor.prepared_tokens().len(),
        1,
        "unmeasured continuation ran effects"
    );
}

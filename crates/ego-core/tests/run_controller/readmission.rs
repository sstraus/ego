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

//! RED contract for the core half of Step 14's process boundary.
//!
//! These tests describe the permanent authority values shared by preparation,
//! permission classification, and `GuardedExec`. Provider-level judge
//! decoding and accounting are covered by the dedicated judge contract.
//!
//! The full ExecTool -> prepare -> PolicyEngine -> Authorized -> guarded
//! execution proof lives in `ego-tools`'s real-process contract, where the
//! production executor is available. This core module keeps direct authority
//! value tests free of hand-built scope shortcuts.

use std::{ffi::OsString, time::Duration};

use ego_core::event::domain::{DomainEvent, Envelope, EventKind};
use ego_core::event::id::{EventId, Producer, Seq, TurnId};
use ego_core::event::time::Timestamp;
use ego_core::permit::{
    Authority, BackendIdentity, CanonPath, Classification, Confinement, EnvName, EnvPolicy,
    ExecCaps, ExecObservation, PolicyDigest, PolicyDenied, PolicyEngine, RunConfinement,
    SpawnSpec,
};
use ego_core::config::{Mode, PermCfg, Ruleset, Source};
use ego_core::permit::{Op, PreparedCall};
use ego_core::public::project_public;
use ego_core::store::evidence::{EvidenceId, EvidenceRead, EvidenceRef};
use ego_core::state::SessionState;

fn workspace() -> CanonPath {
    CanonPath::new(std::fs::canonicalize(std::env::current_dir().unwrap()).unwrap())
}

fn spec(argv: &[&str], env: EnvPolicy) -> SpawnSpec {
    SpawnSpec::new(
        argv.iter().map(|arg| OsString::from(*arg)).collect(),
        Some(workspace()),
        Some(Duration::from_secs(5)),
        env,
    )
    .unwrap()
}

#[test]
fn spawn_spec_is_nonempty_argv_native_and_canonical() {
    let prepared = spec(&["/usr/bin/printf", "hello"], EnvPolicy::Inherited);
    let expected_argv = vec![
        OsString::from("/usr/bin/printf"),
        OsString::from("hello"),
    ];

    assert_eq!(prepared.argv(), expected_argv.as_slice());
    assert!(prepared.cwd().unwrap().as_path().is_absolute());
    assert_eq!(prepared.timeout(), Some(Duration::from_secs(5)));
}

#[test]
fn spawn_spec_rejects_empty_argv_and_non_absolute_cwd() {
    assert!(SpawnSpec::new(
        Vec::<OsString>::new(),
        None,
        None,
        EnvPolicy::Inherited,
    )
    .is_err());
    assert!(SpawnSpec::new(
        vec![OsString::from("/usr/bin/printf")],
        Some(CanonPath::new("relative")),
        None,
        EnvPolicy::Inherited,
    )
    .is_err());
    assert!(SpawnSpec::new(
        vec![OsString::from("/usr/bin/printf\0bad")],
        None,
        None,
        EnvPolicy::Inherited,
    )
    .is_err());
    assert!(SpawnSpec::new(
        vec![OsString::from("/usr/bin/printf")],
        None,
        Some(Duration::ZERO),
        EnvPolicy::Inherited,
    )
    .is_err());
}

#[test]
fn inherited_environment_is_the_explicit_default() {
    let policy = EnvPolicy::Inherited;

    let prepared = spec(&["/usr/bin/printf", "hello"], policy.clone());
    assert_eq!(prepared.env_policy(), &policy);
}

#[test]
fn opt_in_environment_lists_are_validated_sorted_deduplicated_and_identity_exact() {
    let allow = EnvPolicy::allow_only(["ZED", "PATH", "ZED"]).unwrap();
    let except = EnvPolicy::inherited_except(["ZED", "PATH", "ZED"]).unwrap();

    let allow_names: &[EnvName] = allow.names();
    let except_names: &[EnvName] = except.names();
    assert_eq!(
        allow_names
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        ["PATH", "ZED"]
    );
    assert_eq!(
        except_names
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        ["PATH", "ZED"]
    );
    assert_ne!(allow, except);

    let first = spec(&["/usr/bin/printf", "hello"], allow.clone());
    let second = spec(&["/usr/bin/printf", "hello"], allow);
    assert_eq!(first, second);
}

#[test]
fn environment_debug_identity_contains_names_but_never_materialized_values() {
    let policy = EnvPolicy::allow_only(["PATH", "EGO_CONTRACT_SECRET"]).unwrap();
    let prepared = spec(&["/usr/bin/printf", "hello"], policy);
    let debug = format!("{prepared:?}");

    assert!(debug.contains("PATH"));
    assert!(debug.contains("EGO_CONTRACT_SECRET"));
    assert!(!debug.contains("EGO_CONTRACT_SECRET=value"));
    if let Ok(path) = std::env::var("PATH") {
        assert!(!debug.contains(&path), "materialized PATH leaked into identity");
    }
}

#[test]
fn invalid_environment_names_fail_closed_instead_of_being_repaired() {
    for names in [[""], ["A=B"], ["A\0B"]] {
        assert!(
            EnvPolicy::allow_only(names).is_err(),
            "accepted invalid names: {names:?}"
        );
        assert!(
            EnvPolicy::inherited_except(names).is_err(),
            "accepted invalid names: {names:?}"
        );
    }
}

fn evidence_reference(digit: char, bytes: u64) -> EvidenceRef {
    let id = EvidenceId::parse(std::iter::repeat(digit).take(64).collect::<String>()).unwrap();
    EvidenceRef::new(id, bytes)
}

fn evidence_request(digit: char, bytes: u64, offset: u64) -> EvidenceRead {
    EvidenceRead::new(evidence_reference(digit, bytes), offset).unwrap()
}

#[test]
fn read_tool_result_is_an_intrinsic_exact_session_capability() {
    let request = evidence_request('a', 128, 64);
    let call = PreparedCall {
        tool: "read_tool_result".to_string(),
        ops: vec![Op::ReadEvidence {
            request: request.clone(),
        }],
    };
    let policy = PolicyEngine::new(
        workspace(),
        Mode::Plan,
        Ruleset::from_layer(
            PermCfg {
                allow: Vec::new(),
                deny: Vec::new(),
                judge: None,
            },
            Source::Project,
        ),
    );

    let Classification::Settled(Ok(authorized)) = policy.classify(call) else {
        panic!("session-owned evidence must settle without a rule, mode, judge, or human");
    };
    assert_eq!(authorized.by(), &Authority::SessionEvidence);
    assert_eq!(
        authorized.call().ops,
        vec![Op::ReadEvidence {
            request: request.clone(),
        }]
    );
    assert_eq!(authorized.call().ops.len(), 1);
    assert!(authorized
        .permit()
        .allows_evidence(&request)
        .is_ok());
    assert!(authorized
        .permit()
        .allows_evidence(&evidence_request('b', 128, 64))
        .is_err());

    let combined = PreparedCall {
        tool: "read_tool_result".to_string(),
        ops: vec![
            Op::ReadEvidence { request },
            Op::Read {
                path: workspace(),
            },
        ],
    };
    assert!(matches!(
        policy.classify(combined),
        Classification::Settled(Err(PolicyDenied::EvidenceMustStandAlone { .. }))
    ));
}

#[test]
fn confinement_is_exact_workspace_default_and_orthogonal_to_mode() {
    assert_eq!("ro".parse::<Confinement>().unwrap().to_string(), "ro");
    assert_eq!(
        "workspace".parse::<Confinement>().unwrap().to_string(),
        "workspace"
    );
    assert_eq!("off".parse::<Confinement>().unwrap().to_string(), "off");
    assert_eq!(Confinement::default(), Confinement::Workspace);
    assert!("bogus".parse::<Confinement>().is_err());
}

#[test]
fn run_confinement_maps_levels_to_explicit_requirements() {
    let root = workspace();
    let ro = RunConfinement::new(Confinement::Ro, false, root.clone()).unwrap();
    assert_eq!(ro.level(), Confinement::Ro);
    assert!(!ro.offline());
    assert_eq!(ro.workspace(), &root);
    assert!(ro
        .requirements()
        .check(ExecCaps::READ_SCOPED.union(ExecCaps::WRITE_DENIED))
        .is_ok());
    assert!(ro.requirements().check(ExecCaps::WRITE_SCOPED).is_err());

    let scoped = RunConfinement::new(Confinement::Workspace, false, root.clone()).unwrap();
    assert!(scoped
        .requirements()
        .check(ExecCaps::READ_SCOPED.union(ExecCaps::WRITE_SCOPED))
        .is_ok());
    assert!(scoped.requirements().check(ExecCaps::WRITE_DENIED).is_err());

    let off = RunConfinement::new(Confinement::Off, false, root.clone()).unwrap();
    assert!(off.requirements().check(ExecCaps::empty()).is_ok());

    let offline = RunConfinement::new(Confinement::Off, true, root).unwrap();
    assert!(offline.requirements().check(ExecCaps::empty()).is_err());
}

#[test]
fn run_confinement_requires_an_absolute_workspace() {
    assert!(RunConfinement::new(
        Confinement::Workspace,
        false,
        CanonPath::new("relative"),
    )
    .is_err());
}

#[test]
fn capability_sets_are_composable_and_missing_bits_fail_closed() {
    let required = ExecCaps::READ_SCOPED.union(ExecCaps::WRITE_SCOPED);
    assert!(required.contains(ExecCaps::READ_SCOPED));
    assert!(required.contains(ExecCaps::WRITE_SCOPED));
    assert!(!ExecCaps::empty().contains(ExecCaps::READ_SCOPED));
    assert!(required.check(ExecCaps::READ_SCOPED).is_err());
}

#[test]
fn policy_digest_is_exactly_lowercase_sha256_hex_and_rejects_other_spellings() {
    let valid = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let digest: PolicyDigest = valid.try_into().expect("valid digest");
    assert_eq!(serde_json::to_value(&digest).unwrap(), valid);
    assert_eq!(digest.as_str(), valid);

    for invalid in [
        "",
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcde",
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0",
        "0123456789ABCDEF0123456789abcdef0123456789abcdef0123456789abcdef",
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdeg",
    ] {
        assert!(
            PolicyDigest::try_from(invalid).is_err(),
            "accepted invalid policy digest {invalid:?}"
        );
        assert!(
            serde_json::from_value::<PolicyDigest>(serde_json::json!(invalid)).is_err(),
            "serde accepted invalid policy digest {invalid:?}"
        );
    }
}

#[test]
fn exec_caps_serde_uses_stable_semantic_names_and_rejects_unknown_values() {
    let caps = ExecCaps::READ_SCOPED
        .union(ExecCaps::WRITE_SCOPED)
        .union(ExecCaps::NO_IP_NETWORK);
    let encoded = serde_json::to_string(&caps).expect("capabilities are durable JSON");
    assert_eq!(
        serde_json::to_value(caps).unwrap(),
        serde_json::json!(["read_scoped", "write_scoped", "no_ip_network"])
    );
    for name in ["read_scoped", "write_scoped", "no_ip_network"] {
        assert!(encoded.contains(name), "capability name missing from {encoded}");
    }
    assert_eq!(serde_json::from_str::<ExecCaps>(&encoded).unwrap(), caps);
    assert!(serde_json::from_value::<ExecCaps>(serde_json::json!("future_capability")).is_err());
    assert!(serde_json::from_value::<ExecCaps>(
        serde_json::json!(["read_scoped", "future_capability"])
    )
    .is_err());
    assert!(serde_json::from_value::<ExecCaps>(
        serde_json::json!(["read_scoped", "read_scoped"])
    )
    .is_err());
}

#[test]
fn execution_observation_and_backend_identity_are_serde_values_without_authority() {
    let digest = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let backend = BackendIdentity::new("landlock", "landlock-safe-reexec-v1-abi9");
    assert_eq!(backend.name(), "landlock");
    assert_eq!(backend.revision(), "landlock-safe-reexec-v1-abi9");
    let backend_roundtrip: BackendIdentity =
        serde_json::from_value(serde_json::to_value(&backend).unwrap()).unwrap();
    assert_eq!(backend_roundtrip, backend);
    let observation = ExecObservation::new(
        backend.clone(),
        ExecCaps::READ_SCOPED.union(ExecCaps::WRITE_SCOPED),
        digest.try_into().expect("valid policy digest"),
    );
    assert_eq!(observation.backend().name(), "landlock");
    assert_eq!(observation.backend().revision(), "landlock-safe-reexec-v1-abi9");
    assert!(observation.capabilities().contains(ExecCaps::READ_SCOPED));
    assert!(observation.capabilities().contains(ExecCaps::WRITE_SCOPED));
    assert_eq!(
        serde_json::to_value(observation.policy_digest()).unwrap(),
        digest
    );

    let roundtrip: ExecObservation =
        serde_json::from_value(serde_json::to_value(&observation).unwrap()).unwrap();
    assert_eq!(roundtrip.backend().name(), "landlock");
    assert_eq!(roundtrip.backend().revision(), "landlock-safe-reexec-v1-abi9");
    assert!(serde_json::from_value::<ExecObservation>(serde_json::json!({
        "backend": {"name": "null", "revision": "test"},
        "capabilities": [],
        "policy_digest": digest,
        "unknown": true,
    }))
    .is_err());
}

#[test]
fn execution_observed_is_additive_internal_noop_and_has_no_public_projection() {
    let observation = ExecObservation::new(
        BackendIdentity::new("null", "test"),
        ExecCaps::empty(),
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
            .try_into()
            .expect("valid policy digest"),
    );
    let kind = EventKind::ExecutionObserved {
        observation: observation.clone(),
    };
    assert_eq!(kind.name(), "execution_observed");
    assert_eq!(
        serde_json::from_value::<EventKind>(serde_json::to_value(&kind).unwrap()).unwrap(),
        kind
    );

    let event = DomainEvent {
        env: Envelope {
            seq: Seq::default(),
            id: EventId::new(),
            turn: TurnId::new(),
            item: None,
            parent: None,
            ts: Timestamp::EPOCH,
            producer: Producer::Main,
        },
        kind,
    };
    assert!(project_public(&event).is_none());

    let mut before = serde_json::to_value(SessionState::default()).unwrap();
    let mut state = SessionState::default();
    state.apply(&event);
    let mut after = serde_json::to_value(&state).unwrap();
    // Every domain event advances the replay cursor and records its id.
    // ExecutionObserved must not alter any semantic session field.
    before.as_object_mut().unwrap().remove("known_events");
    after.as_object_mut().unwrap().remove("known_events");
    assert_eq!(after, before);
}

mod turn_execution_observation {
    use super::*;
    use ego_core::budget::{BudgetGate, Metering};
    use ego_core::cost::{Amount, CurrencyCode, MeteringRevision, Money};
    use ego_core::generator::capabilities::{
        CacheRetention, CancelSemantics, ModelCapabilities, ReasoningCarryover,
        ReasoningEffortSupport, Transport,
    };
    use ego_core::generator::fake::FakeGenerator;
    use ego_core::generator::port::AiEvent;
    use ego_core::generator::types::{StopReason, ThinkingLevel};
    use ego_core::permit::fake::FakeFs;
    use ego_core::permit::{ProcessError, ProcessExecutor, ToolResult};
    use ego_core::sink::CollectingSink;
    use ego_core::store::evidence::{
        EvidenceError, EvidencePage, EvidenceRead, EvidenceStore, EvidenceWriter,
    };
    use ego_core::store::session_store::{SessionMeta, SessionStore, StoreError};
    use ego_core::turn::{CancelToken, Model, Session, Tools, TurnConfig, TurnLoop};
    use futures::future::BoxFuture;
    use futures::stream::BoxStream;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct MemoryStore {
        events: Mutex<Vec<DomainEvent>>,
        fail_at: AtomicUsize,
        calls: AtomicUsize,
    }

    impl MemoryStore {
        fn fail_at(&self, index: usize) {
            self.fail_at.store(index, Ordering::SeqCst);
        }

        fn events(&self) -> Vec<DomainEvent> {
            self.events.lock().unwrap().clone()
        }
    }

    impl SessionStore for MemoryStore {
        fn append(&self, _id: &ego_core::event::id::SessionId, event: &DomainEvent) -> Result<(), StoreError> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            if call == self.fail_at.load(Ordering::SeqCst) {
                return Err(StoreError::Io(std::io::Error::other("observation append failed")));
            }
            self.events.lock().unwrap().push(event.clone());
            Ok(())
        }

        fn read(
            &self,
            _id: &ego_core::event::id::SessionId,
        ) -> Result<BoxStream<'static, Result<DomainEvent, StoreError>>, StoreError> {
            unimplemented!("observation turn tests only append")
        }

        fn list(&self) -> Result<Vec<SessionMeta>, StoreError> {
            unimplemented!("observation turn tests only append")
        }
    }

    struct FixedClock;

    impl ego_core::event::time::Clock for FixedClock {
        fn now(&self) -> Timestamp {
            Timestamp::EPOCH
        }
    }

    struct NoProcess;

    impl ProcessExecutor for NoProcess {
        fn execute<'a>(
            &'a self,
            _spec: &'a SpawnSpec,
        ) -> BoxFuture<'a, Result<ToolResult, ProcessError>> {
            Box::pin(futures::future::ready(Err(ProcessError::Invariant(
                "process execution is not used by observation tests".into(),
            ))))
        }
    }

    struct NoEvidence;

    impl EvidenceStore for NoEvidence {
        fn begin(&self) -> Result<Box<dyn EvidenceWriter>, EvidenceError> {
            Err(EvidenceError::Limit { limit: 0 })
        }

        fn read_page<'a>(
            &'a self,
            _request: &'a EvidenceRead,
        ) -> BoxFuture<'a, Result<EvidencePage, EvidenceError>> {
            Box::pin(futures::future::ready(Err(EvidenceError::Limit { limit: 0 })))
        }
    }

    struct Fixture {
        store: MemoryStore,
        clock: FixedClock,
        generator: FakeGenerator,
        sink: CollectingSink,
        gate: BudgetGate,
        policy: PolicyEngine,
        fs: FakeFs,
        process: NoProcess,
        evidence: NoEvidence,
        registry: Vec<Arc<dyn ego_core::permit::Tool>>,
    }

    impl Fixture {
        fn new(scripts: Vec<Vec<AiEvent>>) -> Self {
            Self {
                store: MemoryStore::default(),
                clock: FixedClock,
                generator: FakeGenerator::new(scripts),
                sink: CollectingSink::new(),
                gate: BudgetGate::new(Money::new(
                    "1000".parse::<Amount>().unwrap(),
                    CurrencyCode::new("USD").unwrap(),
                )),
                policy: PolicyEngine::new(
                    CanonPath::new("/ws"),
                    Mode::Default,
                    Ruleset::default(),
                ),
                fs: FakeFs::default(),
                process: NoProcess,
                evidence: NoEvidence,
                registry: Vec::new(),
            }
        }

        fn loop_(&mut self) -> TurnLoop<'_> {
            let tools = Tools::new(
                &self.registry,
                &self.policy,
                &self.fs,
                &self.process,
                &self.evidence,
                CanonPath::new("/ws"),
            );
            TurnLoop::new(
                Session {
                    id: ego_core::event::id::SessionId::new(),
                    state: SessionState::default(),
                    store: &self.store,
                    clock: &self.clock,
                    sink: &self.sink,
                    cancel: CancelToken::new(),
                },
                Model {
                    generator: &self.generator,
                    gate: &mut self.gate,
                    metering: Metering::Unmetered {
                        revision: MeteringRevision(String::from("test")),
                    },
                    rates: None,
                    token_ceiling: None,
                    caps: ModelCapabilities {
                        schema_version: 1,
                        max_context_tokens: Some(10_000),
                        max_output_tokens: Some(1_000),
                        max_tools: Some(16),
                        supports_parallel_tool_calls: false,
                        reasoning_carryover: ReasoningCarryover::Unsupported,
                        reasoning_efforts: ReasoningEffortSupport::Any,
                        reasoning_state_dialect: None,
                        cancel: CancelSemantics::Cooperative,
                        cache: CacheRetention::Short,
                        transport: Transport::Sse,
                    },
                    config: TurnConfig {
                        model: "test-model".into(),
                        system: None,
                        thinking: ThinkingLevel::Off,
                        max_output_tokens: Some(1_000),
                        parallel_tool_calls: false,
                    },
                },
                tools,
            )
        }
    }

    fn observation() -> ExecObservation {
        ExecObservation::new(
            BackendIdentity::new("null", "test"),
            ExecCaps::empty(),
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                .try_into()
                .unwrap(),
        )
    }

    #[test]
    fn observation_is_appended_before_public_turn_and_consumed_once_per_loop() {
        let mut fixture = Fixture::new(vec![vec![AiEvent::Stop(StopReason::Stop)], vec![AiEvent::Stop(StopReason::Stop)]]);
        let mut loop_ = fixture.loop_().with_execution_observation(observation());
        futures::executor::block_on(loop_.run("first")).expect("first turn");
        futures::executor::block_on(loop_.run("second")).expect("second turn");

        let events = fixture.store.events();
        let observation_at = events
            .iter()
            .position(|event| matches!(event.kind, EventKind::ExecutionObserved { .. }))
            .expect("one durable observation");
        let prompt_at = events
            .iter()
            .position(|event| matches!(event.kind, EventKind::UserPrompt { .. }))
            .expect("first prompt");
        assert!(observation_at < prompt_at);
        let observation_turn = events[observation_at].env.turn;
        let prompt_turn = events[prompt_at].env.turn;
        assert_eq!(observation_turn, prompt_turn, "observation and prompt share one minted turn");
        match fixture.sink.events().first() {
            Some(ego_core::public::PublicEvent::TurnStarted { turn }) => {
                assert_eq!(*turn, observation_turn, "public turn uses the same minted identity")
            }
            other => panic!("turn start was not published first: {other:?}"),
        }
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event.kind, EventKind::ExecutionObserved { .. }))
                .count(),
            1,
            "schema/turn retries reuse one prepared observation"
        );
    }

    #[test]
    fn observation_append_failure_abandons_before_public_turn_or_prompt() {
        let mut fixture = Fixture::new(vec![vec![AiEvent::Stop(StopReason::Stop)]]);
        fixture.store.fail_at(1);
        let mut loop_ = fixture.loop_().with_execution_observation(observation());
        let abandoned = futures::executor::block_on(loop_.run("must not publish"))
            .expect_err("observation append must be fatal before the turn");
        assert!(fixture.store.events().is_empty());
        assert!(fixture.sink.events().is_empty());
        assert!(fixture.generator.calls().is_empty(), "abandoned observation reached generation");
        assert!(abandoned.state.last_seq == Seq::default());
    }

    #[test]
    fn a_loop_without_an_observation_preserves_the_existing_turn_path() {
        let mut fixture = Fixture::new(vec![vec![AiEvent::Stop(StopReason::Stop)]]);
        let mut loop_ = fixture.loop_();
        futures::executor::block_on(loop_.run("without observation")).expect("turn completes");

        assert!(fixture
            .store
            .events()
            .iter()
            .all(|event| !matches!(event.kind, EventKind::ExecutionObserved { .. })));
        assert!(matches!(
            fixture.sink.events().first(),
            Some(ego_core::public::PublicEvent::TurnStarted { .. })
        ));
        assert_eq!(fixture.generator.calls().len(), 1);
    }
}

//! RED contract for M4 S1: typed child, skill, hook, accounting, and codec facts.
//!
//! These tests deliberately build the durable wire shapes through serde.  The
//! event log is the contract boundary: a future implementation must make the
//! shapes below typed, closed, replayable, and privacy preserving rather than
//! teaching this test a second in-memory DTO.

use ego_core::config::ModelRef;
use ego_core::event::domain::{DomainEvent, GenerationRole};
use ego_core::event::hook::{
    HookContentDigest, HookLifecycle, HookSourceAuthority, HookSourceLocation,
};
use ego_core::event::id::{EventId, ItemId, Producer, Seq, SessionId, TurnId};
use ego_core::event::skill::{
    SkillCatalogueEvidence, SkillContentDigest, SkillDescriptorEvidence, SkillId,
    SkillSourceAuthority, SkillSourceLocation,
};
use ego_core::event::time::Timestamp;
use ego_core::evidence::{EvidenceError, EvidenceWrite, RunContextEvidence};
use ego_core::generator::capabilities::ModelCapabilities;
use ego_core::public::project_public;
use ego_core::run::{
    EventSinkFactory, IdSource, RecoveryEvidence, RequestDigest, RequestId, RunAppend,
    RunAttachment, RunCommandOutcome, RunController, RunError, RunExecutionBindings, RunExecutor,
    RunExecutorError, RunHandle, RunOrigin, RunPreparation, RunPrepared, RunPrompt, RunRetention,
    RunRoot, RunSettlement, RunTransaction, StartRequest,
};
use ego_core::sink::{EventSink, NullSink};
use ego_core::state::SessionState;
use ego_core::store::session_store::{SessionEntry, SessionMeta, SessionStore, StoreError};
use ego_core::usage::{USAGE_REPORT_VERSION, UsageReport};
use futures::future::{self, BoxFuture};
use futures::stream::{self, BoxStream};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

const SESSION: &str = "00000000-0000-0000-0000-000000000101";
const TURN_ID: &str = "00000000-0000-0000-0000-000000000102";
const ITEM_ID: &str = "00000000-0000-0000-0000-000000000103";
const INVOCATION: &str = "01900000-0000-7000-8000-000000000104";
const CHILD_SESSION: &str = "00000000-0000-0000-0000-000000000105";
const CHILD_NONCE: &str = "01900000-0000-7000-8000-000000000106";
const DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const DIGEST_B: &str = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
const BLOB: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn event(kind: Value) -> Value {
    json!({
        "env": {
            "seq": 1,
            "id": EventId::new(),
            "turn": TurnId::new(),
            "item": ItemId::new(),
            "parent": Value::Null,
            "ts": Timestamp::from_millis(1_000),
            "producer": Producer::Main,
        },
        "kind": kind,
    })
}

fn decode(kind: Value) -> DomainEvent {
    serde_json::from_value(event(kind)).expect("M4 durable event must decode")
}

fn no_unsafe_keys(value: &Value) {
    const FORBIDDEN: &[&str] = &[
        "raw_arguments",
        "arguments",
        "host_path",
        "absolute_path",
        "cwd",
        "environment",
        "env",
        "secret",
        "secrets",
        "credential",
        "handler",
        "body",
    ];
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                assert!(
                    !FORBIDDEN.contains(&key.as_str()),
                    "unsafe key leaked: {key}"
                );
                no_unsafe_keys(child);
            }
        }
        Value::Array(values) => values.iter().for_each(no_unsafe_keys),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

fn selections() -> Value {
    json!({
        "model": "provider/model",
        "thinking": null,
        "mode": "default",
        "budget": null,
        "profile": null,
    })
}

fn child() -> Value {
    json!({"session_id": CHILD_SESSION, "nonce": CHILD_NONCE})
}

fn assert_json_key_order(serialized: &str, keys: &[&str]) {
    let positions = keys
        .iter()
        .map(|key| {
            serialized
                .find(&format!("\"{key}\":"))
                .unwrap_or_else(|| panic!("missing serialized key: {key}"))
        })
        .collect::<Vec<_>>();
    assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
}

mod regression_case {
    use super::*;

    #[derive(Default)]
    struct OriginStore {
        entries: Mutex<Vec<SessionEntry>>,
    }

    impl OriginStore {
        fn entries(&self) -> Vec<SessionEntry> {
            self.entries.lock().expect("origin store entries").clone()
        }

        fn replace_latest_origin(&self, origin: Value) {
            let mut entries = self.entries.lock().expect("origin store entries");
            let index = entries
                .iter()
                .rposition(|entry| matches!(entry, SessionEntry::RunTransaction(_)))
                .expect("origin fixture entry");
            let SessionEntry::RunTransaction(transaction) = &mut entries[index] else {
                panic!("origin fixture entry is not a run transaction");
            };
            let mut encoded = serde_json::to_value(&*transaction).expect("transaction encoding");
            encoded["origin"] = origin;
            *transaction = serde_json::from_value(encoded).expect("transaction remains typed");
        }
    }

    impl SessionStore for OriginStore {
        fn append(&self, _id: &SessionId, event: &DomainEvent) -> Result<(), StoreError> {
            self.entries
                .lock()
                .expect("origin store entries")
                .push(SessionEntry::Event(event.clone()));
            Ok(())
        }

        fn read(
            &self,
            _id: &SessionId,
        ) -> Result<BoxStream<'static, Result<SessionEntry, StoreError>>, StoreError> {
            let entries = self.entries();
            if entries.is_empty() {
                return Err(StoreError::NotFound);
            }
            Ok(Box::pin(stream::iter(entries.into_iter().map(Ok))))
        }

        fn list(&self) -> Result<Vec<SessionMeta>, StoreError> {
            Ok(Vec::new())
        }

        fn compare_and_append_run(
            &self,
            _session: &SessionId,
            expected_revision: Option<ego_core::run::RunRevision>,
            transaction: RunTransaction,
        ) -> Result<RunAppend, StoreError> {
            let mut entries = self.entries.lock().expect("origin store entries");
            let current = entries.iter().rev().find_map(|entry| match entry {
                SessionEntry::RunTransaction(transaction) => Some(transaction.revision()),
                SessionEntry::Event(_) | SessionEntry::Evidence(_) => None,
            });
            if expected_revision != current {
                return Ok(RunAppend::Conflict {
                    actual: current.unwrap_or(ego_core::run::RunRevision::new(0)),
                });
            }
            let revision = transaction.revision();
            entries.push(SessionEntry::RunTransaction(transaction));
            Ok(RunAppend::Committed { revision })
        }
    }

    struct Prepared {
        context: RunContextEvidence,
    }

    #[derive(Default)]
    struct OriginExecutor {
        contexts: Mutex<Vec<RunContextEvidence>>,
    }

    impl OriginExecutor {
        fn contexts(&self) -> Vec<RunContextEvidence> {
            self.contexts.lock().expect("origin contexts").clone()
        }
    }

    impl RunExecutor for OriginExecutor {
        type Prepared = Prepared;

        fn prepare<'a>(
            &'a self,
            request: &'a RunPreparation,
        ) -> BoxFuture<'a, Result<RunPrepared<Self::Prepared>, RunExecutorError>> {
            let context = RunContextEvidence::new(
                request.run_id.session_id(),
                request.invocation,
                request.run_id,
                ego_core::evidence::ConfigProvenance::default(),
            )
            .with_origin(request.origin.clone());
            self.contexts
                .lock()
                .expect("origin contexts")
                .push(context.clone());
            Box::pin(future::ready(Ok(RunPrepared::new(
                ModelRef("provider/model".to_owned()),
                Prepared { context },
            ))))
        }

        fn admit_run_context<'a>(
            &'a self,
            prepared: &'a Self::Prepared,
        ) -> ego_core::run::RunFuture<'a, Result<EvidenceWrite, EvidenceError>> {
            let _ = &prepared.context;
            Box::pin(future::ready(Ok(EvidenceWrite::Published)))
        }

        fn attach<'a>(
            &'a self,
            request: &'a RunAttachment,
        ) -> BoxFuture<'a, Result<RunHandle, RunExecutorError>> {
            Box::pin(future::ready(Ok(RunHandle::new(request.run_id, ()))))
        }

        fn recover<'a>(
            &'a self,
            _run: &'a ego_core::run::RunId,
        ) -> BoxFuture<'a, Result<RecoveryEvidence, RunExecutorError>> {
            Box::pin(future::ready(Ok(RecoveryEvidence::Terminal)))
        }

        fn observe<'a>(
            &'a self,
            _handle: &'a RunHandle,
            _bindings: RunExecutionBindings,
        ) -> BoxFuture<'a, Result<RunSettlement, RunExecutorError>> {
            Box::pin(future::ready(Ok(RunSettlement::Completed)))
        }

        fn submit<'a>(
            &'a self,
            _handle: &'a RunHandle,
            _prompt: &'a RunPrompt,
            _prepared: Self::Prepared,
            _bindings: RunExecutionBindings,
        ) -> BoxFuture<'a, Result<RunSettlement, RunExecutorError>> {
            Box::pin(future::ready(Ok(RunSettlement::Completed)))
        }

        fn interrupt<'a>(
            &'a self,
            _handle: &'a RunHandle,
        ) -> BoxFuture<'a, Result<(), RunExecutorError>> {
            Box::pin(future::ready(Ok(())))
        }

        /// A no-op for the same reason `interrupt` is one: this double never
        /// runs a turn, so there is nothing here that a hold could hold back.
        fn pause<'a>(
            &'a self,
            _handle: &'a RunHandle,
            _request: RequestId,
        ) -> BoxFuture<'a, Result<(), RunExecutorError>> {
            Box::pin(future::ready(Ok(())))
        }
    }

    #[derive(Default)]
    struct NullFactory;

    impl EventSinkFactory for NullFactory {
        fn create(&self, _capabilities: &ModelCapabilities) -> Arc<dyn EventSink> {
            Arc::new(NullSink)
        }
    }

    #[derive(Default)]
    struct FixedClock;

    impl ego_core::event::time::Clock for FixedClock {
        fn now(&self) -> Timestamp {
            Timestamp::from_millis(1_000)
        }
    }

    #[derive(Default)]
    struct FixedIds;

    impl IdSource for FixedIds {
        fn next_uuid(&self) -> uuid::Uuid {
            uuid::Uuid::from_u128(0x1234)
        }
    }

    type Controller = RunController<OriginStore, OriginExecutor, FixedClock, FixedIds>;

    fn controller(store: Arc<OriginStore>, executor: Arc<OriginExecutor>) -> Controller {
        RunController::new(
            store,
            executor,
            Arc::new(FixedClock),
            Arc::new(FixedIds),
            RunRetention::new(std::num::NonZeroUsize::new(2).unwrap()).unwrap(),
        )
    }

    fn start_request(origin: RunOrigin, digest: &str) -> StartRequest {
        StartRequest::new(
            RequestId::session_id(RequestId::from_uuid(
                uuid::Uuid::parse_str(INVOCATION).unwrap(),
            )),
            RunRoot::new("/workspace").unwrap(),
            RequestId::from_uuid(uuid::Uuid::parse_str(INVOCATION).unwrap()),
            RequestDigest::parse(digest).unwrap(),
            ego_core::run::RunSelections::empty(),
            "child task",
        )
        .with_origin(origin)
    }

    fn bindings() -> RunExecutionBindings {
        RunExecutionBindings::new(Arc::new(NullFactory), None)
    }

    #[test]
    fn run_origin_and_subagent_producer_join_parent_and_child_without_string_identity() {
        let origin: RunOrigin = serde_json::from_value(json!({
            "kind": "subagent",
            "invocation": INVOCATION,
            "parent_run": {"session_id": SESSION, "nonce": CHILD_NONCE},
            "parent_turn": TURN_ID,
            "parent_item": ITEM_ID,
        }))
        .expect("subagent origin must be a typed durable value");
        let encoded = serde_json::to_value(origin).unwrap();
        assert_eq!(encoded["kind"], "subagent");
        assert_eq!(encoded["invocation"], INVOCATION);
        assert!(!encoded.to_string().contains("subagent_id"));

        let producer: Producer = serde_json::from_value(json!({"Subagent": INVOCATION}))
            .expect("subagent producer must carry the typed invocation id");
        assert_eq!(
            serde_json::to_value(producer).unwrap(),
            json!({"Subagent": INVOCATION})
        );
    }

    fn blob(bytes: u64) -> Value {
        json!({"id": BLOB, "bytes": bytes})
    }

    fn zero_usage() -> Value {
        json!({
            "uncached_input_tokens": 0,
            "cache_read_tokens": 0,
            "cache_write_tokens": 0,
            "cache_write_extended_tokens": 0,
            "output_tokens": 0,
        })
    }

    #[test]
    fn child_lifecycle_is_typed_and_has_no_legacy_string_or_duplicate_admission_shape() {
        let requested = decode(json!({
            "kind": "SubagentRequested",
            "invocation": INVOCATION,
            "task": blob(12),
            "selections": selections(),
        }));
        let spawned = decode(json!({
            "kind": "SubagentSpawned",
            "invocation": INVOCATION,
            "child": child(),
        }));
        let settled = decode(json!({
            "kind": "SubagentSettled",
            "invocation": INVOCATION,
            "child": child(),
            "terminal": {"kind": "returned", "final_message": blob(18)},
        }));

        for event in [requested, spawned, settled] {
            let value = serde_json::to_value(event).unwrap();
            assert!(!value.to_string().contains("subagent_id"));
            assert!(!value.to_string().contains("SubagentAdmitted"));
            no_unsafe_keys(value.get("kind").expect("event kind"));
        }
    }

    #[test]
    fn every_closed_subagent_terminal_preserves_typed_failure_and_rejects_generic_result_text() {
        for terminal in [
            json!({"kind": "returned", "final_message": blob(4)}),
            json!({"kind": "refused", "failure": {"kind": "invalid_task", "diagnostic": "bad task"}}),
            json!({"kind": "failed", "settlement": {"kind": "cancelled", "reason": "user"}}),
            json!({"kind": "return_blocked", "final_message": blob(4), "hook_invocation": INVOCATION}),
        ] {
            let event = decode(json!({
                "kind": "SubagentSettled",
                "invocation": INVOCATION,
                "child": child(),
                "terminal": terminal,
            }));
            let encoded = serde_json::to_value(event).unwrap();
            assert!(!encoded.to_string().contains("\"result\":\""));
        }

        let generic = event(json!({
            "kind": "SubagentSettled",
            "invocation": INVOCATION,
            "child": child(),
            "terminal": {"kind": "returned", "result": "untyped"},
        }));
        assert!(serde_json::from_value::<DomainEvent>(generic).is_err());
    }

    #[test]
    fn skill_catalogue_instruction_and_resource_facts_are_distinct_and_minimal() {
        let skill_id = canonical_skill_id(
            SkillSourceAuthority::Project,
            "explain-failure",
            "skills/explain-failure/SKILL.md",
        );
        let descriptor = json!({
            "id": skill_id,
            "name": "explain-failure",
            "description": "Explain a failing check.",
            "source": "project",
            "location": "skills/explain-failure/SKILL.md",
            "source_digest": DIGEST,
            "user_invocable": true,
            "model_invocable": true,
        });
        let catalogue = decode(json!({
            "kind": "SkillCatalogueAdmitted",
            "generation": DIGEST,
            "descriptors": [descriptor],
        }));
        let instruction = decode(json!({
            "kind": "SkillInstructionAdmitted",
            "invocation": INVOCATION,
            "skill": canonical_skill_id(
                SkillSourceAuthority::Project,
                "explain-failure",
                "skills/explain-failure/SKILL.md",
            ),
            "catalogue_generation": DIGEST,
            "source": "project",
            "location": "skills/explain-failure/SKILL.md",
            "source_digest": DIGEST,
            "admission": blob(32),
            "rendered": blob(33),
        }));
        let resource = decode(json!({
            "kind": "SkillResourceAdmitted",
            "invocation": INVOCATION,
            "skill": canonical_skill_id(
                SkillSourceAuthority::Project,
                "explain-failure",
                "skills/explain-failure/SKILL.md",
            ),
            "location": "references/checklist.md",
            "content": blob(20),
        }));

        let catalogue_value = serde_json::to_value(catalogue).unwrap();
        let instruction_value = serde_json::to_value(instruction).unwrap();
        let resource_value = serde_json::to_value(resource).unwrap();
        no_unsafe_keys(catalogue_value.get("kind").expect("event kind"));
        no_unsafe_keys(instruction_value.get("kind").expect("event kind"));
        no_unsafe_keys(resource_value.get("kind").expect("event kind"));
        assert!(catalogue_value.to_string().contains("explain-failure"));
        assert!(!catalogue_value.to_string().contains("rendered"));
        assert!(instruction_value.to_string().contains("rendered"));
        assert!(resource_value.to_string().contains("content"));
        assert_ne!(instruction_value, resource_value);
    }

    fn canonical_skill_id(source: SkillSourceAuthority, name: &str, location: &str) -> String {
        let location = SkillSourceLocation::parse(location).unwrap();
        let digest = SkillContentDigest::parse(DIGEST).unwrap();
        SkillId::derive(source, &location, name, &digest).to_string()
    }

    fn skill_descriptor(name: &str, location: &str) -> SkillDescriptorEvidence {
        let source = SkillSourceAuthority::User;
        let location = SkillSourceLocation::parse(location).unwrap();
        let source_digest = SkillContentDigest::parse(DIGEST).unwrap();
        SkillDescriptorEvidence {
            id: SkillId::derive(source, &location, name, &source_digest),
            name: name.to_owned(),
            description: format!("description for {name}"),
            trigger: None,
            argument_hint: None,
            source,
            location,
            source_digest,
            user_invocable: true,
            model_invocable: true,
        }
    }

    #[test]
    fn skill_evidence_owns_distinct_source_types_and_validates_catalogue_identity() {
        assert_ne!(
            std::any::TypeId::of::<SkillContentDigest>(),
            std::any::TypeId::of::<HookContentDigest>()
        );
        assert_ne!(
            std::any::TypeId::of::<SkillSourceAuthority>(),
            std::any::TypeId::of::<HookSourceAuthority>()
        );
        assert_ne!(
            std::any::TypeId::of::<SkillSourceLocation>(),
            std::any::TypeId::of::<HookSourceLocation>()
        );

        let first = skill_descriptor("alpha", "skills/alpha/SKILL.md");
        let second = skill_descriptor("bravo", "skills/bravo/SKILL.md");
        let canonical = SkillCatalogueEvidence::new(vec![first.clone(), second.clone()])
            .expect("canonical catalogue must be constructible");
        assert_eq!(canonical.descriptors(), &[first.clone(), second.clone()]);

        let mut mismatched_descriptor = first.clone();
        mismatched_descriptor.id = SkillId::parse(&format!("sk1:{DIGEST}")).unwrap();
        assert!(SkillCatalogueEvidence::new(vec![mismatched_descriptor]).is_err());
        assert!(SkillCatalogueEvidence::new(vec![first.clone(), first.clone()]).is_err());
        assert!(
            SkillCatalogueEvidence::new(vec![
                first.clone(),
                skill_descriptor("charlie", "skills/alpha/SKILL.md"),
            ])
            .is_err()
        );
        assert!(SkillCatalogueEvidence::new(vec![second.clone(), first.clone()]).is_err());

        // Decode malformed wires independently of the validating constructor.
        let canonical_wire = serde_json::to_value(&canonical).unwrap();
        let mut mismatched_wire = canonical_wire.clone();
        mismatched_wire[0]["id"] = serde_json::json!(format!("sk1:{DIGEST}"));
        assert!(serde_json::from_value::<SkillCatalogueEvidence>(mismatched_wire).is_err());

        let mut duplicate_wire = canonical_wire.clone();
        duplicate_wire[1] = duplicate_wire[0].clone();
        assert!(serde_json::from_value::<SkillCatalogueEvidence>(duplicate_wire).is_err());

        let mut duplicate_location_wire = canonical_wire.clone();
        duplicate_location_wire[1] =
            serde_json::to_value(skill_descriptor("charlie", "skills/alpha/SKILL.md")).unwrap();
        assert!(serde_json::from_value::<SkillCatalogueEvidence>(duplicate_location_wire).is_err());

        let mut reversed_wire = canonical_wire;
        reversed_wire[0] = reversed_wire[1].clone();
        reversed_wire[1] = serde_json::to_value(first).unwrap();
        assert!(serde_json::from_value::<SkillCatalogueEvidence>(reversed_wire).is_err());
    }

    #[test]
    fn reducer_and_evidence_keep_typed_skill_facts_and_do_not_publish_private_bytes() {
        let catalogue = decode(json!({
            "kind": "SkillCatalogueAdmitted",
            "generation": DIGEST,
            "descriptors": [],
        }));
        let instruction = decode(json!({
            "kind": "SkillInstructionAdmitted",
            "invocation": INVOCATION,
            "skill": canonical_skill_id(
                SkillSourceAuthority::Project,
                "explain-failure",
                "skills/explain-failure/SKILL.md",
            ),
            "catalogue_generation": DIGEST,
            "source": "project",
            "location": "skills/explain-failure/SKILL.md",
            "source_digest": DIGEST,
            "admission": blob(32),
            "rendered": blob(33),
        }));
        let resource = decode(json!({
            "kind": "SkillResourceAdmitted",
            "invocation": INVOCATION,
            "skill": canonical_skill_id(
                SkillSourceAuthority::Project,
                "explain-failure",
                "skills/explain-failure/SKILL.md",
            ),
            "location": "references/checklist.md",
            "content": blob(20),
        }));

        let mut state = SessionState::default();
        for event in [&catalogue, &instruction, &resource] {
            state.apply(event);
            assert!(project_public(event).is_none(), "skill evidence is private");
        }
        assert!(state.refusals.is_empty(), "typed skill facts must reduce");
        assert_eq!(instruction.kind.blob_references().len(), 2);
        assert_eq!(resource.kind.blob_references().len(), 1);
    }

    #[test]
    fn model_switch_lifecycle_roles_and_v2_usage_are_current_closed_spellings() {
        for lifecycle in [
            HookLifecycle::PreModelSwitch,
            HookLifecycle::PostModelSwitch,
        ] {
            assert!(serde_json::to_value(lifecycle).unwrap().is_string());
        }
        for role in [GenerationRole::Subagent, GenerationRole::Hook] {
            assert!(serde_json::to_value(role).unwrap().is_string());
        }
        assert_eq!(USAGE_REPORT_VERSION, 2);
        let report: UsageReport = serde_json::from_value(json!({
            "v": 2,
            "as_of": 1000,
            "period": {"kind": "all"},
            "sessions": [],
            "totals": {
                "main": {"usage": zero_usage(), "cost": {"known": [], "unknown": []}},
                "judge": {"usage": zero_usage(), "cost": {"known": [], "unknown": []}},
                "compaction": {"usage": zero_usage(), "cost": {"known": [], "unknown": []}},
                "subagent": {"usage": zero_usage(), "cost": {"known": [], "unknown": []}},
                "hook": {"usage": zero_usage(), "cost": {"known": [], "unknown": []}},
                "total": {"usage": zero_usage(), "cost": {"known": [], "unknown": []}}
            }
        }))
        .expect("v2 usage report must decode");
        assert_eq!(report.v, 2);
        let encoded = serde_json::to_value(&report).unwrap();
        let totals = encoded.get("totals").unwrap().as_object().unwrap();
        assert_eq!(totals.len(), 6);
        for key in ["main", "judge", "compaction", "subagent", "hook", "total"] {
            assert!(totals.contains_key(key), "missing usage line: {key}");
        }
        let serialized = serde_json::to_string(&report).unwrap();
        assert!(serialized.starts_with("{\"v\":2,\"as_of\":1000,\"period\":"));
        assert_json_key_order(&serialized, &["v", "as_of", "period", "sessions", "totals"]);
        assert_json_key_order(
            &serialized,
            &["main", "judge", "compaction", "subagent", "hook", "total"],
        );
    }

    #[test]
    fn run_origin_survives_transaction_snapshot_and_evidence_accessors() {
        let origin_value = json!({
            "kind": "subagent",
            "invocation": INVOCATION,
            "parent_run": {"session_id": SESSION, "nonce": CHILD_NONCE},
            "parent_turn": TURN_ID,
            "parent_item": ITEM_ID,
        });
        let origin: RunOrigin =
            serde_json::from_value(origin_value.clone()).expect("subagent origin must be typed");
        let store = Arc::new(OriginStore::default());
        let executor = Arc::new(OriginExecutor::default());
        let controller = controller(Arc::clone(&store), Arc::clone(&executor));
        let admission = match futures::executor::block_on(
            controller.start(start_request(origin.clone(), "digest-a"), bindings()),
        )
        .expect("child-origin start must be admitted")
        {
            RunCommandOutcome::Admitted(admission) => admission,
            RunCommandOutcome::Replayed(_) => panic!("fresh origin start was replayed"),
        };
        let run_id = admission.run_id();
        let transaction = store
            .entries()
            .into_iter()
            .find_map(|entry| match entry {
                SessionEntry::RunTransaction(transaction) => Some(transaction),
                SessionEntry::Event(_) | SessionEntry::Evidence(_) => None,
            })
            .expect("admission transaction");
        assert_eq!(transaction.origin(), &origin);
        assert_eq!(admission.snapshot().origin(), &origin);
        assert_eq!(executor.contexts()[0].origin(), &origin);

        let different_origin: RunOrigin = serde_json::from_value(json!({
            "kind": "subagent",
            "invocation": INVOCATION,
            "parent_run": {"session_id": SESSION, "nonce": CHILD_NONCE},
            "parent_turn": TURN_ID,
            "parent_item": "00000000-0000-0000-0000-000000000107",
        }))
        .expect("alternate origin must be typed");
        assert!(
            futures::executor::block_on(
                controller.start(start_request(different_origin, DIGEST), bindings())
            )
            .is_err()
        );

        assert!(matches!(
            futures::executor::block_on(
                controller.start(start_request(origin.clone(), DIGEST_B), bindings())
            ),
            Err(RunError::RequestIdMismatch { .. })
        ));

        futures::executor::block_on(controller.execute(admission.into_execution()))
            .expect("child-origin transaction must settle");
        store.replace_latest_origin(json!({"kind": "top_level"}));
        assert!(matches!(
            futures::executor::block_on(controller.status(run_id)),
            Err(RunError::Integrity { message }) if message.contains("origin")
        ));
    }

    #[test]
    fn unknown_and_legacy_shapes_fail_closed_while_existing_event_records_remain_readable() {
        let legacy = event(json!({"kind": "SubagentSpawned", "subagent_id": "legacy"}));
        assert!(serde_json::from_value::<DomainEvent>(legacy).is_err());

        let unknown = event(json!({"kind": "FutureM4Fact", "payload": "opaque"}));
        assert!(serde_json::from_value::<DomainEvent>(unknown).is_err());

        let existing = event(json!({"kind": "UserPrompt", "text": "kept"}));
        assert!(serde_json::from_value::<DomainEvent>(existing).is_ok());
    }

    #[test]
    fn domain_event_sequence_is_one_typed_event_per_causal_fact() {
        let mut seq = Seq::default();
        assert_eq!(seq.next(), serde_json::from_value(json!(1)).unwrap());
        let session: SessionId = SESSION.parse().unwrap();
        assert_eq!(session.to_string(), SESSION);
    }
}

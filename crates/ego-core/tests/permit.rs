//! `PreparedCall` / `Permit` / `GuardedIo`: enforced, not declared, effects.
//!
//! Policy grants remain scoped to prepared calls.

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use ego_core::config::{Mode, OriginSet, PermCfg, Ruleset, Source};
use ego_core::event::hook::HookId;
use ego_core::permit::rule::Rule;
use ego_core::permit::{
    Authority, Authorized, CanonPath, Change, Classification, DuplexHandle, DuplexSpawner, Entry,
    EntryKind, Escalation, Fs, FsError, GuardedEffects, GuardedIo, HttpRequest, HttpRoute,
    HttpScheme, IoDenied, NetworkTarget, OneHop, Op, OpName, PathResolver, Pending, PermitDenied,
    PolicyDenied, PolicyEngine, PrepareCtx, PreparedCall, ProcessError, ProcessExecutor,
    Resolution, ResolvedConnect, Resolver, Scan, ScanLimits, ScanRequest, SpawnSpec, Tool,
    ToolError, ToolResult, TransportError, TransportFailure, Typed,
};
use ego_core::store::evidence::{
    EvidenceError, EvidencePage, EvidenceRead, EvidenceStore, EvidenceWriter,
};
use futures::executor::block_on;
use futures::future::{self, BoxFuture};
use serde_json::json;

// Wrapped in a module named `permit` so `cargo test -p ego-core permit` (the
// story's VERIFY command) matches these tests by substring: cargo's test
// filter matches the full test path, not the source file name, and
// tests/permit.rs's bare functions wouldn't otherwise contain "permit".
mod common;

mod permit {
    use super::*;
    use ego_core::permit::fake::{FakeFs, FakeWriteTool};

    struct NoProcess;

    impl ProcessExecutor for NoProcess {
        fn execute<'a>(
            &'a self,
            _spec: &'a ego_core::permit::SpawnSpec,
        ) -> BoxFuture<'a, Result<ToolResult, ProcessError>> {
            Box::pin(future::ready(Err(ProcessError::Invariant(
                "process execution is not used by filesystem tests".to_string(),
            ))))
        }
    }

    struct NoEvidence;

    impl EvidenceStore for NoEvidence {
        fn blob_store(&self) -> &dyn ego_core::store::blob::BlobStore {
            &common::EMPTY_BLOB_STORE
        }

        fn begin(&self) -> Result<Box<dyn EvidenceWriter>, EvidenceError> {
            Err(EvidenceError::Limit { limit: 0 })
        }

        fn read_page<'a>(
            &'a self,
            request: &'a EvidenceRead,
        ) -> BoxFuture<'a, Result<EvidencePage, EvidenceError>> {
            Box::pin(future::ready(Err(EvidenceError::NotFound {
                reference: request.reference.id.clone(),
            })))
        }
    }

    static NO_PROCESS: NoProcess = NoProcess;
    static NO_EVIDENCE: NoEvidence = NoEvidence;

    struct NoResolver;
    impl Resolver for NoResolver {
        fn resolve(
            &self,
            _target: ego_core::permit::NetworkTarget,
            _deadline: Instant,
        ) -> BoxFuture<'static, Result<ego_core::permit::DnsResolution, TransportError>> {
            Box::pin(future::ready(Err(TransportError::definitely_unsent(
                TransportFailure::Offline,
            ))))
        }
    }
    struct NoHop;
    impl OneHop for NoHop {
        fn begin_call(&self) -> std::time::Instant {
            std::time::Instant::now() + std::time::Duration::from_secs(60)
        }
        fn execute(
            &self,
            _connect: ResolvedConnect,
            _request: HttpRequest,
            _deadline: Instant,
        ) -> BoxFuture<'static, Result<ego_core::permit::OneHopResponse, TransportError>> {
            Box::pin(future::ready(Err(TransportError::definitely_unsent(
                TransportFailure::Offline,
            ))))
        }
    }
    struct NoDuplex;
    impl DuplexSpawner for NoDuplex {
        fn spawn(
            &self,
            _spec: SpawnSpec,
        ) -> BoxFuture<'static, Result<DuplexHandle, TransportError>> {
            Box::pin(future::ready(Err(TransportError::definitely_unsent(
                TransportFailure::Offline,
            ))))
        }
    }
    fn guarded<'a>(granted: Authorized, fs: &'a dyn Fs) -> GuardedEffects<'a> {
        let (_, permit) = granted.into_parts();
        GuardedEffects::new(
            permit,
            fs,
            &NO_PROCESS,
            &NO_EVIDENCE,
            Arc::new(NoResolver),
            Arc::new(NoHop),
            Arc::new(NoDuplex),
        )
    }

    // Catches: cancellation reporting success without reaching the process owner,
    // or accepting inspect authority / another process ID as cancel authority.
    #[test]
    fn background_cancel_requires_exact_authority_and_reaches_the_owner() {
        use ego_core::event::id::DetachedExecId;
        use ego_core::permit::SessionProcessAction;
        use std::sync::Mutex;

        #[derive(Default)]
        struct Owner(Mutex<Vec<DetachedExecId>>);
        impl ProcessExecutor for Owner {
            fn execute<'a>(
                &'a self,
                _: &'a SpawnSpec,
            ) -> BoxFuture<'a, Result<ToolResult, ProcessError>> {
                unreachable!("cancellation never spawns")
            }
            fn cancel_background(&self, id: DetachedExecId) -> Result<(), ProcessError> {
                self.0.lock().unwrap().push(id);
                Err(ProcessError::UnknownBackground { id })
            }
        }
        let id = DetachedExecId::new();
        let other = DetachedExecId::new();
        let owner = Owner::default();
        let fs = FakeFs::default();
        let policy = PolicyEngine::new(CanonPath::new("/ws"), Mode::Yolo, Ruleset::default());
        for (action, requested, reaches_owner) in [
            (SessionProcessAction::Inspect, id, false),
            (SessionProcessAction::Cancel, other, false),
            (SessionProcessAction::Cancel, id, true),
        ] {
            let call = PreparedCall::new("exec_cancel", Op::SessionProcess { id, action });
            let Classification::Settled(Ok(granted)) = policy.classify(call) else {
                panic!("session process control must be authorized");
            };
            let (_, permit) = granted.into_parts();
            let io = GuardedEffects::new(
                permit,
                &fs,
                &owner,
                &NO_EVIDENCE,
                Arc::new(NoResolver),
                Arc::new(NoHop),
                Arc::new(NoDuplex),
            );
            let result = io.background_cancel(requested);
            if reaches_owner {
                assert!(
                    matches!(result, Err(ProcessError::UnknownBackground { id: actual }) if actual == id)
                );
                assert_eq!(*owner.0.lock().unwrap(), vec![id]);
            } else {
                assert!(matches!(result, Err(ProcessError::Invariant(_))));
                assert!(owner.0.lock().unwrap().is_empty());
            }
        }
    }

    // Catches: a GuardedIo implementation without an owner claiming it cancelled a job.
    #[test]
    fn background_cancel_without_an_owner_fails_closed() {
        use ego_core::permit::{Found, LineMatcher, McpHttpClaim, OneHopResponse};
        struct NoOwner;
        impl<'s> GuardedIo<'s> for NoOwner {
            fn write(&self, _: &CanonPath, _: &[u8]) -> Result<(), IoDenied> {
                unreachable!()
            }
            fn read(&self, _: &CanonPath) -> Result<Vec<u8>, IoDenied> {
                unreachable!()
            }
            fn list(&self, _: &ScanRequest) -> Result<Scan, IoDenied> {
                unreachable!()
            }
            fn search(&self, _: &ScanRequest, _: &dyn LineMatcher) -> Result<Found, IoDenied> {
                unreachable!()
            }
            fn exec<'a>(
                &'a self,
                _: &'a SpawnSpec,
            ) -> BoxFuture<'a, Result<ToolResult, ProcessError>> {
                unreachable!()
            }
            fn read_evidence<'a>(
                &'a self,
                _: &'a EvidenceRead,
            ) -> BoxFuture<'a, Result<EvidencePage, ToolError>> {
                unreachable!()
            }
            fn guarded_http(
                &self,
                _: PreparedCall,
                _: HttpRequest,
            ) -> BoxFuture<'static, Result<OneHopResponse, TransportError>> {
                unreachable!()
            }
            fn claim_mcp_http(&self, _: PreparedCall) -> Result<McpHttpClaim, TransportError> {
                unreachable!()
            }
            fn guarded_duplex(
                &self,
                _: PreparedCall,
            ) -> BoxFuture<'static, Result<DuplexHandle, TransportError>> {
                unreachable!()
            }
            fn store_evidence(
                &self,
                _: &[u8],
            ) -> Result<ego_core::store::evidence::EvidenceRef, ToolError> {
                unreachable!()
            }
        }
        let result = NoOwner.background_cancel(ego_core::event::id::DetachedExecId::new());
        assert!(
            matches!(result, Err(ProcessError::Invariant(message)) if message == "guarded I/O has no background process owner")
        );
    }

    fn prepare_ctx<'a>(fs: &'a FakeFs) -> PrepareCtx<'a> {
        PrepareCtx {
            cwd: CanonPath::new("/ws"),
            resolver: fs,
        }
    }

    #[test]
    fn permit_invalid_params_become_tool_error() {
        let fs = FakeFs::default();
        let tool = Typed(FakeWriteTool);
        let ctx = prepare_ctx(&fs);
        let err = tool.prepare(json!({ "path": 123 }), &ctx).unwrap_err();
        assert!(matches!(err, ToolError::InvalidParams(_)));
    }

    #[test]
    fn permit_typed_descriptor_exposes_schemars_schema() {
        let tool = Typed(FakeWriteTool);
        let descriptor = tool.descriptor();
        assert_eq!(descriptor.name, "fake_write");
        let properties = descriptor.schema.as_value()["properties"]
            .as_object()
            .unwrap();
        assert!(properties.contains_key("path"));
        assert!(properties.contains_key("contents"));
    }

    // --- policy: what a `PreparedCall` is actually authorized to do ---

    /// The engine an operator gets when they configure nothing: `default`
    /// mode, no rules of their own.
    ///
    /// Every test below that does not name a mode is asserting about *that*
    /// operator, which is why the helper hardcodes the default rather than
    /// taking a mode: a test that had to spell out `Mode::Default` could be
    /// changed to spell out something else, and the claim "this is what you
    /// get out of the box" would quietly stop being tested.
    fn in_default_mode(root: &str) -> PolicyEngine {
        PolicyEngine::new(CanonPath::new(root), Mode::Default, Ruleset::default())
    }

    /// The answer, for a call the run settles without a human.
    ///
    /// Runs the whole order: `classify` for steps 0 to 3, then `settle` for
    /// step 5's mode ceiling. Both, because since the ceiling became the
    /// `Approver` port neither half is the answer on its own — `classify`
    /// leaves an ordinary read open, and the mode is what closes it.
    ///
    /// Lives in the test rather than on `PolicyEngine` because a caller that
    /// cannot handle a question still being open is a test and never the loop
    /// — that outcome is the loop's whole job, and a library method that
    /// flattened it away would be there for something to accidentally call.
    ///
    /// `block_on` rather than an async test: nothing in the order awaits I/O,
    /// and a runtime dependency in `ego-core`'s tests to drive a future that
    /// is ready on construction would be a dependency on nothing.
    type SettledResult = Result<Authorized, Box<PolicyDenied>>;

    trait Settles {
        fn classify_settled(&self, call: PreparedCall) -> SettledResult;
    }

    impl Settles for PolicyEngine {
        fn classify_settled(&self, call: PreparedCall) -> SettledResult {
            let unanswered = |pending: &Pending| {
                panic!(
                    "expected an answer, got a question for the operator: {}",
                    pending.request()
                )
            };
            let pending = match self.classify(call) {
                Classification::Settled(answer) => return answer.map_err(Box::new),
                Classification::Ask(pending) => pending,
            };
            match block_on(self.settle(pending)) {
                Resolution::Settled(answer) => answer.map_err(Box::new),
                Resolution::Ask(pending) => unanswered(&pending),
                Resolution::HumanSettled { .. } | Resolution::Reclassify { .. } => {
                    unreachable!("this engine has no human seat")
                }
            }
        }
    }

    fn read_call(path: &str) -> PreparedCall {
        PreparedCall::new(
            "read",
            Op::Read {
                path: CanonPath::new(path),
            },
        )
    }

    #[test]
    fn permit_policy_admits_a_read_inside_the_workspace() {
        let engine = in_default_mode("/ws");
        let granted = engine
            .classify_settled(read_call("/ws/src/main.rs"))
            .expect("a workspace read is what this policy exists to allow");
        let permit = granted.permit();
        assert!(
            permit
                .allows_read(&CanonPath::new("/ws/src/main.rs"))
                .is_ok()
        );
    }

    #[test]
    fn permit_policy_grants_only_the_paths_the_call_named() {
        // The whole reason `authorize` exists rather than the loop calling
        // `mint`: a permit must not be wider than the call it authorises.
        let engine = in_default_mode("/ws");
        let granted = engine
            .classify_settled(read_call("/ws/src/main.rs"))
            .expect("a workspace read is allowed");
        let permit = granted.permit();
        assert_eq!(
            permit.allows_read(&CanonPath::new("/ws/secrets.env")),
            Err(PermitDenied::Read {
                path: CanonPath::new("/ws/secrets.env")
            }),
            "reading one file must not authorise reading the workspace"
        );
    }

    fn write_call(path: &str) -> PreparedCall {
        PreparedCall::new(
            "edit",
            Op::Write {
                path: CanonPath::new(path),
                change: Change::Whole(b"pwned".to_vec()),
            },
        )
    }

    #[test]
    fn permit_policy_refuses_a_write_in_plan_mode_and_names_it() {
        // This assertion used to hold in every mode, because the engine had
        // one hardcoded rule saying writes are refused. What refuses now is
        // the mode floor, so the same call gets a different answer under
        // `edits` — and *that* is the property worth pinning: `plan` is the
        // mode where a write cannot happen, not the build.
        let engine = PolicyEngine::new(CanonPath::new("/ws"), Mode::Plan, Ruleset::default());
        let denied = engine
            .classify_settled(write_call("/ws/src/main.rs"))
            .expect_err("plan mode changes nothing");
        assert_eq!(
            &*denied,
            &PolicyDenied::ModeForbids {
                tool: "edit".to_string(),
                mode: Mode::Plan,
                op: OpName::Write,
                path: CanonPath::new("/ws/src/main.rs"),
            }
        );
        assert!(denied.to_string().contains("/ws/src/main.rs"));
        assert!(
            denied.to_string().contains("plan"),
            "a refusal by the mode must name the mode, or the operator has \
             nothing to go and change: {denied}"
        );
    }

    #[test]
    fn permit_policy_asks_about_a_write_in_default_mode() {
        // The other half of the old test's claim. `default` does not refuse a
        // write and does not perform one either: it has no answer, which is a
        // third outcome and not a quieter denial.
        let engine = in_default_mode("/ws");
        let Classification::Ask(pending) = engine.classify(write_call("/ws/src/main.rs")) else {
            panic!("default mode has no rule covering a write, so it must ask");
        };
        assert_eq!(pending.request().tool(), "edit");
        assert_eq!(
            pending.request().why(),
            &Escalation::NoRuleCovers {
                op: OpName::Write,
                path: CanonPath::new("/ws/src/main.rs"),
            }
        );
        assert_eq!(pending.call().ops(), write_call("/ws/src/main.rs").ops());
    }

    #[test]
    fn permit_policy_refuses_a_read_outside_the_workspace_and_names_it() {
        let engine = in_default_mode("/ws");
        let denied = engine
            .classify_settled(read_call("/etc/passwd"))
            .expect_err("the workspace is the boundary");
        assert_eq!(
            &*denied,
            &PolicyDenied::OutsideWorkspace {
                tool: "read".to_string(),
                op: OpName::Read,
                path: CanonPath::new("/etc/passwd"),
            }
        );
        assert!(denied.to_string().contains("/etc/passwd"));
    }

    /// regression. Plausible bug: a bare "outside the workspace" refusal
    /// leaves the model and the operator without the grant that lifts it, so
    /// the model falls back to `cat` under a perimeter the read tool does not
    /// share. A write refusal must not suggest a read grant.
    #[test]
    fn a_refused_read_outside_the_workspace_names_the_grant_and_the_resume_step() {
        let outside = |op| PolicyDenied::OutsideWorkspace {
            tool: "t".to_string(),
            op,
            path: CanonPath::new("/sibling/a b.md"),
        };
        for op in [OpName::Read, OpName::List, OpName::Search] {
            let text = outside(op).to_string();
            assert!(
                text.contains("`ego roots add --access read '/sibling/a b.md'`")
                    && text.contains("exit and resume"),
                "{op}: {text}"
            );
        }
        // writes need an explicit write grant, not silence or a read grant.
        let write = outside(OpName::Write).to_string();
        assert!(
            write.contains("`ego roots add --access read-write '/sibling/a b.md'`")
                && write.contains("exit and resume"),
            "{write}"
        );
        assert!(!write.contains("--access read '"), "{write}");
    }

    ///. Catches: a pasted read-root hint splits a path or executes
    /// shell syntax instead of passing the exact path as one argument.
    #[cfg(unix)]
    #[test]
    fn outside_read_roots_hint_round_trips_special_paths() {
        let engine = in_default_mode("/ws");
        let mut failures = Vec::new();
        for path in [
            "/sibling/plain.md",
            "/sibling/a b.md",
            "/sibling/owner's file; echo unsafe.md",
        ] {
            let denied = engine.classify_settled(read_call(path)).unwrap_err();
            let text = denied.to_string();
            let Some(command) = text.split('`').nth(1) else {
                failures.push(format!("missing code-delimited command: {text}"));
                continue;
            };
            let output = std::process::Command::new("sh")
                .arg("-c")
                .arg(format!("ego() {{ printf '%s\\n' \"$@\"; }}; {command}"))
                .output()
                .expect("run the hinted command through a real shell");
            let expected = format!("roots\nadd\n--access\nread\n{path}\n");
            if !output.status.success()
                || output.stdout != expected.as_bytes()
                || !output.stderr.is_empty()
                || !text.contains("exit and resume")
            {
                failures.push(format!("{text}: {output:?}"));
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    ///. Catches: a quoted filename turns the suggested write-root
    /// command into shell syntax, or credential writes receive an unsafe grant hint.
    #[test]
    fn outside_write_recovery_quotes_the_exact_path_and_omits_credentials() {
        let path = CanonPath::new("/sibling/owner's file; echo unsafe.rs");
        let engine = PolicyEngine::new(CanonPath::new("/ws"), Mode::Yolo, Ruleset::default());
        let denied = engine
            .classify_settled(write_call(path.as_path().to_str().unwrap()))
            .expect_err("the write remains outside admitted roots");
        let text = denied.to_string();
        assert!(
            text.contains(
                "`ego roots add --access read-write '/sibling/owner'\\''s file; echo unsafe.rs'`"
            ) && text.contains("ask the user")
                && text.contains("exit and resume"),
            "{text}"
        );
        for path in ["/sibling/.env", "/home/u/.ssh/id_rsa"] {
            let denied = engine.classify_settled(write_call(path)).unwrap_err();
            assert!(!denied.to_string().contains("roots add"), "{denied}");
        }
    }

    /// regression. Plausible bug: the grant hint is attached to every refused
    /// read outside the workspace, so a model that tried `~/.ssh/id_rsa` or an
    /// `.env` is handed a ready-made `ego roots add` for a credential the
    /// credential defaults keep denied anyway.
    #[test]
    fn a_refused_credential_read_outside_the_workspace_does_not_offer_a_grant() {
        let engine = in_default_mode("/ws");
        for path in [
            "/home/u/.ssh/id_rsa",
            "/home/u/.aws/credentials",
            "/sibling/.env",
            "/home/u/.netrc",
        ] {
            let denied = engine
                .classify_settled(read_call(path))
                .expect_err("the workspace is the boundary");
            let text = denied.to_string();
            assert!(text.contains(path), "{text}");
            assert!(!text.contains("roots add"), "{text}");
        }
    }

    /// Critic regression. Plausible bug: the exception for a refused read drops
    /// `list` or `search` (a model listing `~/.codex` still ends the run), or
    /// widens to a mutating or spawning op outside the workspace.
    #[test]
    fn only_a_boundary_refusal_of_a_read_list_or_search_goes_back_to_the_model() {
        let outside = |op| PolicyDenied::OutsideWorkspace {
            tool: "t".to_string(),
            op,
            path: CanonPath::new("/etc"),
        };
        for op in [OpName::Read, OpName::List, OpName::Search] {
            assert!(outside(op).is_unreachable_read(), "{op} is a read");
        }
        for op in [
            OpName::Write,
            OpName::Spawn,
            OpName::ReadEvidence,
            OpName::Opaque,
            OpName::Connect,
            OpName::SecretEgress,
        ] {
            assert!(!outside(op).is_unreachable_read(), "{op} is not a read");
        }
        // A read refused for any other reason is somebody's decision, not
        // the boundary's.
        let mode = PolicyDenied::ModeForbids {
            tool: "t".to_string(),
            mode: Mode::Plan,
            op: OpName::Read,
            path: CanonPath::new("/ws/a"),
        };
        assert!(!mode.is_unreachable_read());
        let root = PolicyDenied::ReadOnlyRoot {
            tool: "t".to_string(),
            op: OpName::Read,
            path: CanonPath::new("/ws/a"),
        };
        assert!(!root.is_unreachable_read());
    }

    // --- the evaluation order ---------------------------------------------
    //
    // Decision order is part of the security contract. Pairwise cases check
    // which step wins, not only each step's isolated result.

    /// An engine in `mode`, with those rules, over `/ws`.
    fn engine(mode: Mode, allow: &[&str], deny: &[&str]) -> PolicyEngine {
        let load = |raws: &[&str]| raws.iter().map(|raw| rule(raw)).collect();
        PolicyEngine::new(
            CanonPath::new("/ws"),
            mode,
            // `Project` because these tests are about the evaluation order,
            // not about attribution: an operator wrote these rules and can
            // edit them. The system layer's authority is tested where it
            // belongs, against `resolve`, in `tests/config.rs`.
            Ruleset::from_layer(
                PermCfg {
                    judge: None,
                    allow: load(allow),
                    deny: load(deny),
                },
                Source::Project,
            ),
        )
    }

    /// These engine tests parse rule syntax independently of producer availability. They
    /// exercise real mutating prepared operations and plan/allow ordering, not the registry's
    /// separate enforceability check.
    fn rule(raw: &str) -> Rule {
        Rule::parse(raw).unwrap_or_else(|err| panic!("{raw} is a sentence in the grammar: {err}"))
    }

    #[test]
    fn permit_step0_the_workspace_boundary_is_not_lifted_by_yolo() {
        // The bypass mode bypasses *asking*.
        let engine = engine(Mode::Yolo, &["read(**)"], &[]);
        let denied = engine
            .classify_settled(read_call("/etc/passwd"))
            .expect_err("yolo does not move the workspace");
        assert!(matches!(*denied, PolicyDenied::OutsideWorkspace { .. }));
    }

    #[test]
    fn permit_step1_a_deny_beats_an_allow_the_old_string_match_would_have_kept() {
        // The engine half of the property `tests/config.rs`'s
        // `config_system_deny_survives_a_higher_precedence_layer_that_clears_it`
        // used to own under the name `config_system_deny_ceiling_cannot_be_
        // relaxed_by_flags`. Resolution guarantees the system deny survives
        // the merge; *this* is where it beats the allow sitting beside it.
        //
        // The allow is deliberately narrower than the deny rather than equal
        // to it. `resolve` once ended with `allow.retain(|e|
        // !deny.contains(e))`, which is string equality: `write(src/**)`
        // survived `deny = ["write(**)"]` untouched and the call went through.
        let engine = engine(Mode::Edits, &["write(src/**)"], &["write(**)"]);
        let denied = engine
            .classify_settled(write_call("/ws/src/main.rs"))
            .expect_err("the deny covers it and no allow may relax one");
        assert_eq!(
            &*denied,
            &PolicyDenied::Denied {
                tool: "edit".to_string(),
                op: OpName::Write,
                path: CanonPath::new("/ws/src/main.rs"),
                by: rule("write(**)").id(),
                // `engine` builds its ruleset from one layer, so one origin
                // and no subordinate clause.
                from: Source::Project,
                also: OriginSet::default(),
            }
        );
        assert!(
            denied
                .to_string()
                .contains(&rule("write(**)").id().to_string()),
            "a denial must name the rule, or an operator greps forty lines of \
             config to find which one did it: {denied}"
        );
    }

    #[test]
    fn permit_step1_a_deny_beats_the_bypass_mode() {
        // `yolo` is step 4 and deny is step 1, so this is settled by the
        // order alone. Worth pinning because the reading that puts bypass
        // first is the intuitive one, and it would make every deny an
        // operator wrote conditional on nobody passing a flag.
        let engine = engine(Mode::Yolo, &[], &["read(secrets/**)"]);
        let denied = engine
            .classify_settled(read_call("/ws/secrets/prod.env"))
            .expect_err("a deny is not a suggestion");
        assert!(matches!(*denied, PolicyDenied::Denied { .. }));
    }

    #[test]
    fn permit_step2_the_plan_floor_is_above_the_allow_list() {
        // The floor at step 2 and the allow at step 3. A `plan` run that an
        // allow rule could talk into writing would not mean what `plan`
        // means, and the operator who set the mode is not always the one who
        // wrote the rules.
        let engine = engine(Mode::Plan, &["write(**)"], &[]);
        let denied = engine
            .classify_settled(write_call("/ws/src/main.rs"))
            .expect_err("plan changes nothing, whatever the rules say");
        assert!(matches!(
            *denied,
            PolicyDenied::ModeForbids {
                mode: Mode::Plan,
                op: OpName::Write,
                ..
            }
        ));
    }

    #[test]
    fn permit_step2_the_plan_floor_lets_a_read_through() {
        // The floor refuses what *changes* something, not everything. A plan
        // run that could not read would be a plan run that cannot plan.
        let engine = engine(Mode::Plan, &[], &[]);
        engine
            .classify_settled(read_call("/ws/src/main.rs"))
            .expect("planning is reading");
    }

    #[test]
    fn permit_step3_an_allow_rule_settles_what_the_mode_would_have_asked_about() {
        let engine = engine(Mode::Default, &["write(src/**)"], &[]);
        let granted = engine
            .classify_settled(write_call("/ws/src/main.rs"))
            .expect("the operator allowed exactly this");
        assert!(
            granted
                .permit()
                .allows_write(&CanonPath::new("/ws/src/main.rs"))
                .is_ok()
        );
    }

    #[test]
    fn permit_step3_an_allow_that_matches_grants_only_the_call_and_not_the_rule() {
        // Grant scope follows the call's one file, not the broader pattern of its matching
        // rule. An allow is not standing authority over every pattern match.
        let engine = engine(Mode::Default, &["write(src/**)"], &[]);
        let granted = engine
            .classify_settled(write_call("/ws/src/main.rs"))
            .expect("the operator allowed exactly this");
        assert_eq!(
            granted
                .permit()
                .allows_write(&CanonPath::new("/ws/src/other.rs")),
            Err(PermitDenied::Write {
                path: CanonPath::new("/ws/src/other.rs")
            })
        );
    }

    #[test]
    fn permit_step3_an_allow_that_does_not_cover_the_path_still_asks() {
        let engine = engine(Mode::Default, &["write(src/**)"], &[]);
        let Classification::Ask(pending) = engine.classify(write_call("/ws/docs/README.md")) else {
            panic!("no rule covers docs/, so nobody here has answered");
        };
        assert_eq!(
            pending.request().why(),
            &Escalation::NoRuleCovers {
                op: OpName::Write,
                path: CanonPath::new("/ws/docs/README.md"),
            }
        );
    }

    /// The mode axis, as one table. Read down the right-hand column: this is
    /// the whole of what choosing a mode buys, for the one operation the
    /// modes disagree about.
    ///
    /// Runs the **whole order**, `classify` then `settle`, and it has to.
    /// Step 5 is the `Approver` port now, so `classify` alone answers `Asked`
    /// for all five rows and this table would read as though the modes agreed
    /// — a green test asserting the opposite of what it is named for. The
    /// subject did not change; where the subject lives did.
    #[test]
    fn permit_step5_the_ceiling_answers_a_write_differently_in_every_mode() {
        #[derive(Debug, PartialEq)]
        enum Answer {
            Authorized,
            Refused,
            Asked,
        }

        let cases = [
            (Mode::Plan, Answer::Refused),
            (Mode::Default, Answer::Asked),
            // The deterministic bounded native write settles after the mode
            // seat without consulting the optional judge.
            (Mode::Auto, Answer::Authorized),
            (Mode::Edits, Answer::Authorized),
            (Mode::Yolo, Answer::Authorized),
        ];

        for (mode, expected) in cases {
            let engine = engine(mode, &[], &[]);
            let answer = match engine.classify(write_call("/ws/src/main.rs")) {
                Classification::Settled(Ok(_)) => Answer::Authorized,
                Classification::Settled(Err(_)) => Answer::Refused,
                Classification::Ask(pending) => match block_on(engine.settle(pending)) {
                    Resolution::Settled(Ok(_)) => Answer::Authorized,
                    Resolution::Settled(Err(_)) => Answer::Refused,
                    Resolution::Ask(_) => Answer::Asked,
                    Resolution::HumanSettled { .. } | Resolution::Reclassify { .. } => {
                        unreachable!("this engine has no human seat")
                    }
                },
            };
            assert_eq!(answer, expected, "{mode} mode, writing inside /ws");
        }
    }

    /// The mode that declined says so in its own words, and they are not the
    /// words `classify` used.
    ///
    /// Without this, `settle`'s escalation could hand back the `Pending` it
    /// was given and every assertion above would still pass — the run stops
    /// either way. What would be lost is the only thing the operator gets:
    /// "no rule covers write /ws/src/main.rs" tells them to write a rule,
    /// which is one of the two available remedies. "default mode does not
    /// approve it on its own" tells them the other one exists.
    #[test]
    fn permit_step5_a_mode_that_declines_replaces_the_reason_with_its_own() {
        let engine = engine(Mode::Default, &[], &[]);
        let Classification::Ask(pending) = engine.classify(write_call("/ws/src/main.rs")) else {
            panic!("no rule covers it, so step 3 cannot settle it");
        };
        assert_eq!(
            pending.request().why(),
            &Escalation::NoRuleCovers {
                op: OpName::Write,
                path: CanonPath::new("/ws/src/main.rs"),
            },
            "steps 0-3 ran and none of them covered it"
        );

        let Resolution::Ask(pending) = block_on(engine.settle(pending)) else {
            panic!("default mode does not auto-approve a write");
        };
        assert_eq!(
            pending.request().why(),
            &Escalation::ModeAsks {
                mode: Mode::Default,
                op: OpName::Write,
                path: CanonPath::new("/ws/src/main.rs"),
            },
            "and the mode declined too, which is the reason that stopped the run"
        );
    }

    /// A refusal anywhere in a call refuses the call, and it must not be
    /// pre-empted by a *question* about an earlier operation.
    /// Two-pass classification must check every hard boundary before asking
    /// about any operation. Otherwise an ordinary first write could hide an
    /// out-of-scope second read and approval would grant the entire call, even
    /// in yolo. The assertion covers the complete call, not isolated operations.
    #[test]
    fn permit_step0_a_question_about_one_op_cannot_pre_empt_a_refusal_of_another() {
        let engine = engine(Mode::Yolo, &[], &[]);
        let call = PreparedCall::new(
            "write",
            Op::Write {
                path: CanonPath::new("/ws/src/main.rs"),
                change: Change::Whole(b"harmless".to_vec()),
            },
        )
        .with_op(Op::Read {
            path: CanonPath::new("/etc/shadow"),
        });

        let Classification::Settled(Err(denied)) = engine.classify(call) else {
            panic!("the second op is outside the workspace, and yolo does not lift that");
        };
        assert_eq!(
            denied,
            PolicyDenied::OutsideWorkspace {
                tool: "write".to_string(),
                op: OpName::Read,
                path: CanonPath::new("/etc/shadow"),
            }
        );
    }

    #[test]
    fn permit_step4_no_mode_asks_about_a_read_inside_the_workspace() {
        // A mode that stopped the run to ask about reading a file in the workspace it was
        // pointed at is how an operator learns to approve without looking.
        for mode in [
            Mode::Plan,
            Mode::Default,
            Mode::Auto,
            Mode::Edits,
            Mode::Yolo,
        ] {
            let engine = engine(mode, &[], &[]);
            engine
                .classify_settled(read_call("/ws/src/main.rs"))
                .unwrap_or_else(|err| panic!("{mode} mode refused a workspace read: {err}"));
        }
    }

    #[test]
    fn permit_yolo_secret_egress_names_mode_while_rule_covered_read_names_rules() {
        let target = NetworkTarget::new("example.com", 443).unwrap();
        let route = HttpRoute::new(HttpScheme::Https, target).unwrap();
        let request = HttpRequest::get("/secret").unwrap();
        let hook_call = PreparedCall::hook_http(
            HookId::parse(&format!("h1:{}", "00".repeat(32))).unwrap(),
            route,
            &request,
            vec!["TOKEN".to_owned()],
        );
        let secret_op = hook_call
            .ops()
            .iter()
            .find(|op| matches!(op, Op::SecretEgress { .. }))
            .cloned()
            .expect("hook preparation carries one SecretEgress operation");
        let secret_call = PreparedCall::new("secret-egress", secret_op);
        let yolo = engine(Mode::Yolo, &[], &[]);
        let secret = yolo
            .classify_settled(secret_call)
            .expect("yolo settles SecretEgress without a matching rule");
        assert_eq!(secret.by(), &Authority::Mode { mode: Mode::Yolo });

        let ruled = engine(Mode::Yolo, &["read(**)"], &[])
            .classify_settled(read_call("/ws/src/main.rs"))
            .expect("a matching rule settles an ordinary read");
        assert!(matches!(ruled.by(), Authority::Rules { rules } if !rules.is_empty()));
    }

    #[test]
    fn permit_yolo_mixed_hook_http_names_mode_even_when_network_rule_covers_connect() {
        let target = NetworkTarget::new("example.com", 443).unwrap();
        let route = HttpRoute::new(HttpScheme::Https, target).unwrap();
        let request = HttpRequest::get("/secret").unwrap();
        let call = PreparedCall::hook_http(
            HookId::parse(&format!("h1:{}", "00".repeat(32))).unwrap(),
            route,
            &request,
            vec!["TOKEN".to_owned()],
        );
        let yolo = engine(Mode::Yolo, &["net(example.com:443)"], &[]);
        let granted = yolo
            .classify_settled(call)
            .expect("yolo settles the mixed hook call");

        assert_eq!(granted.by(), &Authority::Mode { mode: Mode::Yolo });
    }

    #[test]
    fn permit_the_order_is_per_operation_and_the_first_unanswered_one_stops_it() {
        // A call with three ops is three questions, and the answer to the
        // call is the first one that is not "yes". Here op two is denied and
        // op three would have been asked about: a loop that answered the
        // *last* question, or that collected them, would let the tool run
        // under a denial it had already been given.
        let engine = engine(Mode::Default, &["read(**)"], &["read(secrets/**)"]);
        let call = PreparedCall::new(
            "read",
            Op::Read {
                path: CanonPath::new("/ws/src/main.rs"),
            },
        )
        .with_op(Op::Read {
            path: CanonPath::new("/ws/secrets/prod.env"),
        })
        .with_op(Op::Write {
            path: CanonPath::new("/ws/src/main.rs"),
            change: Change::Whole(b"pwned".to_vec()),
        });
        let denied = engine
            .classify_settled(call)
            .expect_err("op two is denied, and that ends the call");
        assert_eq!(
            &*denied,
            &PolicyDenied::Denied {
                tool: "read".to_string(),
                op: OpName::Read,
                path: CanonPath::new("/ws/secrets/prod.env"),
                by: rule("read(secrets/**)").id(),
                // `engine` builds its ruleset from one layer, so one origin
                // and no subordinate clause.
                from: Source::Project,
                also: OriginSet::default(),
            }
        );
    }

    // --- denial attribution and how the operator can lift it ---

    /// An engine denying `raw`, with that rule carried by `origins`.
    fn denying(raw: &str, origins: &[Source]) -> PolicyEngine {
        let (first, rest) = origins.split_first().expect("at least one origin");
        let mut ruleset = Ruleset::from_layer(
            PermCfg {
                judge: None,
                allow: vec![rule("read(**)")],
                deny: vec![rule(raw)],
            },
            *first,
        );
        for origin in rest {
            Ruleset::merge(&mut ruleset.deny, rule(raw), *origin);
        }
        PolicyEngine::new(CanonPath::new("/ws"), Mode::Default, ruleset)
    }

    fn refusal(engine: &PolicyEngine, path: &str) -> String {
        engine
            .classify_settled(read_call(path))
            .expect_err("the rule denies this read")
            .to_string()
    }

    /// A deny the operator wrote is theirs to delete, and the message says so
    /// by naming their layer. One origin, so it reads as one answer — no list
    /// of length one (decision 5a).
    #[test]
    fn permit_a_denial_names_the_layer_that_wrote_it() {
        let denied = refusal(
            &denying("read(secrets/**)", &[Source::User]),
            "/ws/secrets/prod.env",
        );
        assert!(denied.contains("user config"), "{denied}");
        assert!(
            !denied.contains("denies it too"),
            "one origin must not render a subordinate clause: {denied}"
        );
    }

    /// The fact an operator most needs and least expects. Every other layer is
    /// theirs to edit, so the default assumption is that this one is too —
    /// and a message that merely named the layer would leave them to infer
    /// that `--allow` is not a way around it.
    #[test]
    fn permit_a_system_denial_says_that_nothing_lifts_it() {
        let denied = refusal(
            &denying("read(secrets/**)", &[Source::System]),
            "/ws/secrets/prod.env",
        );
        assert!(denied.contains("system config"), "{denied}");
        assert!(
            denied.contains("--allow"),
            "a system deny must say the flag will not lift it, because that is \
             the remedy an operator reaches for first: {denied}"
        );
    }

    /// Exercise denial attribution end to end.
    ///
    /// The rule is denied in both `/etc/ego` and the operator's own config.
    /// The old code attributed it to whichever layer the merge loop left —
    /// the user's — and sent them to delete a line that changes nothing. The
    /// message must lead with the system layer and say that their own copy is
    /// redundant, or they will delete it, re-run, and be denied again by a
    /// copy nothing has mentioned.
    #[test]
    fn permit_a_denial_written_in_two_layers_leads_with_the_one_that_binds() {
        let denied = refusal(
            &denying("read(secrets/**)", &[Source::User, Source::System]),
            "/ws/secrets/prod.env",
        );
        assert!(
            denied.contains("system config"),
            "the system layer is the one that binds: {denied}"
        );
        assert!(
            denied.contains("user config denies it too"),
            "and their own copy must be named as redundant rather than \
             silently dropped: {denied}"
        );
        let system = denied.find("system config").expect("system is named");
        let user = denied.find("user config").expect("user is named");
        assert!(
            system < user,
            "decision 5a: the winner leads, the rest is subordinate: {denied}"
        );
    }

    #[test]
    fn permit_policy_refuses_a_whole_call_for_one_bad_op() {
        // A call is authorised or it is not. Minting a permit for the
        // acceptable half would let a tool do part of something nobody
        // approved.
        let engine = in_default_mode("/ws");
        let call = PreparedCall::new(
            "read",
            Op::Read {
                path: CanonPath::new("/ws/fine.txt"),
            },
        )
        .with_op(Op::Read {
            path: CanonPath::new("/etc/shadow"),
        });
        assert!(engine.classify_settled(call).is_err());
    }

    // --- listing and searching: looking without reading -------------------
    //

    fn list_call(root: &str, pattern: &str) -> PreparedCall {
        PreparedCall::new(
            "glob",
            Op::List {
                root: CanonPath::new(root),
                pattern: pattern.to_string(),
            },
        )
    }

    fn search_call(root: &str, query: &str) -> PreparedCall {
        PreparedCall::new(
            "grep",
            Op::Search {
                root: CanonPath::new(root),
                query: query.to_string(),
            },
        )
    }

    /// A workspace with something worth finding and something worth not
    /// leaking.
    fn workspace() -> FakeFs {
        let fs = FakeFs::default();
        fs.file("/ws/src/main.rs", "fn main() { todo!() }");
        fs.file("/ws/src/deep/lib.rs", "// TODO: write this");
        fs.file("/ws/.env", "SECRET=hunter2");
        fs
    }

    #[test]
    fn permit_a_list_grant_does_not_authorise_reading_what_it_found() {
        // The whole reason `list_paths` is not `read_paths`. A glob that can
        // enumerate a directory must not be able to read the files it names,
        // or "list the workspace" quietly means "exfiltrate the workspace".
        let engine = in_default_mode("/ws");
        let granted = engine
            .classify_settled(list_call("/ws/src", "**/*.rs"))
            .expect("listing inside the workspace is allowed");
        let permit = granted.permit();

        assert!(
            permit
                .allows_list(&CanonPath::new("/ws/src/main.rs"))
                .is_ok()
        );
        assert!(matches!(
            permit.allows_read(&CanonPath::new("/ws/src/main.rs")),
            Err(PermitDenied::Read { .. })
        ));
        assert!(matches!(
            permit.allows_search(&CanonPath::new("/ws/src/main.rs")),
            Err(PermitDenied::Search { .. })
        ));
    }

    #[test]
    fn permit_a_read_grant_does_not_authorise_listing_the_directory() {
        // And the converse, which is the one a "read implies list" shortcut
        // would break: being allowed to read a directory says nothing about
        // being allowed to enumerate it.
        //
        // The grant names the directory itself, so "list it" and "read it"
        // are the same path and only the *authority* differs. Granting a file
        // and then asking about its parent would pass even if the two lists
        // were confused, because the parent is not under the file either way
        // — a test that cannot fail for the reason it names.
        let engine = in_default_mode("/ws");
        let granted = engine
            .classify_settled(read_call("/ws/src"))
            .expect("reading inside the workspace is allowed");
        let permit = granted.permit();

        assert!(
            permit
                .allows_read(&CanonPath::new("/ws/src/main.rs"))
                .is_ok()
        );
        assert!(matches!(
            permit.allows_list(&CanonPath::new("/ws/src")),
            Err(PermitDenied::List { .. })
        ));
    }

    #[test]
    fn permit_a_search_grant_does_not_authorise_reading_what_it_searched() {
        // The symmetric half of the list case, and the one that matters most:
        // `Op::Search` exists precisely so that grep does not need read
        // authority over the subtree it searches. If authorising a search also
        // granted `read_paths`, the whole distinction would be decorative.
        let engine = in_default_mode("/ws");
        let granted = engine
            .classify_settled(search_call("/ws/src", "TODO"))
            .expect("searching inside the workspace is allowed");
        let permit = granted.permit();

        assert!(
            permit
                .allows_search(&CanonPath::new("/ws/src/main.rs"))
                .is_ok()
        );
        assert!(matches!(
            permit.allows_read(&CanonPath::new("/ws/src/main.rs")),
            Err(PermitDenied::Read { .. })
        ));
        assert!(matches!(
            permit.allows_list(&CanonPath::new("/ws/src")),
            Err(PermitDenied::List { .. })
        ));
    }

    #[test]
    fn permit_policy_refuses_a_list_outside_the_workspace_and_names_the_operation() {
        let engine = in_default_mode("/ws");
        let denied = engine
            .classify_settled(list_call("/etc", "*"))
            .expect_err("the workspace is the boundary for listing too");
        assert_eq!(
            &*denied,
            &PolicyDenied::OutsideWorkspace {
                tool: "glob".to_string(),
                op: OpName::List,
                path: CanonPath::new("/etc"),
            }
        );
        // "may not list /etc" and "may not read /etc" are different refusals,
        // and the message has to be able to say which.
        assert!(denied.to_string().contains("may not list /etc"));
    }

    #[test]
    fn permit_policy_refuses_a_search_outside_the_workspace_and_names_the_operation() {
        let engine = in_default_mode("/ws");
        let denied = engine
            .classify_settled(search_call("/etc", "password"))
            .expect_err("the workspace is the boundary for searching too");
        assert!(denied.to_string().contains("may not search /etc"));
    }

    #[test]
    fn permit_guarded_list_returns_the_names_under_an_authorised_root() {
        let fs = workspace();
        let engine = in_default_mode("/ws");
        let granted = engine.classify_settled(list_call("/ws/src", "**")).unwrap();
        let guarded = guarded(granted, &fs);

        let scan = guarded
            .list(&ScanRequest::new(CanonPath::new("/ws/src")))
            .expect("the root is inside the permit");

        let mut found: Vec<String> = scan
            .entries
            .iter()
            .filter(|e| e.kind == EntryKind::File)
            .map(|e| e.relative.display().to_string())
            .collect();
        found.sort();
        assert_eq!(found, vec!["deep/lib.rs", "main.rs"]);
        assert!(!scan.truncated);
        // The relative name is what a glob pattern is written against, and it
        // is not the canonical path.
        let main = scan
            .entries
            .iter()
            .find(|e| e.relative == Path::new("main.rs"))
            .unwrap();
        assert_eq!(main.path, CanonPath::new("/ws/src/main.rs"));
    }

    #[test]
    fn permit_guarded_list_refuses_an_unauthorised_root_without_walking_it() {
        // Refused *before* any traversal: filtering afterwards would mean the
        // directory had already been read, and only the report of the denial
        // suppressed.
        let fs = workspace();
        let engine = in_default_mode("/ws");
        let granted = engine.classify_settled(list_call("/ws/src", "**")).unwrap();
        let guarded = guarded(granted, &fs);

        let denied = guarded
            .list(&ScanRequest::new(CanonPath::new("/ws")))
            .expect_err("/ws is not what the permit named");

        assert!(matches!(
            denied,
            IoDenied::Permit(PermitDenied::List { .. })
        ));
        assert_eq!(fs.scans(), 0, "nothing may be walked before the refusal");
    }

    #[test]
    fn permit_guarded_list_fails_the_whole_scan_when_an_entry_escapes() {
        // A symlinked file is resolved and reported under its real target, so
        // an escape is visible to the permit check — and one escapee fails the
        // whole operation rather than being quietly dropped from the results.
        let fs = workspace();
        fs.file("/etc/passwd", "root:x:0:0");
        fs.symlink("/ws/src/innocent.rs", "/etc/passwd");
        let engine = in_default_mode("/ws");
        let granted = engine.classify_settled(list_call("/ws/src", "**")).unwrap();
        let guarded = guarded(granted, &fs);

        let denied = guarded
            .list(&ScanRequest::new(CanonPath::new("/ws/src")))
            .expect_err("one entry left the authorised subtree");

        assert!(matches!(
            denied,
            IoDenied::Permit(PermitDenied::List { ref path }) if path == &CanonPath::new("/etc/passwd")
        ));
    }

    #[test]
    fn permit_a_scan_does_not_descend_into_a_directory_symlink() {
        // The escape the root check cannot see: a walk resolves many paths
        // where a read resolves one.
        let fs = workspace();
        fs.file("/elsewhere/secret.txt", "not yours");
        fs.symlink("/ws/src/out", "/elsewhere");
        let engine = in_default_mode("/ws");
        let granted = engine.classify_settled(list_call("/ws/src", "**")).unwrap();
        let guarded = guarded(granted, &fs);

        let scan = guarded
            .list(&ScanRequest::new(CanonPath::new("/ws/src")))
            .expect("the link is not followed, so nothing escapes to refuse");

        assert!(
            !scan
                .entries
                .iter()
                .any(|e| e.path.as_path().starts_with("/elsewhere")),
            "a directory symlink must not be descended into: {:?}",
            scan.entries
        );
    }

    #[test]
    fn permit_a_scan_that_hits_a_limit_says_so_rather_than_returning_a_short_list() {
        // A model that cannot tell a complete answer from a truncated one
        // concludes the file it needed does not exist.
        let fs = workspace();
        let engine = in_default_mode("/ws");
        let granted = engine.classify_settled(list_call("/ws/src", "**")).unwrap();
        let guarded = guarded(granted, &fs);

        let capped = ScanRequest {
            root: CanonPath::new("/ws/src"),
            skip_dirs: Vec::new(),
            respect_ignore: false,
            limits: ScanLimits {
                max_entries: 1,
                ..ScanLimits::default()
            },
        };
        let scan = guarded.list(&capped).expect("the root is authorised");

        assert_eq!(scan.entries.len(), 1);
        assert!(scan.truncated, "hitting max_entries must be reported");
    }

    #[test]
    fn permit_a_scan_stops_at_max_depth() {
        let fs = workspace();
        let engine = in_default_mode("/ws");
        let granted = engine.classify_settled(list_call("/ws/src", "**")).unwrap();
        let guarded = guarded(granted, &fs);

        let shallow = ScanRequest {
            root: CanonPath::new("/ws/src"),
            skip_dirs: Vec::new(),
            respect_ignore: false,
            limits: ScanLimits {
                max_depth: 0,
                ..ScanLimits::default()
            },
        };
        let scan = guarded.list(&shallow).expect("the root is authorised");

        let files: Vec<String> = scan
            .entries
            .iter()
            .filter(|e| e.kind == EntryKind::File)
            .map(|e| e.relative.display().to_string())
            .collect();
        assert_eq!(files, vec!["main.rs"], "deep/lib.rs is a level too far");
        assert!(scan.truncated);
    }

    #[test]
    fn permit_skip_dirs_excludes_a_directory_but_is_not_a_security_boundary() {
        // Two halves, and the second is the one that matters. `skip_dirs` is
        // a convenience: it keeps `target/` out of a glob's results. It must
        // never be mistaken for containment, so a path under a skipped
        // directory is still refused by the permit exactly like any other.
        let fs = workspace();
        fs.file("/ws/src/target/build.rs", "generated");
        let engine = in_default_mode("/ws");
        let granted = engine.classify_settled(list_call("/ws/src", "**")).unwrap();
        let guarded = guarded(granted, &fs);

        let skipping = ScanRequest {
            root: CanonPath::new("/ws/src"),
            skip_dirs: vec!["target".to_string()],
            respect_ignore: false,
            limits: ScanLimits::default(),
        };
        let scan = guarded.list(&skipping).expect("the root is authorised");
        assert!(
            !scan
                .entries
                .iter()
                .any(|e| e.relative.starts_with("target")),
            "a skipped directory is not descended into: {:?}",
            scan.entries
        );

        // And the permit does not consult the skip list at all: a permit that
        // never named this subtree still refuses it, skipped or not.
        let narrow_grant = engine
            .classify_settled(list_call("/ws/src/deep", "**"))
            .unwrap();
        let narrow = narrow_grant.permit();
        assert!(matches!(
            narrow.allows_list(&CanonPath::new("/ws/src/target/build.rs")),
            Err(PermitDenied::List { .. })
        ));
    }

    #[test]
    fn permit_guarded_search_returns_matching_lines_and_never_the_file() {
        // The point of `Op::Search`: the seam reads, the caller judges, and
        // only the verdicts come back. `grep` never holds the bytes.
        let fs = workspace();
        let engine = in_default_mode("/ws");
        let granted = engine
            .classify_settled(search_call("/ws/src", "TODO"))
            .unwrap();
        let guarded = guarded(granted, &fs);

        let found = guarded
            .search(
                &ScanRequest::new(CanonPath::new("/ws/src")),
                &|line: &str| line.contains("TODO"),
            )
            .expect("the root is inside the permit");

        assert_eq!(found.matches.len(), 1);
        assert_eq!(found.matches[0].path, CanonPath::new("/ws/src/deep/lib.rs"));
        assert_eq!(found.matches[0].line, 1, "line numbers count from one");
        assert_eq!(found.matches[0].text, "// TODO: write this");
        assert!(!found.truncated);
    }

    /// A filesystem that performs the exact mutation behind regression:
    /// the scan observes an ordinary in-scope file, then that name becomes a
    /// symlink to an out-of-scope secret before search opens it.
    struct ScanThenEscapeFs {
        escaped: std::sync::atomic::AtomicBool,
        reads: std::sync::atomic::AtomicUsize,
    }

    impl ScanThenEscapeFs {
        fn new() -> Self {
            Self {
                escaped: std::sync::atomic::AtomicBool::new(false),
                reads: std::sync::atomic::AtomicUsize::new(0),
            }
        }

        fn reads(&self) -> usize {
            self.reads.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    impl PathResolver for ScanThenEscapeFs {
        fn canonicalize(&self, path: &Path) -> Result<CanonPath, FsError> {
            if path == Path::new("/ws/src/innocent.rs")
                && self.escaped.load(std::sync::atomic::Ordering::SeqCst)
            {
                Ok(CanonPath::new("/outside/secret.txt"))
            } else {
                Ok(CanonPath::new(path))
            }
        }
    }

    impl Fs for ScanThenEscapeFs {
        fn read(&self, path: &CanonPath) -> Result<Vec<u8>, FsError> {
            self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if path == &CanonPath::new("/outside/secret.txt") {
                Ok(b"SECRET=not-yours".to_vec())
            } else {
                Ok(b"harmless".to_vec())
            }
        }

        fn write(&self, _path: &CanonPath, _bytes: &[u8]) -> Result<(), FsError> {
            unreachable!("this filesystem exists only to exercise search")
        }

        fn scan(&self, _req: &ScanRequest) -> Result<Scan, FsError> {
            let scan = Scan::new(
                vec![Entry {
                    relative: "innocent.rs".into(),
                    path: CanonPath::new("/ws/src/innocent.rs"),
                    kind: EntryKind::File,
                }],
                false,
            );
            self.escaped
                .store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(scan)
        }
    }

    #[test]
    fn permit_search_revalidates_each_file_after_scan_before_reading_it() {
        let fs = ScanThenEscapeFs::new();
        let engine = in_default_mode("/ws");
        let granted = engine
            .classify_settled(search_call("/ws/src", "SECRET"))
            .unwrap();
        let guarded = guarded(granted, &fs);

        let denied = guarded
            .search(
                &ScanRequest::new(CanonPath::new("/ws/src")),
                &|line: &str| line.contains("SECRET"),
            )
            .expect_err("the scanned file escaped before its contents were opened");

        assert!(matches!(
            denied,
            IoDenied::Permit(PermitDenied::Search { ref path })
                if path == &CanonPath::new("/outside/secret.txt")
        ));
        assert_eq!(
            fs.reads(),
            0,
            "the fresh permit check must happen before the escaped file is read"
        );
    }

    #[test]
    fn permit_guarded_read_tells_a_missing_file_apart_from_a_refused_one() {
        // Criterion 2's whole point. A model told "denied" should stop asking;
        // a model told "not found" should look elsewhere. One collapsed
        // failure produces the wrong next move in both cases.
        let fs = workspace();
        let engine = in_default_mode("/ws");
        let granted = engine.classify_settled(read_call("/ws/src")).unwrap();
        let guarded = guarded(granted, &fs);

        // Authorised, but there is nothing there.
        assert!(matches!(
            guarded.read(&CanonPath::new("/ws/src/missing.rs")),
            Err(IoDenied::Fs(FsError::NotFound { .. }))
        ));
        // Present, and readable by anyone with the authority — which this
        // permit does not have.
        assert!(matches!(
            guarded.read(&CanonPath::new("/ws/.env")),
            Err(IoDenied::Permit(PermitDenied::Read { .. }))
        ));
    }

    #[test]
    fn permit_a_search_that_exhausts_its_byte_budget_says_so() {
        // The same honesty `Scan::truncated` buys for a listing: a search that
        // stopped reading has not searched, and must not report as though it
        // had.
        let fs = workspace();
        let engine = in_default_mode("/ws");
        let granted = engine
            .classify_settled(search_call("/ws/src", "fn"))
            .unwrap();
        let guarded = guarded(granted, &fs);

        let starved = ScanRequest {
            root: CanonPath::new("/ws/src"),
            skip_dirs: Vec::new(),
            respect_ignore: false,
            limits: ScanLimits {
                max_bytes: 1,
                ..ScanLimits::default()
            },
        };
        let found = guarded
            .search(&starved, &|line: &str| line.contains("fn"))
            .expect("the root is authorised; it is the budget that ran out");

        assert!(found.matches.is_empty());
        assert!(
            found.truncated,
            "a search that ran out of budget must say so"
        );
    }

    #[test]
    fn permit_guarded_search_refuses_an_unauthorised_root_without_reading_anything() {
        let fs = workspace();
        let engine = in_default_mode("/ws");
        let granted = engine
            .classify_settled(search_call("/ws/src", "SECRET"))
            .unwrap();
        let guarded = guarded(granted, &fs);

        let denied = guarded
            .search(&ScanRequest::new(CanonPath::new("/ws")), &|line: &str| {
                line.contains("SECRET")
            })
            .expect_err("/ws is wider than what was authorised");

        assert!(matches!(
            denied,
            IoDenied::Permit(PermitDenied::Search { .. })
        ));
        assert_eq!(fs.scans(), 0, "nothing may be walked before the refusal");
        assert_eq!(fs.reads(), 0, "and nothing may be read");
    }

    // --- Leg 2 of Invariant W: containment ------------------------------
    //
    // Generated cases are evidence over their sampled domain, not proof for
    // all calls. Compiler-enforced boundaries and runtime sampling establish
    // different claims.
    //
    // What is generated is small and exhaustive rather than large and random:
    // every call of one or two ops drawn from three op kinds over six paths,
    // and every one of those probed against every authority at every path in
    // a wider set that includes escapes and sibling-prefix traps. That is
    // 342 calls and 8208 authority checks, deterministic and reproducible,
    // which is what this repository can do without a `proptest` dependency
    // it does not have. A randomised generator would cover a wider domain and
    // report a different one on every run.
    //
    // Assert the biconditional: `allows_x(p)` succeeds exactly for paths at or
    // below an operation of kind `x`. Check both containment and that the
    // permit still grants the access the call actually requested.

    /// Every path this leg probes, including three that must never be in
    /// scope: an escape, a sibling sharing a textual prefix, and a
    /// descendant of a sibling.
    const PROBES: &[&str] = &[
        "/ws",
        "/ws/a",
        "/ws/a/b",
        "/ws/a/b/c",
        "/ws/c",
        "/ws/c-evil",
        "/ws/c-evil/secret",
        "/ws/a-evil",
        "/wsx",
        "/etc/passwd",
    ];

    /// The paths ops are built over. A subset of [`PROBES`]: the interesting
    /// cases are the probes that are *not* here.
    const ROOTS: &[&str] = &[
        "/ws",
        "/ws/a",
        "/ws/a/b",
        "/ws/c",
        "/ws/c-evil",
        "/ws/a/b/c",
    ];

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum Kind {
        Read,
        List,
        Search,
        Write,
    }

    const KINDS: &[Kind] = &[Kind::Read, Kind::List, Kind::Search, Kind::Write];

    fn op_of(kind: Kind, path: &str) -> Op {
        match kind {
            Kind::Read => Op::Read {
                path: CanonPath::new(path),
            },
            Kind::List => Op::List {
                root: CanonPath::new(path),
                pattern: "**".to_string(),
            },
            Kind::Search => Op::Search {
                root: CanonPath::new(path),
                query: "q".to_string(),
            },
            Kind::Write => Op::Write {
                path: CanonPath::new(path),
                change: Change::Whole(b"x".to_vec()),
            },
        }
    }

    /// Every (kind, path) pair, as the alphabet calls are drawn from.
    fn alphabet() -> Vec<(Kind, &'static str)> {
        KINDS
            .iter()
            .flat_map(|kind| ROOTS.iter().map(move |path| (*kind, *path)))
            .collect()
    }

    /// Is `probe` at or under a path some op of `kind` contributed?
    fn expected(ops: &[(Kind, &str)], kind: Kind, probe: &str) -> bool {
        let probe = CanonPath::new(probe);
        ops.iter()
            .filter(|(k, _)| *k == kind)
            .any(|(_, root)| probe.is_or_under(&CanonPath::new(root)))
    }

    fn check(engine: &PolicyEngine, ops: &[(Kind, &str)]) {
        let mut operations = ops.iter().map(|(k, p)| op_of(*k, p));
        let first = operations
            .next()
            .expect("generated probe call is non-empty");
        let call = operations.fold(PreparedCall::new("probe", first), |call, op| {
            call.with_op(op)
        });
        let granted = engine
            .classify_settled(call)
            .expect("every generated call operates inside /ws, in a mode that admits it");
        let permit = granted.permit();

        for probe in PROBES {
            let path = CanonPath::new(probe);
            let checks = [
                (Kind::Read, permit.allows_read(&path).is_ok()),
                (Kind::List, permit.allows_list(&path).is_ok()),
                (Kind::Search, permit.allows_search(&path).is_ok()),
                (Kind::Write, permit.allows_write(&path).is_ok()),
            ];
            for (kind, allowed) in checks {
                assert_eq!(
                    allowed,
                    expected(ops, kind, probe),
                    "{kind:?} of {probe} under a permit for {ops:?}"
                );
            }
        }
    }

    /// The engine this leg generates against: `edits`, so that a write is
    /// *authorised* rather than asked about and the alphabet can include one.
    ///
    /// Write joined the alphabet late, and the reason is worth keeping. While
    /// this ran in `default` mode a write was never authorised, so `check`
    /// carried a blanket "no generated call may write anywhere" instead of the
    /// biconditional the other three kinds get. That hole survived a mutation:
    /// making `Op::Write` contribute to `read_paths` *as well as*
    /// `write_paths` — a tool handed authority to read back the file it was
    /// only allowed to overwrite — left all 774 tests green.
    ///
    /// The mode is not what is being tested here; it is what makes the
    /// alphabet complete. Which modes authorise a write is `permit_step4_*`'s
    /// question, and it is asked there.
    fn leg2_engine() -> PolicyEngine {
        PolicyEngine::new(CanonPath::new("/ws"), Mode::Edits, Ruleset::default())
    }

    #[test]
    fn permit_leg2_a_one_op_call_grants_exactly_what_that_op_named() {
        let engine = leg2_engine();
        for (kind, path) in alphabet() {
            check(&engine, &[(kind, path)]);
        }
    }

    #[test]
    fn permit_leg2_a_two_op_call_grants_exactly_the_union_and_no_more() {
        // The case a one-op sweep cannot see: two ops of *different* kinds
        // over the same path must not lend each other authority, and two ops
        // of the same kind must union rather than overwrite.
        let engine = leg2_engine();
        let alphabet = alphabet();
        for first in &alphabet {
            for second in &alphabet {
                check(&engine, &[*first, *second]);
            }
        }
    }

    /// `Authority`'s wire form, pinned because two doc comments state it and
    /// an operator greps for it.
    ///
    /// `Authority` is persisted — `EventKind::PermissionDecided` carries it —
    /// so the tag name and the variant spellings are a durable contract, not a
    /// serde default anybody may retune. Changing `tag` or `rename_all` here
    /// is a re-tag, which `event::domain::EventKind`'s codec rule prices at a
    /// `CODEC_VERSION` bump and a migration.
    ///
    /// `party` is in the list deliberately. Interactive human decisions write
    /// it; this assertion pins the durable spelling independently of a whole
    /// turn fixture.
    #[test]
    fn permit_the_authority_wire_form_is_a_durable_contract() {
        let cases = [
            (Authority::Party, json!({ "by": "party" })),
            (Authority::Boundary, json!({ "by": "boundary" })),
            (
                Authority::Mode {
                    mode: Mode::Default,
                },
                json!({ "by": "mode", "mode": "default" }),
            ),
            (
                Authority::Rules { rules: Vec::new() },
                json!({ "by": "rules", "rules": [] }),
            ),
        ];
        for (authority, expected) in cases {
            let encoded = serde_json::to_value(&authority).expect("an authority serialises");
            assert_eq!(encoded, expected, "{authority:?} moved on the wire");
            let decoded: Authority =
                serde_json::from_value(encoded).expect("and reads back as itself");
            assert_eq!(decoded, authority);
        }
    }
}

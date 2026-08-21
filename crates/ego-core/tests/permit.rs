//! `PreparedCall` / `Permit` / `GuardedIo`: enforced, not declared, effects.
//!
//! See `ARCHITECTURE.md` §3.2 and §3.4, and `plans/m0a-contracts.md` Step 11.

use std::path::Path;

use ego_core::config::{Mode, PermCfg};
use ego_core::permit::rule::Rule;
use ego_core::permit::{
    Authorized, CanonPath, Classification, EntryKind, Escalation, FsError, GuardedFs, GuardedIo,
    IoDenied, Op, PermitDenied, PolicyDenied, PolicyEngine, PrepareCtx, PreparedCall, ScanLimits,
    ScanRequest, Tool, ToolError, Typed,
};
use serde_json::json;

// Wrapped in a module named `permit` so `cargo test -p ego-core permit` (the
// story's VERIFY command) matches these tests by substring: cargo's test
// filter matches the full test path, not the source file name, and
// tests/permit.rs's bare functions wouldn't otherwise contain "permit".
mod common;

mod permit {
    use super::*;
    use ego_core::permit::fake::{FakeFs, FakeWriteTool};

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

    // --- policy: what a `PreparedCall` is actually authorised to do ---
    // `docs/decisions/2026-08-21-the-turn-loop.md` §1.

    /// The engine an operator gets when they configure nothing: `default`
    /// mode, no rules of their own.
    ///
    /// Every test below that does not name a mode is asserting about *that*
    /// operator, which is why the helper hardcodes the default rather than
    /// taking a mode: a test that had to spell out `Mode::Default` could be
    /// changed to spell out something else, and the claim "this is what you
    /// get out of the box" would quietly stop being tested.
    fn in_default_mode(root: &str) -> PolicyEngine {
        PolicyEngine::new(CanonPath::new(root), Mode::Default, PermCfg::default())
    }

    /// The answer, for a call the engine settles on its own.
    ///
    /// Lives in the test rather than on `PolicyEngine` because a caller that
    /// cannot handle [`Classification::Ask`] is a test and never the loop —
    /// the third outcome is the loop's whole job, and a library method that
    /// flattened it away would be there for something to accidentally call.
    trait Settles {
        fn classify_settled(&self, call: PreparedCall) -> Result<Authorized, PolicyDenied>;
    }

    impl Settles for PolicyEngine {
        fn classify_settled(&self, call: PreparedCall) -> Result<Authorized, PolicyDenied> {
            match self.classify(call) {
                Classification::Settled(answer) => answer,
                Classification::Ask(pending) => panic!(
                    "expected an answer, got a question for the operator: {}",
                    pending.request()
                ),
            }
        }
    }

    fn read_call(path: &str) -> PreparedCall {
        PreparedCall {
            tool: "read".to_string(),
            ops: vec![Op::Read {
                path: CanonPath::new(path),
            }],
        }
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
        PreparedCall {
            tool: "edit".to_string(),
            ops: vec![Op::Write {
                path: CanonPath::new(path),
                contents: b"pwned".to_vec(),
            }],
        }
    }

    #[test]
    fn permit_policy_refuses_a_write_in_plan_mode_and_names_it() {
        // This assertion used to hold in every mode, because the engine had
        // one hardcoded rule saying writes are refused. What refuses now is
        // the mode floor, so the same call gets a different answer under
        // `edits` — and *that* is the property worth pinning: `plan` is the
        // mode where a write cannot happen, not the build.
        let engine = PolicyEngine::new(CanonPath::new("/ws"), Mode::Plan, PermCfg::default());
        let denied = engine
            .classify_settled(write_call("/ws/src/main.rs"))
            .expect_err("plan mode changes nothing");
        assert_eq!(
            denied,
            PolicyDenied::ModeForbids {
                tool: "edit".to_string(),
                mode: Mode::Plan,
                op: "write",
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
                op: "write",
                path: CanonPath::new("/ws/src/main.rs"),
            }
        );
        assert_eq!(pending.call().ops, write_call("/ws/src/main.rs").ops);
    }

    #[test]
    fn permit_policy_refuses_a_read_outside_the_workspace_and_names_it() {
        let engine = in_default_mode("/ws");
        let denied = engine
            .classify_settled(read_call("/etc/passwd"))
            .expect_err("the workspace is the boundary");
        assert_eq!(
            denied,
            PolicyDenied::OutsideWorkspace {
                tool: "read".to_string(),
                op: "read",
                path: CanonPath::new("/etc/passwd"),
            }
        );
        assert!(denied.to_string().contains("/etc/passwd"));
    }

    // --- the evaluation order ---------------------------------------------
    //
    // `docs/decisions/2026-08-21-a-permit-nobody-can-widen.md` §6. The order
    // is the whole security property: what each step decides matters less
    // than which step gets asked first, so every test below pins a *pair* of
    // steps against each other rather than one step on its own.

    /// An engine in `mode`, with those rules, over `/ws`.
    fn engine(mode: Mode, allow: &[&str], deny: &[&str]) -> PolicyEngine {
        let load = |raws: &[&str]| raws.iter().map(|raw| rule(raw)).collect();
        PolicyEngine::new(
            CanonPath::new("/ws"),
            mode,
            PermCfg {
                allow: load(allow),
                deny: load(deny),
            },
        )
    }

    fn rule(raw: &str) -> Rule {
        Rule::load(raw).unwrap_or_else(|err| panic!("{raw} is a rule this build enforces: {err}"))
    }

    #[test]
    fn permit_step0_the_workspace_boundary_is_not_lifted_by_yolo() {
        // The bypass mode bypasses *asking*. Where ego may reach is
        // `ARCHITECTURE.md` §3.4 axis 4 — the sandbox — and it is a separate
        // control, so a mode that turned the boundary off would be one axis
        // silently doing another's job.
        let engine = engine(Mode::Yolo, &["read(**)"], &[]);
        let denied = engine
            .classify_settled(read_call("/etc/passwd"))
            .expect_err("yolo does not move the workspace");
        assert!(matches!(denied, PolicyDenied::OutsideWorkspace { .. }));
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
            denied,
            PolicyDenied::Denied {
                tool: "edit".to_string(),
                op: "write",
                path: CanonPath::new("/ws/src/main.rs"),
                by: rule("write(**)").id(),
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
        assert!(matches!(denied, PolicyDenied::Denied { .. }));
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
            denied,
            PolicyDenied::ModeForbids {
                mode: Mode::Plan,
                op: "write",
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
        // The rule says `write(src/**)`; the call named one file. The permit
        // must cover the file. A rule that widened the grant to its own
        // pattern would make every allow a standing authority rather than an
        // answer to one call — §1's whole subject.
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
                op: "write",
                path: CanonPath::new("/ws/docs/README.md"),
            }
        );
    }

    #[test]
    fn permit_step4_the_ceiling_answers_a_write_differently_in_every_mode() {
        // The mode axis, as one table. Read down the right-hand column: this
        // is the whole of what choosing a mode buys, for the one operation
        // the modes disagree about.
        #[derive(Debug, PartialEq)]
        enum Answer {
            Authorized,
            Refused,
            Asked,
        }

        let cases = [
            (Mode::Plan, Answer::Refused),
            (Mode::Default, Answer::Asked),
            // No judge in this build, so `auto` has nothing to consult and
            // falls through to the same question `default` asks. When step
            // 6 exists this row changes and no other one does.
            (Mode::Auto, Answer::Asked),
            (Mode::Edits, Answer::Authorized),
            (Mode::Yolo, Answer::Authorized),
        ];

        for (mode, expected) in cases {
            let engine = engine(mode, &[], &[]);
            let answer = match engine.classify(write_call("/ws/src/main.rs")) {
                Classification::Settled(Ok(_)) => Answer::Authorized,
                Classification::Settled(Err(_)) => Answer::Refused,
                Classification::Ask(_) => Answer::Asked,
            };
            assert_eq!(answer, expected, "{mode} mode, writing inside /ws");
        }
    }

    #[test]
    fn permit_step4_no_mode_asks_about_a_read_inside_the_workspace() {
        // `ARCHITECTURE.md:238` counts prompt fatigue as a security bug, and
        // :242 says `default` and `auto` review unresolved **sensitive**
        // actions rather than every action. A mode that stopped the run to
        // ask about reading a file in the workspace it was pointed at is how
        // an operator learns to approve without looking.
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
    fn permit_the_order_is_per_operation_and_the_first_unanswered_one_stops_it() {
        // A call with three ops is three questions, and the answer to the
        // call is the first one that is not "yes". Here op two is denied and
        // op three would have been asked about: a loop that answered the
        // *last* question, or that collected them, would let the tool run
        // under a denial it had already been given.
        let engine = engine(Mode::Default, &["read(**)"], &["read(secrets/**)"]);
        let call = PreparedCall {
            tool: "read".to_string(),
            ops: vec![
                Op::Read {
                    path: CanonPath::new("/ws/src/main.rs"),
                },
                Op::Read {
                    path: CanonPath::new("/ws/secrets/prod.env"),
                },
                Op::Write {
                    path: CanonPath::new("/ws/src/main.rs"),
                    contents: b"pwned".to_vec(),
                },
            ],
        };
        let denied = engine
            .classify_settled(call)
            .expect_err("op two is denied, and that ends the call");
        assert_eq!(
            denied,
            PolicyDenied::Denied {
                tool: "read".to_string(),
                op: "read",
                path: CanonPath::new("/ws/secrets/prod.env"),
                by: rule("read(secrets/**)").id(),
            }
        );
    }

    #[test]
    fn permit_policy_refuses_a_whole_call_for_one_bad_op() {
        // A call is authorised or it is not. Minting a permit for the
        // acceptable half would let a tool do part of something nobody
        // approved.
        let engine = in_default_mode("/ws");
        let call = PreparedCall {
            tool: "read".to_string(),
            ops: vec![
                Op::Read {
                    path: CanonPath::new("/ws/fine.txt"),
                },
                Op::Read {
                    path: CanonPath::new("/etc/shadow"),
                },
            ],
        };
        assert!(engine.classify_settled(call).is_err());
    }

    // --- listing and searching: looking without reading -------------------
    //
    // See `docs/decisions/2026-08-21-reading-without-reading-everything.md`.

    fn list_call(root: &str, pattern: &str) -> PreparedCall {
        PreparedCall {
            tool: "glob".to_string(),
            ops: vec![Op::List {
                root: CanonPath::new(root),
                pattern: pattern.to_string(),
            }],
        }
    }

    fn search_call(root: &str, query: &str) -> PreparedCall {
        PreparedCall {
            tool: "grep".to_string(),
            ops: vec![Op::Search {
                root: CanonPath::new(root),
                query: query.to_string(),
            }],
        }
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
            denied,
            PolicyDenied::OutsideWorkspace {
                tool: "glob".to_string(),
                op: "list",
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
        let guarded = GuardedFs::new(granted.into_parts().1, &fs);

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
        let guarded = GuardedFs::new(granted.into_parts().1, &fs);

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
        let guarded = GuardedFs::new(granted.into_parts().1, &fs);

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
        let guarded = GuardedFs::new(granted.into_parts().1, &fs);

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
        let guarded = GuardedFs::new(granted.into_parts().1, &fs);

        let capped = ScanRequest {
            root: CanonPath::new("/ws/src"),
            skip_dirs: Vec::new(),
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
        let guarded = GuardedFs::new(granted.into_parts().1, &fs);

        let shallow = ScanRequest {
            root: CanonPath::new("/ws/src"),
            skip_dirs: Vec::new(),
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
        let guarded = GuardedFs::new(granted.into_parts().1, &fs);

        let skipping = ScanRequest {
            root: CanonPath::new("/ws/src"),
            skip_dirs: vec!["target".to_string()],
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
        let guarded = GuardedFs::new(granted.into_parts().1, &fs);

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

    #[test]
    fn permit_guarded_read_tells_a_missing_file_apart_from_a_refused_one() {
        // Criterion 2's whole point. A model told "denied" should stop asking;
        // a model told "not found" should look elsewhere. One collapsed
        // failure produces the wrong next move in both cases.
        let fs = workspace();
        let engine = in_default_mode("/ws");
        let granted = engine.classify_settled(read_call("/ws/src")).unwrap();
        let guarded = GuardedFs::new(granted.into_parts().1, &fs);

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
        let guarded = GuardedFs::new(granted.into_parts().1, &fs);

        let starved = ScanRequest {
            root: CanonPath::new("/ws/src"),
            skip_dirs: Vec::new(),
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
        let guarded = GuardedFs::new(granted.into_parts().1, &fs);

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
    // `docs/decisions/2026-08-21-a-permit-nobody-can-widen.md` calls this leg
    // **evidence, and not proof**, and that framing is load-bearing. Leg 1 is
    // decided by the compiler and is total. This is a check over a domain
    // somebody chose, and it is true of the domain it generated and silent
    // about every call outside it. Nobody downstream may cite it as more.
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
    // The assertion is stronger than the ADR asks for. The ADR asks that a
    // path *not* contributed by an op be refused. This asserts the biconditional:
    // `allows_x(p)` is `Ok` exactly when `p` is at or under a path some op of
    // kind `x` contributed. One direction is containment; the other catches a
    // scope that has quietly stopped granting what the call asked for, which
    // is how a permit gets "fixed" by widening the next one.

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
                contents: b"x".to_vec(),
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
        let call = PreparedCall {
            tool: "probe".to_string(),
            ops: ops.iter().map(|(k, p)| op_of(*k, p)).collect(),
        };
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
        PolicyEngine::new(CanonPath::new("/ws"), Mode::Edits, PermCfg::default())
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
}

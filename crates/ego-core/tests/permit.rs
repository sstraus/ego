//! `PreparedCall` / `Permit` / `GuardedIo`: enforced, not declared, effects.
//!
//! See `ARCHITECTURE.md` §3.2 and §3.4, and `plans/m0a-contracts.md` Step 11.

use std::path::Path;

use ego_core::permit::{
    CanonPath, EntryKind, FsError, GuardedFs, GuardedIo, IoDenied, Op, PermitDenied, PolicyDenied,
    PolicyEngine, PrepareCtx, PreparedCall, ScanLimits, ScanRequest, Tool, ToolError, Typed,
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
        let engine = PolicyEngine::new(CanonPath::new("/ws"));
        let granted = engine
            .authorize(read_call("/ws/src/main.rs"))
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
        let engine = PolicyEngine::new(CanonPath::new("/ws"));
        let granted = engine
            .authorize(read_call("/ws/src/main.rs"))
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

    #[test]
    fn permit_policy_refuses_a_write_and_names_it() {
        let engine = PolicyEngine::new(CanonPath::new("/ws"));
        let call = PreparedCall {
            tool: "edit".to_string(),
            ops: vec![Op::Write {
                path: CanonPath::new("/ws/src/main.rs"),
                contents: b"pwned".to_vec(),
            }],
        };
        let denied = engine
            .authorize(call)
            .expect_err("this session cannot write");
        assert_eq!(
            denied,
            PolicyDenied::WriteRefused {
                tool: "edit".to_string(),
                path: CanonPath::new("/ws/src/main.rs"),
            }
        );
        assert!(denied.to_string().contains("/ws/src/main.rs"));
    }

    #[test]
    fn permit_policy_refuses_a_read_outside_the_workspace_and_names_it() {
        let engine = PolicyEngine::new(CanonPath::new("/ws"));
        let denied = engine
            .authorize(read_call("/etc/passwd"))
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

    #[test]
    fn permit_policy_refuses_a_whole_call_for_one_bad_op() {
        // A call is authorised or it is not. Minting a permit for the
        // acceptable half would let a tool do part of something nobody
        // approved.
        let engine = PolicyEngine::new(CanonPath::new("/ws"));
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
        assert!(engine.authorize(call).is_err());
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
        let engine = PolicyEngine::new(CanonPath::new("/ws"));
        let granted = engine
            .authorize(list_call("/ws/src", "**/*.rs"))
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
        let engine = PolicyEngine::new(CanonPath::new("/ws"));
        let granted = engine
            .authorize(read_call("/ws/src"))
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
        let engine = PolicyEngine::new(CanonPath::new("/ws"));
        let granted = engine
            .authorize(search_call("/ws/src", "TODO"))
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
        let engine = PolicyEngine::new(CanonPath::new("/ws"));
        let denied = engine
            .authorize(list_call("/etc", "*"))
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
        let engine = PolicyEngine::new(CanonPath::new("/ws"));
        let denied = engine
            .authorize(search_call("/etc", "password"))
            .expect_err("the workspace is the boundary for searching too");
        assert!(denied.to_string().contains("may not search /etc"));
    }

    #[test]
    fn permit_guarded_list_returns_the_names_under_an_authorised_root() {
        let fs = workspace();
        let engine = PolicyEngine::new(CanonPath::new("/ws"));
        let granted = engine.authorize(list_call("/ws/src", "**")).unwrap();
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
        let engine = PolicyEngine::new(CanonPath::new("/ws"));
        let granted = engine.authorize(list_call("/ws/src", "**")).unwrap();
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
        let engine = PolicyEngine::new(CanonPath::new("/ws"));
        let granted = engine.authorize(list_call("/ws/src", "**")).unwrap();
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
        let engine = PolicyEngine::new(CanonPath::new("/ws"));
        let granted = engine.authorize(list_call("/ws/src", "**")).unwrap();
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
        let engine = PolicyEngine::new(CanonPath::new("/ws"));
        let granted = engine.authorize(list_call("/ws/src", "**")).unwrap();
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
        let engine = PolicyEngine::new(CanonPath::new("/ws"));
        let granted = engine.authorize(list_call("/ws/src", "**")).unwrap();
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
        let engine = PolicyEngine::new(CanonPath::new("/ws"));
        let granted = engine.authorize(list_call("/ws/src", "**")).unwrap();
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
        let narrow_grant = engine.authorize(list_call("/ws/src/deep", "**")).unwrap();
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
        let engine = PolicyEngine::new(CanonPath::new("/ws"));
        let granted = engine.authorize(search_call("/ws/src", "TODO")).unwrap();
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
        let engine = PolicyEngine::new(CanonPath::new("/ws"));
        let granted = engine.authorize(read_call("/ws/src")).unwrap();
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
        let engine = PolicyEngine::new(CanonPath::new("/ws"));
        let granted = engine.authorize(search_call("/ws/src", "fn")).unwrap();
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
        let engine = PolicyEngine::new(CanonPath::new("/ws"));
        let granted = engine.authorize(search_call("/ws/src", "SECRET")).unwrap();
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
    }

    const KINDS: &[Kind] = &[Kind::Read, Kind::List, Kind::Search];

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
            .authorize(call)
            .expect("every generated call reads, lists or searches inside /ws");
        let permit = granted.permit();

        for probe in PROBES {
            let path = CanonPath::new(probe);
            let checks = [
                (Kind::Read, permit.allows_read(&path).is_ok()),
                (Kind::List, permit.allows_list(&path).is_ok()),
                (Kind::Search, permit.allows_search(&path).is_ok()),
            ];
            for (kind, allowed) in checks {
                assert_eq!(
                    allowed,
                    expected(ops, kind, probe),
                    "{kind:?} of {probe} under a permit for {ops:?}"
                );
            }
            // No op in this alphabet contributes write authority, so no
            // generated call may write anywhere at all.
            assert!(
                permit.allows_write(&path).is_err(),
                "a call of reads, lists and searches granted a write to {probe}"
            );
        }
    }

    #[test]
    fn permit_leg2_a_one_op_call_grants_exactly_what_that_op_named() {
        let engine = PolicyEngine::new(CanonPath::new("/ws"));
        for (kind, path) in alphabet() {
            check(&engine, &[(kind, path)]);
        }
    }

    #[test]
    fn permit_leg2_a_two_op_call_grants_exactly_the_union_and_no_more() {
        // The case a one-op sweep cannot see: two ops of *different* kinds
        // over the same path must not lend each other authority, and two ops
        // of the same kind must union rather than overwrite.
        let engine = PolicyEngine::new(CanonPath::new("/ws"));
        let alphabet = alphabet();
        for first in &alphabet {
            for second in &alphabet {
                check(&engine, &[*first, *second]);
            }
        }
    }
}

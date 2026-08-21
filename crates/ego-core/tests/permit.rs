//! `PreparedCall` / `Permit` / `GuardedIo`: enforced, not declared, effects.
//!
//! See `ARCHITECTURE.md` §3.2 and §3.4, and `plans/m0a-contracts.md` Step 11.

use std::path::Path;

use ego_core::permit::{
    CanonPath, EntryKind, Fs, FsError, GuardedFs, GuardedIo, IoDenied, Op, Permit, PermitDenied,
    PolicyDenied, PolicyEngine, PrepareCtx, PreparedCall, ScanLimits, ScanRequest, Scope, Tool,
    ToolError, ToolResult, Typed, TypedTool,
};
use futures::executor::block_on;
use futures::future::BoxFuture;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

// Wrapped in a module named `permit` so `cargo test -p ego-core permit` (the
// story's VERIFY command) matches these tests by substring: cargo's test
// filter matches the full test path, not the source file name, and
// tests/permit.rs's bare functions wouldn't otherwise contain "permit".
mod common;

mod permit {
    use super::*;
    use crate::common::FakeFs;

    #[derive(Debug, Deserialize, JsonSchema)]
    struct WriteParams {
        path: String,
        contents: String,
    }

    /// A fake builtin tool: writes `contents` to `path`, going through
    /// `GuardedIo` exactly like a real tool must — it never sees a `Fs`.
    struct FakeWriteTool;

    impl TypedTool for FakeWriteTool {
        type Params = WriteParams;

        fn name(&self) -> &str {
            "fake_write"
        }

        fn description(&self) -> &str {
            "writes contents to a file, for testing the permit seam"
        }

        fn prepare_typed(
            &self,
            params: WriteParams,
            ctx: &PrepareCtx<'_>,
        ) -> Result<PreparedCall, ToolError> {
            let resolved = ctx.resolver.canonicalize(Path::new(&params.path))?;
            Ok(PreparedCall {
                tool: self.name().to_string(),
                ops: vec![Op::Write {
                    path: resolved,
                    contents: params.contents.into_bytes(),
                }],
            })
        }

        fn execute<'a>(
            &'a self,
            call: PreparedCall,
            _permit: &'a Permit,
            io: &'a dyn GuardedIo,
        ) -> BoxFuture<'a, Result<ToolResult, ToolError>> {
            Box::pin(async move {
                let Some(Op::Write { path, contents }) = call.ops.into_iter().next() else {
                    return Err(ToolError::InvalidParams("expected one Write op".into()));
                };
                io.write(&path, &contents)?;
                Ok(ToolResult {
                    content: format!("wrote {} bytes to {path}", contents.len()),
                })
            })
        }
    }

    fn prepare_ctx<'a>(fs: &'a FakeFs) -> PrepareCtx<'a> {
        PrepareCtx {
            cwd: CanonPath::new("/ws"),
            resolver: fs,
        }
    }

    #[test]
    fn permit_typed_tool_writes_inside_permit_ok() {
        let fs = FakeFs::default();
        let tool = Typed(FakeWriteTool);
        let ctx = prepare_ctx(&fs);
        let call = tool
            .prepare(
                json!({ "path": "/ws/allowed/note.txt", "contents": "hello" }),
                &ctx,
            )
            .unwrap();

        let engine = PolicyEngine::new(CanonPath::new("/ws"));
        let permit = engine.mint(Scope {
            write_paths: vec![CanonPath::new("/ws/allowed")],
            ..Scope::default()
        });
        let guarded = GuardedFs::new(&permit, &fs);

        let result = block_on(tool.execute(call, &permit, &guarded)).unwrap();
        assert_eq!(result.content, "wrote 5 bytes to /ws/allowed/note.txt");
        assert_eq!(
            fs.read(&CanonPath::new("/ws/allowed/note.txt")).unwrap(),
            b"hello"
        );
    }

    #[test]
    fn permit_typed_tool_write_outside_permit_denied_names_op() {
        let fs = FakeFs::default();
        let tool = Typed(FakeWriteTool);
        let ctx = prepare_ctx(&fs);
        let call = tool
            .prepare(json!({ "path": "/etc/passwd", "contents": "pwned" }), &ctx)
            .unwrap();

        let engine = PolicyEngine::new(CanonPath::new("/ws"));
        let permit = engine.mint(Scope {
            write_paths: vec![CanonPath::new("/ws/allowed")],
            ..Scope::default()
        });
        let guarded = GuardedFs::new(&permit, &fs);

        let err = block_on(tool.execute(call, &permit, &guarded)).unwrap_err();
        assert_eq!(
            err,
            ToolError::Denied(PermitDenied::Write {
                path: CanonPath::new("/etc/passwd")
            })
        );
        let message = err.to_string();
        assert!(
            message.contains("write"),
            "denial should name the op: {message}"
        );
        assert!(
            message.contains("/etc/passwd"),
            "denial should name the path: {message}"
        );
        assert!(
            fs.read(&CanonPath::new("/etc/passwd")).is_err(),
            "denied write must not happen"
        );
    }

    #[test]
    fn permit_symlink_resolved_at_prepare_is_rechecked_at_execute() {
        let fs = FakeFs::default();
        fs.symlink("/ws/link", "/ws/allowed/file.txt");

        let tool = Typed(FakeWriteTool);
        let ctx = prepare_ctx(&fs);
        let call = tool
            .prepare(json!({ "path": "/ws/link", "contents": "hi" }), &ctx)
            .expect("prepare chases the symlink");
        assert_eq!(
            call.ops,
            vec![Op::Write {
                path: CanonPath::new("/ws/allowed/file.txt"),
                contents: b"hi".to_vec(),
            }]
        );

        let engine = PolicyEngine::new(CanonPath::new("/ws"));
        let permit = engine.mint(Scope {
            write_paths: vec![CanonPath::new("/ws/allowed")],
            ..Scope::default()
        });

        // Race: between prepare (which classified /ws/allowed/file.txt as
        // the target) and execute, the resolved target itself is swapped
        // for a symlink pointing outside the permitted scope.
        fs.symlink("/ws/allowed/file.txt", "/ws/secret/passwords.txt");

        let guarded = GuardedFs::new(&permit, &fs);
        let err = block_on(tool.execute(call, &permit, &guarded)).unwrap_err();
        assert_eq!(
            err,
            ToolError::Denied(PermitDenied::Write {
                path: CanonPath::new("/ws/secret/passwords.txt"),
            })
        );
        assert!(
            fs.read(&CanonPath::new("/ws/secret/passwords.txt"))
                .is_err(),
            "the raced-to target must never be written"
        );
    }

    #[test]
    fn permit_guarded_fs_read_respects_scope() {
        let fs = FakeFs::default();
        fs.file("/ws/readable/notes.md", "public notes");
        fs.file("/ws/private/notes.md", "private notes");

        let engine = PolicyEngine::new(CanonPath::new("/ws"));
        let permit = engine.mint(Scope {
            read_paths: vec![CanonPath::new("/ws/readable")],
            ..Scope::default()
        });
        let guarded = GuardedFs::new(&permit, &fs);

        assert_eq!(
            guarded
                .read(&CanonPath::new("/ws/readable/notes.md"))
                .unwrap(),
            b"public notes".to_vec()
        );
        assert!(matches!(
            guarded.read(&CanonPath::new("/ws/private/notes.md")),
            Err(IoDenied::Permit(PermitDenied::Read { .. }))
        ));
    }

    #[test]
    fn permit_guarded_fs_write_denies_sibling_prefix_directory() {
        let fs = FakeFs::default();
        let engine = PolicyEngine::new(CanonPath::new("/ws"));
        let permit = engine.mint(Scope {
            write_paths: vec![CanonPath::new("/ws/allowed")],
            ..Scope::default()
        });
        let guarded = GuardedFs::new(&permit, &fs);

        // "/ws/allowed-evil" shares a textual prefix with the scoped
        // "/ws/allowed" but is a sibling, not a descendant of it —
        // `Path::starts_with` is component-wise, so this must stay denied.
        // A string-prefix "simplification" of `CanonPath::is_or_under`
        // would silently let this through.
        let err = guarded
            .write(&CanonPath::new("/ws/allowed-evil/file.txt"), b"pwned")
            .unwrap_err();
        assert!(matches!(err, IoDenied::Permit(PermitDenied::Write { .. })));
        assert!(
            fs.read(&CanonPath::new("/ws/allowed-evil/file.txt"))
                .is_err(),
            "denied write must not happen"
        );
    }

    #[test]
    fn permit_guarded_fs_write_allows_scope_root_and_nested_descendant() {
        let fs = FakeFs::default();
        let engine = PolicyEngine::new(CanonPath::new("/ws"));
        let permit = engine.mint(Scope {
            write_paths: vec![CanonPath::new("/ws/allowed")],
            ..Scope::default()
        });
        let guarded = GuardedFs::new(&permit, &fs);

        // The scope boundary itself is in scope ("is or under", not
        // strictly-under-only)...
        guarded
            .write(&CanonPath::new("/ws/allowed"), b"root")
            .unwrap();
        // ...and so is a nested descendant, arbitrarily deep.
        guarded
            .write(&CanonPath::new("/ws/allowed/nested/deep/file.txt"), b"deep")
            .unwrap();

        assert_eq!(fs.read(&CanonPath::new("/ws/allowed")).unwrap(), b"root");
        assert_eq!(
            fs.read(&CanonPath::new("/ws/allowed/nested/deep/file.txt"))
                .unwrap(),
            b"deep"
        );
    }

    #[test]
    fn permit_guarded_fs_read_denies_sibling_prefix_directory() {
        let fs = FakeFs::default();
        fs.file("/ws/allowed-evil/secret.txt", "leaked");

        let engine = PolicyEngine::new(CanonPath::new("/ws"));
        let permit = engine.mint(Scope {
            read_paths: vec![CanonPath::new("/ws/allowed")],
            ..Scope::default()
        });
        let guarded = GuardedFs::new(&permit, &fs);

        // Same sibling-prefix hazard as the write case, checked against
        // `Scope::read_paths` — read and write scope independently, each
        // with its own `.iter().any()`, so this needs its own pin.
        let err = guarded
            .read(&CanonPath::new("/ws/allowed-evil/secret.txt"))
            .unwrap_err();
        assert!(matches!(err, IoDenied::Permit(PermitDenied::Read { .. })));
    }

    #[test]
    fn permit_scope_covers_paths_endpoints_and_argv() {
        let engine = PolicyEngine::new(CanonPath::new("/ws"));
        let permit = engine.mint(Scope {
            write_paths: vec![CanonPath::new("/ws/allowed")],
            read_paths: vec![CanonPath::new("/ws/readable")],
            list_paths: vec![CanonPath::new("/ws/listable")],
            search_paths: vec![CanonPath::new("/ws/searchable")],
            endpoints: vec!["api.example.com:443".to_string()],
            argv: vec![vec!["git".to_string(), "status".to_string()]],
        });

        assert!(
            permit
                .allows_write(&CanonPath::new("/ws/allowed/file.txt"))
                .is_ok()
        );
        assert!(matches!(
            permit.allows_write(&CanonPath::new("/etc/passwd")),
            Err(PermitDenied::Write { .. })
        ));
        // Write scope does not imply read scope, or vice versa.
        assert!(matches!(
            permit.allows_read(&CanonPath::new("/ws/allowed/file.txt")),
            Err(PermitDenied::Read { .. })
        ));

        assert!(
            permit
                .allows_read(&CanonPath::new("/ws/readable/notes.md"))
                .is_ok()
        );
        assert!(matches!(
            permit.allows_read(&CanonPath::new("/ws/private/notes.md")),
            Err(PermitDenied::Read { .. })
        ));

        assert!(permit.allows_connect("api.example.com:443").is_ok());
        assert!(matches!(
            permit.allows_connect("evil.example.com:443"),
            Err(PermitDenied::Connect { .. })
        ));

        assert!(
            permit
                .allows_argv(&[
                    "git".to_string(),
                    "status".to_string(),
                    "--short".to_string()
                ])
                .is_ok()
        );
        assert!(matches!(
            permit.allows_argv(&["rm".to_string(), "-rf".to_string()]),
            Err(PermitDenied::Argv { .. })
        ));
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
        let guarded = GuardedFs::new(granted.permit(), &fs);

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
        let guarded = GuardedFs::new(granted.permit(), &fs);

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
        let guarded = GuardedFs::new(granted.permit(), &fs);

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
        let guarded = GuardedFs::new(granted.permit(), &fs);

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
        let guarded = GuardedFs::new(granted.permit(), &fs);

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
        let guarded = GuardedFs::new(granted.permit(), &fs);

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
        let guarded = GuardedFs::new(granted.permit(), &fs);

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
        let guarded = GuardedFs::new(granted.permit(), &fs);

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
        let guarded = GuardedFs::new(granted.permit(), &fs);

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
        let guarded = GuardedFs::new(granted.permit(), &fs);

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
        let guarded = GuardedFs::new(granted.permit(), &fs);

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
}

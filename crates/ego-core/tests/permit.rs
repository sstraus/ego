//! `PreparedCall` / `Permit` / `GuardedIo`: enforced, not declared, effects.
//!
//! See `ARCHITECTURE.md` §3.2 and §3.4, and `plans/m0a-contracts.md` Step 11.

use std::path::Path;

use ego_core::permit::{
    CanonPath, Fs, GuardedFs, GuardedIo, Op, Permit, PermitDenied, PolicyDenied, PolicyEngine,
    PrepareCtx, PreparedCall, Scope, Tool, ToolError, ToolResult, Typed, TypedTool,
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
            let resolved = ctx.resolver.canonicalize(Path::new(&params.path));
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
            fs.read(&CanonPath::new("/etc/passwd")).is_none(),
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
                .is_none(),
            "the raced-to target must never be written"
        );
    }

    #[test]
    fn permit_guarded_fs_read_respects_scope() {
        let fs = FakeFs::default();
        fs.write(&CanonPath::new("/ws/readable/notes.md"), b"public notes");
        fs.write(&CanonPath::new("/ws/private/notes.md"), b"private notes");

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
            Some(b"public notes".to_vec())
        );
        assert!(matches!(
            guarded.read(&CanonPath::new("/ws/private/notes.md")),
            Err(PermitDenied::Read { .. })
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
        assert!(matches!(err, PermitDenied::Write { .. }));
        assert!(
            fs.read(&CanonPath::new("/ws/allowed-evil/file.txt"))
                .is_none(),
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
        fs.write(&CanonPath::new("/ws/allowed-evil/secret.txt"), b"leaked");

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
        assert!(matches!(err, PermitDenied::Read { .. }));
    }

    #[test]
    fn permit_scope_covers_paths_endpoints_and_argv() {
        let engine = PolicyEngine::new(CanonPath::new("/ws"));
        let permit = engine.mint(Scope {
            write_paths: vec![CanonPath::new("/ws/allowed")],
            read_paths: vec![CanonPath::new("/ws/readable")],
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
        let permit = engine
            .authorize(&read_call("/ws/src/main.rs"))
            .expect("a workspace read is what this policy exists to allow");
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
        let permit = engine
            .authorize(&read_call("/ws/src/main.rs"))
            .expect("a workspace read is allowed");
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
            .authorize(&call)
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
            .authorize(&read_call("/etc/passwd"))
            .expect_err("the workspace is the boundary");
        assert_eq!(
            denied,
            PolicyDenied::ReadOutsideWorkspace {
                tool: "read".to_string(),
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
        assert!(engine.authorize(&call).is_err());
    }
}

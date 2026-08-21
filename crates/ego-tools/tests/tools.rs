//! The three read-only builtins, over a real filesystem.
//!
//! `crates/ego-core/tests/permit.rs` proves the *seam* refuses what it should.
//! What is proved here is that each tool asks for the authority it actually
//! needs and no more — `glob` cannot read what it listed, `grep` cannot read
//! what it searched — and that a denial comes back named rather than as a
//! panic or an empty result.
//!
//! Unix-only for the same reason as `tests/fs.rs`: the escapes worth testing
//! are spelled with symlinks.
#![cfg(unix)]

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use ego_core::budget::{BudgetGate, PricingQuote};
use ego_core::cost::{Amount, CurrencyCode, Money, PricingRevision};
use ego_core::event::domain::ExitReason;
use ego_core::event::id::SessionId;
use ego_core::generator::capabilities::{
    CacheRetention, CancelSemantics, ModelCapabilities, ReasoningCarryover, ReasoningEffortSupport,
    Transport,
};
use ego_core::generator::fake::FakeGenerator;
use ego_core::generator::port::AiEvent;
use ego_core::generator::types::{StopReason, ThinkingLevel};
use ego_core::permit::{
    CanonPath, GuardedFs, Op, Permit, PermitDenied, PolicyDenied, PolicyEngine, PrepareCtx,
    PreparedCall, Tool, ToolError, ToolResult,
};
use ego_core::sink::CollectingSink;
use ego_core::state::SessionState;
use ego_core::turn::{CancelToken, Model, Session, Tools, TurnConfig, TurnLoop};
use ego_tools::clock::SystemClock;
use ego_tools::fs::OsFs;
use ego_tools::store::{JsonlStore, OsIo};
use ego_tools::tools::builtins;
use futures::executor::block_on;
use serde_json::json;
use tempfile::TempDir;

/// A workspace on disk, its policy engine, and the three tools.
///
/// Everything real except the model: a real `OsFs`, a real `PolicyEngine`
/// whose workspace is the temp directory, and the registry `builtins()`
/// returns.
struct Workspace {
    _dir: TempDir,
    root: PathBuf,
    fs: OsFs,
    policy: PolicyEngine,
    registry: Vec<Arc<dyn Tool>>,
}

impl Workspace {
    /// `src/main.rs`, `src/deep/lib.rs` and a `.env` nobody should be able to
    /// reach by listing.
    fn new() -> Self {
        let dir = TempDir::new().expect("a temp directory");
        // Canonical: on macOS `TempDir` hands back a `/var/folders/...` that is
        // itself a symlink, and a policy whose workspace is not canonical
        // refuses every path the resolver returns.
        let root = fs::canonicalize(dir.path()).expect("the temp directory resolves");
        let policy = PolicyEngine::new(CanonPath::new(&root));
        Self {
            _dir: dir,
            root,
            fs: OsFs,
            policy,
            registry: builtins(),
        }
    }

    fn file(&self, relative: &str, contents: &str) -> PathBuf {
        let at = self.root.join(relative);
        if let Some(parent) = at.parent() {
            fs::create_dir_all(parent).expect("the parent directories");
        }
        fs::write(&at, contents).expect("the file is written");
        at
    }

    fn cwd(&self) -> CanonPath {
        CanonPath::new(&self.root)
    }

    fn tool(&self, name: &str) -> &dyn Tool {
        self.registry
            .iter()
            .find(|tool| tool.descriptor().name == name)
            .unwrap_or_else(|| panic!("{name} is registered"))
            .as_ref()
    }

    /// Run `name` through all three stages the turn loop runs it through.
    fn call(&self, name: &str, params: serde_json::Value) -> Result<String, ToolError> {
        let call = self.prepare(name, params)?;
        let granted = self.policy.authorize(call).expect("policy admits this");
        // `Authorized` cannot be split outside `ego-core` — `into_parts` is
        // crate-private — so a test that needs the call by value clones it.
        // The clone grants nothing: the permit is still the one the engine
        // minted, for this call.
        self.execute(name, granted.call().clone(), granted.permit())
    }

    fn prepare(&self, name: &str, params: serde_json::Value) -> Result<PreparedCall, ToolError> {
        let ctx = PrepareCtx {
            cwd: self.cwd(),
            resolver: &self.fs,
        };
        self.tool(name).prepare(params, &ctx)
    }

    fn execute(
        &self,
        name: &str,
        call: PreparedCall,
        permit: &Permit,
    ) -> Result<String, ToolError> {
        let guarded = GuardedFs::new(permit, &self.fs);
        block_on(self.tool(name).execute(call, permit, &guarded))
            .map(|result: ToolResult| result.content)
    }
}

/// A workspace with the usual three files in it.
fn populated() -> Workspace {
    let ws = Workspace::new();
    ws.file("src/main.rs", "fn main() {\n    println!(\"hello\");\n}\n");
    ws.file(
        "src/deep/lib.rs",
        "// TODO: write the library\npub fn lib() {}\n",
    );
    ws.file(".env", "SECRET=hunter2\n");
    ws
}

mod registration {
    use super::*;

    #[test]
    fn the_three_builtins_are_registered_under_the_names_a_model_calls() {
        let mut names: Vec<String> = builtins()
            .iter()
            .map(|tool| tool.descriptor().name)
            .collect();
        names.sort();

        assert_eq!(names, ["glob", "grep", "read"]);
    }

    #[test]
    fn each_builtin_describes_its_parameters_as_a_json_schema() {
        // What the model is shown. A tool with no schema is a tool the model
        // has to guess the arguments for.
        for tool in builtins() {
            let descriptor = tool.descriptor();
            let schema = descriptor.schema.to_value();
            assert_eq!(
                schema["type"], "object",
                "{}'s schema is not an object: {schema}",
                descriptor.name
            );
            assert!(
                schema["properties"].is_object(),
                "{} describes no properties: {schema}",
                descriptor.name
            );
            assert!(
                !descriptor.description.is_empty(),
                "{} has no description",
                descriptor.name
            );
        }
    }
}

mod read {
    use super::*;

    #[test]
    fn read_returns_the_file_through_the_guarded_seam() {
        let ws = populated();

        let content = ws
            .call("read", json!({ "path": "src/main.rs" }))
            .expect("the file is inside the workspace");

        assert!(content.contains("println!(\"hello\")"), "got {content:?}");
    }

    #[test]
    fn read_resolves_a_relative_path_against_the_working_directory() {
        let ws = populated();

        let relative = ws.call("read", json!({ "path": "src/main.rs" })).unwrap();
        let absolute = ws
            .call(
                "read",
                json!({ "path": ws.root.join("src/main.rs").display().to_string() }),
            )
            .unwrap();

        assert_eq!(relative, absolute);
    }

    #[test]
    fn read_of_a_missing_file_is_a_typed_error_from_prepare_and_never_a_panic() {
        // It fails at `prepare`, before there is anything to authorise: a
        // permit for a path that does not resolve would be a grant over
        // nothing.
        let ws = populated();

        let err = ws
            .prepare("read", json!({ "path": "src/nowhere.rs" }))
            .expect_err("the file is not there");

        assert!(
            matches!(
                err,
                ToolError::Fs(ego_core::permit::FsError::NotFound { .. })
            ),
            "expected Fs(NotFound), got {err:?}"
        );
    }

    #[test]
    fn read_of_a_directory_is_a_typed_error_rather_than_an_empty_result() {
        let ws = populated();

        let err = ws
            .call("read", json!({ "path": "src" }))
            .expect_err("a directory has no contents to read");

        assert!(
            matches!(
                err,
                ToolError::Fs(ego_core::permit::FsError::NotAFile { .. })
            ),
            "expected Fs(NotAFile), got {err:?}"
        );
    }

    #[test]
    fn read_outside_the_workspace_is_refused_by_policy_and_named() {
        let ws = populated();
        let outside = Workspace::new();
        let elsewhere = outside.file("secret.txt", "not yours");

        let call = ws
            .prepare("read", json!({ "path": elsewhere.display().to_string() }))
            .expect("the file exists, so it resolves");
        let denied = ws
            .policy
            .authorize(call)
            .expect_err("it is outside the workspace");

        assert!(matches!(
            denied,
            PolicyDenied::OutsideWorkspace { op: "read", .. }
        ));
        assert!(
            denied.to_string().contains("may not read"),
            "the refusal has to say what was refused: {denied}"
        );
    }

    #[test]
    fn read_under_a_permit_that_does_not_cover_the_path_is_denied_by_name() {
        // The permit, not the policy: this is the seam refusing a grant it was
        // never given, which is what stops a tool that resolved one path at
        // `prepare` from reading another at `execute`.
        let ws = populated();
        let allowed = ws
            .prepare("read", json!({ "path": "src/main.rs" }))
            .unwrap();
        let granted = ws.policy.authorize(allowed).unwrap();
        let permit = granted.permit();
        let other = ws.prepare("read", json!({ "path": ".env" })).unwrap();

        let err = ws
            .execute("read", other, permit)
            .expect_err("the permit names src/main.rs and nothing else");

        assert!(
            matches!(err, ToolError::Denied(PermitDenied::Read { .. })),
            "expected Denied(Read), got {err:?}"
        );
        assert!(err.to_string().contains("read denied"));
    }
}

mod glob {
    use super::*;

    #[test]
    fn glob_returns_the_paths_that_match_relative_to_the_search_root() {
        let ws = populated();

        let content = ws
            .call("glob", json!({ "pattern": "**/*.rs", "path": "src" }))
            .expect("src is inside the workspace");

        let mut lines: Vec<&str> = content.lines().collect();
        lines.sort();
        assert_eq!(lines, ["deep/lib.rs", "main.rs"]);
    }

    #[test]
    fn glob_defaults_its_root_to_the_working_directory() {
        let ws = populated();

        let content = ws
            .call("glob", json!({ "pattern": "**/*.rs" }))
            .expect("the working directory is the workspace");

        let mut lines: Vec<&str> = content.lines().collect();
        lines.sort();
        assert_eq!(lines, ["src/deep/lib.rs", "src/main.rs"]);
    }

    #[test]
    fn glob_says_so_when_the_builtin_limits_stopped_it_short() {
        // Reachable through the tool's own defaults, which is the point: a
        // model never sets a limit, so the only way it can tell a complete
        // answer from a truncated one is if the tool says. `ScanLimits`
        // defaults to 32 levels, so 34 is one tree too deep.
        let ws = populated();
        let deep: String = std::iter::repeat_n("d", 34).collect::<Vec<_>>().join("/");
        ws.file(&format!("{deep}/buried.rs"), "// too far down");

        let content = ws
            .call("glob", json!({ "pattern": "**/*.rs" }))
            .expect("the working directory is the workspace");

        assert!(
            !content.contains("buried.rs"),
            "the limit did not bite: {content}"
        );
        assert!(
            content.contains("this answer is incomplete"),
            "a truncated listing has to say so: {content}"
        );
    }

    #[test]
    fn glob_prepares_a_list_and_therefore_cannot_read_what_it_found() {
        // The whole point of `Op::List` existing. A glob expressed as
        // `Op::Read { path: root }` would come back holding the authority to
        // read every byte under the root, having asked only for names.
        let ws = populated();
        let call = ws
            .prepare("glob", json!({ "pattern": "**/*.rs", "path": "src" }))
            .unwrap();

        assert!(matches!(call.ops.as_slice(), [Op::List { .. }]));
        let granted = ws.policy.authorize(call).unwrap();
        let permit = granted.permit();
        let found = CanonPath::new(ws.root.join("src/main.rs"));
        assert!(permit.allows_list(&found).is_ok());
        assert!(
            matches!(permit.allows_read(&found), Err(PermitDenied::Read { .. })),
            "a list grant must not authorise reading what it listed"
        );
    }

    #[test]
    fn glob_reports_no_match_rather_than_an_empty_answer() {
        // An empty string and "there is nothing here" read the same to a
        // model only if it guesses. One of them is a statement.
        let ws = populated();

        let content = ws
            .call("glob", json!({ "pattern": "**/*.py", "path": "src" }))
            .expect("the root is fine; nothing matches");

        assert!(content.contains("no files"), "got {content:?}");
        assert!(content.contains("**/*.py"), "got {content:?}");
    }

    #[test]
    fn glob_lists_files_and_not_the_directories_they_sit_in() {
        let ws = populated();

        let content = ws
            .call("glob", json!({ "pattern": "*", "path": "src" }))
            .expect("src is inside the workspace");

        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(
            lines,
            ["main.rs"],
            "`deep` is a directory, and a glob answers `which files`"
        );
    }

    #[test]
    fn a_pattern_that_does_not_compile_is_reported_at_prepare() {
        let ws = populated();

        let err = ws
            .prepare("glob", json!({ "pattern": "src/[", "path": "src" }))
            .expect_err("an unterminated class is not a pattern");

        assert!(
            matches!(err, ToolError::InvalidParams(_)),
            "expected InvalidParams, got {err:?}"
        );
    }

    #[test]
    fn glob_outside_the_workspace_is_refused_and_named_as_a_list() {
        // "may not list /etc" and "may not read /etc" are different refusals,
        // and a prompt that flattens them teaches a model nothing.
        let ws = populated();
        let outside = Workspace::new();

        let call = ws
            .prepare(
                "glob",
                json!({ "pattern": "**", "path": outside.root.display().to_string() }),
            )
            .expect("the directory exists, so it resolves");
        let denied = ws
            .policy
            .authorize(call)
            .expect_err("it is outside the workspace");

        assert!(matches!(
            denied,
            PolicyDenied::OutsideWorkspace { op: "list", .. }
        ));
        assert!(denied.to_string().contains("may not list"));
    }

    #[test]
    fn glob_under_a_permit_that_does_not_cover_its_root_is_denied_by_name() {
        let ws = populated();
        let allowed = ws
            .prepare("glob", json!({ "pattern": "**", "path": "src/deep" }))
            .unwrap();
        let granted = ws.policy.authorize(allowed).unwrap();
        let permit = granted.permit();
        let wider = ws
            .prepare("glob", json!({ "pattern": "**", "path": "src" }))
            .unwrap();

        let err = ws
            .execute("glob", wider, permit)
            .expect_err("the permit names src/deep, not src");

        assert!(
            matches!(err, ToolError::Denied(PermitDenied::List { .. })),
            "expected Denied(List), got {err:?}"
        );
        assert!(err.to_string().contains("list denied"));
    }

    #[test]
    fn glob_does_not_descend_into_the_builtin_skips() {
        let ws = populated();
        ws.file("target/debug/generated.rs", "// built");

        let content = ws
            .call("glob", json!({ "pattern": "**/*.rs" }))
            .expect("the working directory is the workspace");

        assert!(
            !content.contains("target"),
            "`target` is one of the builtin skips: {content}"
        );
    }
}

mod grep {
    use super::*;

    #[test]
    fn grep_returns_matching_lines_with_their_path_and_a_one_based_line_number() {
        let ws = populated();

        let content = ws
            .call("grep", json!({ "query": "TODO", "path": "src" }))
            .expect("src is inside the workspace");

        assert_eq!(
            content, "deep/lib.rs:1:// TODO: write the library",
            "the first line of a file is line 1, the way every editor counts"
        );
    }

    #[test]
    fn grep_matches_a_regular_expression_and_not_only_a_literal() {
        let ws = populated();

        let content = ws
            .call("grep", json!({ "query": "^pub fn \\w+", "path": "src" }))
            .expect("src is inside the workspace");

        assert!(content.contains("pub fn lib()"), "got {content:?}");
    }

    #[test]
    fn grep_says_so_when_the_builtin_limits_stopped_it_short() {
        // Same obligation as `glob`, and it matters more here: a grep that
        // reports "no matches" when it stopped looking sends a model off to
        // conclude the string is not in the repository.
        let ws = populated();
        let deep: String = std::iter::repeat_n("d", 34).collect::<Vec<_>>().join("/");
        ws.file(&format!("{deep}/buried.rs"), "// TODO: too far down");

        let content = ws
            .call("grep", json!({ "query": "TODO" }))
            .expect("the working directory is the workspace");

        assert!(
            !content.contains("too far down"),
            "the limit did not bite: {content}"
        );
        assert!(
            content.contains("this answer is incomplete"),
            "a truncated search has to say so: {content}"
        );
    }

    #[test]
    fn grep_prepares_a_search_and_therefore_cannot_read_what_it_searched() {
        // `Op::Search` rather than `List` + `Read { root }`: the seam reads
        // the subtree, the tool never receives read authority over it, and the
        // query that was authorised is the query that runs.
        let ws = populated();
        let call = ws
            .prepare("grep", json!({ "query": "TODO", "path": "src" }))
            .unwrap();

        assert!(matches!(
            call.ops.as_slice(),
            [Op::Search { query, .. }] if query == "TODO"
        ));
        let granted = ws.policy.authorize(call).unwrap();
        let permit = granted.permit();
        let searched = CanonPath::new(ws.root.join("src/main.rs"));
        assert!(permit.allows_search(&searched).is_ok());
        assert!(
            matches!(
                permit.allows_read(&searched),
                Err(PermitDenied::Read { .. })
            ),
            "a search grant must not authorise reading what it searched"
        );
        assert!(
            matches!(
                permit.allows_list(&searched),
                Err(PermitDenied::List { .. })
            ),
            "nor listing it"
        );
    }

    #[test]
    fn grep_reports_no_match_rather_than_an_empty_answer() {
        let ws = populated();

        let content = ws
            .call(
                "grep",
                json!({ "query": "nowhere-to-be-found", "path": "src" }),
            )
            .expect("the root is fine; nothing matches");

        assert!(content.contains("no line"), "got {content:?}");
    }

    #[test]
    fn a_query_that_does_not_compile_is_reported_at_prepare() {
        let ws = populated();

        let err = ws
            .prepare("grep", json!({ "query": "(unclosed", "path": "src" }))
            .expect_err("an unbalanced group is not a regex");

        assert!(
            matches!(err, ToolError::InvalidParams(_)),
            "expected InvalidParams, got {err:?}"
        );
    }

    #[test]
    fn grep_outside_the_workspace_is_refused_and_named_as_a_search() {
        let ws = populated();
        let outside = Workspace::new();

        let call = ws
            .prepare(
                "grep",
                json!({ "query": "password", "path": outside.root.display().to_string() }),
            )
            .expect("the directory exists, so it resolves");
        let denied = ws
            .policy
            .authorize(call)
            .expect_err("it is outside the workspace");

        assert!(matches!(
            denied,
            PolicyDenied::OutsideWorkspace { op: "search", .. }
        ));
        assert!(denied.to_string().contains("may not search"));
    }

    #[test]
    fn grep_under_a_permit_that_does_not_cover_its_root_is_denied_by_name() {
        let ws = populated();
        let allowed = ws
            .prepare("grep", json!({ "query": "TODO", "path": "src/deep" }))
            .unwrap();
        let granted = ws.policy.authorize(allowed).unwrap();
        let permit = granted.permit();
        let wider = ws
            .prepare("grep", json!({ "query": "TODO", "path": "src" }))
            .unwrap();

        let err = ws
            .execute("grep", wider, permit)
            .expect_err("the permit names src/deep, not src");

        assert!(
            matches!(err, ToolError::Denied(PermitDenied::Search { .. })),
            "expected Denied(Search), got {err:?}"
        );
        assert!(err.to_string().contains("search denied"));
    }

    #[test]
    fn a_search_that_reaches_a_file_symlinked_out_of_the_root_fails_the_whole_call() {
        // Not filtered out — refused. By the time there is something to
        // filter, the file has already been read, and the model would be told
        // "here are the matches" when the truthful answer is "you may not look
        // there".
        let ws = populated();
        let outside = Workspace::new();
        let secret = outside.file("secret.txt", "TODO: not yours\n");
        std::os::unix::fs::symlink(&secret, ws.root.join("src/innocent.rs"))
            .expect("the symlink is created");

        let err = ws
            .call("grep", json!({ "query": "TODO", "path": "src" }))
            .expect_err("one entry left the authorised subtree");

        assert!(
            matches!(err, ToolError::Denied(PermitDenied::Search { .. })),
            "expected Denied(Search), got {err:?}"
        );
    }
}

mod end_to_end {
    use super::*;

    /// A whole turn, with only the model faked: a real `OsFs`, the real
    /// builtins, a real `PolicyEngine`, a real `SystemClock` and a real
    /// `JsonlStore` writing to disk.
    #[test]
    fn a_turn_drives_a_real_read_from_the_model_s_call_to_the_file_s_contents() {
        let ws = populated();
        let store_dir = TempDir::new().expect("a temp directory");
        let store = JsonlStore::new(store_dir.path(), Box::new(OsIo));
        let clock = SystemClock;
        let generator = FakeGenerator::new([
            vec![
                AiEvent::ToolCall {
                    call_id: "call_1".to_string(),
                    name: "read".to_string(),
                    args: json!({ "path": "src/main.rs" }).to_string(),
                },
                AiEvent::Stop(StopReason::ToolUse),
            ],
            vec![
                AiEvent::TextDelta {
                    text: "it prints hello".to_string(),
                },
                AiEvent::Stop(StopReason::Stop),
            ],
        ]);
        let sink = CollectingSink::new();
        let mut gate = BudgetGate::new(usd("1000"));

        let outcome = {
            let mut loop_ = TurnLoop::new(
                Session {
                    id: SessionId::new(),
                    state: SessionState::default(),
                    store: &store,
                    clock: &clock,
                    sink: &sink,
                    cancel: CancelToken::new(),
                },
                Model {
                    generator: &generator,
                    gate: &mut gate,
                    quote: quote(),
                    caps: caps(),
                    config: config(),
                },
                Tools::new(&ws.registry, &ws.policy, &ws.fs, ws.cwd()),
            );
            block_on(loop_.run("what does main.rs print?")).expect("the log is writable")
        };

        assert_eq!(outcome.exit_reason, ExitReason::Completed);
        assert!(outcome.ok);

        // The model was told what the tool descriptors said it could call...
        let offered: Vec<String> = generator.calls()[0]
            .tools
            .iter()
            .map(|tool| tool.name.clone())
            .collect();
        assert!(offered.contains(&"read".to_string()), "got {offered:?}");

        // ...and the second request carries the file's real contents back,
        // which is only possible if the whole chain ran: prepare resolved the
        // path on disk, policy minted a permit for it, and `GuardedFs` opened
        // it.
        let history = format!("{:?}", generator.calls()[1].messages);
        assert!(
            history.contains("println!(\\\"hello\\\")") || history.contains("println!(\"hello\")"),
            "the tool result never reached the model: {history}"
        );
    }

    #[test]
    fn a_turn_stops_when_a_tool_call_is_refused_and_names_what_was_refused() {
        // This test used to assert the opposite ending: the denial was logged
        // as a failed result, the model read it in the next generation, and
        // the turn completed. `CLI.md` §4.4 is ratified and says otherwise —
        // a run with nobody to ask does not answer a permission question on
        // the model's behalf; it stops with the denied action in the result.
        //
        // What it protected is protected harder here: the denial still names
        // what it refused, the refused *contents* still never reach the
        // model, and now there is no second generation for them to reach.
        // See `docs/decisions/2026-08-21-two-levels-of-ending.md` §4.
        let ws = populated();
        let outside = Workspace::new();
        let secret = outside.file("secret.txt", "not yours");
        let store_dir = TempDir::new().expect("a temp directory");
        let store = JsonlStore::new(store_dir.path(), Box::new(OsIo));
        let clock = SystemClock;
        let generator = FakeGenerator::new([
            vec![
                AiEvent::ToolCall {
                    call_id: "call_1".to_string(),
                    name: "read".to_string(),
                    args: json!({ "path": secret.display().to_string() }).to_string(),
                },
                AiEvent::Stop(StopReason::ToolUse),
            ],
            vec![
                AiEvent::TextDelta {
                    text: "I am not allowed to read that".to_string(),
                },
                AiEvent::Stop(StopReason::Stop),
            ],
        ]);
        let sink = CollectingSink::new();
        let mut gate = BudgetGate::new(usd("1000"));

        let outcome = {
            let mut loop_ = TurnLoop::new(
                Session {
                    id: SessionId::new(),
                    state: SessionState::default(),
                    store: &store,
                    clock: &clock,
                    sink: &sink,
                    cancel: CancelToken::new(),
                },
                Model {
                    generator: &generator,
                    gate: &mut gate,
                    quote: quote(),
                    caps: caps(),
                    config: config(),
                },
                Tools::new(&ws.registry, &ws.policy, &ws.fs, ws.cwd()),
            );
            block_on(loop_.run("read that file")).expect("the log is writable")
        };

        assert_eq!(outcome.exit_reason, ExitReason::PermissionRequired);
        assert!(!outcome.ok);

        let denied = outcome
            .denied
            .as_deref()
            .expect("a permission-required turn names the action it was refused");
        assert!(
            denied.contains("outside the workspace") && denied.contains("secret.txt"),
            "the denial must name what it denied: {denied}"
        );
        assert!(
            !denied.contains("not yours"),
            "and must not quote what it refused to read: {denied}"
        );

        assert_eq!(
            generator.calls().len(),
            1,
            "the run stopped, so there is no second generation"
        );
        let history = format!("{:?}", generator.calls()[0].messages);
        assert!(
            !history.contains("not yours"),
            "the refused contents must not reach the model: {history}"
        );
    }

    fn usd(decimal: &str) -> Money {
        Money::new(
            decimal.parse::<Amount>().expect("a valid test amount"),
            CurrencyCode::new("USD").expect("USD is a valid code"),
        )
    }

    fn quote() -> PricingQuote {
        let rate = |d: &str| Some(d.parse::<Amount>().expect("a valid test rate"));
        PricingQuote {
            revision: PricingRevision("test-2026-08".to_string()),
            currency: CurrencyCode::new("USD").expect("USD is a valid code"),
            uncached_input: rate("0.000003"),
            cache_read: rate("0.0000003"),
            cache_write: rate("0.00000375"),
            output: rate("0.000015"),
        }
    }

    fn caps() -> ModelCapabilities {
        ModelCapabilities {
            schema_version: 1,
            max_context_tokens: Some(200_000),
            max_output_tokens: Some(64_000),
            max_tools: Some(128),
            supports_parallel_tool_calls: true,
            reasoning_carryover: ReasoningCarryover::Unsupported,
            reasoning_efforts: ReasoningEffortSupport::Any,
            cancel: CancelSemantics::Cooperative,
            cache: CacheRetention::Short,
            transport: Transport::Sse,
        }
    }

    fn config() -> TurnConfig {
        TurnConfig {
            model: "test-model".to_string(),
            thinking: ThinkingLevel::Off,
            max_output_tokens: Some(1_024),
            parallel_tool_calls: false,
            system: None,
        }
    }
}

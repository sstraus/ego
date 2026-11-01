//! The pure halves of the command line: what argv means, what a resolved
//! ceiling becomes, and where a prompt comes from.
//!
//! These are functions, so they are tested as functions. Everything that needs
//! a *process* — an exit status, two streams, a signal — is in `run.rs`, and
//! the split is deliberate: a table of thirty budget spellings does not need
//! thirty forks of a binary to say what it says.
//!
//! The budget *grammar* is no longer here. It moved to `ego-core` with the
//! type (`crates/ego-core/tests/ceiling.rs`), because `--budget` and a config
//! file's `budget` key are one parser with two surfaces, and a table of
//! spellings sitting next to the flag would have said nothing about the key.

use std::io::Result as IoResult;
use std::path::{Path, PathBuf};

use ego_cli::budget;
use ego_cli::cli::{self, Invocation, Output, SessionChoice, Stdin};
use ego_cli::prompt::{self, StdinSource};
use ego_core::budget::Ledger;
use ego_core::ceiling::Ceilings;
use ego_core::config::{Mode, ModelRef};
use ego_core::cost::{Amount, CurrencyCode, Money};
use ego_core::permit::Confinement;
use ego_core::permit::rule::Head;

fn argv<const N: usize>(args: [&str; N]) -> Vec<String> {
    argv_slice(&args)
}

/// `run` and `resume` both produce an [`Args`](ego_cli::cli::Args) — that is
/// the point of them sharing one — so one helper unwraps either, and every
/// table below can be pointed at whichever subcommand it is about.
///
/// The array form exists only because most tables are written as literals; it
/// is the slice form with one coercion, so the exhaustive match that keeps this
/// honest as subcommands are added lives in exactly one place.
fn parsed<const N: usize>(args: [&str; N]) -> ego_cli::cli::Args {
    parsed_slice(&args)
}

fn parsed_slice(args: &[&str]) -> ego_cli::cli::Args {
    match cli::parse(argv_slice(args)).expect("the command line parses") {
        Invocation::Run(args) | Invocation::Resume(args) => *args,
        Invocation::Interactive(_) => {
            panic!("that starts a session: use `converses` for it")
        }
        Invocation::Ls(_) => panic!("`ls` takes no Args: use `listed` for it"),
        Invocation::Models(_) => panic!("`models` takes no Args: use the models tests for it"),
        Invocation::Acp(_) => panic!("`acp` takes no Args: use `served` for it"),
        Invocation::A2a(_) => panic!("`a2a` takes no Args: use the A2A tests for it"),
        Invocation::Auth(_) => panic!("`auth` takes no Args: use the auth tests for it"),
        Invocation::Mcp(_) => panic!("`mcp` takes no Args: use the MCP tests for it"),
        Invocation::McpServer(_) => {
            panic!("`mcp-server` takes no Args: use the MCP server tests for it")
        }
        Invocation::Answered(_) => panic!("that answered a question: use `answered` for it"),
        Invocation::Rm(_) => panic!("`rm` takes no Args: use the rm tests for it"),
        Invocation::Fork(_) => panic!("`fork` takes no Args: use the fork tests for it"),
        Invocation::Compact(_) => panic!("`compact` takes no Args: use the compact tests for it"),
        Invocation::Export(_) => panic!("`export` takes no Args: use the export tests for it"),
        Invocation::Usage(_) => panic!("`usage` takes no Args: use the usage tests for it"),
        Invocation::Hooks(_) => panic!("`hooks` takes no Args: use the hooks tests for it"),
        Invocation::Config(_) => panic!("`config` takes no Args: use the config tests for it"),
        Invocation::Rules(_) => panic!("`rules` takes no Args: use the rules tests for it"),
        Invocation::SessionMetadata(_) => panic!("metadata is covered by metadata grammar tests"),
        Invocation::Worktree(_) => panic!("worktree lifecycle does not construct a run"),
        Invocation::Setup(_) => panic!("setup does not construct a run"),
        Invocation::Doctor(_) => panic!("`doctor` takes no Args: use the doctor tests for it"),
        Invocation::Checkpoint(_) => panic!("checkpoint does not construct a run"),
    }
}

fn argv_slice(args: &[&str]) -> Vec<String> {
    std::iter::once("ego".to_string())
        .chain(args.iter().map(|arg| arg.to_string()))
        .collect()
}

/// The text `--help` or `--version` produced.
///
/// A separate helper from [`refused`] on purpose, and the separation is the
/// property: both used to come back through `Err`, so `ego ls --help` was a
/// usage error printed on stderr at exit 2. A test that reached for `refused`
/// here would still pass under that bug.
fn answered<const N: usize>(args: [&str; N]) -> String {
    match cli::parse(argv(args)).expect("a question is answered, not refused") {
        Invocation::Answered(text) => text,
        _ => panic!("that is a command, not a question about one"),
    }
}

fn listed<const N: usize>(args: [&str; N]) -> ego_cli::cli::LsArgs {
    match cli::parse(argv(args)).expect("the command line parses") {
        Invocation::Ls(args) => *args,
        _ => panic!("that is not `ego ls`"),
    }
}

fn served<const N: usize>(args: [&str; N]) -> ego_cli::cli::AcpArgs {
    match cli::parse(argv(args)).expect("the command line parses") {
        Invocation::Acp(args) => *args,
        _ => panic!("that is not `ego acp`"),
    }
}

fn a2a<const N: usize>(args: [&str; N]) -> ego_cli::cli::A2aArgs {
    match cli::parse(argv(args)).expect("the command line parses") {
        Invocation::A2a(args) => *args,
        _ => panic!("that is not `ego a2a`"),
    }
}

/// The [`Args`](ego_cli::cli::Args) of an invocation that starts a session.
///
/// Separate from [`parsed`] on purpose: `ego hello` and `ego run hello` differ
/// in exactly one way — who supplies the second prompt — and a helper that
/// flattened them would let a regression swap one for the other unnoticed.
fn converses<const N: usize>(args: [&str; N]) -> ego_cli::cli::Args {
    match cli::parse(argv(args)).expect("the command line parses") {
        Invocation::Interactive(args) => *args,
        other => panic!("that does not start a session: {other:?}"),
    }
}

fn refused<const N: usize>(args: [&str; N]) -> String {
    cli::parse(argv(args))
        .expect_err("the command line should have been refused")
        .to_string()
}

fn refused_slice(args: &[&str]) -> String {
    cli::parse(argv_slice(args))
        .expect_err("the command line should have been refused")
        .to_string()
}

fn usd() -> CurrencyCode {
    CurrencyCode::new("USD").expect("USD is a currency")
}

// --- argv ---

/// Honor `--` before deciding subcommands. `ego -- run` starts an interactive prompt containing
/// `run`, not a headless command.
#[test]
fn the_separator_stops_a_subcommand_name_from_being_one() {
    assert_eq!(converses(["--", "run"]).prompt.as_deref(), Some("run"));
    assert_eq!(
        parsed(["run", "--", "--model"]).prompt.as_deref(),
        Some("--model")
    );
    // `run` alone cannot prove the rule: a separator check that had stopped
    // working would still start a session for it by the ordinary route. A name
    // with a *competing* meaning tells the two apart, and every name in either
    // table has one — `fork` is refused by name and `resume` is a command that
    // would have run, so the separator is the only thing that can be turning
    // both into prompts.
    for name in ["resume", "fork"] {
        assert_eq!(
            converses(["--", name]).prompt.as_deref(),
            Some(name),
            "{name}"
        );
    }
}

/// Without a subcommand the invocation is the interactive one, and it now
/// starts instead of refusing.
///
/// **This test has been through the decay it exists to catch, twice.** It first
/// asserted only that a refusal *happened*; the refusal then claimed the
/// surface waited on "an approver that can mint a `Permit`", the `Approver`
/// port landed, the sentence became false and this stayed green. So it was
/// changed to assert the reason character for character — and that is why it
/// went red the moment the line editor shipped, which is the whole point.
///
/// What it guards now is the other direction: the surface starts, and the
/// constant may no longer claim the editor is what is missing. A reason nothing
/// checks is a reason that decays into a confident falsehood, and a reason that
/// outlives the gap it names is the same defect wearing the opposite sign.
#[test]
fn an_invocation_with_no_subcommand_starts_the_surface_it_used_to_refuse() {
    assert_eq!(converses(["hello"]).prompt.as_deref(), Some("hello"));
}

/// The sandbox flag is a real run policy, not a retired spelling. Both
/// `--sandbox=value` and the split form must produce the same exact value for
/// the later config merge; neither form may be swallowed by the availability
/// table.
#[test]
fn the_sandbox_flag_is_parsed_in_both_clap_spellings() {
    for (args, expected) in [
        (
            &["run", "--sandbox=workspace", "Ask"][..],
            Some(ego_core::permit::Confinement::Workspace),
        ),
        (
            &["run", "--sandbox", "ro", "Ask"][..],
            Some(ego_core::permit::Confinement::Ro),
        ),
        (
            &["run", "--sandbox=off", "Ask"][..],
            Some(ego_core::permit::Confinement::Off),
        ),
    ] {
        assert_eq!(parsed_slice(args).sandbox, expected);
    }
}

/// Retired flags must fail by name and name their replacement. Exercise the mechanism even
/// while the production retirement table is empty.
#[test]
fn a_retired_flag_would_be_refused_with_its_replacement() {
    for (retired, replacement) in cli::RETIRED {
        let refusal = refused(["run", retired, "Ask"]);
        assert!(refusal.contains(retired), "{refusal}");
        assert!(refusal.contains(replacement), "{refusal}");
    }
}

/// Unavailable recognized commands must fail by name with a concrete missing subsystem, not
/// fall through to an unrelated interactive refusal.
#[test]
fn a_documented_subcommand_this_build_lacks_is_refused_by_its_own_name() {
    for (name, waiting_on) in cli::UNBUILT {
        let refusal = refused([name]);
        assert!(
            refusal.contains(name),
            "`ego {name}` must be refused by name: {refusal}"
        );
        assert!(
            refusal.contains(waiting_on),
            "`ego {name}` must say what it waits on: {refusal}"
        );
        assert!(
            !refusal.contains("interactive"),
            "`ego {name}` named a command, so the interactive surface is not \
             the thing it is missing: {refusal}"
        );
    }
}

/// Unrecognized command text remains a prompt. Refusing every unknown first token would break
/// `ego "explain this"`.
#[test]
fn a_word_that_is_not_a_documented_command_is_still_a_prompt() {
    for word in ["hello", "resumee", "explain this failing test"] {
        assert_eq!(converses([word]).prompt.as_deref(), Some(word), "{word}");
    }
}

/// Collapse every run of whitespace to one space, so a Rust string broken
/// across lines with `\` continuations and a markdown table cell on one long
/// line can be compared as the same sentence. Nothing else is normalised:
/// backticks, em dashes and `**M3**` must match exactly, because they are what
/// the operator reads.
fn normalised(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Bind unavailable-command diagnostics to the document verbatim, not just command names.
/// Otherwise prose and executable refusals can drift independently.
#[test]
fn every_waiting_on_reason_is_the_one_the_document_prints() {
    let doc = normalised(include_str!("../../../CLI.md"));
    for (name, waiting_on) in cli::UNBUILT {
        assert!(
            doc.contains(&normalised(waiting_on)),
            "`{name}`'s reason is not in CLI.md §2.1 verbatim. The code says:\n\
             {waiting_on}\n\
             Change the table to quote it, or change both."
        );
    }
}

/// Every name the code refuses is one the document actually offers. Without
/// this the table could drift into refusing a command `CLI.md` never promised,
/// which is a worse answer than the interactive fallback it replaced: it would
/// advertise a roadmap item that does not exist.
///
/// Binding a test to the document rather than to a copy of it follows
/// `ego-ai/tests/http.rs:552`, which reads `Cargo.toml` for the same reason.
#[test]
fn every_unbuilt_subcommand_is_one_the_document_offers() {
    let doc = include_str!("../../../CLI.md");
    let commands = doc
        .split("<!-- ego-cli-command-set:start -->")
        .nth(1)
        .expect("CLI.md has the command-set start anchor")
        .split("<!-- ego-cli-command-set:end -->")
        .next()
        .expect("CLI.md has the command-set end anchor");
    for (name, _) in cli::UNBUILT {
        assert!(
            commands.contains(&format!("ego {name}")),
            "`{name}` is refused as documented but CLI.md §2 does not list it"
        );
    }
}

/// `--no-tools` and `--tools` answer the same question, so they are one field.
/// Two would let them disagree, and there is no right answer to
/// `--tools read --no-tools`.
#[test]
fn no_tools_is_the_empty_tool_list_and_not_a_second_flag() {
    assert_eq!(parsed(["run", "--no-tools", "Ask"]).tools, Some(Vec::new()));
    assert_eq!(
        parsed(["run", "--tools", "read,glob", "Ask"]).tools,
        Some(vec!["read".to_string(), "glob".to_string()])
    );
    assert_eq!(parsed(["run", "Ask"]).tools, None);
    let refusal = refused(["run", "--tools", "read", "--no-tools", "Ask"]);
    assert!(refusal.contains("--no-tools"), "{refusal}");
}

/// Mutually exclusive durable-session choices occupy one value.
#[test]
fn the_session_flags_collapse_into_one_choice() {
    assert_eq!(parsed(["run", "Ask"]).session, SessionChoice::New);
    assert_eq!(
        parsed(["run", "--session", "build", "Ask"]).session,
        SessionChoice::Named("build".to_string())
    );
    assert_eq!(
        parsed(["run", "--session-id", "abc", "Ask"]).session,
        SessionChoice::Id("abc".to_string())
    );
    let refusal = refused(["run", "--ephemeral", "Ask"]);
    assert!(refusal.contains("--ephemeral"), "{refusal}");
}

/// The mode group, one axis over, and for the same reason: `--plan` and
/// `--yolo` are spellings of `--mode`, so "which mode" has one answer.
///
/// **This test exists because a mutation survived without it.** `run.rs`
/// drives the whole binary, and the builtins are read-only — so every mode
/// admits a workspace read, refuses a workspace escape and never asks about
/// either, and *no* end-to-end assertion can tell them apart. Mapping both
/// `--plan` and `--yolo` to `Mode::Default` left all 787 tests green.
///
/// The engine's own tests already prove the five modes behave differently
/// (`permit_step4_the_ceiling_answers_a_write_differently_in_every_mode`).
/// What nothing checked was the wire between the flag and the engine, which
/// is exactly the seam a process-level test cannot see and a function-level
/// one can. That split is this file's whole premise.
#[test]
fn the_mode_flags_collapse_into_one_value() {
    assert_eq!(parsed(["run", "Ask"]).mode, None);
    assert_eq!(parsed(["run", "--plan", "Ask"]).mode, Some(Mode::Plan));
    assert_eq!(parsed(["run", "--yolo", "Ask"]).mode, Some(Mode::Yolo));
    for (spelling, mode) in [
        ("plan", Mode::Plan),
        ("default", Mode::Default),
        ("auto", Mode::Auto),
        ("edits", Mode::Edits),
        ("accept-edits", Mode::Edits),
        ("yolo", Mode::Yolo),
    ] {
        assert_eq!(
            parsed(["run", "--mode", spelling, "Ask"]).mode,
            Some(mode),
            "--mode {spelling}"
        );
    }
    let refusal = refused(["run", "--plan", "--yolo", "Ask"]);
    assert!(
        refusal.contains("--plan") && refusal.contains("--yolo"),
        "{refusal}"
    );
}

/// The rules arrive in the order they were written, and both lists are kept.
/// Rule order is first-match-wins within a list. Sorting or deduplicating
/// arguments could change denial attribution. Flags stay text until the
/// shared configuration parser loads them.
#[test]
fn the_rule_flags_are_repeatable_and_keep_their_order() {
    let args = parsed([
        "run",
        "--allow",
        "read(a/**)",
        "--deny",
        "write(**)",
        "--allow",
        "read(b/**)",
        "Ask",
    ]);
    assert_eq!(args.allow, ["read(a/**)", "read(b/**)"]);
    assert_eq!(args.deny, ["write(**)"]);

    let none = parsed(["run", "Ask"]);
    assert!(none.allow.is_empty() && none.deny.is_empty());
}

// --- retry and fallback grammar ---

#[test]
fn retry_defaults_to_zero_and_accepts_explicit_nonnegative_values() {
    assert_eq!(parsed(["run", "--model", "ollama/qwen3", "Ask"]).retry, 0);

    for (value, expected) in [("0", 0), ("3", 3)] {
        assert_eq!(
            parsed_slice(&["run", "--model", "ollama/qwen3", "--retry", value, "Ask"]).retry,
            expected,
            "--retry {value}"
        );
    }
}

#[test]
fn interactive_body_failure_retry_is_not_disabled_by_headless_default() {
    // Catches: a managed interactive seat inherits --retry 0 and goes idle on a dropped body.
    for args in [&["Ask"][..], &["resume"][..]] {
        let Invocation::Interactive(args) = cli::parse(argv_slice(args)).unwrap() else {
            panic!("interactive seat")
        };
        assert_eq!(args.retry, 6);
    }
    assert_eq!(converses(["--retry", "0", "Ask"]).retry, 0);
    assert_eq!(converses(["--retry", "2", "Ask"]).retry, 2);
    assert_eq!(parsed(["run", "Ask"]).retry, 0);
}

#[test]
fn repeatable_fallbacks_preserve_ordered_model_refs() {
    let args = parsed([
        "run",
        "--model",
        "ollama/qwen3",
        "--fallback",
        "openai/gpt-5.1",
        "--fallback",
        "anthropic/claude-sonnet-4-6",
        "Ask",
    ]);

    assert_eq!(
        args.fallbacks,
        vec![
            ModelRef("openai/gpt-5.1".to_owned()),
            ModelRef("anthropic/claude-sonnet-4-6".to_owned()),
        ]
    );
}

#[test]
fn retry_and_fallback_values_refuse_when_missing_or_malformed() {
    assert!(refused(["run", "--model", "ollama/qwen3", "--retry"]).contains("--retry"));
    for value in ["nope", "-1"] {
        let refusal = refused(["run", "--model", "ollama/qwen3", "--retry", value, "Ask"]);
        assert!(
            !refusal.is_empty(),
            "malformed retry value was accepted: {value}"
        );
    }

    assert!(refused(["run", "--model", "ollama/qwen3", "--fallback"]).contains("--fallback"));
    for value in ["ollama", "/qwen3", "ollama/", "not a model"] {
        let refusal = refused(["run", "--model", "ollama/qwen3", "--fallback", value, "Ask"]);
        assert!(
            !refusal.is_empty(),
            "malformed fallback value was accepted: {value}"
        );
    }
}

/// `-` is not a prompt that happens to be a hyphen; it says where the prompt
/// is.
#[test]
fn a_lone_dash_says_the_prompt_is_on_stdin() {
    let args = parsed(["run", "-"]);
    assert_eq!(args.prompt, None);
    assert_eq!(args.stdin, Stdin::IsThePrompt);
    assert_eq!(parsed(["run", "Ask"]).stdin, Stdin::Ambient);
}

#[test]
fn the_output_mode_defaults_to_text_and_takes_the_three_names() {
    assert_eq!(parsed(["run", "Ask"]).output, Output::Text);
    for (name, expected) in [
        ("text", Output::Text),
        ("json", Output::Json),
        ("jsonl", Output::Jsonl),
    ] {
        assert_eq!(parsed(["run", "--output", name, "Ask"]).output, expected);
    }
    assert!(refused(["run", "--output", "yaml", "Ask"]).contains("yaml"));
}

// --- budgets ---

fn usd_money(decimal: &str) -> Money {
    Money::new(decimal.parse::<Amount>().expect("a valid amount"), usd())
}

/// A run with only a token ceiling still gets a gate, because the loop cannot
/// run without one — and the gate is genuinely unlimited, because the ceiling
/// that run is subject to is checked at the generation boundary instead.
#[test]
fn a_gate_exists_even_when_the_ceiling_is_counted_in_tokens() {
    let tokens_only = budget::gate(
        &Ceilings {
            money: None,
            tokens: Some(400_000),
        },
        None,
        usd(),
    );
    let nothing = budget::gate(&Ceilings::default(), None, usd());

    assert_eq!(*tokens_only.ledger(), Ledger::Unlimited);
    assert_eq!(*nothing.ledger(), Ledger::Unlimited);
    assert_eq!(
        tokens_only.currency(),
        usd(),
        "and it still knows what the session settles in"
    );
}

/// A limited ledger retains the requested figure when an administrator clamps it, so the
/// published limit does not misrepresent what binds.
#[test]
fn a_money_ceiling_becomes_the_ledger_and_a_clamp_is_carried_into_it() {
    let plain = budget::gate(
        &Ceilings {
            money: Some(usd_money("5")),
            tokens: None,
        },
        None,
        usd(),
    );
    assert_eq!(
        *plain.ledger(),
        Ledger::Limited {
            limit: usd_money("5"),
            clamped_from: None,
            committed: usd_money("0"),
        }
    );

    let clamped = budget::gate(
        &Ceilings {
            money: Some(usd_money("5")),
            tokens: None,
        },
        Some(usd_money("20")),
        usd(),
    );
    assert_eq!(
        *clamped.ledger(),
        Ledger::Limited {
            limit: usd_money("5"),
            clamped_from: Some(usd_money("20")),
            committed: usd_money("0"),
        }
    );
}

// --- the prompt ---

struct Piped(Option<&'static str>);

impl StdinSource for Piped {
    fn is_terminal(&self) -> bool {
        self.0.is_none()
    }

    fn read(&self) -> IoResult<String> {
        Ok(self.0.unwrap_or_default().to_string())
    }
}

/// Check every prompt-source precedence combination so no supplied input is silently dropped.
#[test]
fn every_row_of_the_input_precedence_table_lands_somewhere() {
    // `-` given: stdin is the prompt, whatever else was passed.
    assert_eq!(
        prompt::assemble(None, Stdin::IsThePrompt, &Piped(Some("from the pipe")))
            .expect("a prompt"),
        "from the pipe"
    );
    // An argument and a terminal on stdin: the argument.
    assert_eq!(
        prompt::assemble(Some("Ask"), Stdin::Ambient, &Piped(None)).expect("a prompt"),
        "Ask"
    );
    // An argument and a pipe: the argument, then the pipe, fenced.
    assert_eq!(
        prompt::assemble(Some("Ask"), Stdin::Ambient, &Piped(Some("context"))).expect("a prompt"),
        "Ask\n\n--- piped input ---\ncontext\n---"
    );
    // A pipe and no argument: the pipe.
    assert_eq!(
        prompt::assemble(None, Stdin::Ambient, &Piped(Some("just this"))).expect("a prompt"),
        "just this"
    );
    // Neither: refused, naming one way to supply a prompt.
    let refusal = prompt::assemble(None, Stdin::Ambient, &Piped(None))
        .expect_err("nothing to ask")
        .to_string();
    assert!(
        refusal.contains("no prompt was supplied; pass your prompt as an argument"),
        "{refusal}"
    );
}

/// An empty pipe beside an argument is not a context block of nothing: it is a
/// run with only the argument, which is what the author typed.
#[test]
fn an_empty_pipe_beside_an_argument_adds_no_fence() {
    assert_eq!(
        prompt::assemble(Some("Ask"), Stdin::Ambient, &Piped(Some("   \n"))).expect("a prompt"),
        "Ask"
    );
}

/// Interactive input has the same precedence as one-shot input, except that
/// absence is itself a valid seed. In particular, a closed non-TTY stdin is
/// absence rather than an explicit promise that a prompt exists.
#[test]
fn every_row_of_the_interactive_seed_table_lands_somewhere() {
    let cases = [
        (None, Stdin::Ambient, Piped(None), ""),
        (None, Stdin::Ambient, Piped(Some("")), ""),
        (
            None,
            Stdin::Ambient,
            Piped(Some("from the pipe")),
            "from the pipe",
        ),
        (Some("Ask"), Stdin::Ambient, Piped(None), "Ask"),
        (Some("Ask"), Stdin::Ambient, Piped(Some("")), "Ask"),
        (
            Some("Ask"),
            Stdin::Ambient,
            Piped(Some("context")),
            "Ask\n\n--- piped input ---\ncontext\n---",
        ),
    ];

    for (argv, mode, stdin, expected) in cases {
        assert_eq!(
            prompt::seed(argv, mode, &stdin).expect("an interactive seed"),
            expected
        );
    }

    let refusal = prompt::seed(None, Stdin::IsThePrompt, &Piped(Some("")))
        .expect_err("explicit `-` still promises a prompt")
        .to_string();
    assert!(refusal.contains("stdin was empty"), "{refusal}");
}

/// `-` with nothing on stdin is refused rather than run as the empty prompt:
/// the caller said the prompt was there and it was not.
#[test]
fn a_dash_with_an_empty_pipe_is_refused() {
    let refusal = prompt::assemble(None, Stdin::IsThePrompt, &Piped(Some("")))
        .expect_err("nothing to ask")
        .to_string();
    assert!(refusal.contains("stdin was empty"), "{refusal}");
}

/// A missing `@file` is a usage error, not a warning: a run whose system prompt
/// silently became the literal text `@missing.md` has been misled about what it
/// asked for.
#[test]
fn an_at_file_that_is_not_there_is_refused_rather_than_taken_literally() {
    assert_eq!(
        prompt::at_file("plain text").expect("not an @file"),
        "plain text"
    );
    let refusal = prompt::at_file("@definitely/not/here.md")
        .expect_err("it is not there")
        .to_string();
    assert!(refusal.contains("definitely/not/here.md"), "{refusal}");
}

// --- `ego resume` and `ego ls` ---

const SOME_ID: &str = "0192f3a1-1c2b-7d3e-9a4f-5b6c7d8e8c41";

/// Resume with both an existing session ID and a prompt.
#[test]
fn resume_takes_an_id_and_a_prompt() {
    let args = parsed(["resume", SOME_ID, "next step"]);
    assert_eq!(
        args.session,
        SessionChoice::Resume(Some(SOME_ID.to_string()))
    );
    assert_eq!(args.prompt.as_deref(), Some("next step"));
}

/// A single resume positional is an ID only if it parses as one; otherwise it is the next
/// prompt for the latest session.
#[test]
fn a_lone_positional_that_is_not_an_id_is_the_prompt() {
    let args = parsed(["resume", "next step"]);
    assert_eq!(args.session, SessionChoice::Resume(None));
    assert_eq!(args.prompt.as_deref(), Some("next step"));
}

/// A lone positional that *is* an id is an id, not a prompt that happens to
/// look like one. With no prompt source, the existing interactive driver takes
/// over that session.
#[test]
fn a_lone_positional_that_is_an_id_is_not_read_as_a_prompt() {
    let args = converses(["resume", SOME_ID]);
    assert_eq!(
        args.session,
        SessionChoice::Resume(Some(SOME_ID.to_string()))
    );
    assert_eq!(args.prompt, None);
    assert_eq!(args.stdin, Stdin::Ambient);
}

/// With a second positional the first was unambiguously meant as an id, so a
/// word that is not one is refused by name rather than silently demoted to a
/// prompt with a stray argument after it.
#[test]
fn a_first_of_two_positionals_must_be_an_id() {
    let refusal = refused(["resume", "notanid", "next step"]);
    assert!(refusal.contains("notanid"), "{refusal}");
    assert!(refusal.contains("not a session id"), "{refusal}");
}

/// Promptless resume selects the latest session for interactive continuation.
#[test]
fn resume_with_no_prompt_enters_the_interactive_driver() {
    let args = converses(["resume"]);
    assert_eq!(args.session, SessionChoice::Resume(None));
    assert_eq!(args.prompt, None);
    assert_eq!(args.stdin, Stdin::Ambient);
}

/// `-` is the prompt from stdin here exactly as it is for `run`: one contract,
/// because `resume` is a run against a session that already exists.
#[test]
fn resume_reads_a_prompt_from_stdin_like_run_does() {
    let args = parsed(["resume", SOME_ID, "-"]);
    assert_eq!(args.stdin, Stdin::IsThePrompt);
    assert_eq!(args.prompt, None);
}

/// Each of the session flags names a session, and so does `resume`. Refused by
/// name rather than silently losing to the positional — an ignored `--session`
/// would write a run into a session nobody asked for.
#[test]
fn the_session_flags_and_resume_cannot_both_name_a_session() {
    for flag in ["--session", "--session-id"] {
        let refusal = refused(["resume", "hi", flag, SOME_ID]);
        assert!(refusal.contains(flag), "{refusal}");
        assert!(
            refusal.contains("cannot be combined with 'ego resume'"),
            "{refusal}"
        );
    }
    let refusal = refused(["resume", "hi", "--ephemeral"]);
    assert!(refusal.contains("--ephemeral"), "{refusal}");
}

/// `resume` shares `run`'s whole flag block, which is the reason it produces
/// the same `Args`. A second grammar for a second subcommand is the parallel
/// implementation the reuse rule forbids, and this is what says the reuse is
/// real rather than intended.
#[test]
fn resume_takes_every_flag_run_takes() {
    let args = parsed([
        "resume",
        SOME_ID,
        "go on",
        "--model",
        "ollama/qwen3",
        "--max-turns",
        "3",
        "--output",
        "json",
        "--no-tools",
    ]);
    assert_eq!(args.model.as_deref(), Some("ollama/qwen3"));
    assert_eq!(args.max_turns, Some(3));
    assert_eq!(args.output, Output::Json);
    assert_eq!(args.tools, Some(Vec::new()));
}

/// `ego ls` with nothing else: this workspace, as a table.
#[test]
fn ls_defaults_to_this_workspace_as_text() {
    let args = listed(["ls"]);
    assert!(!args.all);
    assert_eq!(args.output, Output::Text);
    assert_eq!(args.session_dir, None);
}

/// Session listing accepts its documented selection and output flags.
#[test]
fn ls_widens_with_all_and_renders_with_output() {
    let args = listed(["ls", "--all", "--output", "json"]);
    assert!(args.all);
    assert_eq!(args.output, Output::Json);
}

/// The three flags `run`, `resume` and `ls` share come from one declaration,
/// so `--session-dir` cannot mean one store to the command that writes a
/// session and another to the command that lists them.
#[test]
fn ls_honours_the_flags_it_shares_with_run() {
    let args = listed(["ls", "--session-dir", "/tmp/store", "-C", "/tmp/ws"]);
    assert_eq!(
        args.session_dir.as_deref(),
        Some(std::path::Path::new("/tmp/store"))
    );
    assert_eq!(
        args.directory.as_deref(),
        Some(std::path::Path::new("/tmp/ws"))
    );
}

/// `ls` takes no prompt, so a positional is a mistake and clap says so rather
/// than the word being quietly ignored.
#[test]
fn ls_takes_no_positional() {
    let refusal = refused(["ls", "something"]);
    assert!(refusal.contains("something"), "{refusal}");
}

// --- `ego acp` ---

/// `ego acp` with nothing else: serve on stdio, from here, against the default
/// store.
#[test]
fn acp_needs_nothing_to_serve() {
    let args = served(["acp"]);
    assert_eq!(args.session_dir, None);
    assert_eq!(args.directory, None);
    assert_eq!(
        args.listen, None,
        "without `--listen`, ACP stays on its existing stdio transport"
    );
    assert_eq!(
        args.sandbox, None,
        "no ceiling was named, so the resolved configuration must stand \
         rather than a default invented at the parser"
    );
}

/// `--listen` is an opt-in transport change. The endpoint spelling remains
/// opaque to clap: `ego_core::endpoint` decides whether it names a local,
/// loopback or public endpoint after parsing.
#[test]
fn acp_accepts_a_listener_endpoint_without_changing_the_stdio_default() {
    let args = served(["acp", "--listen", "/tmp/ego-listener-test.sock"]);
    assert_eq!(args.listen.as_deref(), Some("/tmp/ego-listener-test.sock"));
}

/// The shared listener controls are valid only when ACP is actually opening
/// an endpoint. Keeping them out of the stdio grammar avoids silently claiming
/// that a process-wide socket bound or ingress budget affects a pipe it does
/// not own.
#[test]
fn acp_listener_only_controls_require_listen() {
    for args in [
        &["acp", "--max-connections", "1"][..],
        &["acp", "--connection-lifetime", "1m"][..],
        &["acp", "--max-ingress-bytes", "4096"][..],
        &["acp", "--allow-public-bind"][..],
    ] {
        let refusal = refused_slice(args);
        assert!(
            refusal.contains("--listen"),
            "listener-only flags must say that `--listen` is required: {refusal}"
        );
    }
}

#[test]
fn a2a_requires_listen_and_inherits_the_acp_sandbox_ceiling() {
    let refusal = refused(["a2a"]);
    assert!(
        refusal.contains("--listen"),
        "A2A must not invent a default endpoint: {refusal}"
    );

    let args = a2a([
        "a2a",
        "--listen",
        "/tmp/ego-listener-test-a2a.sock",
        "--sandbox",
        "ro",
    ]);
    assert_eq!(args.listen, "/tmp/ego-listener-test-a2a.sock");
    assert_eq!(args.sandbox, Some(Confinement::Ro));
}

/// `--sandbox` names the weakest confinement a host may select.
///
/// Headless ACP starts untrusted and does not load Project or Local layers.
/// An explicit flag supplies the value without relying on those files.
#[test]
fn acp_takes_a_confinement_ceiling_by_name() {
    for (spelling, expected) in [
        ("ro", Confinement::Ro),
        ("workspace", Confinement::Workspace),
        ("off", Confinement::Off),
    ] {
        assert_eq!(
            served(["acp", "--sandbox", spelling]).sandbox,
            Some(expected),
            "`--sandbox {spelling}`"
        );
    }
}

/// An unknown confinement is refused by name, with the alternatives that work.
///
/// The value and the alternatives both, because a refusal that says only
/// "invalid" leaves the operator guessing at the closed vocabulary.
#[test]
fn acp_refuses_a_confinement_it_does_not_have() {
    let refusal = refused(["acp", "--sandbox", "readonly"]);
    assert!(
        refusal.contains("readonly"),
        "the refusal does not say what was rejected: {refusal}"
    );
    for spelling in ["ro", "workspace", "adaptive", "off"] {
        assert!(
            refusal.contains(spelling),
            "the refusal omits `{spelling}`, which would have worked: {refusal}"
        );
    }
}

/// It shares `--session-dir` and `-C` with every other command, from the same
/// declaration — a host launched against a particular store must write to the
/// store `ego ls` then lists, because the sessions it opens are ordinary ones.
#[test]
fn acp_honours_the_two_flags_it_shares() {
    let args = served(["acp", "--session-dir", "/tmp/store", "-C", "/tmp/ws"]);
    assert_eq!(
        args.session_dir.as_deref(),
        Some(std::path::Path::new("/tmp/store"))
    );
    assert_eq!(
        args.directory.as_deref(),
        Some(std::path::Path::new("/tmp/ws"))
    );
}

/// Every general session flag `ego run` takes, `ego acp` takes too, from the
/// same declaration.
///
/// A host launching `ego acp` could not shape the session it got: no tool
/// selection, no policy on the launch line, and nothing to tell the model
/// about the environment it is working in. None of these is a new concept —
/// they are the flags `ego run` has always had, reaching the one surface that
/// could not reach them.
#[test]
fn acp_takes_every_general_session_flag_run_takes() {
    let args = served([
        "acp",
        "--tools",
        "read,write",
        "--allow",
        "read(src/**)",
        "--deny",
        "write(**)",
        "--profile",
        "ci",
        "--append-system",
        "this session is driven by a host",
        "--instructions",
        "/tmp/AGENTS.md",
    ]);
    assert_eq!(
        args.shape.tools,
        Some(vec!["read".to_owned(), "write".to_owned()])
    );
    assert_eq!(args.shape.allow, vec!["read(src/**)".to_owned()]);
    assert_eq!(args.shape.deny, vec!["write(**)".to_owned()]);
    assert_eq!(args.shape.profile.as_deref(), Some("ci"));
    assert_eq!(
        args.shape.profile_source,
        Some(ego_core::config::Source::Flags),
        "a named profile without its source is a panic waiting in config::profile"
    );
    assert_eq!(
        args.shape.append_system.as_deref(),
        Some("this session is driven by a host")
    );
    assert_eq!(
        args.shape.instructions.as_deref(),
        Some(Path::new("/tmp/AGENTS.md"))
    );
}

/// `--no-tools` over ACP is the empty tool list, exactly as it is for `ego
/// run` — and a tool-less session is the one that can never ask a host for a
/// permission seat, because a permission request only ever originates from a
/// tool call.
#[test]
fn acp_no_tools_is_the_empty_tool_list_so_nothing_can_ask_for_a_seat() {
    let args = served(["acp", "--no-tools"]);
    assert_eq!(args.shape.tools, Some(Vec::new()));
}

/// The same conflict `ego run` enforces: "no tools" and "these tools" are one
/// question, so answering it twice is refused rather than silently preferred.
#[test]
fn acp_refuses_no_tools_beside_a_tool_list() {
    let refusal = refused(["acp", "--no-tools", "--tools", "read"]);
    assert!(
        refusal.contains("--tools") || refusal.contains("--no-tools"),
        "the conflict must be refused by name: {refusal}"
    );
}

/// A host that names no session flag gets exactly the session it got before
/// these flags existed. This is the whole backward-compatibility claim, and it
/// is one equality rather than seven.
#[test]
fn acp_without_session_flags_shapes_nothing() {
    assert_eq!(
        served(["acp"]).shape,
        ego_cli::cli::SessionShape::default(),
        "`ego acp` with no flags must not shape the session at all"
    );
}

/// **`--output` is the flag `ego acp` must not have**, and this is the test
/// that says so.
///
/// Its stdout *is* the JSON-RPC stream. A second writer on that descriptor does
/// not produce a run with noisy output — it produces a protocol the host cannot
/// parse, from the first line onward. So the flag is refused rather than
/// accepted and ignored: accepting it would let a caller believe they had
/// chosen a rendering, and the connection would then fail for a reason nothing
/// stated.
#[test]
fn acp_refuses_the_flag_that_would_corrupt_its_own_transport() {
    for spelling in [["acp", "--output", "json"], ["acp", "--output", "text"]] {
        let refusal = refused(spelling);
        assert!(
            refusal.contains("--output"),
            "`ego acp --output` must be refused by name: {refusal}"
        );
    }
}

/// `acp` takes no prompt: a host sends one over the wire, and a word here is a
/// mistake rather than something to run.
#[test]
fn acp_takes_no_positional() {
    let refusal = refused(["acp", "something"]);
    assert!(refusal.contains("something"), "{refusal}");
}

// --- asking, which is not misusing ---

/// `--help` on any built command is answered, not refused.
///
/// Clap represents help through an error-shaped return. It must still write stdout and
/// exit successfully, unlike a genuine usage error.
#[test]
fn asking_a_built_command_what_it_does_is_answered_and_not_refused() {
    for name in cli::BUILT {
        let text = answered([name, "--help"]);
        assert!(
            text.contains("Usage:"),
            "`ego {name} --help` answered without a usage line: {text}"
        );
    }
}

/// The usage line names both words, because one word is a line that does not
/// work.
///
/// clap takes the command in `Usage:` from `argv[0]`, not from the `name`
/// attribute on the struct, and the dispatcher used to overwrite `argv[0]` with
/// the bare program — so `ego ls --help` answered `Usage: ego [OPTIONS]`.
/// Following that is `ego --all`, which has no subcommand and reaches the
/// interactive refusal.
#[test]
fn a_usage_line_names_the_subcommand_it_belongs_to() {
    for name in cli::BUILT {
        let text = answered([name, "--help"]);
        assert!(
            text.contains(&format!("Usage: ego {name}")),
            "`ego {name} --help` names a command that is not itself: {text}"
        );
    }
}

/// Bare help lists commands without falling through to an unrelated interactive refusal.
#[test]
fn the_bare_help_lists_every_command_this_build_has() {
    let text = answered(["--help"]);
    for name in cli::BUILT {
        assert!(
            text.contains(name),
            "`ego --help` does not mention `{name}`: {text}"
        );
    }
}

#[test]
fn top_help_and_readme_offer_the_same_runnable_ollama_quickstart() {
    let help = answered(["--help"]);
    let readme = include_str!("../../../README.md");

    for fact in ["ollama/qwen3", "ollama pull qwen3"] {
        assert!(help.contains(fact), "top help omits `{fact}`: {help}");
        assert!(readme.contains(fact), "README omits `{fact}`");
    }
    for command in cli::BUILT {
        assert!(
            readme.contains(&format!("ego {command}")),
            "README omits `ego {command}`"
        );
    }
    assert!(
        readme.contains("ego run") && readme.contains("--output json"),
        "README has no headless scripting example"
    );
}

/// And it offers nothing this build refuses.
///
/// The overview reads `BUILT` so it cannot advertise an unavailable command independently
/// of the parser.
#[test]
fn the_bare_help_offers_no_command_that_is_not_built() {
    let text = answered(["--help"]);
    for (name, _) in cli::UNBUILT {
        assert!(
            !text.contains(&format!("  {name} ")) && !text.contains(&format!("  {name}\n")),
            "`ego --help` offers `{name}`, which this build refuses: {text}"
        );
    }
}

/// `--version` is answered too, and says a version.
#[test]
fn the_bare_version_is_answered_with_one() {
    let text = answered(["--version"]);
    // Catches: the CLI reports a literal version instead of the package version.
    assert_eq!(text.trim(), concat!("ego ", env!("CARGO_PKG_VERSION")));
}

/// A genuine mistake is still a refusal, so the split above is a split and not
/// a widening.
#[test]
fn an_unknown_flag_is_still_refused_and_not_answered() {
    let refusal = refused(["ls", "--nonsense"]);
    assert!(refusal.contains("--nonsense"), "{refusal}");
}

/// The separator applies to new commands too: `ego -- ls` is prompt text, not session listing.
#[test]
fn the_separator_still_stops_the_new_subcommand_names() {
    for name in ["resume", "ls", "acp"] {
        assert_eq!(
            converses(["--", name]).prompt.as_deref(),
            Some(name),
            "`ego -- {name}` must prompt the word, not run the command"
        );
    }
}

/// **The two tables must not both claim a command.**
///
/// `UNBUILT` is checked before the subcommand is dispatched, so a name in both
/// places would be refused as unavailable *even though it runs* — a working
/// command answering that it does not exist. Nothing could have caught that
/// before: the existing bindings tie each table to `CLI.md`, and a command in
/// both tables is consistent with the document, which offers it either way.
///
/// This is the check one link further along than
/// `every_waiting_on_reason_is_the_one_the_document_prints`. That one stops the
/// prose from drifting from `cli.rs`; this one stops `cli.rs` from drifting
/// from the binary.
#[test]
fn no_command_is_both_built_and_waiting_on_something() {
    for name in cli::BUILT {
        assert!(
            !cli::UNBUILT.iter().any(|(unbuilt, _)| unbuilt == name),
            "`ego {name}` is dispatched and also refused as unavailable; the \
             refusal runs first, so it would never reach the code that \
             implements it"
        );
    }
}

/// Every documented command belongs to exactly one availability table. A command missing from
/// both would fall through to an unrelated interactive path.
#[test]
fn every_command_the_document_offers_is_in_exactly_one_table() {
    let doc = include_str!("../../../CLI.md");
    let block = doc
        .split("<!-- ego-cli-command-set:start -->")
        .nth(1)
        .expect("CLI.md has the command-set start anchor")
        .split("<!-- ego-cli-command-set:end -->")
        .next()
        .expect("CLI.md has the command-set end anchor")
        .split("```")
        .nth(1)
        .expect("the command set is a fenced block");

    // Root session selectors are flags, not subcommands in BUILT.
    let root_selectors = ["--continua"];
    let mut seen = 0;
    for line in block.lines() {
        // `ego [PROMPT]` is the interactive form, which is not a command name;
        // continuation lines of a wrapped description are not either.
        let Some(rest) = line.strip_prefix("ego ") else {
            continue;
        };
        let Some(name) = rest.split_whitespace().next() else {
            continue;
        };
        if name.starts_with('[') {
            continue;
        }
        seen += 1;
        let built = cli::BUILT.contains(&name) || root_selectors.contains(&name);
        let unbuilt = cli::UNBUILT.iter().any(|(listed, _)| *listed == name);
        assert!(
            built ^ unbuilt,
            "`ego {name}` is offered by CLI.md §2 but is {}",
            match (built, unbuilt) {
                (false, false) =>
                    "in neither table, so it falls through to the \
                                   interactive refusal",
                _ => "in both tables",
            }
        );
    }
    assert_eq!(
        seen,
        cli::BUILT.len() + cli::UNBUILT.len() + root_selectors.len(),
        "every table entry must be a command §2 actually offers"
    );
}

/// The two by-name refusal rules, exercised without their tables.
///
/// Both unavailable and retired tables are currently empty. Fixture tables exercise their
/// refusal behavior even when production loops have no entries.
#[test]
fn a_documented_but_unbuilt_command_is_refused_by_name_with_what_it_waits_on() {
    let table = [("teleport", "a transporter")];

    let refusal = cli::unbuilt_refusal("ego", "teleport", &table)
        .expect("a name in the table is refused")
        .to_string();
    assert!(
        refusal.contains("ego teleport") && refusal.contains("a transporter"),
        "the refusal must name the command and the subsystem: {refusal}"
    );

    // The other half, and the more important one: an unrecognised first token
    // is a *prompt*, not a typo, so refusing it by name would break `ego what
    // does this do`.
    assert!(
        cli::unbuilt_refusal("ego", "explain", &table).is_none(),
        "a word that is not in the table is a prompt"
    );
}

#[test]
fn a_retired_flag_is_refused_by_name_with_its_replacement() {
    let table = [("--warp", "--fold")];

    let refusal = cli::retired_refusal("--warp", &table)
        .expect("a retired flag is refused")
        .to_string();
    assert!(
        refusal.contains("--warp") && refusal.contains("--fold"),
        "the refusal must name both spellings: {refusal}"
    );
    assert!(
        cli::retired_refusal("--budget", &table).is_none(),
        "a live flag is not refused"
    );
}

/// `ego doctor` takes exactly one flag, and says so about anything else.
///
/// Doctor has no subject — it reports on this installation and there is only
/// one of those — so its whole grammar is `--json` or nothing. That makes the
/// interesting case the *refusal*: a mistyped flag must be named, not
/// swallowed and quietly ignored by a command whose output an operator is
/// about to trust.
#[test]
fn doctor_takes_only_json_and_names_anything_else() {
    match cli::parse(argv(["doctor"])).expect("bare doctor parses") {
        Invocation::Doctor(args) => assert!(!args.json, "text is the default rendering"),
        other => panic!("that is not doctor: {other:?}"),
    }
    match cli::parse(argv(["doctor", "--json"])).expect("doctor --json parses") {
        Invocation::Doctor(args) => assert!(args.json),
        other => panic!("that is not doctor: {other:?}"),
    }

    let refusal = refused(["doctor", "--jsonn"]);
    assert!(
        refusal.contains("--jsonn"),
        "the refusal must name the flag that was typed: {refusal}"
    );
    let refusal = refused(["doctor", "everything"]);
    assert!(
        !refusal.is_empty(),
        "doctor takes no subject, so a word after it is not one"
    );
}

/// CLI.md names every `--allow`/`--deny` head published by the parser.
///
/// The parser's `OPERATIONS` table decides whether each head can match an
/// operation. Catches public documentation omitting an enforceable head or
/// describing it as unavailable, which can prevent an operator from using it.
#[test]
fn the_documents_name_the_rule_heads_the_parser_actually_enforces() {
    // Find the CLI.md row by its flag pair rather than by line number, so an
    // ordinary table edit does not look like a regression.
    let rows = [("CLI.md", row(include_str!("../../../CLI.md")))];

    let awaiting: Vec<&str> = Head::ALL
        .iter()
        .filter(|head| head.awaiting().is_some())
        .map(|head| head.as_str())
        .collect();

    for (document, row) in rows {
        for head in Head::ALL {
            assert!(
                row.contains(head.as_str()),
                "{document}'s `--allow`/`--deny` row never mentions the \
                 `{head}` head, which the grammar publishes. A head a document \
                 omits is one an operator does not know they may write.\n\
                 row: {row}"
            );
        }

        // The heads are all named; the remaining way to be wrong is to name
        // one and then take it back. CLI.md must not retract a head while the
        // parser reports that every head is enforceable.
        if awaiting.is_empty() {
            for retraction in ["refused", "unsupported", "cannot be enforced", "no tool"] {
                assert!(
                    !row.contains(retraction),
                    "{document}'s `--allow`/`--deny` row says \"{retraction}\", \
                     but `OPERATIONS` reports every head enforceable, so there \
                     is nothing left for it to refuse.\nrow: {row}"
                );
            }
        } else {
            assert!(
                ["refused", "unsupported", "cannot be enforced", "no tool"]
                    .iter()
                    .any(|retraction| row.contains(retraction)),
                "`OPERATIONS` says {awaiting:?} cannot match anything, and \
                 {document}'s `--allow`/`--deny` row does not say so. A rule \
                 with one of those heads loads, renders in `rules ls`, and is \
                 refused at the point it would have protected something.\n\
                 row: {row}"
            );
        }
    }
}

/// No message a person is shown is built by asking a Rust value to describe
/// itself.
///
/// `{value:?}` is the derive talking: it renders `ExecCaps(5)`,
/// `TimedOut { timeout: 30s }`, `RegistryError("…")`, `Seq(7)` — the *shape of
/// a Rust value* offered to somebody who wanted to know what went wrong with
/// their session. Worse, `Debug` renders whatever a field happens to hold, so
/// it is also how a secret gets printed: `ego-cli`'s device-code login
/// interpolated a whole `PollOutcome` this way, and three of that enum's five
/// variants carry an authorization code, a PKCE verifier or a bearer token.
///
/// The fix at each site is the same and is not cosmetic: give the type a
/// `Display` that says what happened, or call the accessor that already does.
/// That forces somebody to decide what the sentence is, which is exactly the
/// decision `{:?}` exists to avoid making.
///
/// Scoped to the three constructors that carry operator-facing prose. A `{:?}`
/// in a log line, a test assertion or an evidence digest is not this rule's
/// business — those have machine readers, and `Debug` is the right tool for
/// them.
#[test]
fn no_message_shown_to_a_person_is_a_debug_rendering() {
    const CONSTRUCTORS: [&str; 3] = [
        "message: format!(",
        "reason: format!(",
        "UsageError(format!(",
    ];

    let mut offences = Vec::new();
    for file in rust_sources(Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."))) {
        let source = std::fs::read_to_string(&file)
            .unwrap_or_else(|error| panic!("source file {} reads: {error}", file.display()));
        for constructor in CONSTRUCTORS {
            let mut from = 0;
            while let Some(found) = source[from..].find(constructor) {
                let start = from + found;
                from = start + constructor.len();
                let Some(template) = format_template(&source, from) else {
                    continue;
                };
                if template.contains(":?}") {
                    let line = source[..start].matches('\n').count() + 1;
                    offences.push(format!(
                        "{}:{line}: {}",
                        file.display(),
                        template.lines().next().unwrap_or_default().trim()
                    ));
                }
            }
        }
    }

    assert!(
        offences.is_empty(),
        "these build a message for a person out of a `Debug` rendering. Give \
         the type a `Display` that says what happened, or call the accessor \
         that already does:\n{}",
        offences.join("\n")
    );
}

/// The format template of the `format!` call whose opening parenthesis ends at
/// `after`, which is the only place a `{:?}` can be written.
///
/// Reading past the template is what the first version of this test did: it
/// scanned to the next `");`, which a call ending in `),` does not have, so the
/// window ran on into the next function and charged an innocent call site with
/// a `Debug` rendering three screens further down.
fn format_template(source: &str, after: usize) -> Option<&str> {
    let rest = source[after..].trim_start();
    // `format!(a_constant)` and friends have no template to read.
    let body = rest.strip_prefix('"')?;
    let mut escaped = false;
    for (offset, character) in body.char_indices() {
        match character {
            _ if escaped => escaped = false,
            '\\' => escaped = true,
            '"' => return Some(&body[..offset]),
            _ => {}
        }
    }
    None
}

/// Every `.rs` file under a crate's `src`, which is where production messages
/// are built. Tests and generated output are deliberately not walked.
fn rust_sources(workspace: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let crates = workspace.join("crates");
    let entries = std::fs::read_dir(&crates).expect("the workspace has a crates directory");
    for entry in entries.flatten() {
        collect_rust(&entry.path().join("src"), &mut found);
    }
    assert!(
        found.len() > 100,
        "the walk found only {} sources, so it is looking in the wrong place \
         and this test is passing vacuously",
        found.len()
    );
    found
}

fn collect_rust(directory: &Path, found: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rust(&path, found);
        } else if path.extension().is_some_and(|extension| extension == "rs")
            && !path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("._"))
        {
            found.push(path);
        }
    }
}

/// The one table row in `doc` that is about `--allow` and `--deny`.
fn row(doc: &str) -> String {
    doc.lines()
        .find(|line| {
            line.starts_with('|') && line.contains("`--allow`") && line.contains("`--deny`")
        })
        .unwrap_or_else(|| {
            panic!("the document still has a table row describing `--allow` and `--deny`")
        })
        .to_owned()
}

// Catches: run or compact silently drops explicit output and spend bounds.
#[test]
fn run_and_compact_accept_positive_output_caps_and_budget() {
    let args = parsed([
        "run",
        "--max-output-tokens",
        "100",
        "--budget",
        "1$",
        "hello",
    ]);
    assert_eq!(args.max_output_tokens.unwrap().get(), 100);
    assert_eq!(args.budget.as_deref(), Some("1$"));
    let Invocation::Compact(args) = cli::parse(
        vec![
            "ego",
            "compact",
            "01900000-0000-7000-8000-000000000001",
            "--max-output-tokens",
            "100",
            "--budget",
            "1$",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect(),
    )
    .unwrap() else {
        panic!("compact")
    };
    assert_eq!(args.max_output_tokens.unwrap().get(), 100);
    assert_eq!(args.budget.as_deref(), Some("1$"));
}

// Catches: --vim is rejected interactively or silently accepted by a headless consumer.
#[test]
fn vim_flag_selects_only_interactive_composer_editing() {
    assert!(converses(["--vim"]).vim);
    assert!(converses(["--vim", "hello"]).vim);
    assert!(converses(["resume", "--vim"]).vim);
    assert!(!converses([]).vim);
    assert!(refused(["run", "--vim", "hello"]).contains("interactive"));
    assert!(refused(["resume", "--vim", "--", "hello"]).contains("interactive"));
    assert!(answered(["--help"]).contains("--vim"));
}

#[test]
fn output_style_is_shared_by_run_and_served_session_shape() {
    // Catches: a served session silently dropping the launch output style.
    let run = parsed(["run", "--output-style", "concise", "hello"]);
    let acp = served(["acp", "--output-style", "concise"]);
    assert_eq!(run.output_style.as_deref(), Some("concise"));
    assert_eq!(acp.shape.output_style, run.output_style);
    assert_eq!(
        parsed(["resume", "--output-style", "concise", "next step"])
            .output_style
            .as_deref(),
        Some("concise")
    );
    assert!(parsed(["run", "hello"]).output_style.is_none());
}

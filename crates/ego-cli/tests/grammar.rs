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

use ego_cli::budget;
use ego_cli::cli::{self, Invocation, Output, SessionChoice, Stdin};
use ego_cli::prompt::{self, StdinSource};
use ego_core::budget::Ledger;
use ego_core::ceiling::Ceilings;
use ego_core::config::Mode;
use ego_core::cost::{Amount, CurrencyCode, Money};

fn argv<const N: usize>(args: [&str; N]) -> Vec<String> {
    std::iter::once("ego".to_string())
        .chain(args.iter().map(|arg| arg.to_string()))
        .collect()
}

/// `run` and `resume` both produce an [`Args`](ego_cli::cli::Args) — that is
/// the point of them sharing one — so one helper unwraps either, and every
/// table below can be pointed at whichever subcommand it is about.
fn parsed<const N: usize>(args: [&str; N]) -> ego_cli::cli::Args {
    match cli::parse(argv(args)).expect("the command line parses") {
        Invocation::Run(args) | Invocation::Resume(args) => *args,
        Invocation::Interactive(_) => {
            panic!("that starts a session: use `converses` for it")
        }
        Invocation::Ls(_) => panic!("`ls` takes no Args: use `listed` for it"),
        Invocation::Models(_) => panic!("`models` takes no Args: use the models tests for it"),
        Invocation::Acp(_) => panic!("`acp` takes no Args: use `served` for it"),
        Invocation::Auth(_) => panic!("`auth` takes no Args: use the auth tests for it"),
        Invocation::Mcp(_) => panic!("`mcp` takes no Args: use the MCP tests for it"),
        Invocation::McpServer(_) => {
            panic!("`mcp-server` takes no Args: use the MCP server tests for it")
        }
        Invocation::Answered(_) => panic!("that answered a question: use `answered` for it"),
        Invocation::Fork(_) => panic!("`fork` takes no Args: use the fork tests for it"),
        Invocation::Compact(_) => panic!("`compact` takes no Args: use the compact tests for it"),
        Invocation::Export(_) => panic!("`export` takes no Args: use the export tests for it"),
        Invocation::Usage(_) => panic!("`usage` takes no Args: use the usage tests for it"),
        Invocation::Hooks(_) => panic!("`hooks` takes no Args: use the hooks tests for it"),
        Invocation::Config(_) => panic!("`config` takes no Args: use the config tests for it"),
        Invocation::Rules(_) => panic!("`rules` takes no Args: use the rules tests for it"),
    }
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
        Invocation::Auth(_) => panic!("`auth` takes no Args: use the auth tests for it"),
        Invocation::Mcp(_) => panic!("`mcp` takes no Args: use the MCP tests for it"),
        Invocation::McpServer(_) => {
            panic!("`mcp-server` takes no Args: use the MCP server tests for it")
        }
        Invocation::Answered(_) => panic!("that answered a question: use `answered` for it"),
        Invocation::Fork(_) => panic!("`fork` takes no Args: use the fork tests for it"),
        Invocation::Compact(_) => panic!("`compact` takes no Args: use the compact tests for it"),
        Invocation::Export(_) => panic!("`export` takes no Args: use the export tests for it"),
        Invocation::Usage(_) => panic!("`usage` takes no Args: use the usage tests for it"),
        Invocation::Hooks(_) => panic!("`hooks` takes no Args: use the hooks tests for it"),
        Invocation::Config(_) => panic!("`config` takes no Args: use the config tests for it"),
        Invocation::Rules(_) => panic!("`rules` takes no Args: use the rules tests for it"),
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

fn usd() -> CurrencyCode {
    CurrencyCode::new("USD").expect("USD is a currency")
}

// --- argv ---

/// `--` is honoured before the subcommand is decided, so a token after it is
/// never read as one. Doing this inside clap is not possible: clap's own `--`
/// handling is per-subcommand and would have to decide `run` *is* the
/// subcommand before it could apply.
///
/// `ego -- run` is §4.5's own example, and its answer here is a session whose
/// first prompt is the word "run" rather than a headless run: the token after
/// the separator is a prompt, so no subcommand was given. The whole contract is
/// now statable — the prompt is readable rather than merely inferred from a
/// refusal — and the load-bearing half is still that `run` is not read as a
/// command.
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

/// §4.3: flags are never removed silently — a retired flag errors **by name**
/// with its replacement. The table is empty today, so this tests the mechanism
/// rather than an entry: the moment the rule is easiest to forget is the first
/// retirement, when there is nothing but a note in a changelog.
#[test]
fn a_retired_flag_would_be_refused_with_its_replacement() {
    for (retired, replacement) in cli::RETIRED {
        let refusal = refused(["run", retired, "Ask"]);
        assert!(refusal.contains(retired), "{refusal}");
        assert!(refusal.contains(replacement), "{refusal}");
    }
}

/// §2 documents fourteen commands and this build implements three. The other
/// eleven were reaching the *interactive* refusal, which told a caller who
/// typed `ego fork` that the permission dialog was missing — a true sentence
/// about a surface they did not ask for.
///
/// The standard is already written down for the other half of the grammar
/// (`CLI.md` §3.1: "refused **by name**, with the subsystem it waits on — not
/// swallowed, and not reported as an unknown argument"). This is that rule
/// applied to §2, and the failure it prevents is the one §3.1 exists to
/// prevent: a documented surface whose absence is invisible to anyone who has
/// not read the source.
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

/// The complement, and the reason the check above is a table rather than "any
/// unrecognised word". §2's first line is `ego [PROMPT]`, so a word that is
/// *not* a documented command is a prompt and the interactive refusal is the
/// correct answer for it. Widening the new rule to every unknown token would
/// turn `ego "explain this"` into a spelling complaint.
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

/// The reason is bound to the document, not just the name.
///
/// The first version of this file bound only the command *name* to §2 and left
/// `waiting_on` free on both sides. `ego rules` then went stale in the code and
/// in §2.1 independently, within an hour, when the permission rule grammar
/// shipped and both surfaces went on blaming it — caught by a person, which is
/// the mechanism this test replaces.
///
/// Two prose surfaces that agree today, with nothing that can say when they
/// stop, are the same defect as a field that is omitted when empty: two
/// different states, identical bytes. The code string is the source and the
/// table quotes it.
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
        .split("## 2. Command set")
        .nth(1)
        .expect("CLI.md has a command set section")
        .split("## 3.")
        .next()
        .expect("the command set section ends");
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

/// The session group of §4.5 is one value, so "which session" has exactly one
/// answer and no rule anyone has to remember to check.
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
///
/// Order matters: §7 of the rule ADR is first-match-wins within a list, so a
/// parser that sorted or deduplicated would change which rule an operator's
/// denial is attributed to. They stay text here — `Rule::load` runs in the
/// config layer, so a flag and a file go through one parser and not two.
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

/// A money ceiling becomes a limited ledger, and a clamped one carries the
/// figure that was asked for. §4 requires the clamp be stated, not silent:
/// dropping `clamped_from` here would make `--budget 20$` publish as though
/// it bound at $20 when it binds at $5.
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
            outstanding: usd_money("0"),
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
            outstanding: usd_money("0"),
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

/// The precedence table of §4.5, every row of it. It is total on purpose:
/// every combination lands somewhere and nothing is silently dropped.
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
    // Neither: refused, saying all three ways to supply one.
    let refusal = prompt::assemble(None, Stdin::Ambient, &Piped(None))
        .expect_err("nothing to ask")
        .to_string();
    assert!(
        refusal.contains("argument") && refusal.contains("stdin"),
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

/// §2's `ego resume [ID] [PROMPT]`, both positionals given.
#[test]
fn resume_takes_an_id_and_a_prompt() {
    let args = parsed(["resume", SOME_ID, "next step"]);
    assert_eq!(
        args.session,
        SessionChoice::Resume(Some(SOME_ID.to_string()))
    );
    assert_eq!(args.prompt.as_deref(), Some("next step"));
}

/// **The first positional is an id only if it is one.** §2 spells both
/// optional, so `ego resume "next step"` has to mean the latest session and
/// that prompt — deciding by position alone would turn it into a
/// malformed-id complaint about the prompt.
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

/// §2 spells both positionals optional because the interactive surface takes
/// over the latest session when neither was given.
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
        assert!(refusal.contains("both name a session"), "{refusal}");
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

/// The two flags §2 line 39 and §4.6 give it.
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
/// It used to be refused. clap reports `--help` through `Err` alongside the
/// genuine failures, and one `map_err` turned every `Err` into a `UsageError`
/// — so help went to stderr behind an `ego:` prefix and exited 2, the code
/// `CLI.md` §4.3 reserves for a usage error. A script that piped `--help` got
/// an empty stdout and a failure.
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

/// `ego --help` on its own lists the command set.
///
/// It used to reach the interactive refusal, so someone looking for the
/// commands was told about the permission dialog — true, and about a surface
/// they had not asked for. The same defect §2.1 exists to stop, one level
/// further out.
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
/// The complement, and the reason the overview reads `BUILT` rather than being
/// typed out: an entry here for a command that is waiting on a subsystem would
/// be `CLI.md` §2.1's hole again, in the one place a caller looks first.
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
    assert!(
        text.starts_with("ego ") && text.trim().len() > "ego ".len(),
        "`ego --version` answered with no version: {text}"
    );
}

/// A genuine mistake is still a refusal, so the split above is a split and not
/// a widening.
#[test]
fn an_unknown_flag_is_still_refused_and_not_answered() {
    let refusal = refused(["ls", "--nonsense"]);
    assert!(refusal.contains("--nonsense"), "{refusal}");
}

/// §4.5's separator rule reaches the new commands too: after `--` the word is
/// a prompt, so `ego -- ls` is the interactive form and not the catalogue.
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

/// Every command `CLI.md` §2 offers is accounted for by exactly one of the two
/// tables — implemented, or refused by name with what it waits on.
///
/// The gap this closes is a command §2 promises that neither table mentions.
/// It would fall through to the *interactive* refusal, which is the failure
/// §2.1 was written to end: a caller who typed a documented command told about
/// a surface they had not asked for.
#[test]
fn every_command_the_document_offers_is_in_exactly_one_table() {
    let doc = include_str!("../../../CLI.md");
    let block = doc
        .split("## 2. Command set")
        .nth(1)
        .expect("CLI.md has a command set section")
        .split("```")
        .nth(1)
        .expect("the command set is a fenced block");

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
        let built = cli::BUILT.contains(&name);
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
        cli::BUILT.len() + cli::UNBUILT.len(),
        "every table entry must be a command §2 actually offers"
    );
}

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

fn parsed<const N: usize>(args: [&str; N]) -> ego_cli::cli::Args {
    match cli::parse(argv(args)).expect("the command line parses") {
        Invocation::Run(args) => *args,
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
/// `ego -- run` is §4.5's own example, and its answer here is the *interactive*
/// refusal rather than a headless run: the word after the separator is a prompt,
/// so no subcommand was given. That is the half of the contract this build can
/// state — and it is the load-bearing half, since the failure the rule exists
/// to prevent is `run` being read as a command.
#[test]
fn the_separator_stops_a_subcommand_name_from_being_one() {
    assert!(refused(["--", "run"]).contains("interactive"));
    assert_eq!(
        parsed(["run", "--", "--model"]).prompt.as_deref(),
        Some("--model")
    );
    // `run` alone cannot prove the rule any more: it is the one command that
    // *is* built, so a separator check that had stopped working would still
    // reach the interactive refusal by the ordinary route. A name from
    // `UNBUILT` has a competing meaning and so can tell the two apart.
    assert!(refused(["--", "resume"]).contains("interactive"));
}

/// Without a subcommand the invocation is the interactive one, and this build
/// says which surface is missing rather than printing a usage block.
#[test]
fn an_invocation_with_no_subcommand_names_the_surface_it_would_need() {
    let refusal = refused(["hello"]);
    assert!(refusal.contains("interactive"), "{refusal}");
    assert!(refusal.contains("ego run"), "{refusal}");
}

/// A flag whose subsystem does not exist is refused by name *before* clap sees
/// it, so the answer is a sentence about that flag rather than clap's
/// "unexpected argument". `--budget=2.5$` and `--budget 2.5$` are the same flag
/// and the split has to see that.
#[test]
fn an_unavailable_flag_is_named_however_it_was_spelled() {
    for spelling in ["--sandbox=workspace", "--sandbox"] {
        let refusal = refused(["run", spelling, "Ask"]);
        assert!(refusal.contains("--sandbox"), "{refusal}");
        assert!(refusal.contains("sandbox backend"), "{refusal}");
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

/// §2 documents fourteen commands and this build implements one. The other
/// thirteen were reaching the *interactive* refusal, which told a caller who
/// typed `ego resume` that the permission dialog was missing — a true sentence
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
        let refusal = refused([word]);
        assert!(refusal.contains("interactive"), "{refusal}");
        assert!(refusal.contains("no subcommand"), "{refusal}");
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
    assert_eq!(
        parsed(["run", "--ephemeral", "Ask"]).session,
        SessionChoice::Ephemeral
    );
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

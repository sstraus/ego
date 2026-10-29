//! The near side of the provider seam names no provider and no protocol.
//!
//! Source-level name absence is necessary but not sufficient for the
//! provider-neutral boundary. External adapter composition in
//! `crates/ego-ai/tests/invariants.rs` checks the complementary condition.
//!
//! **It is a grep in a test**, which is inelegant, and this repository already
//! keeps one where the property is worth more than the elegance:
//! `crates/ego-ai/tests/http.rs` pins that `reqwest`'s `system-proxy` feature
//! stays off. The same trade applies here.
//!
//! **Comment lines are exempt, code lines are not.** Every rationale in this
//! codebase is written by naming the provider whose behaviour forced it —
//! "Anthropic charges 1.25x to write the cache", "OpenRouter's `usage.cost`
//! and xAI's `usage.cost_in_usd_ticks` differ in the unit" — and those
//! sentences are the *reason* the near side is provider-neutral, not a breach
//! of it. What is forbidden is a name the compiler can see: an identifier, a
//! string literal, a match arm. So a line whose first non-whitespace is `//`
//! is prose and is skipped, and every other line is read whole, string
//! literals included.

use std::path::{Path, PathBuf};

/// The files this invariant is about: the turn loop, the budget gate, the
/// pricing, and the port every mapping delivers events through.
///
/// Named rather than globbed over `src/`, because the claim is about these
/// four and a glob would quietly extend it to files it was never argued for —
/// `config.rs` may legitimately mention a provider id it is parsing.
const NEAR_SIDE: &[&str] = &[
    "src/turn/mod.rs",
    "src/turn/tools.rs",
    "src/turn/journal.rs",
    "src/budget.rs",
    "src/cost.rs",
    "src/generator/port.rs",
];

/// Every provider id and protocol name this build could be tempted to branch
/// on.
///
/// Provider and wire-level spellings identify forbidden core dependencies.
/// Match case-insensitively so changing letter case cannot evade the check.
const FORBIDDEN: &[&str] = &[
    "openai",
    "openrouter",
    "anthropic",
    "gemini",
    "xai",
    "grok",
    "ollama",
    "chat/completions",
    "x-api-key",
];

/// Whether this line is prose rather than code.
fn is_comment(line: &str) -> bool {
    line.trim_start().starts_with("//")
}

fn near_side_file(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(relative)
}

/// No file on the near side of the seam names a provider or a protocol in
/// code.
#[test]
fn the_near_side_names_no_provider_and_no_protocol() {
    let mut breaches = Vec::new();

    for relative in NEAR_SIDE {
        let path = near_side_file(relative);
        let source = std::fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("{}: {err}", path.display()));

        for (number, line) in source.lines().enumerate() {
            if is_comment(line) {
                continue;
            }
            let lowered = line.to_lowercase();
            for needle in FORBIDDEN {
                if lowered.contains(needle) {
                    breaches.push(format!(
                        "{relative}:{}: names `{needle}` in code — {}",
                        number + 1,
                        line.trim()
                    ));
                }
            }
        }
    }

    assert!(
        breaches.is_empty(),
        "the near side of the provider seam must not name a provider or a protocol.\n\
         A difference between providers belongs in `ego-ai`: a model fact in \
         `ModelCapabilities`, a transport fact in the row's headers, or a wire \
         format of its own \
         (`ARCHITECTURE.md`).\n\n{}",
        breaches.join("\n")
    );
}

/// The grep can actually find something.
///
/// Without this the test above is indistinguishable from one whose needle
/// list, path list or comment rule silently matches nothing — and a guard that
/// cannot fail is not a guard. It runs the same two predicates the real test
/// runs over a line that breaches, and over the shape of comment the near side
/// is full of.
#[test]
fn the_grep_finds_a_breach_and_forgives_a_comment() {
    let breach = r#"    let base = "https://api.openai.com/v1/";"#;
    assert!(!is_comment(breach));
    assert!(
        FORBIDDEN
            .iter()
            .any(|needle| breach.to_lowercase().contains(needle))
    );

    let rationale = "    /// Anthropic charges 1.25x the base rate to *write* the cache.";
    assert!(is_comment(rationale));
}

/// Every file the invariant claims to cover is a file that exists.
///
/// A renamed module would otherwise turn the test above into one that reads
/// nothing and passes, which is the quietest way for a guard to stop guarding.
/// This is why the list is not a glob and also why it cannot be trusted
/// without this.
#[test]
fn the_near_side_is_the_files_this_test_thinks_it_is() {
    for relative in NEAR_SIDE {
        let path = near_side_file(relative);
        assert!(
            path.is_file(),
            "{} is listed as the near side of the provider seam and does not exist; \
             if it moved, move this list with it",
            path.display()
        );
    }
}

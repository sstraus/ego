//! Documentation contracts: the prose that names code must name the code that exists.
//!
//! The oracle is the source text of `EventKind` and the repository tree, read
//! independently of the documents under test.

use std::collections::BTreeSet;
use std::path::Path;

const ARCHITECTURE: &str = include_str!("../../../ARCHITECTURE.md");
const EVENT_SOURCE: &str = include_str!("../src/event/domain.rs");
const REEDLINE_PATCHES: &str = include_str!("../../../vendor/reedline/EGO_PATCHES.md");

/// Variant names declared by `pub enum EventKind`, from its source text.
fn declared_event_kinds() -> BTreeSet<String> {
    let body = EVENT_SOURCE
        .split("pub enum EventKind {")
        .nth(1)
        .expect("EventKind is declared in event/domain.rs");
    let body = body.split("\n}\n").next().expect("the enum body ends");
    body.lines()
        .filter(|line| line.starts_with("    ") && !line.starts_with("     "))
        .filter(|line| {
            line.trim_start()
                .starts_with(|c: char| c.is_ascii_uppercase())
        })
        .map(|line| {
            line.trim()
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric())
                .collect()
        })
        .collect()
}

fn names_in_list(list: &str) -> BTreeSet<String> {
    list.split('|')
        .map(|name| name.replace("///", "").trim().to_owned())
        .filter(|name| !name.is_empty())
        .collect()
}

/// The first fenced block after `anchor`, with line-comment markers removed.
fn fenced_block_after<'a>(text: &'a str, anchor: &str) -> &'a str {
    let after = text.split(anchor).nth(1).expect("the anchor is present");
    let after = after.split("```").nth(1).expect("a fence opens");
    after.split("```").next().expect("the fence closes")
}

fn architecture_event_list() -> BTreeSet<String> {
    let block = fenced_block_after(ARCHITECTURE, "reduction over an append-only event log");
    names_in_list(block)
}

fn source_doc_event_list() -> BTreeSet<String> {
    let doc = EVENT_SOURCE
        .split("/// The exhaustive set of internal reducer inputs")
        .nth(1)
        .expect("the EventKind doc comment lists the reducer inputs");
    let doc = doc
        .split("pub enum EventKind")
        .next()
        .expect("doc precedes enum");
    let block = doc
        .split("```text")
        .nth(1)
        .and_then(|fenced| fenced.split("```").next())
        .expect("the doc comment holds a text fence");
    names_in_list(block)
}

fn section_3_4() -> &'static str {
    let start = ARCHITECTURE
        .find("### 3.4 Permissions and sandbox")
        .expect("section 3.4 exists");
    let rest = &ARCHITECTURE[start..];
    let end = rest.find("\n### 3.5").expect("section 3.5 follows");
    &rest[..end]
}

fn difference(left: &BTreeSet<String>, right: &BTreeSet<String>) -> Vec<String> {
    left.difference(right).cloned().collect()
}

#[test]
fn architecture_event_list_names_exactly_the_declared_event_kinds() {
    let declared = declared_event_kinds();
    let listed = architecture_event_list();
    assert!(
        declared.len() > 40,
        "the oracle found {} kinds",
        declared.len()
    );
    assert_eq!(
        difference(&declared, &listed),
        Vec::<String>::new(),
        "missing from ARCHITECTURE.md §1"
    );
    assert_eq!(
        difference(&listed, &declared),
        Vec::<String>::new(),
        "unknown in ARCHITECTURE.md §1"
    );
}

#[test]
fn architecture_event_count_matches_the_declared_event_kinds() {
    let declared = declared_event_kinds().len();
    let stated = format!("{declared} variants in the current schema");
    assert!(
        ARCHITECTURE.contains(&stated),
        "ARCHITECTURE.md §1 must state `{stated}`"
    );
}

#[test]
fn event_kind_doc_comment_names_exactly_the_declared_event_kinds() {
    let declared = declared_event_kinds();
    let listed = source_doc_event_list();
    assert_eq!(
        difference(&declared, &listed),
        Vec::<String>::new(),
        "missing from the doc comment"
    );
    assert_eq!(
        difference(&listed, &declared),
        Vec::<String>::new(),
        "unknown in the doc comment"
    );
}

#[test]
fn section_3_4_describes_adaptive_and_readmission() {
    let section = section_3_4();
    for required in [
        "`adaptive`",
        "ExecutionObserved",
        "ExecutionMeasured",
        "re-admission",
    ] {
        assert!(section.contains(required), "§3.4 must mention {required}");
    }
}

#[test]
fn reedline_patches_cite_only_existing_repository_paths() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let cited: Vec<&str> = REEDLINE_PATCHES
        .split('`')
        .skip(1)
        .step_by(2)
        .filter(|span| span.starts_with("docs/") && span.ends_with(".md"))
        .collect();
    assert!(
        !cited.is_empty(),
        "EGO_PATCHES.md cites a decision document"
    );
    for path in cited {
        assert!(
            root.join(path).is_file(),
            "EGO_PATCHES.md cites a missing path: {path}"
        );
    }
}

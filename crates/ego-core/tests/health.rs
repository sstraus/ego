//! The health vocabulary every diagnostic surface renders.
//!
//! What is pinned here is the part a subsystem cannot get right on its own:
//! how severities fold, what the fold means for an exit code, that a section
//! nobody looked at still appears, and that a detail is checked before it is
//! printed rather than after.

use std::path::PathBuf;

use ego_core::event::time::Timestamp;
use ego_core::health::{
    HEALTH_SNAPSHOT_VERSION, HealthDetail, HealthItem, HealthItemId, HealthProbe, HealthSection,
    HealthSectionReport, HealthSnapshot, HealthStatus, RedactionReason, RedactionRefusal,
    VisibleRoots,
};

fn roots() -> VisibleRoots {
    VisibleRoots::new([PathBuf::from("/home/op/.ego"), PathBuf::from("/work/repo")])
}

fn item(probe: &'static str, status: HealthStatus) -> HealthItem {
    HealthItem::new(
        HealthItemId::probe(probe),
        status,
        HealthDetail::fixed("a fact"),
        HealthDetail::fixed("a test"),
    )
}

fn section(section: HealthSection, items: Vec<HealthItem>) -> HealthSectionReport {
    HealthSectionReport::new(section, items)
}

#[test]
fn a_probe_that_was_skipped_never_summarises_as_healthy() {
    // The whole reason this projection exists rather than a pile of subsystem
    // strings. An installation where eight things are fine and the ninth was
    // never looked at is not an installation anybody has checked, and an
    // overall line reading `ok` would be the projection asserting something it
    // does not know.
    let snapshot = HealthSnapshot::new([section(
        HealthSection::Credentials,
        vec![
            item("a", HealthStatus::Ok),
            item("b", HealthStatus::NotChecked),
        ],
    )]);

    assert_eq!(snapshot.status(), HealthStatus::NotChecked);
}

#[test]
fn not_looking_is_not_a_failure_and_only_an_error_fails_the_command() {
    // The other half of the same decision, and the one that keeps it usable:
    // `not_checked` outranks `ok` in the summary and still exits 0, because a
    // missing optional credential must not fail somebody's script. Both halves
    // are asserted together — apart, either one alone is satisfied by a fold
    // that gets the other backwards.
    for (status, expected) in [
        (HealthStatus::Ok, 0),
        (HealthStatus::NotChecked, 0),
        (HealthStatus::Warning, 0),
        (HealthStatus::Error, 1),
    ] {
        let snapshot =
            HealthSnapshot::new([section(HealthSection::Sandbox, vec![item("p", status)])]);
        assert_eq!(
            snapshot.exit_code(),
            expected,
            "{status:?} should exit {expected}"
        );
    }
}

#[test]
fn the_worst_item_anywhere_decides_the_whole_diagnosis() {
    let snapshot = HealthSnapshot::new([
        section(
            HealthSection::Configuration,
            vec![item("a", HealthStatus::Ok)],
        ),
        section(HealthSection::Mcp, vec![item("b", HealthStatus::Error)]),
        section(HealthSection::Hooks, vec![item("c", HealthStatus::Warning)]),
    ]);

    assert_eq!(snapshot.status(), HealthStatus::Error);
    assert_eq!(snapshot.exit_code(), 1);
}

#[test]
fn a_broken_section_does_not_take_the_others_down_with_it() {
    // Failure semantics: a section that could not be read becomes an item, not
    // a missing section and not an aborted snapshot. Someone diagnosing a
    // broken credential store still needs to be told their MCP servers are
    // fine.
    let snapshot = HealthSnapshot::new([
        section(
            HealthSection::Credentials,
            vec![item("unreadable", HealthStatus::Error)],
        ),
        section(HealthSection::Mcp, vec![item("healthy", HealthStatus::Ok)]),
    ]);

    let mcp = snapshot
        .sections
        .iter()
        .find(|report| report.section == HealthSection::Mcp)
        .expect("every section is rendered");
    assert_eq!(mcp.status(), HealthStatus::Ok);
    assert_eq!(mcp.items.len(), 1);
}

#[test]
fn every_section_is_rendered_even_when_nobody_produced_a_fact() {
    // Omitting a silent subsystem would read as "nothing wrong here" in the
    // one case where nothing was asked. The empty section is the evidence that
    // it was considered.
    let snapshot = HealthSnapshot::new([section(
        HealthSection::Skills,
        vec![item("one", HealthStatus::Ok)],
    )]);

    assert_eq!(snapshot.v, HEALTH_SNAPSHOT_VERSION);
    assert_eq!(
        snapshot
            .sections
            .iter()
            .map(|report| report.section)
            .collect::<Vec<_>>(),
        HealthSection::ORDER.to_vec(),
    );
    assert_eq!(snapshot.items().count(), 1);
}

#[test]
fn a_detail_naming_a_path_outside_the_operator_visible_roots_is_refused() {
    // Doctor output is something people paste into issues. A subsystem that
    // mentions an absolute path from somewhere ego has no business publishing
    // gets refused rather than scrubbed: a caller that silently lost half its
    // detail does not notice, and a caller holding an `Err` has to decide.
    let refusal = HealthDetail::sanitized(
        "credentials.store",
        "could not read /Users/someone/.ssh/id_rsa",
        &roots(),
    )
    .expect_err("a path outside every visible root is not printable");

    assert_eq!(refusal.probe, "credentials.store");
    assert_eq!(refusal.reason, RedactionReason::PathOutsideVisibleRoots);
}

#[test]
fn a_path_inside_a_visible_root_is_exactly_what_doctor_is_for() {
    // The negative half. Without it the redaction rule is satisfied by
    // refusing every path, which would leave doctor unable to say the one
    // thing an operator needs — which file to go and look at.
    let detail = HealthDetail::sanitized(
        "config.load",
        "/home/op/.ego/config.toml is not valid TOML",
        &roots(),
    )
    .expect("a path under EGO_HOME is what the operator is meant to see");

    assert_eq!(
        detail.as_str(),
        "/home/op/.ego/config.toml is not valid TOML"
    );
}

#[test]
fn a_refused_detail_becomes_a_loud_item_that_does_not_carry_it() {
    // "Refuses output rather than printing the unsafe detail" — and says so.
    // A silent drop would leave the operator with a diagnosis that is missing
    // a line and no way to know it.
    let refusal = HealthDetail::sanitized("mcp.admission", "/etc/shadow is unreadable", &roots())
        .expect_err("outside the roots");
    let item = HealthItem::refused(HealthSection::Mcp, &refusal);

    assert_eq!(item.status, HealthStatus::Error);
    assert_eq!(item.id.probe, "doctor.redaction_refused");
    assert_eq!(item.id.subject.as_deref(), Some("mcp.admission"));
    assert!(
        !item.detail.as_str().contains("/etc/shadow"),
        "the refused path must not reach the output the refusal kept it out of",
    );
    assert!(item.detail.as_str().contains("mcp.admission"));
}

/// A probe that answers with whatever it was built with.
struct Fixed {
    section: HealthSection,
    answer: Result<Vec<HealthItem>, RedactionRefusal>,
}

impl HealthProbe for Fixed {
    fn section(&self) -> HealthSection {
        self.section
    }
    fn observe(&self, _roots: &VisibleRoots) -> Result<Vec<HealthItem>, RedactionRefusal> {
        self.answer.clone()
    }
}

#[test]
fn a_probe_that_refuses_to_speak_does_not_silence_the_ones_that_can() {
    // The collector's whole job. A probe returning `Err` has said it was about
    // to print something it should not; that becomes one loud item in its own
    // section, and every other probe is still asked.
    let refusing = Fixed {
        section: HealthSection::Credentials,
        answer: Err(RedactionRefusal {
            probe: "credentials.store",
            reason: RedactionReason::PathOutsideVisibleRoots,
        }),
    };
    let healthy = Fixed {
        section: HealthSection::Mcp,
        answer: Ok(vec![item("admitted", HealthStatus::Ok)]),
    };

    let snapshot = HealthSnapshot::observe(&[&refusing, &healthy], &roots());

    let credentials = snapshot
        .sections
        .iter()
        .find(|report| report.section == HealthSection::Credentials)
        .expect("every section is rendered");
    assert_eq!(credentials.status(), HealthStatus::Error);
    assert_eq!(
        credentials.items[0].id.probe, "doctor.redaction_refused",
        "a refusal is reported as one, not as the subsystem being broken",
    );

    let mcp = snapshot
        .sections
        .iter()
        .find(|report| report.section == HealthSection::Mcp)
        .expect("every section is rendered");
    assert_eq!(mcp.status(), HealthStatus::Ok);
    assert_eq!(mcp.items.len(), 1);
}

#[test]
fn an_item_names_its_probe_its_subject_and_when_the_fact_was_true() {
    let observed = Timestamp::from_millis(1_700_000_000_000);
    let item = HealthItem::new(
        HealthItemId::about("credentials.entry", "anthropic"),
        HealthStatus::Warning,
        HealthDetail::fixed("the stored credential expires within a day"),
        HealthDetail::fixed("the credential store"),
    )
    .observed_at(observed)
    .remediation(HealthDetail::fixed("ego auth login anthropic"));

    assert_eq!(item.id.to_string(), "credentials.entry:anthropic");
    assert_eq!(item.observed_at, Some(observed));
    assert_eq!(
        item.remediation.as_ref().map(HealthDetail::as_str),
        Some("ego auth login anthropic"),
    );
}

#[test]
fn a_section_label_is_the_key_a_json_reader_looks_for() {
    // The label is wire vocabulary, not a caption: a rendering prints it and a
    // reader keys on it, so it has to be the snake_case spelling serde derives
    // for the same variant. Anything that agrees with itself but not with the
    // serialization splits the two readers apart.
    for section in HealthSection::ORDER {
        let serialized = serde_json::to_value(section).expect("a section serializes");
        assert_eq!(
            serde_json::Value::String(section.label().to_owned()),
            serialized,
            "{section:?} labels itself differently from how it serializes",
        );
    }

    assert_eq!(HealthSection::SessionStore.label(), "session_store");
    assert_eq!(HealthSection::Acp.label(), "acp");
}

#[test]
fn every_section_has_its_own_label() {
    // A duplicated or empty label makes two subsystems indistinguishable in the
    // one output an operator reads to tell them apart.
    let mut labels: Vec<&str> = HealthSection::ORDER.iter().map(|s| s.label()).collect();
    labels.sort_unstable();
    let count = labels.len();
    labels.dedup();
    assert_eq!(labels.len(), count, "two sections share a label");
    assert!(
        labels.iter().all(|label| !label.is_empty()),
        "a section labels itself with nothing",
    );
}

#[test]
fn a_detail_prints_the_text_it_was_checked_with() {
    // Display is how a detail reaches a terminal. Printing anything other than
    // the accepted text would put a diagnosis on screen that no redaction check
    // ever saw.
    let fixed = HealthDetail::fixed("the catalogue holds no providers");
    assert_eq!(fixed.to_string(), "the catalogue holds no providers");
    assert_eq!(fixed.to_string(), fixed.as_str());

    let checked = HealthDetail::sanitized(
        "config.load",
        "/home/op/.ego/config.toml is unreadable",
        &roots(),
    )
    .expect("a path inside a visible root is publishable");
    assert_eq!(
        checked.to_string(),
        "/home/op/.ego/config.toml is unreadable"
    );
    assert_eq!(
        format!("detail: {checked}"),
        "detail: /home/op/.ego/config.toml is unreadable",
        "a detail interpolated into a sentence carries its whole text",
    );
}

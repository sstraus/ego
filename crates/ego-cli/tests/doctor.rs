//! One diagnosis, two renderings, and nothing changed by looking.
//!
//! The contract these hold is
//! `ARCHITECTURE.md`,
//! and its testing section names exactly what has to be shown: identical
//! semantic snapshots from both surfaces, deterministic order, exit folding,
//! partial section failure, every redaction path, and proof that observing
//! writes nothing.
//!
//! These are functions over an explicit workspace and an explicit `EGO_HOME`,
//! not forks of a binary. Doctor's whole job is to be a pure reading of state,
//! so a test that had to spawn a process to see the reading would be admitting
//! it is not one.

use std::fs;
use std::path::{Path, PathBuf};

use tempfile::{TempDir, tempdir};

use ego_cli::doctor;
use ego_core::health::{HealthSection, HealthStatus};

#[cfg(not(windows))]
const PRIVATE_MACHINE_PATH: &str = "/Users/someone/.ssh/id_ed25519";
#[cfg(windows)]
const PRIVATE_MACHINE_PATH: &str = r"C:\Users\someone\.ssh\id_ed25519";

/// An empty workspace with an empty `EGO_HOME` inside it.
///
/// Held by the caller, because dropping it deletes it — and every assertion
/// below is about what doctor saw in a directory that still exists.
fn scratch() -> TempDir {
    tempdir().expect("a scratch workspace is creatable")
}

/// The home doctor is handed, which is *not* the one the environment names.
///
/// Doctor takes its home as an argument precisely so a diagnosis can be taken
/// of somewhere other than the machine running it; a test that had to set
/// `EGO_HOME` would be a test proving that argument does nothing.
fn home_in(workspace: &Path) -> PathBuf {
    let home = workspace.join("home");
    fs::create_dir_all(&home).expect("the scratch EGO_HOME is creatable");
    home
}

fn observed(workspace: &Path) -> ego_core::health::HealthSnapshot {
    let home = home_in(workspace);
    let roots = doctor::visible_roots(workspace, Some(&home));
    doctor::snapshot(workspace.to_path_buf(), Some(home), &roots)
}

/// Every file under `root`, with its bytes, as one sorted listing.
fn contents(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if let Ok(bytes) = fs::read(&path) {
                found.push((path, bytes));
            }
        }
    }
    found.sort();
    found
}

#[test]
fn the_interactive_surface_and_the_command_render_one_diagnosis() {
    let scratch = scratch();
    let workspace = scratch.path();
    let snapshot = observed(workspace);

    // Not a tautology: `render(_, false)` is what `ego doctor` prints and
    // `text` is what `/doctor` prints, and the failure this catches is the
    // obvious future one — a `--json`-shaped tweak applied to only one of them.
    assert_eq!(
        doctor::render(&snapshot, false).expect("the text rendering never fails"),
        doctor::text(&snapshot),
        "the two surfaces must render the same snapshot the same way",
    );
}

#[test]
fn width_bounded_text_preserves_every_non_whitespace_character() {
    let scratch = scratch();
    let snapshot = observed(scratch.path());
    let expected = doctor::text(&snapshot)
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect::<String>();

    for width in [80, 60, 40] {
        let rendered = doctor::text_with_width(&snapshot, width);
        assert!(
            rendered
                .lines()
                .all(|line| ego_cli::render::line::cells(line) <= width),
            "doctor exceeded {width} cells:\n{rendered}"
        );
        assert_eq!(
            rendered
                .chars()
                .filter(|character| !character.is_whitespace())
                .collect::<String>(),
            expected,
            "doctor lost information at width {width}"
        );
    }
}

#[test]
fn width_bounded_text_preserves_the_complete_health_snapshot() {
    let scratch = scratch();
    let snapshot = observed(scratch.path());
    let canonical = doctor::text(&snapshot);
    let compact = |text: &str| {
        text.chars()
            .filter(|character| !character.is_whitespace())
            .collect::<String>()
    };

    for width in [80, 60, 40] {
        let rendered = doctor::text_with_width(&snapshot, width);
        assert!(
            rendered
                .lines()
                .all(|line| ego_cli::render::line::cells(line) <= width),
            "doctor exceeded {width} cells:\n{rendered}"
        );
        assert_eq!(
            compact(&rendered),
            compact(&canonical),
            "doctor lost semantic characters at width {width}"
        );
    }
}

#[test]
fn every_section_is_named_even_when_it_had_nothing_to_say() {
    let scratch = scratch();
    let workspace = scratch.path();
    let rendered = doctor::text(&observed(workspace));

    for section in HealthSection::ORDER {
        assert!(
            rendered
                .lines()
                .any(|line| line == format!("{}\tok", section.label())
                    || line.starts_with(&format!("{}\t", section.label()))),
            "the {} section is missing from:\n{rendered}",
            section.label(),
        );
    }
}

#[test]
fn the_sections_come_out_in_the_order_the_projection_fixes() {
    let scratch = scratch();
    let workspace = scratch.path();
    let snapshot = observed(workspace);

    let order: Vec<HealthSection> = snapshot
        .sections
        .iter()
        .map(|report| report.section)
        .collect();
    assert_eq!(
        order,
        HealthSection::ORDER.to_vec(),
        "doctor's output order is the projection's, not the probe list's",
    );
}

#[test]
fn two_readings_of_one_unchanged_workspace_say_the_same_thing() {
    let scratch = scratch();
    let workspace = scratch.path();
    let first = doctor::text(&observed(workspace));
    let second = doctor::text(&observed(workspace));

    assert_eq!(
        first, second,
        "a diagnosis that changes with nothing else changing cannot be diffed",
    );
}

#[test]
fn the_json_rendering_carries_the_version_and_every_section() {
    let scratch = scratch();
    let workspace = scratch.path();
    let snapshot = observed(workspace);
    let rendered = doctor::render(&snapshot, true).expect("the snapshot serializes");
    let parsed: serde_json::Value = serde_json::from_str(&rendered).expect("it is one JSON object");

    assert_eq!(
        parsed["v"],
        serde_json::json!(ego_core::health::HEALTH_SNAPSHOT_VERSION),
        "a consumer keys off the version before anything else",
    );
    let sections = parsed["sections"]
        .as_array()
        .expect("sections is an array")
        .iter()
        .map(|section| section["section"].as_str().expect("a section label"))
        .collect::<Vec<_>>();
    assert_eq!(
        sections,
        HealthSection::ORDER
            .iter()
            .map(|section| section.label())
            .collect::<Vec<_>>(),
        "the JSON carries the same nine sections in the same order as the text",
    );
    assert!(
        !rendered.contains('\n'),
        "`--json` is one line on a protocol-clean stdout",
    );
}

#[test]
fn a_warning_or_an_unchecked_fact_is_visible_and_still_exits_zero() {
    let scratch = scratch();
    let workspace = scratch.path();
    // macOS defaults to Adaptive, which refuses a workspace inside `$TMPDIR`
    // (where `tempdir` puts it) and doctor reports that refusal as an error.
    // Doctor runs in this process, so the sandbox is chosen by its config.
    fs::write(
        home_in(workspace).join("config.toml"),
        "sandbox = \"workspace\"\n",
    )
    .expect("the scratch config is writable");
    let snapshot = observed(workspace);

    // An empty workspace has no credentials and no measured sandbox, so it
    // reaches this assertion with `not_checked` items in it by construction.
    assert!(
        snapshot
            .items()
            .any(|item| item.status == HealthStatus::NotChecked),
        "an empty workspace has facts doctor refuses to establish",
    );
    assert!(
        snapshot.status() < HealthStatus::Error,
        "nothing an empty workspace reports is an operational error",
    );
    assert_eq!(
        snapshot.exit_code(),
        0,
        "a missing optional credential must not fail an otherwise valid install",
    );
}

#[test]
fn the_facts_doctor_will_not_establish_are_named_rather_than_claimed_healthy() {
    let scratch = scratch();
    let workspace = scratch.path();
    let snapshot = observed(workspace);

    for (section, probe) in [
        (HealthSection::Sandbox, "sandbox.capabilities"),
        (HealthSection::Catalogue, "catalogue.snapshot"),
    ] {
        let report = snapshot
            .sections
            .iter()
            .find(|report| report.section == section)
            .expect("the section is present");
        let item = report
            .items
            .iter()
            .find(|item| item.id.probe == probe)
            .unwrap_or_else(|| panic!("{probe} is reported"));
        assert_eq!(
            item.status,
            HealthStatus::NotChecked,
            "{probe} must not read as healthy: doctor never established it",
        );
        assert!(
            item.remediation.is_some(),
            "{probe} must name the command that would establish it",
        );
    }
}

#[test]
fn the_acp_section_publishes_what_a_host_would_actually_read() {
    let scratch = scratch();
    let workspace = scratch.path();
    let snapshot = observed(workspace);

    let report = snapshot
        .sections
        .iter()
        .find(|report| report.section == HealthSection::Acp)
        .expect("the ACP section is present");
    let advertised = ego_acp::serve::durable_advertisement();
    let extensions = advertised
        .meta
        .as_ref()
        .and_then(|meta| meta.get("ego"))
        .and_then(serde_json::Value::as_object)
        .expect("this build advertises _ego extensions");

    // regression case's defect, made local: TUICommander could not reach
    // `_ego/pause` because it expected two entries where ego publishes one,
    // and no ego surface said which. This is the surface that says it.
    for name in extensions.keys() {
        assert!(
            report
                .items
                .iter()
                .any(|item| item.id.subject.as_deref() == Some(name.as_str())),
            "the {name} extension is advertised but doctor does not report it",
        );
    }
    assert_eq!(
        report
            .items
            .iter()
            .filter(|item| item.id.probe == "acp.extension")
            .count(),
        extensions.len(),
        "doctor must report exactly the extensions the initialize answer carries",
    );
}

#[test]
fn observing_a_workspace_leaves_every_byte_of_it_alone() {
    let scratch = scratch();
    let workspace = scratch.path();
    fs::write(workspace.join("AGENTS.md"), b"be honest\n").expect("the fixture is writable");
    fs::create_dir_all(workspace.join(".ego")).expect("the session directory is creatable");

    let before = contents(workspace);
    let _ = observed(workspace);
    let after = contents(workspace);

    assert_eq!(
        before.iter().map(|(path, _)| path).collect::<Vec<_>>(),
        after.iter().map(|(path, _)| path).collect::<Vec<_>>(),
        "doctor created or removed a file",
    );
    assert_eq!(before, after, "doctor rewrote a file it was only reading");
}

#[test]
fn a_detail_may_name_a_visible_root_and_nothing_else_on_the_machine() {
    let scratch = scratch();
    let workspace = scratch.path();
    let home = home_in(workspace);
    let roots = doctor::visible_roots(workspace, Some(&home));

    for admitted in [
        workspace.join("config.toml"),
        home.join("auth.json"),
        PathBuf::from("/etc/ego/config.toml"),
    ] {
        ego_core::health::HealthDetail::sanitized("probe", admitted.display().to_string(), &roots)
            .unwrap_or_else(|_| panic!("{} is inside a visible root", admitted.display()));
    }
    // The case the redaction rule exists for: a path an operator would paste
    // into a public issue without noticing what it says about their machine.
    ego_cli::doctor::visible_roots(workspace, Some(&home));
    ego_core::health::HealthDetail::sanitized("probe", PRIVATE_MACHINE_PATH, &roots)
        .expect_err("a path outside every visible root must be refused, not printed");
}

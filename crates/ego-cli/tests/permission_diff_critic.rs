use ego_cli::render::{Capabilities, permission_diff};
use ego_core::permit::Change;

// Catches: treating an empty replacement as no diff, hiding the deletion to approve.
#[test]
fn permission_diff_deletion_shows_removed_content_without_additions() {
    let output = permission_diff::render(
        &Change::Replace {
            old_string: "delete this\n".into(),
            new_string: String::new(),
            replace_all: false,
        },
        Capabilities::plain(),
    );
    assert!(output.contains("@@ -1 +0,0 @@\n-delete this"), "{output}");
    assert!(output.lines().any(|line| line == "-delete this"));
    assert!(
        !output
            .lines()
            .any(|line| line.starts_with('+') && !line.starts_with("+++"))
    );
}

// Catches: dropping newline-only edits, making a real file change look unchanged.
#[test]
fn permission_diff_final_newline_removal_is_visible() {
    let output = permission_diff::render(
        &Change::Replace {
            old_string: "keep this\n".into(),
            new_string: "keep this".into(),
            replace_all: false,
        },
        Capabilities::plain(),
    );
    assert!(output.contains("-keep this\n+keep this\n"), "{output}");
    assert!(output.contains("\\ No newline at end of file"), "{output}");
}

// Catches: approval renders an all-occurrence edit as a single replacement.
#[test]
fn permission_diff_replace_all_states_scope_without_reading_the_file() {
    let output = permission_diff::render(
        &Change::Replace {
            old_string: "old\n".into(),
            new_string: "new\n".into(),
            replace_all: true,
        },
        Capabilities::plain(),
    );
    assert!(output.contains("all occurrences"), "{output}");
    assert!(output.contains("final content depends"), "{output}");
    assert!(output.contains("-old\n+new"), "{output}");
}

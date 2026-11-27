use ego_cli::render::{Capabilities, permission_diff};
use ego_core::permit::Change;

// Catches: treating an empty replacement as no diff, hiding the deletion to approve.
#[test]
fn permission_diff_deletion_shows_removed_content_without_additions() {
    let output = permission_diff::render(
        &Change::Replace {
            replace_all: false,
            chunks: vec![ego_core::permit::ReplaceChunk {
                old_string: "delete this\n".into(),
                new_string: String::new(),
            }],
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
            replace_all: false,
            chunks: vec![ego_core::permit::ReplaceChunk {
                old_string: "keep this\n".into(),
                new_string: "keep this".into(),
            }],
        },
        Capabilities::plain(),
    );
    assert!(output.contains("-keep this\n+keep this\n"), "{output}");
    assert!(output.contains("\\ No newline at end of file"), "{output}");
}

// Catches: the permission diff displays a bulk edit as one reviewed substitution.
#[test]
fn replace_all_permission_diff_names_the_mode_without_claiming_final_content() {
    for replace_all in [false, true] {
        let output = permission_diff::render(
            &Change::Replace {
                replace_all,
                chunks: vec![ego_core::permit::ReplaceChunk {
                    old_string: "old\n".into(),
                    new_string: "new\n".into(),
                }],
            },
            Capabilities::plain(),
        );
        assert!(
            output.contains(if replace_all {
                "all original non-overlapping occurrences"
            } else {
                "exactly one occurrence"
            }),
            "{output}"
        );
        assert!(output.contains("final content depends on the file at execution time"));
        assert!(output.contains("-old\n+new"));
    }
}

// Catches a prompt that silently omits later replacement chunks.
#[test]
fn permission_diff_shows_each_replacement_chunk() {
    let output = permission_diff::render(
        &Change::Replace {
            replace_all: false,
            chunks: vec![
                ego_core::permit::ReplaceChunk {
                    old_string: "first\n".into(),
                    new_string: "one\n".into(),
                },
                ego_core::permit::ReplaceChunk {
                    old_string: "second\n".into(),
                    new_string: "two\n".into(),
                },
            ],
        },
        Capabilities::plain(),
    );
    assert!(output.contains("-first\n+one"), "{output}");
    assert!(output.contains("-second\n+two"), "{output}");
}

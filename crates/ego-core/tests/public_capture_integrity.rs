use ego_core::store::codec::{StoreLine, decode_log};

// Catches: privacy-neutralizing a recorded identity or timestamp leaves a stale
// durable checksum, so the real store rejects otherwise meaningful replay data.
#[test]
fn neutralized_recorded_journals_keep_valid_durable_checksums() {
    let fixtures = [
        (
            "empty cut",
            include_str!("../../ego-cli/tests/fixtures/empty-cut-stream.jsonl"),
        ),
        (
            "unrun exec",
            include_str!("../../ego-cli/tests/fixtures/unrun-exec.jsonl"),
        ),
        (
            "unrun exec without reasoning",
            include_str!("../../ego-cli/tests/fixtures/unrun-exec-without-reasoning.jsonl"),
        ),
        (
            "unrun swift",
            include_str!("../../ego-cli/tests/fixtures/unrun-swift.jsonl"),
        ),
        (
            "unrun write",
            include_str!("../../ego-cli/tests/fixtures/unrun-write.jsonl"),
        ),
    ];
    let failures = fixtures
        .into_iter()
        .filter_map(|(name, text)| match decode_log(text.as_bytes()) {
            Ok(lines)
                if lines
                    .iter()
                    .all(|line| matches!(line, StoreLine::Record(_))) =>
            {
                None
            }
            Ok(_) => Some(format!("{name}: expected only durable records")),
            Err(error) => Some(format!("{name}: {error}")),
        })
        .collect::<Vec<_>>();
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

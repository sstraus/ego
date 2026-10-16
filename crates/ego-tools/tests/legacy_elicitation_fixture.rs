//! Capture the version-2 writer before the durable-round format changes.
include!("mcp_continuation.rs");

#[test]
fn record_legacy_single_form_continuation_fixture() {
    let root = tempfile::tempdir().expect("isolated fixture directory");
    let journal = JsonlContinuationJournal::new(root.path(), Box::new(ego_tools::store::OsIo), Box::new(FixedClock(Timestamp::from_millis(1000))));
    let record = record(&authorized(), Arc::from(b"deterministic-key-0123456789abcdef".as_slice()), Timestamp::from_millis(1000), 300_000);
    ContinuationJournal::record(&journal, record).expect("record with the old writer");
    let wire = persisted_wire(root.path());
    assert_eq!(wire["version"], json!(2));
    println!("LEGACY_FIXTURE:{}", serde_json::to_string(&wire).unwrap());
}

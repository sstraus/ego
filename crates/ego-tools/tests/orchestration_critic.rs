use ego_tools::orchestration_events::ObservationPump;

// Catches: a failed tracking read marks the result seen before creating its card.
#[test]
fn result_is_retried_after_tracking_read_failure_recovers() {
    let home = tempfile::tempdir().unwrap();
    let results = home.path().join("Gits/.tmp/results");
    std::fs::create_dir_all(&results).unwrap();
    let mut pump = ObservationPump::open(home.path(), home.path()).unwrap();
    let children = home.path().join("Gits/.tmp/coordinator/children.tsv");
    // A bad tracking path is a real filesystem read error, without permission
    // assumptions that differ when the suite runs as root.
    std::fs::create_dir(&children).unwrap();
    std::fs::write(results.join("worker-a-1.md"), "RESULT: task completed").unwrap();
    assert!(pump.scan_results(0).is_err());
    std::fs::remove_dir(&children).unwrap();
    std::fs::write(&children, "").unwrap();
    pump.scan_results(10_000).unwrap();
    assert_eq!(pump.ready(30_000).len(), 1);
}

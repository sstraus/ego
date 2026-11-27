use crate::admission_support as admission_fixture;

// Catches: interrupted admission tests leave instruction files in the source tree.
#[test]
fn admission_fixture_stays_outside_source_tree() {
    let root = admission_fixture::root();
    let source = std::fs::canonicalize(env!("CARGO_MANIFEST_DIR")).unwrap();
    let path = std::fs::canonicalize(root.path()).unwrap();
    assert!(
        !path.starts_with(source),
        "fixture is inside source: {path:?}"
    );
}

// Catches: discarding the TempDir owner persists nested instruction fixtures.
#[test]
fn admission_fixture_removes_nested_instructions_on_drop() {
    let path = {
        let root = admission_fixture::root();
        let instructions = root.path().join("home/.claude");
        std::fs::create_dir_all(&instructions).unwrap();
        std::fs::write(instructions.join("CLAUDE.md"), "operator instructions").unwrap();
        root.path().to_path_buf()
    };
    assert!(!path.exists(), "fixture survived its owner: {path:?}");
}

// Catches: a failed assertion bypasses fixture cleanup during unwinding.
#[test]
fn admission_fixture_removes_nested_instructions_on_unwind() {
    let root = admission_fixture::root();
    let path = root.path().to_path_buf();
    let result = std::thread::spawn(move || {
        let _root = root;
        let instructions = _root.path().join("home/Gits/other");
        std::fs::create_dir_all(&instructions).unwrap();
        std::fs::write(instructions.join("AGENTS.md"), "operator instructions").unwrap();
        std::panic::resume_unwind(Box::new("fixture cleanup probe"));
    })
    .join();
    assert!(result.is_err());
    assert!(!path.exists(), "fixture survived unwinding: {path:?}");
}

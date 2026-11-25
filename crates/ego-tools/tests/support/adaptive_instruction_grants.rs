use super::*;
use ego_core::permit::residual::ResidualWriteDesk;

// Catches: a broad judge grant removes the native instruction-file write floor.
#[test]
fn adaptive_residual_directory_grant_keeps_nested_instructions_read_only() {
    let Some(root) = isolated(
        "adaptive_instruction_grants::adaptive_residual_directory_grant_keeps_nested_instructions_read_only",
    ) else {
        return;
    };
    let outside = root.join("home/Gits/other");
    std::fs::create_dir_all(&outside).unwrap();
    for name in ["AGENTS.md", "CLAUDE.md"] {
        std::fs::write(outside.join(name), "operator instructions").unwrap();
    }
    let policy = admit(&root.join("workspace")).unwrap();
    let desk = ResidualWriteDesk::new();
    let heard = super::adaptive_residual::serve(&desk, super::adaptive_residual::grant());
    let shell = |script: &str| vec!["/bin/sh".into(), "-c".into(), script.into()];
    let first = execute_with_desk(
        policy.clone(),
        &root,
        shell("/usr/bin/touch \"$HOME/Gits/other\""),
        None,
        Some(desk.clone()),
    );
    assert!(first.is_err(), "directory write was already open");
    assert_eq!(
        desk.grants(),
        vec![CanonPath::new(std::fs::canonicalize(&outside).unwrap())]
    );
    let ordinary = execute_with_desk(
        policy.clone(),
        &root,
        shell("printf exported > \"$HOME/Gits/other/report.txt\""),
        None,
        Some(desk.clone()),
    );
    assert!(ordinary.is_ok(), "grant was not applied: {ordinary:?}");
    assert_eq!(
        std::fs::read_to_string(outside.join("report.txt")).unwrap(),
        "exported"
    );
    for name in ["AGENTS.md", "CLAUDE.md"] {
        let result = execute_with_desk(
            policy.clone(),
            &root,
            shell(&format!("printf changed > \"$HOME/Gits/other/{name}\"")),
            None,
            Some(desk.clone()),
        );
        assert!(result.is_err(), "instruction write reached a broad grant");
        assert_eq!(
            std::fs::read_to_string(outside.join(name)).unwrap(),
            "operator instructions"
        );
    }
    assert_eq!(
        heard.lock().unwrap().len(),
        1,
        "instruction floor reached judge"
    );
}

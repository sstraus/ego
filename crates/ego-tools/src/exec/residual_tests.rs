use super::*;
use ego_core::permit::{AdaptivePerimeter, Confinement};

// Catches: a forced judge write verdict bypasses agent instruction floors,
// including absent files and instruction files beneath a broader writable root.
#[test]
fn residual_recheck_rejects_instruction_files_beneath_broad_grants() {
    let root = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(root.path()).unwrap();
    let home = base.join("home");
    let workspace = base.join("workspace");
    let other = home.join("Gits/other");
    for path in [
        &workspace,
        &other,
        &home.join(".codex"),
        &home.join(".claude/skills"),
    ] {
        std::fs::create_dir_all(path).unwrap();
    }
    let policy = RunConfinement::new(Confinement::Adaptive, false, CanonPath::new(&workspace))
        .unwrap()
        .with_adaptive_perimeter(AdaptivePerimeter {
            home: CanonPath::new(&home),
            writable: vec![CanonPath::new(&other), CanonPath::new(home.join(".codex"))],
            credentials: vec![],
            controls: vec![],
        });
    let recheck = admissible(policy);
    for name in [
        ".codex/instructions.md",
        ".claude/CLAUDE.md",
        ".claude/skills/operator.md",
        "Gits/other/AGENTS.md",
        "Gits/other/CLAUDE.md",
        "Gits/other/agents.MD",
    ] {
        let path = home.join(name);
        assert!(recheck(&CanonPath::new(&path)).is_err(), "absent {name}");
        std::fs::write(&path, "operator instructions").unwrap();
        assert!(recheck(&CanonPath::new(&path)).is_err(), "existing {name}");
    }
    for name in [
        "Gits/other/report.md",
        ".codex/session.jsonl",
        "Gits/other/AGENTS.md.backup",
    ] {
        assert!(
            recheck(&CanonPath::new(home.join(name))).is_ok(),
            "ordinary {name}"
        );
    }
}

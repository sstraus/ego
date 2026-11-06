use ego_core::permit::{
    AdaptivePerimeter, CanonPath, Confinement, EnvPolicy, RunConfinement, SandboxAccess,
    SandboxGrant, SpawnSpec, StreamProvenance, ToolContentBlock, ToolOutputEncoding,
    ToolOutputProvenance, ToolResult,
};

// Catches: the portable refusal diagnostic suppresses the exact owner question
// for a workspace instruction file before the native backend can widen it.
#[test]
fn exact_instruction_denial_offers_owner_write() {
    let temporary = tempfile::tempdir().unwrap();
    let workspace = std::fs::canonicalize(temporary.path()).unwrap();
    let target = workspace.join("AGENTS.md");
    std::fs::write(&target, "original").unwrap();
    let workspace = CanonPath::new(workspace);
    let policy = RunConfinement::new(Confinement::Adaptive, false, workspace.clone())
        .unwrap()
        .with_adaptive_perimeter(AdaptivePerimeter {
            home: workspace.clone(),
            writable: vec![],
            credentials: vec![],
            controls: vec![],
        });
    let spec = SpawnSpec::new(
        vec!["/usr/bin/touch".into(), target.as_os_str().into()],
        Some(workspace),
        None,
        EnvPolicy::Inherited,
    )
    .unwrap();
    let result = ToolResult::new(vec![ToolContentBlock {
        text: format!("touch: {}: Operation not permitted", target.display()),
        provenance: ToolOutputProvenance::Process {
            stream: StreamProvenance::Stderr,
            encoding: ToolOutputEncoding::Utf8,
            truncated: false,
            omitted_bytes: 0,
        },
        full_output: None,
    }]);
    assert_eq!(
        super::analyze(&spec, &result, &policy, &[])
            .unwrap()
            .request,
        Some(SandboxGrant::new(
            CanonPath::new(target),
            SandboxAccess::Write
        ))
    );
}

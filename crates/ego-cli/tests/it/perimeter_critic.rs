//! Critic regression cases for the operator-visible perimeter.

#[cfg(target_os = "macos")]
// Catches: filesystem sandbox off incorrectly hides the Seatbelt backend required by --offline.
#[test]
fn offline_with_filesystem_sandbox_off_reports_seatbelt() {
    use ego_core::config::{Trust, resolve};
    use ego_core::permit::{CanonPath, Confinement, RunConfinement};

    let effective = resolve(&[], Trust::Untrusted, None).expect("default configuration");
    let confinement = RunConfinement::new(
        Confinement::Off,
        true,
        CanonPath::new("/Users/operator/workspace"),
    )
    .expect("supported invocation policy");
    let rendered = ego_cli::perimeter::Perimeter::session(&confinement, &effective, &[]).text();

    assert!(rendered.contains("  sandbox\toff\tdefault"), "{rendered}");
    assert!(
        rendered.contains("  network\toffline\tinvocation"),
        "{rendered}"
    );
    assert!(rendered.contains("  backend\tseatbelt"), "{rendered}");
}

// Catches: a judge grant is applied but missing from perimeter inspection,
// shown under another source, or listed once per repeated publication.
#[test]
fn judge_grants_are_listed_once_as_judge_roots() {
    use ego_core::config::{Trust, resolve};
    use ego_core::permit::{CanonPath, Confinement, RunConfinement};

    let effective = resolve(&[], Trust::Untrusted, None).expect("default configuration");
    let confinement = RunConfinement::new(
        Confinement::Workspace,
        false,
        CanonPath::new("/Users/operator/workspace"),
    )
    .expect("supported invocation policy");
    let granted = CanonPath::new("/Users/operator/Exports/report.json");
    let rendered = ego_cli::perimeter::Perimeter::session(&confinement, &effective, &[])
        .with_residual_grants(&[granted.clone(), granted])
        .text();

    let line = "  root\t/Users/operator/Exports/report.json\tread-write\tjudge";
    assert_eq!(rendered.matches(line).count(), 1, "{rendered}");
}

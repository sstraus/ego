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

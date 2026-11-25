use ego_core::config::{ConfigLayer, Source, Sourced, Trust, resolve};

fn layer(source: Source, raw: &str) -> Sourced<ConfigLayer> {
    ConfigLayer::parse(source, raw).expect("valid config layer")
}

#[test]
fn untrusted_project_and_local_settings_report_every_discarded_setting() {
    let layers = [
        layer(
            Source::Project,
            "sandbox = \"ro\"\nmodel = \"model/project\"\n",
        ),
        layer(Source::Local, "sandbox = \"workspace\"\n"),
    ];
    let effective = resolve(&layers, Trust::Untrusted, None).expect("resolve");
    assert_eq!(effective.sandbox, None);
    assert_eq!(effective.discarded.len(), 2);
    assert_eq!(effective.discarded[0].source, Source::Project);
    assert_eq!(effective.discarded[0].settings, vec!["model", "sandbox"]);
    assert_eq!(effective.discarded[1].source, Source::Local);
    assert_eq!(effective.discarded[1].settings, vec!["sandbox"]);
}

#[test]
fn trusted_ambient_resolution_preserves_sandbox_and_reports_nothing_discarded() {
    let layers = [layer(Source::Project, "sandbox = \"ro\"\n")];
    let effective = resolve(&layers, Trust::Trusted, None).expect("resolve");
    assert_eq!(effective.sandbox, Some(ego_core::permit::Confinement::Ro));
    assert!(effective.discarded.is_empty());
}

#[test]
fn explicit_bare_is_distinguished_from_untrusted_when_describing_discard() {
    let layers = [
        layer(Source::User, "bare = true\n"),
        layer(Source::Project, "sandbox = \"ro\"\n"),
    ];
    let effective = resolve(&layers, Trust::Trusted, None).expect("resolve");
    assert_eq!(effective.discarded.len(), 1);
    let message = effective.discarded[0].to_string();
    assert!(message.contains("bare posture"), "{message}");
    assert!(!message.contains("workspace is untrusted"), "{message}");
}

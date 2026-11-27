use super::*;
use ego_core::config::{ConfigLayer, Source, Sourced, Trust, resolve};
use ego_core::permit::{CanonPath, ProfileRoot};

fn effective(text: &str) -> Effective {
    resolve(
        &[Sourced {
            source: Source::User,
            value: toml::from_str::<ConfigLayer>(text).unwrap(),
        }],
        Trust::Untrusted,
        None,
    )
    .unwrap()
}
fn confinement(roots: &[(&str, RootAccess)]) -> RunConfinement {
    RunConfinement::new(
        Confinement::Off,
        false,
        CanonPath::new(crate::ai_test_support::native("/workspace")),
    )
    .unwrap()
    .with_roots(
        roots
            .iter()
            .map(|(path, access)| ProfileRoot::new(CanonPath::new(path), *access))
            .collect(),
    )
}

// Catches: disabled provider filters fall back globally, or unrelated/unconfigured filters add noise.
#[test]
fn startup_filter_uses_applicable_configuration_and_preserves_disabled_override() {
    for (extra, expected) in [
        ("", None),
        (
            "[auth.other.protocol_filter]\ncommand='/filters/other'\n",
            None,
        ),
        (
            "[protocol_filter]\ncommand='/filters/global'\n",
            Some("enabled (global) · global"),
        ),
        (
            "[protocol_filter]\ncommand='/filters/global'\n[auth.openai.protocol_filter]\ncommand='/filters/provider'\nenabled=false\n",
            Some("disabled (provider) · provider"),
        ),
    ] {
        let shown = render(
            "openai/model",
            "API key · environment",
            &effective(&format!("mode='yolo'\nsandbox='off'\n{extra}")),
            &confinement(&[]),
            None,
            120,
        );
        match expected {
            Some(expected) => {
                assert!(shown.contains(expected), "{shown}");
                assert!(shown.contains("/config details"));
            }
            None => assert!(!shown.contains("filter"), "{shown}"),
        }
        assert!(shown.contains("yolo (user)"));
        assert!(shown.contains("off (user)"));
        assert_eq!(shown.matches("! security").count(), 1);
    }
}

// Catches: grants vanish past three entries, colliding paths look equal, or attachment scope implies workspace access.
#[test]
fn startup_groups_grants_and_complete_perimeter_retains_hidden_paths() {
    let policy = confinement(&[
        ("/attachments/file-set", RootAccess::Read),
        ("/one/shared/data", RootAccess::Read),
        ("/two/shared/data", RootAccess::ReadWrite),
        ("/third/root", RootAccess::Read),
        ("/fourth/root", RootAccess::ReadWrite),
    ]);
    let state = effective("mode='yolo'\nsandbox='off'");
    let shown = render(
        "ollama/model",
        "none (local provider)",
        &state,
        &policy,
        Some(Path::new("/attachments/file-set")),
        120,
    );
    for expected in [
        "attach   1 directory",
        "roots    4 directories",
        "ro …/attachments/file-set",
        "ro …/one/shared/data",
        "rw …/two/shared/data",
        "+1 more · /config perimeter",
        "workspace access is separate",
    ] {
        assert!(shown.contains(expected), "missing {expected}: {shown}");
    }
    assert!(!shown.contains("…/fourth/root"));
    let details = crate::perimeter::Perimeter::session(&policy, &state, policy.roots()).text();
    assert!(details.contains("/fourth/root\tread-write"), "{details}");
}

// Catches: long components, Unicode, and colliding UUID prefixes escape bounded hanging layout.
#[test]
fn startup_rows_fit_supported_widths_with_distinct_directory_references() {
    let policy = confinement(&[
        (
            "/a/attachments/pty-12345678-0000-0000-0000-000000000001",
            RootAccess::Read,
        ),
        (
            "/a/attachments/pty-12345678-0000-0000-0000-000000000002",
            RootAccess::Read,
        ),
        (
            "/資料/this-component-is-too-long-to-display-at-forty-columns",
            RootAccess::ReadWrite,
        ),
    ]);
    for width in [120, 80, 60, 40] {
        let shown = render(
            "provider/long-model-identity-with-漢字",
            "subscription (OAuth) · stored credential",
            &effective("sandbox='off'"),
            &policy,
            None,
            width,
        );
        assert!(
            shown.lines().all(|line| Line::plain(line).width() <= width),
            "width {width}: {shown}"
        );
        let entries = shown
            .lines()
            .filter(|line| line.trim_start().starts_with("ro "))
            .collect::<Vec<_>>();
        assert_eq!(entries.len(), 2, "{shown}");
        assert_ne!(entries[0], entries[1], "{shown}");
        // The route may wrap between its command and argument, never inside a word.
        assert!(
            shown
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .contains("/config perimeter"),
            "{shown}"
        );
        assert!(
            shown
                .lines()
                .filter(|line| line.trim_start().starts_with("stored credential"))
                .all(|line| line.starts_with("           "))
        );
    }
}

// Catches: an adaptive writable child is shown as rw despite an admitted read-only ancestor.
#[test]
fn adaptive_directory_summary_preserves_read_only_ancestor_restrictions() {
    let policy = RunConfinement::new(
        Confinement::Adaptive,
        false,
        CanonPath::new(crate::ai_test_support::native("/workspace")),
    )
    .unwrap()
    .with_roots(vec![ProfileRoot::new(
        CanonPath::new(crate::ai_test_support::native("/workspace/read-only")),
        RootAccess::Read,
    )])
    .with_adaptive_perimeter(ego_core::permit::AdaptivePerimeter {
        home: CanonPath::new("/home"),
        writable: vec![CanonPath::new(crate::ai_test_support::native(
            "/workspace/read-only/child",
        ))],
        credentials: vec![],
        controls: vec![],
    });
    let shown = render(
        "ollama/model",
        "none (local provider)",
        &effective("sandbox='adaptive'"),
        &policy,
        None,
        120,
    );
    assert!(shown.contains("ro …/read-only/child"), "{shown}");
    assert!(!shown.contains("rw …/read-only/child"));
}

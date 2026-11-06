use ego_core::health::{HealthProbe, HealthStatus, VisibleRoots};
use ego_tools::http::{ProbeLayer, Reached, Unreached};
use url::Url;

use super::{Network, Probe, configured_targets, provider_item};

fn url() -> Url {
    Url::parse("https://user:secret@api.example.test:8443/v1/?key=hunter2").unwrap()
}

fn text(item: &ego_core::health::HealthItem) -> String {
    format!(
        "{} {}",
        item.detail,
        item.remediation
            .as_ref()
            .map_or(String::new(), ToString::to_string)
    )
}

#[test]
fn every_failed_layer_is_named_and_leaks_no_url_secret() {
    for (layer, label) in [
        (ProbeLayer::Dns, "DNS"),
        (ProbeLayer::Tcp, "TCP"),
        (ProbeLayer::Tls, "TLS"),
        (ProbeLayer::Http, "HTTP"),
    ] {
        let item = provider_item(
            "p",
            &url(),
            Err(Unreached {
                layer,
                detail: "boom".into(),
            }),
            &VisibleRoots::default(),
        )
        .unwrap();
        assert_eq!(item.status, HealthStatus::Warning);
        let detail = item.detail.to_string();
        assert!(
            detail.contains(&format!("api.example.test:8443 {label} failed: boom")),
            "{detail}"
        );
        assert!(!text(&item).contains("secret") && !text(&item).contains("hunter2"));
    }
}

#[test]
fn an_unauthenticated_refusal_is_reachable_and_a_server_error_is_not() {
    let roots = VisibleRoots::default();
    let ok = provider_item("p", &url(), Ok(Reached { status: 401 }), &roots).unwrap();
    assert_eq!(ok.status, HealthStatus::Ok);
    let down = provider_item("p", &url(), Ok(Reached { status: 503 }), &roots).unwrap();
    assert_eq!(down.status, HealthStatus::Warning);
    assert!(down.detail.to_string().contains("HTTP failed"));
}

#[test]
fn offline_skips_every_probe_even_with_a_configured_provider() {
    let home = tempfile::tempdir().unwrap();
    let network = Network {
        workspace: home.path().to_path_buf(),
        home: Some(home.path().to_path_buf()),
        perimeter: None,
        probe: Probe::Offline,
    };
    let items = network.observe(&VisibleRoots::default()).unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].status, HealthStatus::NotChecked);
    assert!(items[0].detail.to_string().contains("offline posture"));
}

#[test]
fn a_provider_is_configured_by_its_key_variable_and_not_by_default() {
    let home = tempfile::tempdir().unwrap();
    let none = configured_targets(Some(home.path()), &|_| None).unwrap();
    assert!(none.is_empty(), "{none:?}");
    let set = configured_targets(Some(home.path()), &|name| {
        (name == ego_ai::provider::ANTHROPIC_KEY_VAR).then(|| "k".to_owned())
    })
    .unwrap();
    assert_eq!(
        set.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(),
        ["anthropic"]
    );
    let empty = configured_targets(Some(home.path()), &|name| {
        (name == ego_ai::provider::ANTHROPIC_KEY_VAR).then(String::new)
    })
    .unwrap();
    assert!(empty.is_empty());
}

fn observed(home: &tempfile::TempDir, probe: Probe) -> Vec<ego_core::health::HealthItem> {
    Network {
        workspace: home.path().to_path_buf(),
        home: Some(home.path().to_path_buf()),
        perimeter: None,
        probe,
    }
    .observe(&VisibleRoots::default())
    .unwrap()
}

// Catches: doctor probes the network without the user's opt-in.
#[test]
fn without_the_flag_or_the_key_no_probe_runs() {
    let home = tempfile::tempdir().unwrap();
    let items = observed(&home, Probe::Configured);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].status, HealthStatus::NotChecked);
    assert!(
        items[0].detail.to_string().contains("--probe-network"),
        "{}",
        items[0].detail
    );
}

// Catches: the opt-in is ignored (the key or the flag), so the user cannot turn probes on.
#[test]
fn the_user_key_or_the_flag_turns_probes_on() {
    const CHILD: &str = "EGO_DOCTOR_NETWORK_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command.args([
            "--exact",
            "doctor::network::tests::the_user_key_or_the_flag_turns_probes_on",
            "--nocapture",
        ]);
        command.env(CHILD, "1");
        for row in crate::model::rows(&Default::default()).unwrap() {
            if let Some(name) = row.credential.environment_var() {
                command.env_remove(name);
            }
        }
        command.env_remove(crate::model::OLLAMA_HOST);
        command.env_remove("EGO_PROFILE");
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "isolated doctor test failed: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let home = tempfile::tempdir().unwrap();
    // A workspace perimeter avoids adaptive TMPDIR admission affecting this test.
    std::fs::write(home.path().join("config.toml"), "sandbox = \"workspace\"\n").unwrap();
    // No provider is configured, so an enabled probe has nothing to contact.
    let flag = observed(&home, Probe::Requested);
    assert!(
        flag[0]
            .detail
            .to_string()
            .contains("no provider is configured")
    );
    std::fs::write(
        home.path().join("config.toml"),
        "sandbox = \"workspace\"\ndoctor_network = true\n",
    )
    .unwrap();
    let key = observed(&home, Probe::Configured);
    assert!(
        key[0]
            .detail
            .to_string()
            .contains("no provider is configured")
    );
}

// Catches: the offline posture loses to the opt-in, so `--offline` still probes.
#[test]
fn offline_wins_over_the_configured_opt_in() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("config.toml"), "doctor_network = true\n").unwrap();
    let items = observed(&home, Probe::Offline);
    assert!(items[0].detail.to_string().contains("offline posture"));
}

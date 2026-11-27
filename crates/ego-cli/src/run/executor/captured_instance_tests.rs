//! Real ACP session-admission to executable-preparation authority regression.

use super::*;
use ego_acp::serve::SessionConfinementAdmission;
use ego_core::config::{ProviderInstance, ProviderInstanceModel, ProviderModelContract};
use ego_core::event::time::Clock;
use ego_core::generator::capabilities::*;
use ego_core::session_lifecycle::SessionLifecycleStore;

fn instance() -> ProviderInstance {
    ProviderInstance {
        profile: "openai-compatible".parse().unwrap(),
        base_url: "http://127.0.0.1:48888/v1/".parse().unwrap(),
        credential_ref: Some("office".parse().unwrap()),
        models: ["assistant", "other"]
            .into_iter()
            .map(|alias| {
                (
                    alias.parse().unwrap(),
                    ProviderInstanceModel {
                        upstream_id: "deployment-a".into(),
                        contract: ProviderModelContract::ChatStandard,
                        metering: ego_core::budget::MeteringIdentity::Currency {
                            settlement: ego_core::budget::SettlementSource::Quote,
                        },
                        facts: ModelCapabilities {
                            schema_version: ModelCapabilities::SCHEMA_VERSION,
                            max_context_tokens: Some(32000),
                            max_output_tokens: Some(2048),
                            max_tools: Some(8),
                            supports_parallel_tool_calls: false,
                            reasoning_carryover: ReasoningCarryover::Unsupported,
                            reasoning_efforts: ReasoningEffortSupport::Only(
                                [ThinkingLevel::Low, ThinkingLevel::High].into(),
                            ),
                            default_reasoning_effort: Some(ThinkingLevel::Low),
                            reasoning_state_dialect: None,
                            cancel: CancelSemantics::Cooperative,
                            cache: CacheRetention::None,
                            transport: Transport::Sse,
                            image_input: ImageInputSupport::Unknown,
                            audio_input: AudioInputSupport::Unknown,
                        },
                    },
                )
            })
            .collect(),
    }
}

fn save(home: &Path, spec: &ProviderInstance, model: &str) {
    std::fs::write(home.join("config.toml"), format!(
        "model='{model}'\nbudget='1k'\nsandbox='off'\n\
         [connections.office]\nname='Work'\nvendor='openai'\naccess='key'\nkey_env='INSTANCE_A_KEY'\n\
         [connections.replacement]\nname='Replacement'\nvendor='openai'\naccess='key'\nkey_env='INSTANCE_B_KEY'\n\
         {}", toml::to_string(&std::collections::BTreeMap::from([(
             "providers",
             std::collections::BTreeMap::from([(
                 "instances",
                 std::collections::BTreeMap::from([("office", spec)]),
             )]),
         )])).unwrap(),
    )).unwrap();
}

// Catches a seat advertising A while actual ACP turn/resume preparation rereads
// endpoint, credential reference, upstream alias or facts from saved replacement B.
// Also catches freezing away an explicit trusted workspace, or blanket-freezing
// compiled-account execution merely because unused instance definitions exist.
#[tokio::test]
async fn acp_execution_and_resume_use_the_session_instance_capture_after_save() {
    const CHILD: &str = "EGO_ACP_INSTANCE_EXECUTION_CHILD";
    const TEST: &str = "run::executor::captured_instance_tests::acp_execution_and_resume_use_the_session_instance_capture_after_save";
    if std::env::var_os(CHILD).is_none() {
        let home = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST, "--nocapture"])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", home.path())
            .env("EGO_HOME", home.path())
            .env("INSTANCE_A_KEY", "synthetic-instance-a")
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains(&format!("test {TEST} ... ok")));
        return;
    }
    let home = config::ego_home().unwrap();
    let workspace = home.join("workspace");
    std::fs::create_dir_all(workspace.join(".ego")).unwrap();
    std::fs::write(workspace.join(".ego/config.toml"), "mode='plan'\n").unwrap();
    // The executor takes the harness's canonical workspace; on Windows that is the `\\?\` form.
    let workspace = workspace.canonicalize().unwrap();
    let root = RunRoot::new(workspace.clone()).unwrap();
    let store = Arc::new(JsonlStore::new(
        home.join("sessions"),
        workspace.clone(),
        Box::new(ego_tools::store::OsIo),
    ));
    let original = instance();
    save(&home, &original, "office/assistant");
    let effective =
        crate::run::protocol_effective(&workspace, None, SessionShape::default()).unwrap();
    let controls = Arc::new(
        crate::acp_controls::AcpSessionControls::new(&effective, Confinement::Off, None)
            .with_store(store.clone()),
    );
    let admission = crate::acp_confinement::CliSessionConfinementAdmission::new(
        store.clone(),
        controls.clone(),
        Confinement::Off,
        CancelToken::new(),
    );
    let session = SessionId::new();
    admission
        .configure_profiles(session, &root, None, None)
        .unwrap();
    // Session publication precedes measurement/model-history lookup in the real host.
    let publication = store
        .publish_empty_session(session, root.as_path(), SystemClock.now())
        .unwrap();
    assert!(publication.is_visible());
    controls
        .seed_confinement(session, Vec::new(), Vec::new())
        .unwrap();
    let executor = CliRunExecutor::for_acp(
        None,
        SessionShape::default(),
        store.clone(),
        Arc::new(EvidenceAdmissionService::new()),
    )
    .with_acp_controls(controls.clone());

    let mut replacement = original.clone();
    replacement.base_url = "http://127.0.0.1:49999/v1/".parse().unwrap();
    replacement.credential_ref = Some("replacement".parse().unwrap());
    for model in replacement.models.values_mut() {
        model.upstream_id = "deployment-b".into();
        model.facts.max_output_tokens = Some(4096);
    }
    save(&home, &replacement, "office/other");

    for (continuation, trust, selected, model) in [
        (false, Trust::Untrusted, None, "office/assistant"),
        (true, Trust::Trusted, Some("office/other"), "office/other"),
    ] {
        let invocation = RunInvocationId::new();
        let request = RunPreparation {
            run_id: RunId::new(session, invocation.uuid()),
            invocation,
            root: root.clone(),
            selections: RunSelections::new(
                selected.map(|model: &str| ModelRef(model.into())),
                None,
                None,
                None,
                None,
            )
            .with_trust(trust),
            continuation,
            origin: ego_core::run::RunOrigin::TopLevel,
        };
        let prepared = executor.prepare(&request).await.unwrap();
        assert_eq!(prepared.model().0, model);
        let (_, prepared) = prepared.into_parts();
        let instance = prepared
            .effective
            .providers
            .instances
            .values()
            .next()
            .unwrap();
        assert_eq!(instance.value.base_url.as_url().port(), Some(48888));
        assert_eq!(
            instance.value.credential_ref.as_ref().unwrap().as_str(),
            "office"
        );
        assert_eq!(
            instance.value.models.values().next().unwrap().upstream_id,
            "deployment-a"
        );
        assert_eq!(
            instance
                .value
                .models
                .values()
                .next()
                .unwrap()
                .facts
                .max_output_tokens,
            Some(2048)
        );
        assert_eq!(
            prepared.effective.connections["office"].key_env.as_deref(),
            Some("INSTANCE_A_KEY")
        );
        assert_eq!(prepared.trust, trust);
        if trust == Trust::Trusted {
            assert_eq!(prepared.effective.mode, Some(Mode::Plan));
        }
    }
    // An untrusted request must not parse project bytes; a trusted request
    // must refuse the same malformed workspace rather than bypass its source.
    std::fs::write(workspace.join(".ego/config.toml"), "mode = [broken").unwrap();
    for trust in [Trust::Untrusted, Trust::Trusted] {
        let invocation = RunInvocationId::new();
        let request = RunPreparation {
            run_id: RunId::new(session, invocation.uuid()),
            invocation,
            root: root.clone(),
            selections: RunSelections::empty().with_trust(trust),
            continuation: true,
            origin: ego_core::run::RunOrigin::TopLevel,
        };
        let prepared = executor.prepare(&request).await;
        if trust == Trust::Untrusted {
            let (_, prepared) = prepared.unwrap().into_parts();
            assert_eq!(
                prepared
                    .effective
                    .providers
                    .instances
                    .values()
                    .next()
                    .unwrap()
                    .value
                    .base_url
                    .as_url()
                    .port(),
                Some(48888)
            );
        } else {
            assert!(prepared.is_err());
        }
    }

    // Account-bound seats stay on their ordinary execution path even when
    // unrelated instance definitions exist in the same saved configuration.
    save(&home, &original, "openai/gpt-5");
    let text = std::fs::read_to_string(home.join("config.toml")).unwrap();
    std::fs::write(
        home.join("config.toml"),
        format!("connection='office'\n{text}"),
    )
    .unwrap();
    let account_session = SessionId::new();
    admission
        .configure_profiles(account_session, &root, None, None)
        .unwrap();
    let publication = store
        .publish_empty_session(account_session, root.as_path(), SystemClock.now())
        .unwrap();
    assert!(publication.is_visible());
    let invocation = RunInvocationId::new();
    let compiled = RunPreparation {
        run_id: RunId::new(account_session, invocation.uuid()),
        invocation,
        root: root.clone(),
        selections: RunSelections::new(
            Some(ModelRef("openai/gpt-5".into())),
            None,
            None,
            None,
            None,
        ),
        continuation: false,
        origin: ego_core::run::RunOrigin::TopLevel,
    };
    assert!(controls.execution_config(&compiled).unwrap().is_none());
    save(&home, &replacement, "office/other");

    // A new admission may observe B, but its absent declared key refuses before
    // initialize/prepare can create an effect scope; invalid refs do likewise.
    let fresh = SessionId::new();
    assert!(
        admission
            .configure_profiles(fresh, &root, None, None)
            .is_err()
    );
    assert!(controls.captured_config(fresh).unwrap().is_none());
    replacement.credential_ref = Some("missing".parse().unwrap());
    save(&home, &replacement, "office/other");
    assert!(
        admission
            .configure_profiles(SessionId::new(), &root, None, None)
            .is_err()
    );
    // The rejected admission did not overwrite the original seat's source.
    let pinned = controls
        .profile_effective(session, None, None)
        .unwrap()
        .unwrap();
    assert_eq!(
        pinned
            .providers
            .instances
            .values()
            .next()
            .unwrap()
            .value
            .base_url
            .as_url()
            .port(),
        Some(48888)
    );
}

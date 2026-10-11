//! Run one child through production effect ports under an admitted policy.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use ego_core::event::id::SessionId;
use ego_core::permit::{EnvPolicy, SpawnSpec};
use ego_core::turn::CancelToken;
use ego_tools::exec::{
    ExecLimits, ExecRunConfig, HttpCallTimeout, RunEffectPorts, SessionEvidenceStore,
};

pub(super) fn execute(
    policy: ego_core::permit::RunConfinement,
    root: &Path,
    argv: Vec<std::ffi::OsString>,
) -> Result<ego_core::permit::ToolResult, ego_core::permit::ProcessError> {
    execute_with_grants(policy, root, argv, None)
}

pub(super) fn execute_with_grants(
    policy: ego_core::permit::RunConfinement,
    root: &Path,
    argv: Vec<std::ffi::OsString>,
    grants: Option<Arc<dyn ego_core::permit::SandboxGrantPort>>,
) -> Result<ego_core::permit::ToolResult, ego_core::permit::ProcessError> {
    let evidence =
        SessionEvidenceStore::open(root.join("evidence"), SessionId::new(), 1024 * 1024).unwrap();
    let spec = SpawnSpec::new(
        argv,
        Some(policy.workspace().clone()),
        Some(Duration::from_secs(10)),
        EnvPolicy::Inherited,
    )
    .unwrap();
    let mut config = ExecRunConfig::new(
        policy,
        evidence,
        CancelToken::new(),
        ExecLimits::default(),
        HttpCallTimeout::default(),
        None,
        Arc::new(ego_tools::http::SystemResolver::new()),
    );
    if let Some(grants) = grants {
        config = config.with_sandbox_grants(grants);
    }
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let (ports, _, _) = RunEffectPorts::for_run(config).await.unwrap();
            ports.process().execute(&spec).await
        })
}

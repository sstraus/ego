//! Shared advisory request fixture; execution evidence stays with each scenario.
#![allow(dead_code)]

use ego_core::config::{Mode, ModelRef};
use ego_core::generator::types::GenRequest;
use ego_core::permit::judge::{JudgeConfig, JudgeInput, JudgeTaskContext};
use ego_core::permit::{
    CanonPath, Classification, Confinement, EnvPolicy, ExecCaps, Op, PolicyEngine, PreparedCall,
    RunConfinement, SpawnSpec,
};

pub fn judge_call(argv: &[String]) -> PreparedCall {
    PreparedCall::new(
        "exec",
        Op::Spawn {
            spec: SpawnSpec::new(
                argv.iter().map(Into::into).collect(),
                Some(CanonPath::new("/ws")),
                None,
                EnvPolicy::Inherited,
            )
            .unwrap(),
        },
    )
}

pub fn judge_request(call: &PreparedCall, context: &JudgeTaskContext) -> GenRequest {
    let policy = PolicyEngine::new(CanonPath::new("/ws"), Mode::Auto, Default::default());
    let Classification::Ask(pending) = policy.classify(call.clone()) else {
        panic!("command must reach the advisory boundary");
    };
    JudgeInput::new(
        call,
        &RunConfinement::new(Confinement::Workspace, false, CanonPath::new("/ws")).unwrap(),
        ExecCaps::empty(),
        pending.request().why(),
    )
    .request_with_context(
        &JudgeConfig {
            identity: ModelRef("openrouter/example/judge".into()),
            request_model: "example/judge".into(),
            max_output_tokens: Some(512),
        },
        context,
        None,
    )
}

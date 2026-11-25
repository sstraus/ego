//! What the model is told about the agent it runs in.
//!
//! Without it the model answers "which model are you?" from its training data
//! and "which repository?" by running `pwd`, although every one of those facts
//! was resolved before the request was built.

use crate::config::ModelRef;
use crate::generator::types::{ThinkingLevel, ToolDescriptor};
use crate::permit::Confinement;
use crate::turn::GenerationPolicySnapshot;

/// The facts about this agent that only the harness resolves: which build it
/// is, how its processes are confined, and which host connected to it.
///
/// The rest of the identity — model, effort, workspace, permission mode, tools,
/// MCP servers — is read per generation from the resolved profile, the captured
/// policy and the admitted catalogue, so a `/model` switch, a fallback or a
/// tool-list change cannot leave the model reading a stale copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentIdentity {
    pub version: String,
    pub sandbox: Confinement,
    pub offline: bool,
    /// The client that initialized the ACP connection, when there is one.
    pub host: Option<HostClient>,
    /// Configuration entries that were skipped or degraded at startup, so the
    /// model can tell the user what is wrong.
    pub config_notes: Vec<String>,
}

/// An ACP client as it named itself in `initialize`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostClient {
    pub name: String,
    pub version: String,
}

impl AgentIdentity {
    pub(crate) fn stable_text(&self) -> String {
        crate::model_prompts::stable_identity(&self.version, self.sandbox == Confinement::Adaptive)
    }

    pub(crate) fn render(
        &self,
        session: crate::event::id::SessionId,
        model: &ModelRef,
        thinking: ThinkingLevel,
        policy: &GenerationPolicySnapshot,
        tools: &[ToolDescriptor],
        mcp_servers: &[String],
    ) -> String {
        let host = match &self.host {
            Some(host) => format!("{} {} over ACP", host.name, host.version),
            None => "none (direct command-line invocation)".to_owned(),
        };
        let network = if self.offline { "offline" } else { "online" };
        let notices = if self.config_notes.is_empty() {
            String::new()
        } else {
            format!(
                "\n- Configuration notices (ego skipped or narrowed these entries; tell the user): {}",
                self.config_notes.join(" | ")
            )
        };
        format!(
            "Current runtime facts (ego-owned; updated when facts change):\n\
             - Ego session ID: {session}\n\
             - Model: {model} (reasoning effort: {thinking})\n\
             - Workspace root: {workspace}\n\
             - Posture: permission mode: {mode}; sandbox: {sandbox}; network: {network}\n\
             - Host: {host}\n\
             - Tools: {tools}\n\
             - MCP servers: {servers}{notices}",
            thinking = thinking.as_str(),
            workspace = policy.workspace().as_path().display(),
            mode = policy.mode(),
            sandbox = self.sandbox,
            tools = listed(tools.iter().map(|tool| tool.name.as_str())),
            servers = listed(mcp_servers.iter().map(String::as_str)),
        )
    }
}

fn listed<'a>(names: impl Iterator<Item = &'a str>) -> String {
    let names: Vec<String> = names.map(|name| format!("`{name}`")).collect();
    if names.is_empty() {
        "none".to_owned()
    } else {
        names.join(", ")
    }
}

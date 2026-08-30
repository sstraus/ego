//! Strict configuration and the trust gate.
//!
//! See `ARCHITECTURE.md` and `plans/m0a-contracts.md` Step 12. Resolution is a
//! pure function over injected sources — no file I/O lives in core.

mod discovery;
mod error;
mod layer;
mod mutation;
mod resolve;
mod ruleset;
mod source;

pub use discovery::{
    CwdOutsideRoot, Found, INSTRUCTIONS_FILE, Instruction, InstructionIo, InstructionSet, Limits,
    Refusal, Selection, Skipped, SystemPrompt, load, plan, system_prompt,
};
pub use error::ConfigError;
pub use layer::{
    AuthProviderLayer, ConfigLayer, McpEnvironmentName, McpHttpAuth, McpHttpConfig, McpHttpUrl,
    McpLayer, McpLayerEntry, McpServerConfig, McpServerName, McpStdioConfig, McpValueError, Mode,
    ModelRef, PermCfg, ProfileLayer, StatusSegmentName, StatuslineLayer, UiLayer,
};
pub use mutation::{
    UserConfigError, UserConfigMutation, UserConfigPublication, UserConfigStore, UserMcpSlot,
};
pub use resolve::{
    ActiveProfileProvenance, Effective, McpConfigProvenance, Posture, ResolvedMcpConfig,
    ResolvedMcpEntry, resolve, resolve_selected,
};
pub use ruleset::{OriginSet, Ruleset, SourcedRule};
pub use source::{Source, Sourced, Trust};

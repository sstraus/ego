//! Strict configuration and the trust gate.
//!
//! See `ARCHITECTURE.md` and `plans/m0a-contracts.md` Step 12. Resolution is a
//! pure function over injected sources — no file I/O lives in core.

mod discovery;
mod error;
mod layer;
mod mutation;
mod resolve;
pub mod rule_management;
mod ruleset;
mod session_prompt;
mod source;
mod vocabulary;
mod windows_unconfined;
mod workspace_trust;

pub use discovery::{
    CwdOutsideRoot, Found, INSTRUCTIONS_FILE, Instruction, InstructionIo, InstructionSet, Limits,
    Refusal, Selection, Skipped, SystemPrompt, load, plan, system_prompt,
};
pub use error::ConfigError;
pub use layer::{
    AuthProviderLayer, ConfigLayer, ExecEnvironment, ExecLayer, FORBIDDEN_AUTH_HEADER_NAMES,
    McpEnvironmentName, McpHttpAuth, McpHttpConfig, McpHttpUrl, McpLayer, McpLayerEntry,
    McpServerConfig, McpServerName, McpStdioConfig, McpValueError, Mode, ModelRef, Network,
    PermCfg, ProfileLayer, StatusSegmentName, StatuslineLayer, UiLayer, is_forbidden_auth_header,
};
pub use mutation::{
    ManagedRuleEffect, RuleMutation, RuleMutationFacts, RuleSnapshot, UserConfigEditHandle,
    UserConfigEditResult, UserConfigError, UserConfigMutation, UserConfigMutationFacts,
    UserConfigMutationResult, UserConfigPublication, UserConfigSnapshot, UserConfigStore,
};
pub use resolve::{
    ActiveProfileProvenance, Effective, McpConfigProvenance, Posture, ResolvedMcpConfig,
    ResolvedMcpEntry, resolve, resolve_selected, resolve_with_profile_ceiling,
};
pub use ruleset::{OriginSet, Ruleset, SourcedRule};
pub use source::{Source, Sourced, Trust};
pub use vocabulary::{ManagedConfigKey, ManagedConfigValue, ProfileName, UserConfigTarget};
pub use windows_unconfined::{UNCONFINED_CONTRACT_REVISION, UnconfinedLedger, UnconfinedRecord};
pub use workspace_trust::{
    ExecutableDeclaration, ExecutableSurface, ProgramEvidence, TRUST_DIGEST_MAX_BYTES,
    TrustInventory, TrustLedger, TrustRecord, TrustedContent,
};

pub use session_prompt::{
    SessionPromptOptions, SessionSystemPrompt, SystemPromptMode, validate_prompt_options,
};

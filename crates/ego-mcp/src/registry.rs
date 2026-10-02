//! Serialized MCP reconciliation and immutable admitted snapshots.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock, Weak};
use std::time::{Duration, Instant};

use ego_core::config::{McpConfigProvenance, McpServerConfig, McpServerName};
use ego_core::event::time::Clock;
use ego_core::generator::types::ToolDescriptor as GeneratorToolDescriptor;
use ego_core::mcp::{
    CacheScope, ContextEntry, ContextError, ContextSensitivity, ContextSnapshot, ContextSource,
    ForeignQuestionPort, McpAdmissionFingerprint, McpOperationKind, McpRuntimeBinding,
    McpServerInstructions, McpSubject,
};
use ego_core::permit::{PreparedCall, Tool, ToolError};
use ego_core::suspension::{
    ContinuationJournal, ContinuationKind, ContinuationSubject, McpCapturePin, ResumeFingerprints,
    ResumeFloor, ResumeKeySource, ResumeSealer,
};
use ego_core::turn::{
    AdmittedToolSource, CapturedToolCatalogue, GenerationPolicyCaptureStore,
    GenerationPolicySnapshot, McpRegistryEvent, McpRegistryJournal, McpRegistryJournalError,
    RegisteredTool, ToolCatalogueError, ToolEffectPorts,
};
use rmcp::model::{Prompt, Resource};
use sha2::{Digest, Sha256};

use crate::dynamic::{McpCapturedTool, McpOperation, McpToolDefinition};
use crate::hook::McpHookPort;
use crate::operation::{McpCaptureResolver, McpForeignQuestionPort};
use crate::{
    McpCollapsedBootstrap, McpCollapsedCatalogue, McpContinuationOwner, McpDuplexEffectsFactory,
    McpError, McpFailure, McpFuture, McpHttpEffects, McpOperationFactory,
    McpRuntimeCandidateAdmission, McpServerAdmission,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegistryTrigger {
    Add,
    Remove,
    Login,
    Reload,
    ConfigChanged,
    AuthorizationSucceeded,
    TtlExpired,
    ToolsListChanged,
    Rescan,
    Recovery,
}

impl RegistryTrigger {
    fn as_str(self) -> &'static str {
        match self {
            Self::Add => "add",
            Self::Remove => "remove",
            Self::Login => "login",
            Self::Reload => "reload",
            Self::ConfigChanged => "config_changed",
            Self::AuthorizationSucceeded => "authorization_succeeded",
            Self::TtlExpired => "ttl_expired",
            Self::ToolsListChanged => "tools_list_changed",
            Self::Rescan => "rescan",
            Self::Recovery => "recovery",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RegistryScope {
    All,
    Server(McpServerName),
}

impl RegistryScope {
    fn journal_value(&self) -> Option<String> {
        match self {
            Self::All => None,
            Self::Server(name) => Some(name.to_string()),
        }
    }

    fn includes(&self, name: &McpServerName) -> bool {
        matches!(self, Self::All) || matches!(self, Self::Server(selected) if selected == name)
    }
}

#[derive(Clone)]
pub struct McpRegistryCandidate {
    provenance: McpConfigProvenance,
    materialization: CandidateMaterialization,
}

#[derive(Clone)]
enum CandidateMaterialization {
    Ready(Arc<McpServerAdmission>),
    Rejected {
        name: McpServerName,
        config: McpServerConfig,
        failure: McpFailure,
    },
}

impl McpRegistryCandidate {
    pub fn ready(admission: Arc<McpServerAdmission>, provenance: McpConfigProvenance) -> Self {
        Self {
            provenance,
            materialization: CandidateMaterialization::Ready(admission),
        }
    }

    pub fn rejected(
        name: McpServerName,
        config: McpServerConfig,
        provenance: McpConfigProvenance,
        failure: McpFailure,
    ) -> Self {
        Self {
            provenance,
            materialization: CandidateMaterialization::Rejected {
                name,
                config,
                failure,
            },
        }
    }

    pub fn name(&self) -> &McpServerName {
        match &self.materialization {
            CandidateMaterialization::Ready(admission) => admission.name(),
            CandidateMaterialization::Rejected { name, .. } => name,
        }
    }

    pub fn config(&self) -> &McpServerConfig {
        match &self.materialization {
            CandidateMaterialization::Ready(admission) => admission.config(),
            CandidateMaterialization::Rejected { config, .. } => config,
        }
    }

    pub fn provenance(&self) -> &McpConfigProvenance {
        &self.provenance
    }
}

#[derive(Clone)]
pub struct McpRegistryRefresh {
    candidates: Vec<McpRegistryCandidate>,
    trigger: RegistryTrigger,
    scope: RegistryScope,
}

impl McpRegistryRefresh {
    pub fn new(
        mut candidates: Vec<McpRegistryCandidate>,
        trigger: RegistryTrigger,
        scope: RegistryScope,
    ) -> Result<Self, McpRegistryError> {
        if candidates
            .iter()
            .any(|candidate| candidate.config().validate().is_err())
        {
            return Err(McpRegistryError::InvalidCandidate(
                "invalid MCP registry candidate configuration".to_owned(),
            ));
        }
        candidates.sort_by(|left, right| left.name().cmp(right.name()));
        if candidates
            .windows(2)
            .any(|pair| pair[0].name() == pair[1].name())
        {
            return Err(McpRegistryError::InvalidCandidate(
                "duplicate MCP registry candidate name".to_owned(),
            ));
        }
        Ok(Self {
            candidates,
            trigger,
            scope,
        })
    }

    pub fn candidates(&self) -> &[McpRegistryCandidate] {
        &self.candidates
    }

    pub fn trigger(&self) -> RegistryTrigger {
        self.trigger
    }

    pub fn scope(&self) -> &RegistryScope {
        &self.scope
    }
}

#[derive(Clone)]
pub struct McpCandidateRequest {
    config_revision: u64,
    admission: Arc<McpServerAdmission>,
    provenance: McpConfigProvenance,
}

impl McpCandidateRequest {
    pub fn config_revision(&self) -> u64 {
        self.config_revision
    }

    pub fn admission(&self) -> Arc<McpServerAdmission> {
        Arc::clone(&self.admission)
    }

    pub fn name(&self) -> &McpServerName {
        self.admission.name()
    }

    pub fn config(&self) -> &McpServerConfig {
        self.admission.config()
    }

    pub fn provenance(&self) -> &McpConfigProvenance {
        &self.provenance
    }
}

pub trait McpCandidateAdmission: Send + Sync {
    fn admit(
        &self,
        request: McpCandidateRequest,
    ) -> McpFuture<'static, Result<McpAdmittedCandidate, McpCandidateError>>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum McpCandidateError {
    Invalid {
        cause: String,
    },
    AuthorizationRequired {
        issuer: String,
        protected_resource: String,
        scopes: Vec<String>,
    },
}

impl McpCandidateError {
    pub fn failure(&self) -> McpFailure {
        match self {
            Self::Invalid { .. } => McpFailure::InvalidValue,
            Self::AuthorizationRequired { .. } => McpFailure::Authorization,
        }
    }
}

struct CandidateFailure {
    failure: McpFailure,
    authorization: Option<CandidateAuthorizationFailure>,
    detail: Option<String>,
}

/// The longest candidate cause a status diagnostic carries.
const MAX_DIAGNOSTIC_DETAIL_CHARS: usize = 256;

/// One line of at most `MAX_DIAGNOSTIC_DETAIL_CHARS` characters, so a foreign
/// server's error text cannot flood or reshape the diagnostic. Control, line
/// separator and bidi formatting characters become blanks; whitespace runs
/// collapse before the bound so leading blanks cannot hide the cause. Secret
/// looking values are masked before the bound, so a secret straddling the cut
/// never leaks a prefix. `None` when nothing readable remains.
fn bounded_detail(cause: &str) -> Option<String> {
    let mut blanked = String::with_capacity(cause.len());
    for character in cause.chars() {
        if is_invisible(character) && !is_joiner_in_text(character, blanked.chars().next_back()) {
            continue;
        }
        blanked.push(if character.is_control() {
            ' '
        } else {
            character
        });
    }
    let collapsed = blanked.split_whitespace().collect::<Vec<_>>().join(" ");
    let detail: String = mask_secrets(&collapsed)
        .chars()
        .take(MAX_DIAGNOSTIC_DETAIL_CHARS)
        .collect();
    let detail = detail.trim_end().to_owned();
    (!detail.is_empty()).then_some(detail)
}

const SECRET_MASK: &str = "***";

/// Names whose value is a credential: the name ends with one of these
/// (case-insensitive, so `api_key`, `x-api-key` and `backend-secret` match).
const SECRET_NAME_SUFFIXES: [&str; 7] = [
    "key",
    "token",
    "secret",
    "password",
    "passwd",
    "pwd",
    "authorization",
];

/// Authentication schemes that precede the credential proper, as in
/// `token=Bearer <x>`; after an `authorization` name any alphabetic word is one.
const AUTH_SCHEMES: [&str; 6] = ["bearer", "basic", "digest", "token", "negotiate", "ntlm"];

/// Replaces credentials with `***`: PEM private-key blocks, the password of
/// URL userinfo, and the value of every `name=value` / `name: value` (also
/// `"name": "value"`) whose name ends in a secret suffix. Expects whitespace
/// already collapsed to single blanks.
fn mask_secrets(text: &str) -> String {
    mask_key_values(&mask_url_userinfo(&mask_pem_blocks(text)))
}

fn mask_pem_blocks(text: &str) -> String {
    const BEGIN: &str = "-----BEGIN ";
    const END: &str = "-----END ";
    const FENCE: &str = "-----";
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(BEGIN) {
        let label = &rest[start + BEGIN.len()..];
        let header_end = label.find(FENCE);
        let header = &label[..header_end.unwrap_or(label.len())];
        if !header.contains("PRIVATE KEY") {
            out.push_str(&rest[..start + BEGIN.len()]);
            rest = label;
            continue;
        }
        out.push_str(&rest[..start]);
        out.push_str(SECRET_MASK);
        // Without closing dashes the body cannot be told from the text after it.
        rest = match header_end {
            None => "",
            Some(header_end) => {
                let body = &label[header_end + FENCE.len()..];
                match body.find(END) {
                    Some(end) => {
                        let footer = &body[end + END.len()..];
                        footer
                            .find(FENCE)
                            .map_or("", |close| &footer[close + FENCE.len()..])
                    }
                    None => "",
                }
            }
        };
    }
    out.push_str(rest);
    out
}

fn mask_url_userinfo(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(scheme_end) = rest.find("://") {
        let authority_start = scheme_end + 3;
        out.push_str(&rest[..authority_start]);
        rest = &rest[authority_start..];
        let authority_end = rest
            .find(|c: char| c.is_whitespace() || matches!(c, '/' | '?' | '#'))
            .unwrap_or(rest.len());
        let Some(at) = rest[..authority_end].rfind('@') else {
            continue;
        };
        match rest[..at].find(':') {
            Some(colon) => {
                out.push_str(&rest[..=colon]);
                out.push_str(SECRET_MASK);
            }
            None if at > 0 => out.push_str(SECRET_MASK),
            None => {}
        }
        rest = &rest[at..];
    }
    out.push_str(rest);
    out
}

fn mask_key_values(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut index = 0;
    // Set after `name: "x-api-key"`: the next `value` member is the secret.
    let mut record_secret = false;
    while index < chars.len() {
        let character = chars[index];
        out.push(character);
        index += 1;
        // `,` only separates a quoted name from its value inside a tuple:
        // `("x-api-key", "v")`; `["key", "name"]` and `"invalid token","code"` are text.
        let is_separator = match character {
            '=' | ':' => true,
            ',' => opens_tuple(&out[..out.len() - 1]),
            _ => false,
        };
        if !is_separator {
            continue;
        }
        let key = name_before(&out[..out.len() - 1]);
        let secret = secret_suffix(&key);
        let record_value = secret.is_none() && key == "value" && record_secret;
        if secret.is_none() && !record_value {
            if key == "name" && character != ',' {
                record_secret = names_secret(&chars, index);
            }
            continue;
        }
        record_secret = false;
        let blanks = chars[index..].iter().take_while(|&&c| c == ' ').count();
        let start = index + blanks;
        if blanks > 0 && starts_with_pair(&chars, start) {
            // `key= mode=fast`: the value is empty and the next pair is unrelated.
            continue;
        }
        let end = value_end(&chars, start, secret == Some("authorization"));
        if end > start {
            out.push_str(&" ".repeat(blanks));
            out.push_str(SECRET_MASK);
            index = end;
        }
    }
    out
}

fn is_name_char(character: char) -> bool {
    character.is_alphanumeric() || matches!(character, '_' | '-' | '.')
}

/// The lowercase name right before a separator, looking past blanks, one `]`
/// and one closing quote (plain or JSON-escaped at any depth).
fn name_before(prefix: &str) -> String {
    let prefix = prefix.trim_end_matches(' ');
    let prefix = prefix.strip_suffix(']').unwrap_or(prefix);
    let prefix = match prefix.strip_suffix(['"', '\'']) {
        Some(unquoted) => unquoted.trim_end_matches('\\'),
        None => prefix,
    };
    let name: String = prefix
        .chars()
        .rev()
        .take_while(|&c| is_name_char(c))
        .collect();
    name.chars().rev().collect::<String>().to_lowercase()
}

fn secret_suffix(name: &str) -> Option<&'static str> {
    SECRET_NAME_SUFFIXES
        .into_iter()
        .find(|suffix| name.ends_with(suffix))
}

/// Whether `prefix` ends in `("<name>"`, the head of a Debug tuple.
fn opens_tuple(prefix: &str) -> bool {
    let Some(closed) = prefix.strip_suffix(['"', '\'']) else {
        return false;
    };
    closed
        .trim_end_matches('\\')
        .trim_end_matches(is_name_char)
        .strip_suffix(['"', '\''])
        .is_some_and(|opened| opened.trim_end_matches('\\').ends_with('('))
}

/// Whether the value after `name:` is itself a secret-looking single name
/// (`"name": "x-api-key"`), which makes the sibling `value` member a secret.
fn names_secret(chars: &[char], index: usize) -> bool {
    let start = index + chars[index..].iter().take_while(|&&c| c == ' ').count();
    let end = value_end(chars, start, false);
    let content: String = chars[start..end]
        .iter()
        .collect::<String>()
        .trim_matches(|c| matches!(c, '"' | '\'' | '\\' | ','))
        .to_lowercase();
    !content.is_empty() && content.chars().all(is_name_char) && secret_suffix(&content).is_some()
}

/// Whether the word at `start` is a `name=value` pair (`mode=fast`, not the
/// base64 padding of `YWJjZA==`).
fn starts_with_pair(chars: &[char], start: usize) -> bool {
    let word = &chars[start..word_end(chars, start)];
    let Some(equals) = word.iter().position(|&c| c == '=') else {
        return false;
    };
    let name = &word[..equals];
    name.first().is_some_and(|c| c.is_alphabetic() || *c == '_')
        && name
            .iter()
            .all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.'))
        && word.get(equals + 1).is_some_and(|&c| c != '=')
}

/// End of a quoted value starting at `start` (`None` when it is not quoted).
/// The quote may be JSON-escaped at any depth (`\"x\"`, `\\\"x\\\"`); then only
/// a run of exactly the opening backslashes plus the quote closes it, longer
/// runs are escaped quotes of the inner text. In a plain quote a backslash
/// escapes the next character.
fn quoted_end(chars: &[char], start: usize) -> Option<usize> {
    let slashes = chars[start..].iter().take_while(|&&c| c == '\\').count();
    let quote = match chars.get(start + slashes) {
        Some(&quote @ ('"' | '\'')) => quote,
        _ => return None,
    };
    let mut index = start + slashes + 1;
    while index < chars.len() {
        let character = chars[index];
        if slashes == 0 {
            match character {
                '\\' => index += 2,
                c if c == quote => return Some(index + 1),
                _ => index += 1,
            }
        } else if character == '\\' {
            let run = chars[index..].iter().take_while(|&&c| c == '\\').count();
            let after = index + run;
            if chars.get(after) == Some(&quote) {
                if run == slashes {
                    return Some(after + 1);
                }
                index = after + 1;
            } else {
                index = after;
            }
        } else {
            index += 1;
        }
    }
    Some(chars.len())
}

/// End of the balanced `{...}` / `[...]` starting at `start`, brackets inside
/// quoted strings ignored; the end of the text when unbalanced.
fn bracket_end(chars: &[char], start: usize) -> usize {
    let (mut depth, mut in_quote, mut index) = (0usize, false, start);
    while index < chars.len() {
        match chars[index] {
            '\\' => index += 1,
            '"' => in_quote = !in_quote,
            '{' | '[' if !in_quote => depth += 1,
            '}' | ']' if !in_quote => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return index + 1;
                }
            }
            _ => {}
        }
        index += 1;
    }
    chars.len()
}

/// End of the value starting at `start`: a quoted value, else the end of the
/// word, extended over the credential (and, after `authorization`, its
/// parameters, quoted values with blanks included) that follows an
/// authentication scheme.
fn value_end(chars: &[char], start: usize, any_scheme: bool) -> usize {
    if matches!(chars.get(start), Some('{' | '[')) {
        return bracket_end(chars, start);
    }
    if let Some(end) = quoted_end(chars, start) {
        return end;
    }
    let first_end = word_end(chars, start);
    let word: String = chars[start..first_end]
        .iter()
        .collect::<String>()
        .to_lowercase();
    let is_scheme = AUTH_SCHEMES.contains(&word.as_str()) || (any_scheme && !word.is_empty());
    if !is_scheme {
        return first_end;
    }
    let credential = first_end + chars[first_end..].iter().take_while(|&&c| c == ' ').count();
    let mut end = quoted_end(chars, credential).unwrap_or_else(|| word_end(chars, credential));
    if credential == first_end || end == credential {
        return first_end;
    }
    if any_scheme {
        // `AWS4-HMAC-SHA256 Credential=a, SignedHeaders=b, Signature=c` and
        // `Digest realm="my realm", response="c"`: keep taking `name=value`
        // words, and every word while a quote is open.
        let mut open = has_odd_quotes(&chars[credential..end]);
        loop {
            let next = end + chars[end..].iter().take_while(|&&c| c == ' ').count();
            let next_end = word_end(chars, next);
            let word = &chars[next..next_end];
            if next == end || word.is_empty() || !(open || word.contains(&'=')) {
                break;
            }
            open ^= has_odd_quotes(word);
            end = next_end;
        }
    }
    end
}

fn has_odd_quotes(chars: &[char]) -> bool {
    chars.iter().filter(|&&c| c == '"').count() % 2 == 1
}

fn word_end(chars: &[char], start: usize) -> usize {
    chars[start..]
        .iter()
        .position(|c| c.is_whitespace())
        .map_or(chars.len(), |offset| start + offset)
}

/// Joiners and variation selectors that sit after a non-ASCII character are
/// kept: emoji sequences and Persian or Indic text need them. Secret names are
/// ASCII, so a mark splitting one always follows an ASCII character.
fn is_joiner_in_text(character: char, previous: Option<char>) -> bool {
    matches!(
        u32::from(character),
        0x200C | 0x200D | 0xFE00..=0xFE0F | 0xE0100..=0xE01EF
    ) && previous.is_some_and(|previous| !previous.is_ascii())
}

/// Characters that render as nothing: every Unicode `Cf` (format) and `Cc`
/// (control) character that is not whitespace, plus the remaining
/// `Default_Ignorable_Code_Point` ones (variation selectors, combining grapheme
/// joiner, Hangul fillers, tag characters). They are dropped, not blanked, so
/// one cannot split a secret name such as `to<U+200E>ken`. The table follows
/// Unicode 16 and std has no property lookup for it.
fn is_invisible(character: char) -> bool {
    (character.is_control() && !character.is_whitespace())
        || matches!(
            u32::from(character),
            0x00AD
                | 0x034F
                | 0x0600..=0x0605
                | 0x061C
                | 0x06DD
                | 0x070F
                | 0x0890..=0x0891
                | 0x08E2
                | 0x115F..=0x1160
                | 0x17B4..=0x17B5
                | 0x180B..=0x180F
                | 0x200B..=0x200F
                | 0x202A..=0x202E
                | 0x2060..=0x206F
                | 0x3164
                | 0xFE00..=0xFE0F
                | 0xFEFF
                | 0xFFA0
                | 0xFFF0..=0xFFFB
                | 0x110BD
                | 0x110CD
                | 0x13430..=0x1343F
                | 0x1BCA0..=0x1BCA3
                | 0x1D173..=0x1D17A
                | 0xE0000..=0xE0FFF
        )
}

struct CandidateAuthorizationFailure {
    issuer: String,
    protected_resource: String,
    scopes: Vec<String>,
}

impl CandidateFailure {
    fn materialization(failure: McpFailure) -> Self {
        Self {
            failure,
            authorization: None,
            detail: None,
        }
    }

    fn admission(error: McpCandidateError) -> Self {
        match error {
            McpCandidateError::Invalid { cause } => Self {
                detail: bounded_detail(&cause),
                ..Self::materialization(McpFailure::InvalidValue)
            },
            McpCandidateError::AuthorizationRequired {
                issuer,
                protected_resource,
                scopes,
            } => Self {
                failure: McpFailure::Authorization,
                authorization: Some(CandidateAuthorizationFailure {
                    issuer,
                    protected_resource,
                    scopes,
                }),
                detail: None,
            },
        }
    }

    fn sanitized_cause(&self) -> &'static str {
        failure_message(self.failure)
    }

    /// What the host sees for this server: the fixed cause, then the bounded
    /// detail the candidate reported. The journal keeps only the fixed cause.
    fn diagnostic_message(&self) -> String {
        match &self.detail {
            Some(detail) => format!("{}: {detail}", self.sanitized_cause()),
            None => self.sanitized_cause().to_owned(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpCandidateIdentity(Arc<str>);

impl McpCandidateIdentity {
    pub fn validated(value: impl Into<String>) -> Self {
        Self(Arc::from(value.into()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct McpCapabilities {
    tools: bool,
    resources: bool,
    prompts: bool,
}

impl McpCapabilities {
    pub fn new(tools: bool, resources: bool, prompts: bool) -> Self {
        Self {
            tools,
            resources,
            prompts,
        }
    }

    pub fn tools() -> Self {
        Self::new(true, false, false)
    }

    pub fn has_tools(&self) -> bool {
        self.tools
    }

    pub fn has_resources(&self) -> bool {
        self.resources
    }

    pub fn has_prompts(&self) -> bool {
        self.prompts
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct McpCacheMetadata {
    source_revision: Option<Arc<str>>,
    expires_at: Option<Instant>,
}

impl McpCacheMetadata {
    pub fn new(source_revision: Option<String>, expires_at: Option<Instant>) -> Self {
        Self {
            source_revision: source_revision.map(Arc::from),
            expires_at,
        }
    }

    pub fn source_revision(&self) -> Option<&str> {
        self.source_revision.as_deref()
    }

    pub fn expires_at(&self) -> Option<Instant> {
        self.expires_at
    }
}

#[derive(Clone)]
pub struct McpCandidateInventory {
    capabilities: McpCapabilities,
    instructions: Option<String>,
    tools: Vec<GeneratorToolDescriptor>,
    resources: Vec<Resource>,
    prompts: Vec<Prompt>,
    cache: McpCacheMetadata,
    collapsed: Option<McpCollapsedCatalogue>,
}

impl McpCandidateInventory {
    /// `instructions` is what the server said about itself at `initialize`;
    /// `None` is a server that said nothing, which behaves exactly as before.
    pub fn new(
        capabilities: McpCapabilities,
        instructions: Option<String>,
        tools: Vec<GeneratorToolDescriptor>,
        resources: Vec<Resource>,
        prompts: Vec<Prompt>,
        cache: McpCacheMetadata,
        collapsed: Option<McpCollapsedCatalogue>,
    ) -> Self {
        Self {
            capabilities,
            instructions,
            tools,
            resources,
            prompts,
            cache,
            collapsed,
        }
    }
}

#[derive(Clone)]
pub struct McpAdmittedCandidate {
    admission: Arc<McpServerAdmission>,
    identity: McpCandidateIdentity,
    capabilities: McpCapabilities,
    instructions: Option<String>,
    descriptors: Arc<[GeneratorToolDescriptor]>,
    tools: Arc<[McpToolDefinition]>,
    resources: Arc<[Resource]>,
    prompts: Arc<[Prompt]>,
    cache: McpCacheMetadata,
    collapsed_bootstrap: Option<McpCollapsedBootstrap>,
    factory: Arc<dyn McpOperationFactory>,
}

impl McpAdmittedCandidate {
    pub fn new(
        admission: Arc<McpServerAdmission>,
        identity: McpCandidateIdentity,
        inventory: McpCandidateInventory,
        factory: Arc<dyn McpOperationFactory>,
    ) -> Result<Self, McpRegistryError> {
        if identity.as_str().is_empty() || admission.name().as_str() != identity.as_str() {
            return Err(McpRegistryError::InvalidCandidate(
                "MCP candidate identity does not match its configured name".to_owned(),
            ));
        }
        admission
            .config()
            .validate()
            .map_err(|error| McpRegistryError::InvalidCandidate(error.to_string()))?;
        let McpCandidateInventory {
            capabilities,
            instructions,
            tools,
            resources,
            prompts,
            cache,
            collapsed,
        } = inventory;
        validate_candidate_contents(&capabilities, &tools, &resources, &prompts)?;
        let collapsed_bootstrap = McpCollapsedBootstrap::detect(&tools)
            .map_err(|error| McpRegistryError::InvalidCandidate(error.to_string()))?;
        match (&collapsed_bootstrap, &collapsed) {
            (Some(_), Some(_)) => {}
            (Some(_), None) => {}
            (None, None) => {}
            _ => {
                return Err(McpRegistryError::InvalidCandidate(
                    "collapsed MCP catalogue has no compatible bootstrap".to_owned(),
                ));
            }
        }
        let descriptors: Arc<[GeneratorToolDescriptor]> = tools.into();
        let definitions = compile_candidate_tool_definitions(
            &descriptors,
            collapsed_bootstrap.as_ref(),
            collapsed.as_ref(),
        )?;
        let mut names = std::collections::BTreeSet::new();
        for definition in &definitions {
            if !names.insert(definition.name().to_owned()) {
                return Err(McpRegistryError::InvalidCandidate(format!(
                    "duplicate MCP tool {}",
                    definition.name()
                )));
            }
        }
        Ok(Self {
            admission,
            identity,
            capabilities,
            instructions,
            descriptors,
            tools: definitions.into(),
            resources: resources.into(),
            prompts: prompts.into(),
            cache,
            collapsed_bootstrap,
            factory,
        })
    }
}

fn compile_candidate_tool_definitions(
    descriptors: &[GeneratorToolDescriptor],
    collapsed_bootstrap: Option<&McpCollapsedBootstrap>,
    collapsed: Option<&McpCollapsedCatalogue>,
) -> Result<Vec<McpToolDefinition>, McpRegistryError> {
    let mut definitions = Vec::new();
    for descriptor in descriptors {
        let definition = match collapsed_bootstrap {
            Some(bootstrap) if descriptor.name == bootstrap.dispatch_name() => {
                collapsed.map(|catalogue| {
                    McpToolDefinition::collapsed(descriptor.clone(), catalogue.clone())
                })
            }
            _ => Some(McpToolDefinition::direct(descriptor.clone())),
        };
        if let Some(definition) = definition {
            definitions.push(
                definition
                    .map_err(|error| McpRegistryError::InvalidCandidate(error.to_string()))?,
            );
        }
    }
    Ok(definitions)
}

#[derive(Debug)]
pub enum McpRegistryError {
    Journal(McpRegistryJournalError),
    Candidate { server: String, cause: String },
    InvalidCandidate(String),
    PublicationUnavailable,
}

impl fmt::Display for McpRegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Journal(error) => error.fmt(f),
            Self::Candidate { server, cause } => {
                write!(f, "MCP candidate {server} was rejected: {cause}")
            }
            Self::InvalidCandidate(cause) => write!(f, "invalid MCP candidate: {cause}"),
            Self::PublicationUnavailable => f.write_str("MCP registry publication is unavailable"),
        }
    }
}

impl std::error::Error for McpRegistryError {}

impl From<McpRegistryJournalError> for McpRegistryError {
    fn from(error: McpRegistryJournalError) -> Self {
        Self::Journal(error)
    }
}

struct AdmittedServerSlice {
    admission: Arc<McpServerAdmission>,
    identity: McpCandidateIdentity,
    provenance: McpConfigProvenance,
    capabilities: McpCapabilities,
    instructions: Option<String>,
    descriptors: Arc<[GeneratorToolDescriptor]>,
    tools: Arc<[McpToolDefinition]>,
    resources: Arc<[Resource]>,
    prompts: Arc<[Prompt]>,
    cache: McpCacheMetadata,
    collapsed_bootstrap: Option<McpCollapsedBootstrap>,
    factory: Arc<dyn McpOperationFactory>,
}

impl AdmittedServerSlice {
    fn from_candidate(candidate: McpAdmittedCandidate, provenance: McpConfigProvenance) -> Self {
        Self {
            admission: candidate.admission,
            identity: candidate.identity,
            instructions: candidate.instructions,
            provenance,
            capabilities: candidate.capabilities,
            descriptors: candidate.descriptors,
            tools: candidate.tools,
            resources: candidate.resources,
            prompts: candidate.prompts,
            cache: candidate.cache,
            collapsed_bootstrap: candidate.collapsed_bootstrap,
            factory: candidate.factory,
        }
    }

    fn with_collapsed_catalogue(
        &self,
        catalogue: McpCollapsedCatalogue,
    ) -> Result<Self, McpRegistryError> {
        let bootstrap = self.collapsed_bootstrap.as_ref().ok_or_else(|| {
            McpRegistryError::InvalidCandidate(
                "tool schema targets a non-collapsed MCP server".to_owned(),
            )
        })?;
        let tools = compile_candidate_tool_definitions(
            &self.descriptors,
            Some(bootstrap),
            Some(&catalogue),
        )?;
        Ok(Self {
            admission: Arc::clone(&self.admission),
            identity: self.identity.clone(),
            provenance: self.provenance.clone(),
            capabilities: self.capabilities.clone(),
            instructions: self.instructions.clone(),
            descriptors: Arc::clone(&self.descriptors),
            tools: tools.into(),
            resources: Arc::clone(&self.resources),
            prompts: Arc::clone(&self.prompts),
            cache: self.cache.clone(),
            collapsed_bootstrap: self.collapsed_bootstrap,
            factory: Arc::clone(&self.factory),
        })
    }
}

#[derive(Clone)]
pub struct McpCapturedServer {
    admission: Arc<McpServerAdmission>,
    identity: McpCandidateIdentity,
    provenance: McpConfigProvenance,
    capabilities: McpCapabilities,
    instructions: Option<String>,
    cache: McpCacheMetadata,
    collapsed_bootstrap: Option<McpCollapsedBootstrap>,
    factory: Arc<dyn McpOperationFactory>,
}

impl McpCapturedServer {
    pub fn identity(&self) -> &McpCandidateIdentity {
        &self.identity
    }

    pub fn provenance(&self) -> &McpConfigProvenance {
        &self.provenance
    }

    pub fn capabilities(&self) -> &McpCapabilities {
        &self.capabilities
    }

    pub fn admission(&self) -> &McpServerAdmission {
        &self.admission
    }

    pub fn cache(&self) -> &McpCacheMetadata {
        &self.cache
    }

    pub fn collapsed_bootstrap(&self) -> Option<&McpCollapsedBootstrap> {
        self.collapsed_bootstrap.as_ref()
    }

    pub fn operation_factory(&self) -> Arc<dyn McpOperationFactory> {
        Arc::clone(&self.factory)
    }
}

impl From<McpCapturedServer> for Arc<McpServerAdmission> {
    fn from(server: McpCapturedServer) -> Self {
        server.admission
    }
}

pub(crate) struct McpToolSchemaCommand {
    server_admission: Arc<McpServerAdmission>,
    generation: String,
    admission_fingerprint: McpAdmissionFingerprint,
    requested_name: String,
    structured_content: serde_json::Value,
}

impl McpToolSchemaCommand {
    pub(crate) fn new(
        server: impl Into<Arc<McpServerAdmission>>,
        generation: String,
        admission_fingerprint: McpAdmissionFingerprint,
        requested_name: String,
        structured_content: serde_json::Value,
    ) -> Self {
        Self {
            server_admission: server.into(),
            generation,
            admission_fingerprint,
            requested_name,
            structured_content,
        }
    }
}

pub(crate) trait McpToolSchemaAdmission: Send + Sync {
    fn reconcile(&self, command: McpToolSchemaCommand) -> McpFuture<'static, Result<(), McpError>>;
}

struct ControllerToolSchemaAdmission {
    controller: Weak<McpRegistryController>,
}

impl McpToolSchemaAdmission for ControllerToolSchemaAdmission {
    fn reconcile(&self, command: McpToolSchemaCommand) -> McpFuture<'static, Result<(), McpError>> {
        let controller = self.controller.clone();
        Box::pin(async move {
            let controller = controller.upgrade().ok_or_else(|| {
                McpError::new(
                    McpFailure::Authority,
                    "collapsed MCP registry controller is unavailable",
                )
            })?;
            controller
                .reconcile_tool_schema(command)
                .await
                .map(|_| ())
                .map_err(|error| McpError::new(McpFailure::Authority, error.to_string()))
        })
    }
}

#[derive(Clone)]
pub struct McpCapturedContextOperation {
    entry: ContextEntry,
    subject: McpSubject,
    operation: McpOperationKind,
    admission: McpAdmissionFingerprint,
    server_admission: Arc<McpServerAdmission>,
    factory: Arc<dyn McpOperationFactory>,
}

type PreparedCapturedOperation = (
    Arc<McpServerAdmission>,
    Arc<dyn McpOperationFactory>,
    PreparedCall,
);

pub(crate) type PreparedResumeOperation = (
    Arc<McpServerAdmission>,
    Arc<dyn McpOperationFactory>,
    PreparedCall,
    ResumeFingerprints,
);

impl McpCapturedContextOperation {
    pub fn entry(&self) -> &ContextEntry {
        &self.entry
    }

    pub fn subject(&self) -> &McpSubject {
        &self.subject
    }

    pub fn operation(&self) -> McpOperationKind {
        self.operation
    }

    pub fn admission(&self) -> &McpAdmissionFingerprint {
        &self.admission
    }

    pub fn server_admission(&self) -> Arc<McpServerAdmission> {
        Arc::clone(&self.server_admission)
    }

    pub fn operation_factory(&self) -> Arc<dyn McpOperationFactory> {
        Arc::clone(&self.factory)
    }

    fn matches_resume(
        &self,
        subject: &ContinuationSubject,
        fingerprints: &ResumeFingerprints,
    ) -> bool {
        self.subject.server() == subject.server()
            && self.operation.method() == subject.operation()
            && self.admission.capture() == fingerprints.capture()
            && self.admission.operation() == fingerprints.tool()
            && self.admission.schema() == fingerprints.schema()
            && matches!(
                (subject.kind(), self.operation),
                (ContinuationKind::Resource, McpOperationKind::ReadResource)
                    | (ContinuationKind::Prompt, McpOperationKind::GetPrompt)
            )
    }

    fn prepare_captured(
        &self,
        parameters: serde_json::Value,
    ) -> Result<PreparedCapturedOperation, McpError> {
        let operation = McpOperation::captured(
            self.subject.clone(),
            self.operation,
            self.admission.clone(),
            parameters,
            Vec::new(),
        );
        let call = self
            .factory
            .prepare(Arc::clone(&self.server_admission), operation)
            .map_err(|error| {
                McpError::new(
                    McpFailure::Authority,
                    format!("continued MCP context could not be reconstructed: {error}"),
                )
            })?;
        Ok((
            Arc::clone(&self.server_admission),
            Arc::clone(&self.factory),
            call,
        ))
    }
}

pub struct McpCapturedContextSource {
    snapshot: ContextSnapshot,
    operations: Arc<[McpCapturedContextOperation]>,
}

impl McpCapturedContextSource {
    fn new(operations: Vec<McpCapturedContextOperation>) -> Result<Self, McpRegistryError> {
        let snapshot = ContextSnapshot::new(
            operations
                .iter()
                .map(|operation| operation.entry.clone())
                .collect(),
        )
        .map_err(|error| McpRegistryError::InvalidCandidate(error.to_string()))?;
        Ok(Self {
            snapshot,
            operations: operations.into(),
        })
    }

    pub fn operations(&self) -> &[McpCapturedContextOperation] {
        &self.operations
    }
}

impl ContextSource for McpCapturedContextSource {
    fn capture(&self) -> Result<ContextSnapshot, ContextError> {
        Ok(self.snapshot.clone())
    }
}

#[derive(Clone)]
pub struct McpRegistryCapture {
    inner: Arc<McpRegistryCaptureInner>,
}

struct McpRegistryCaptureInner {
    generation: Arc<str>,
    stable_fingerprint: Arc<str>,
    tools: Arc<[Arc<dyn Tool>]>,
    captured_tools: Arc<[Arc<McpCapturedTool>]>,
    contexts: Arc<McpCapturedContextSource>,
    servers: Arc<[McpCapturedServer]>,
    /// One entry per captured server that described itself, bounded, in the
    /// ascending server-name order `servers` is already canonicalised into.
    instructions: Arc<[McpServerInstructions]>,
}

impl McpRegistryCapture {
    pub fn generation(&self) -> &str {
        &self.inner.generation
    }

    pub fn stable_fingerprint(&self) -> &str {
        &self.inner.stable_fingerprint
    }

    pub fn tools(&self) -> &[Arc<dyn Tool>] {
        &self.inner.tools
    }

    pub fn context_source(&self) -> Arc<dyn ContextSource> {
        Arc::clone(&self.inner.contexts) as Arc<dyn ContextSource>
    }

    pub fn context_operations(&self) -> &[McpCapturedContextOperation] {
        self.inner.contexts.operations()
    }

    pub fn servers(&self) -> &[McpCapturedServer] {
        &self.inner.servers
    }

    /// Prepare one hook call from this immutable registry generation.
    pub fn prepare_hook_call(
        &self,
        server: &str,
        tool: &str,
        parameters: serde_json::Value,
    ) -> Result<PreparedCall, ToolError> {
        if !self
            .inner
            .servers
            .iter()
            .any(|candidate| candidate.identity().as_str() == server)
        {
            return Err(ToolError::InvalidParams(
                "hook MCP server is absent from the captured registry".to_owned(),
            ));
        }
        let captured = self
            .inner
            .captured_tools
            .iter()
            .find(|candidate| candidate.descriptor().name == tool)
            .ok_or_else(|| {
                ToolError::InvalidParams(
                    "hook MCP tool is absent from the captured registry".to_owned(),
                )
            })?;
        let (_, _, call) = captured.prepare_captured(parameters)?;
        let matches_server = call.mcp_envelope().is_some_and(|envelope| {
            envelope.operation() == McpOperationKind::CallTool
                && envelope.subject().server() == server
                && envelope.subject().operation_name() == tool
        });
        if !matches_server {
            return Err(ToolError::InvalidParams(
                "hook MCP target does not match the captured operation".to_owned(),
            ));
        }
        Ok(call)
    }

    pub(crate) fn prepare_resume(
        &self,
        subject: &ContinuationSubject,
        fingerprints: &ResumeFingerprints,
        original_parameters: &[u8],
    ) -> Result<PreparedResumeOperation, McpError> {
        let parameters = serde_json::from_slice(original_parameters).map_err(|error| {
            McpError::new(
                McpFailure::Authority,
                format!("stored MCP parameters are invalid: {error}"),
            )
        })?;
        let (admission, factory, call) = match subject.kind() {
            ContinuationKind::Tool => {
                let captured = self
                    .inner
                    .captured_tools
                    .iter()
                    .find(|tool| {
                        tool.matches_subject_parts(
                            subject.server(),
                            fingerprints.capture(),
                            fingerprints.tool(),
                            fingerprints.schema(),
                        )
                    })
                    .ok_or_else(|| {
                        McpError::new(
                            McpFailure::Authority,
                            "continued MCP tool is absent from the current registry capture",
                        )
                    })?;
                captured.prepare_captured(parameters).map_err(|error| {
                    McpError::new(
                        McpFailure::Authority,
                        format!("continued MCP tool could not be reconstructed: {error}"),
                    )
                })?
            }
            ContinuationKind::Resource | ContinuationKind::Prompt => {
                let captured = self
                    .inner
                    .contexts
                    .operations()
                    .iter()
                    .find(|operation| operation.matches_resume(subject, fingerprints))
                    .ok_or_else(|| {
                        McpError::new(
                            McpFailure::Authority,
                            "continued MCP context operation is absent from the current registry capture",
                        )
                    })?;
                captured.prepare_captured(parameters)?
            }
        };
        let envelope = call.mcp_envelope().ok_or_else(|| {
            McpError::new(
                McpFailure::Authority,
                "reconstructed MCP continuation has no protocol envelope",
            )
        })?;
        let current = ResumeFingerprints::new(
            envelope.admission().capture(),
            envelope.admission().operation(),
            envelope.admission().schema(),
            envelope.transport_fingerprint(),
        )
        .map_err(|error| McpError::new(McpFailure::Authority, error.to_string()))?;
        Ok((admission, factory, call, current))
    }
}

impl McpCapturePin for McpRegistryCapture {
    fn stable_fingerprint(&self) -> &str {
        McpRegistryCapture::stable_fingerprint(self)
    }

    fn instructions(&self) -> &[McpServerInstructions] {
        &self.inner.instructions
    }

    fn server_names(&self) -> Vec<String> {
        self.inner
            .servers
            .iter()
            .map(|server| server.identity.as_str().to_owned())
            .collect()
    }
}

/// Bound and order what the captured servers said about themselves.
///
/// The order is the servers' own names ascending, which is what
/// `captured_servers` already carries because it is built from a `BTreeMap`
/// keyed by name. Restated here rather than assumed, because the composition
/// order is a documented contract and an incidental one is not a contract.
fn captured_instructions(servers: &[McpCapturedServer]) -> Vec<McpServerInstructions> {
    let mut instructions: Vec<_> = servers
        .iter()
        .filter_map(|server| {
            let text = server.instructions.as_deref()?;
            let text = text.trim();
            (!text.is_empty()).then(|| McpServerInstructions::new(server.identity.as_str(), text))
        })
        .collect();
    instructions.sort_by(|left, right| left.server().cmp(right.server()));
    instructions
}

struct AdmittedRegistrySnapshot {
    number: u64,
    generation: Arc<str>,
    config_revision: u64,
    servers: BTreeMap<McpServerName, Arc<AdmittedServerSlice>>,
    tools: Arc<[RegisteredTool]>,
    capture: McpRegistryCapture,
}

impl AdmittedRegistrySnapshot {
    fn empty() -> Self {
        let contexts = Arc::new(
            McpCapturedContextSource::new(Vec::new())
                .expect("the empty MCP context capture is valid"),
        );
        Self {
            number: 0,
            generation: Arc::from("generation-0"),
            config_revision: 0,
            servers: BTreeMap::new(),
            tools: Arc::from(Vec::<RegisteredTool>::new()),
            capture: McpRegistryCapture {
                inner: Arc::new(McpRegistryCaptureInner {
                    generation: Arc::from("generation-0"),
                    stable_fingerprint: Arc::from(
                        stable_capture_fingerprint(&BTreeMap::new())
                            .expect("the empty MCP capture has a stable fingerprint"),
                    ),
                    tools: Arc::from(Vec::<Arc<dyn Tool>>::new()),
                    captured_tools: Arc::from(Vec::<Arc<McpCapturedTool>>::new()),
                    contexts,
                    servers: Arc::from(Vec::<McpCapturedServer>::new()),
                    instructions: Arc::from(Vec::<McpServerInstructions>::new()),
                }),
            },
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum McpServerState {
    Admitted,
    Degraded,
    AuthorizationRequired,
    Absent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum McpCacheFreshness {
    Uncached,
    Fresh,
    Expired,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpAuthorizationStatus {
    issuer: String,
    protected_resource: String,
    scopes: Arc<[String]>,
    request_correlation: Arc<str>,
}

impl McpAuthorizationStatus {
    pub fn issuer(&self) -> &str {
        &self.issuer
    }

    pub fn protected_resource(&self) -> &str {
        &self.protected_resource
    }

    pub fn scopes(&self) -> &[String] {
        &self.scopes
    }

    pub fn request_correlation(&self) -> &str {
        &self.request_correlation
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpServerDiagnostic {
    failure: McpFailure,
    message: Arc<str>,
}

impl McpServerDiagnostic {
    pub fn failure(&self) -> McpFailure {
        self.failure
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpServerStatus {
    name: McpServerName,
    state: McpServerState,
    last_good_generation: Option<Arc<str>>,
    capabilities: Option<McpCapabilities>,
    cache_source_revision: Option<Arc<str>>,
    cache_expires_at: Option<Instant>,
    cache_freshness: McpCacheFreshness,
    authorization: Option<McpAuthorizationStatus>,
    diagnostic: Option<McpServerDiagnostic>,
}

impl McpServerStatus {
    pub fn name(&self) -> &McpServerName {
        &self.name
    }

    pub fn state(&self) -> McpServerState {
        self.state
    }

    pub fn generation(&self) -> Option<&str> {
        self.last_good_generation.as_deref()
    }

    pub fn capabilities(&self) -> Option<&McpCapabilities> {
        self.capabilities.as_ref()
    }

    pub fn cache_freshness(&self) -> McpCacheFreshness {
        self.cache_freshness
    }

    pub fn cache_source_revision(&self) -> Option<&str> {
        self.cache_source_revision.as_deref()
    }

    pub fn authorization(&self) -> Option<&McpAuthorizationStatus> {
        self.authorization.as_ref()
    }

    pub fn diagnostic(&self) -> Option<&McpServerDiagnostic> {
        self.diagnostic.as_ref()
    }

    fn refresh_cache_freshness(&mut self, now: Instant) {
        self.cache_freshness = cache_freshness(
            self.cache_source_revision.as_deref(),
            self.cache_expires_at,
            now,
        );
    }
}

#[derive(Clone, Default)]
pub struct McpRegistryStatus {
    servers: Arc<[McpServerStatus]>,
}

impl McpRegistryStatus {
    pub fn servers(&self) -> &[McpServerStatus] {
        &self.servers
    }

    pub fn server(&self, name: &McpServerName) -> Option<&McpServerStatus> {
        self.servers
            .binary_search_by(|status| status.name.cmp(name))
            .ok()
            .map(|index| &self.servers[index])
    }

    pub fn is_degraded(&self) -> bool {
        self.servers
            .iter()
            .any(|server| server.state != McpServerState::Admitted)
    }

    pub fn mcp_errors(&self) -> Vec<String> {
        self.servers
            .iter()
            .filter_map(|server| {
                server.diagnostic.as_ref().map(|diagnostic| {
                    format!("MCP server `{}`: {}", server.name, diagnostic.message)
                })
            })
            .collect()
    }

    fn refreshed_at(&self, now: Instant) -> Self {
        let mut servers = self.servers.to_vec();
        for server in &mut servers {
            server.refresh_cache_freshness(now);
        }
        Self {
            servers: servers.into(),
        }
    }
}

fn cache_freshness(
    source_revision: Option<&str>,
    expires_at: Option<Instant>,
    now: Instant,
) -> McpCacheFreshness {
    if source_revision.is_none() && expires_at.is_none() {
        McpCacheFreshness::Uncached
    } else if expires_at.is_some_and(|expires_at| expires_at <= now) {
        McpCacheFreshness::Expired
    } else {
        McpCacheFreshness::Fresh
    }
}

fn status_from_snapshot(
    snapshot: &AdmittedRegistrySnapshot,
    overrides: BTreeMap<McpServerName, McpServerStatus>,
    now: Instant,
) -> McpRegistryStatus {
    let mut servers = snapshot
        .servers
        .iter()
        .map(|(name, server)| {
            (
                name.clone(),
                McpServerStatus {
                    name: name.clone(),
                    state: McpServerState::Admitted,
                    last_good_generation: Some(Arc::clone(&snapshot.generation)),
                    capabilities: Some(server.capabilities.clone()),
                    cache_source_revision: server.cache.source_revision.clone(),
                    cache_expires_at: server.cache.expires_at,
                    cache_freshness: cache_freshness(
                        server.cache.source_revision(),
                        server.cache.expires_at,
                        now,
                    ),
                    authorization: None,
                    diagnostic: None,
                },
            )
        })
        .collect::<BTreeMap<_, _>>();
    servers.extend(overrides);
    McpRegistryStatus {
        servers: servers.into_values().collect::<Vec<_>>().into(),
    }
}

fn failed_server_status(
    name: &McpServerName,
    failure: &CandidateFailure,
    request_id: &str,
    predecessor: &AdmittedRegistrySnapshot,
    now: Instant,
) -> McpServerStatus {
    let retained = predecessor.servers.get(name);
    let authorization = failure.authorization.as_ref().map(|authorization| {
        let mut scopes = authorization.scopes.clone();
        scopes.sort();
        scopes.dedup();
        McpAuthorizationStatus {
            issuer: authorization.issuer.clone(),
            protected_resource: authorization.protected_resource.clone(),
            scopes: scopes.into(),
            request_correlation: Arc::from(request_id),
        }
    });
    let cache_source_revision = retained.and_then(|server| server.cache.source_revision.clone());
    let cache_expires_at = retained.and_then(|server| server.cache.expires_at);
    McpServerStatus {
        name: name.clone(),
        state: if authorization.is_some() {
            McpServerState::AuthorizationRequired
        } else if retained.is_some() {
            McpServerState::Degraded
        } else {
            McpServerState::Absent
        },
        last_good_generation: retained.map(|_| Arc::clone(&predecessor.generation)),
        capabilities: retained.map(|server| server.capabilities.clone()),
        cache_freshness: cache_freshness(cache_source_revision.as_deref(), cache_expires_at, now),
        cache_source_revision,
        cache_expires_at,
        authorization,
        diagnostic: Some(McpServerDiagnostic {
            failure: failure.failure,
            message: Arc::from(failure.diagnostic_message()),
        }),
    }
}

fn failed_publication_status(
    predecessor: &AdmittedRegistrySnapshot,
    attempted_servers: Vec<McpServerName>,
    now: Instant,
) -> McpRegistryStatus {
    let mut servers = BTreeMap::new();
    for (name, server) in &predecessor.servers {
        servers.insert(
            name.clone(),
            McpServerStatus {
                name: name.clone(),
                state: McpServerState::Degraded,
                last_good_generation: Some(Arc::clone(&predecessor.generation)),
                capabilities: Some(server.capabilities.clone()),
                cache_source_revision: server.cache.source_revision.clone(),
                cache_expires_at: server.cache.expires_at,
                cache_freshness: cache_freshness(
                    server.cache.source_revision(),
                    server.cache.expires_at,
                    now,
                ),
                authorization: None,
                diagnostic: Some(McpServerDiagnostic {
                    failure: McpFailure::Protocol,
                    message: Arc::from("registry publication failed"),
                }),
            },
        );
    }
    for name in attempted_servers {
        servers.entry(name.clone()).or_insert(McpServerStatus {
            name,
            state: McpServerState::Absent,
            last_good_generation: None,
            capabilities: None,
            cache_source_revision: None,
            cache_expires_at: None,
            cache_freshness: McpCacheFreshness::Uncached,
            authorization: None,
            diagnostic: Some(McpServerDiagnostic {
                failure: McpFailure::Protocol,
                message: Arc::from("registry publication failed"),
            }),
        });
    }
    McpRegistryStatus {
        servers: servers.into_values().collect::<Vec<_>>().into(),
    }
}

fn failure_message(failure: McpFailure) -> &'static str {
    match failure {
        McpFailure::Absent => "server is unavailable",
        McpFailure::InvalidValue => "server configuration is invalid",
        McpFailure::InvalidResponse => "server returned an invalid response",
        McpFailure::Authority => "server authority was rejected",
        McpFailure::Transport => "server transport is unavailable",
        McpFailure::Protocol => "server protocol failed",
        McpFailure::Authorization => "authorization is required",
        McpFailure::Credential => "server credential is unavailable",
        McpFailure::Cancelled => "server admission was cancelled",
    }
}

struct PublishedState {
    snapshot: Arc<AdmittedRegistrySnapshot>,
    status: McpRegistryStatus,
    captures: BTreeMap<Arc<str>, Vec<Weak<McpRegistryCaptureInner>>>,
}

impl PublishedState {
    fn empty() -> Self {
        Self {
            snapshot: Arc::new(AdmittedRegistrySnapshot::empty()),
            status: McpRegistryStatus::default(),
            captures: BTreeMap::new(),
        }
    }
}

struct PublishedCaptureResolver {
    state: Arc<RwLock<PublishedState>>,
}

impl McpCaptureResolver for PublishedCaptureResolver {
    fn resolve_capture(
        &self,
        stable_fingerprint: &str,
    ) -> Result<McpRegistryCapture, McpRegistryError> {
        resolve_capture_in_state(&self.state, stable_fingerprint)
    }
}

fn resolve_capture_in_state(
    state: &RwLock<PublishedState>,
    stable_fingerprint: &str,
) -> Result<McpRegistryCapture, McpRegistryError> {
    state
        .read()
        .map_err(|_| McpRegistryError::PublicationUnavailable)?
        .captures
        .get(stable_fingerprint)
        .and_then(|captures| captures.iter().find_map(Weak::upgrade))
        .map(|inner| McpRegistryCapture { inner })
        .ok_or_else(|| {
            McpRegistryError::InvalidCandidate(
                "MCP registry capture identity is unavailable".to_owned(),
            )
        })
}

#[derive(Clone)]
pub struct McpRegistryPublication {
    generation: Arc<str>,
    predecessor: Option<Arc<str>>,
    config_revision: u64,
    admitted_servers: Vec<String>,
    admitted_tools: Vec<String>,
    capture: McpRegistryCapture,
}

impl McpRegistryPublication {
    pub fn generation(&self) -> &str {
        &self.generation
    }

    pub fn predecessor(&self) -> Option<&str> {
        self.predecessor.as_deref()
    }

    pub fn config_revision(&self) -> u64 {
        self.config_revision
    }

    pub fn admitted_servers(&self) -> &[String] {
        &self.admitted_servers
    }

    pub fn admitted_tools(&self) -> &[String] {
        &self.admitted_tools
    }

    pub fn capture(&self) -> &McpRegistryCapture {
        &self.capture
    }
}

struct RegistryToolSource {
    state: Arc<RwLock<PublishedState>>,
}

impl AdmittedToolSource for RegistryToolSource {
    fn capture(&self) -> Result<CapturedToolCatalogue, ToolCatalogueError> {
        let snapshot = self
            .state
            .read()
            .map_err(|_| ToolCatalogueError::CaptureUnavailable)?
            .snapshot
            .clone();
        CapturedToolCatalogue::new_mcp(
            snapshot.generation.to_string(),
            snapshot.tools.iter().cloned().collect(),
            snapshot.capture.clone(),
        )
    }
}

#[derive(Clone)]
pub struct McpRegistryRuntime {
    controller: Arc<McpRegistryController>,
    continuations: Arc<McpContinuationOwner>,
    resume_sealer: Arc<ResumeSealer>,
}

impl McpRegistryRuntime {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        registry_journal: Arc<dyn McpRegistryJournal>,
        continuation_journal: Arc<dyn ContinuationJournal>,
        policy_captures: Arc<dyn GenerationPolicyCaptureStore>,
        http_management: Arc<dyn McpHttpEffects>,
        stdio_management: Arc<dyn McpDuplexEffectsFactory>,
        keys: Arc<dyn ResumeKeySource>,
        clock: Arc<dyn Clock>,
        max_sse_event_size: usize,
        timeout: Duration,
    ) -> Result<Self, McpRegistryError> {
        Self::new_with_acp(
            registry_journal,
            continuation_journal,
            policy_captures,
            http_management,
            stdio_management,
            keys,
            clock,
            max_sse_event_size,
            timeout,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_with_acp(
        registry_journal: Arc<dyn McpRegistryJournal>,
        continuation_journal: Arc<dyn ContinuationJournal>,
        policy_captures: Arc<dyn GenerationPolicyCaptureStore>,
        http_management: Arc<dyn McpHttpEffects>,
        stdio_management: Arc<dyn McpDuplexEffectsFactory>,
        keys: Arc<dyn ResumeKeySource>,
        clock: Arc<dyn Clock>,
        max_sse_event_size: usize,
        timeout: Duration,
        acp_management: Option<Arc<dyn crate::McpAcpEffects>>,
    ) -> Result<Self, McpRegistryError> {
        if max_sse_event_size == 0 || timeout.is_zero() {
            return Err(McpRegistryError::InvalidCandidate(
                "invalid MCP runtime limits".to_owned(),
            ));
        }
        let lifetime_ms = u64::try_from(timeout.as_millis()).map_err(|_| {
            McpRegistryError::InvalidCandidate("invalid MCP runtime timeout".to_owned())
        })?;
        let resume_sealer = Arc::new(
            ResumeSealer::new(Arc::clone(&keys), Arc::clone(&clock), lifetime_ms)
                .map_err(|error| McpRegistryError::InvalidCandidate(error.to_string()))?,
        );
        registry_journal.replay()?;
        let state = Arc::new(RwLock::new(PublishedState::empty()));
        let resolver: Arc<dyn McpCaptureResolver> = Arc::new(PublishedCaptureResolver {
            state: Arc::clone(&state),
        });
        let continuations = Arc::new(McpContinuationOwner::new(
            continuation_journal,
            policy_captures,
            resolver,
            keys,
            clock,
        ));
        let owner = Arc::clone(&continuations);
        let controller = Arc::new_cyclic(move |controller| {
            let schemas: Arc<dyn McpToolSchemaAdmission> =
                Arc::new(ControllerToolSchemaAdmission {
                    controller: controller.clone(),
                });
            let admission: Arc<dyn McpCandidateAdmission> =
                Arc::new(McpRuntimeCandidateAdmission::new(
                    Arc::clone(&http_management),
                    Arc::clone(&stdio_management),
                    Arc::clone(&owner),
                    schemas,
                    max_sse_event_size,
                    timeout,
                    acp_management,
                ));
            McpRegistryController {
                journal: registry_journal,
                admission,
                reconcile: tokio::sync::Mutex::new(()),
                state,
                next_request: AtomicU64::new(1),
            }
        });
        Ok(Self {
            controller,
            continuations,
            resume_sealer,
        })
    }

    pub fn controller(&self) -> &Arc<McpRegistryController> {
        &self.controller
    }

    pub fn continuation_owner(&self) -> &Arc<McpContinuationOwner> {
        &self.continuations
    }
}

impl McpRuntimeBinding for McpRegistryRuntime {
    fn resume_sealer(&self) -> Option<Arc<ResumeSealer>> {
        Some(Arc::clone(&self.resume_sealer))
    }

    fn load_pending_foreign_policy(
        &self,
        continuation_id: &str,
    ) -> Result<Arc<GenerationPolicySnapshot>, String> {
        self.continuations
            .load_pending_foreign_policy(continuation_id)
            .map_err(|error| error.to_string())
    }

    fn bind_pending_foreign_continuation(
        &self,
        continuation_id: &str,
        snapshot: Arc<GenerationPolicySnapshot>,
    ) -> Result<(), String> {
        self.continuations
            .bind_pending_foreign_continuation(continuation_id, snapshot)
            .map_err(|error| error.to_string())
    }

    fn hook_invocation_port<'a>(
        &self,
        policy: Arc<ego_core::permit::PolicyEngine>,
        effects: ToolEffectPorts<'a>,
    ) -> Result<Arc<dyn ego_core::hook::HookInvocationPort + 'a>, String> {
        let capture = self
            .controller
            .capture()
            .map_err(|error| error.to_string())?;
        Ok(Arc::new(McpHookPort::new(capture, policy, effects)))
    }

    fn foreign_question_port<'session, 'turn>(
        &self,
        runtime: ego_core::hook::SessionHookRuntime<'session>,
        floor: ResumeFloor,
        effects: ToolEffectPorts<'turn>,
    ) -> Arc<dyn ForeignQuestionPort + 'turn>
    where
        'session: 'turn,
    {
        Arc::new(McpForeignQuestionPort::new(
            Arc::clone(&self.continuations),
            runtime,
            floor,
            effects,
        ))
    }
}

pub struct McpRegistryController {
    journal: Arc<dyn McpRegistryJournal>,
    admission: Arc<dyn McpCandidateAdmission>,
    reconcile: tokio::sync::Mutex<()>,
    state: Arc<RwLock<PublishedState>>,
    next_request: AtomicU64,
}

impl McpRegistryController {
    pub fn new(
        journal: Arc<dyn McpRegistryJournal>,
        admission: Arc<dyn McpCandidateAdmission>,
    ) -> Result<Arc<Self>, McpRegistryError> {
        journal.replay()?;
        Ok(Arc::new(Self {
            journal,
            admission,
            reconcile: tokio::sync::Mutex::new(()),
            state: Arc::new(RwLock::new(PublishedState::empty())),
            next_request: AtomicU64::new(1),
        }))
    }

    pub async fn reconcile(
        &self,
        refresh: McpRegistryRefresh,
    ) -> Result<McpRegistryPublication, McpRegistryError> {
        let _serial = self.reconcile.lock().await;
        let request_id = format!(
            "request-{}",
            self.next_request.fetch_add(1, Ordering::Relaxed)
        );
        let config_revision = self.journal.begin_refresh(
            request_id.clone(),
            refresh.trigger.as_str().to_owned(),
            refresh.scope.journal_value(),
        )?;
        let (predecessor, prior_status) = self.current_state()?;
        let mut successor = if matches!(&refresh.scope, RegistryScope::All) {
            BTreeMap::new()
        } else {
            predecessor.servers.clone()
        };
        let mut status_overrides = match &refresh.scope {
            RegistryScope::All => BTreeMap::new(),
            RegistryScope::Server(selected) => prior_status
                .servers()
                .iter()
                .filter(|status| status.name() != selected)
                .cloned()
                .map(|status| (status.name().clone(), status))
                .collect(),
        };

        for candidate in refresh.candidates() {
            let name = candidate.name();
            if !refresh.scope.includes(name) {
                continue;
            }
            let failure = match &candidate.materialization {
                CandidateMaterialization::Rejected { failure, .. } => {
                    Some(CandidateFailure::materialization(*failure))
                }
                CandidateMaterialization::Ready(admission) => {
                    let request = McpCandidateRequest {
                        config_revision,
                        admission: Arc::clone(admission),
                        provenance: candidate.provenance.clone(),
                    };
                    match self.admission.admit(request).await {
                        Ok(admitted) if Arc::ptr_eq(&admitted.admission, admission) => {
                            successor.insert(
                                name.clone(),
                                Arc::new(AdmittedServerSlice::from_candidate(
                                    admitted,
                                    candidate.provenance.clone(),
                                )),
                            );
                            None
                        }
                        Ok(_) => Some(CandidateFailure::admission(McpCandidateError::Invalid {
                            cause: "candidate changed its live admission authority".to_owned(),
                        })),
                        Err(error) => Some(CandidateFailure::admission(error)),
                    }
                }
            };

            if let Some(failure) = failure {
                self.record_candidate_failure(&request_id, name, &failure, &predecessor)?;
                if let Some(retained) = predecessor.servers.get(name) {
                    successor.insert(name.clone(), Arc::clone(retained));
                } else {
                    successor.remove(name);
                }
                status_overrides.insert(
                    name.clone(),
                    failed_server_status(name, &failure, &request_id, &predecessor, Instant::now()),
                );
            }
        }

        if let RegistryScope::Server(name) = &refresh.scope
            && !refresh
                .candidates()
                .iter()
                .any(|candidate| candidate.name() == name)
        {
            successor.remove(name);
            status_overrides.remove(name);
        }

        self.publish(
            &request_id,
            config_revision,
            predecessor,
            successor,
            status_overrides,
        )
    }

    pub(crate) async fn reconcile_tool_schema(
        &self,
        command: McpToolSchemaCommand,
    ) -> Result<McpRegistryPublication, McpRegistryError> {
        let _serial = self.reconcile.lock().await;
        let (predecessor, prior_status) = self.current_state()?;
        if command.generation.as_str() != predecessor.generation.as_ref()
            || command.admission_fingerprint.capture() != predecessor.capture.stable_fingerprint()
        {
            return Err(McpRegistryError::InvalidCandidate(
                "collapsed schema lookup source generation is stale".to_owned(),
            ));
        }
        let name = command.server_admission.name().clone();
        let server = predecessor.servers.get(&name).ok_or_else(|| {
            McpRegistryError::InvalidCandidate(
                "collapsed schema lookup source server is not admitted".to_owned(),
            )
        })?;
        if !Arc::ptr_eq(&server.admission, &command.server_admission) {
            return Err(McpRegistryError::InvalidCandidate(
                "collapsed schema lookup changed its live server admission".to_owned(),
            ));
        }
        let bootstrap = server.collapsed_bootstrap.as_ref().ok_or_else(|| {
            McpRegistryError::InvalidCandidate(
                "tool schema targets a non-collapsed MCP server".to_owned(),
            )
        })?;
        let lookup_name = bootstrap.lookup_name();
        let lookup = server
            .tools
            .iter()
            .find(|definition| definition.name() == lookup_name)
            .ok_or_else(|| {
                McpRegistryError::InvalidCandidate(
                    "collapsed schema lookup operation is not admitted".to_owned(),
                )
            })?;
        let expected = McpAdmissionFingerprint::new(
            predecessor.capture.stable_fingerprint(),
            format!("{name}/{lookup_name}"),
            lookup.admission_schema_fingerprint(),
        )
        .map_err(|error| McpRegistryError::InvalidCandidate(error.to_string()))?;
        if command.admission_fingerprint.capture() != expected.capture()
            || command.admission_fingerprint.operation() != expected.operation()
            || command.admission_fingerprint.schema() != expected.schema()
        {
            return Err(McpRegistryError::InvalidCandidate(
                "collapsed schema lookup admission fingerprint is stale".to_owned(),
            ));
        }
        let previous = server
            .tools
            .iter()
            .find_map(McpToolDefinition::collapsed_catalogue);
        let catalogue = McpCollapsedCatalogue::reconcile_structured_content(
            previous,
            &command.requested_name,
            command.structured_content,
        )
        .map_err(|error| McpRegistryError::InvalidCandidate(error.to_string()))?;
        let successor_server = Arc::new(server.with_collapsed_catalogue(catalogue)?);
        let request_id = format!(
            "request-{}",
            self.next_request.fetch_add(1, Ordering::Relaxed)
        );
        let config_revision = self.journal.begin_refresh(
            request_id.clone(),
            RegistryTrigger::ToolsListChanged.as_str().to_owned(),
            Some(name.to_string()),
        )?;
        let status_overrides = prior_status
            .servers()
            .iter()
            .filter(|status| status.name() != &name)
            .cloned()
            .map(|status| (status.name().clone(), status))
            .collect();
        let mut successor = predecessor.servers.clone();
        successor.insert(name, successor_server);
        self.publish(
            &request_id,
            config_revision,
            predecessor,
            successor,
            status_overrides,
        )
    }

    pub fn publication(&self) -> McpRegistryPublication {
        self.current_snapshot()
            .map(|snapshot| publication_of(&snapshot))
            .unwrap_or_else(|_| publication_of(&Arc::new(AdmittedRegistrySnapshot::empty())))
    }

    pub fn status(&self) -> McpRegistryStatus {
        self.state.read().map_or_else(
            |_| McpRegistryStatus::default(),
            |state| state.status.refreshed_at(Instant::now()),
        )
    }

    pub fn admitted_tools(&self) -> Arc<dyn AdmittedToolSource> {
        Arc::new(RegistryToolSource {
            state: Arc::clone(&self.state),
        })
    }

    pub fn capture(&self) -> Result<McpRegistryCapture, McpRegistryError> {
        self.current_snapshot()
            .map(|snapshot| snapshot.capture.clone())
    }

    pub fn resolve_capture(
        &self,
        stable_fingerprint: &str,
    ) -> Result<McpRegistryCapture, McpRegistryError> {
        resolve_capture_in_state(&self.state, stable_fingerprint)
    }

    fn current_snapshot(&self) -> Result<Arc<AdmittedRegistrySnapshot>, McpRegistryError> {
        self.state
            .read()
            .map(|state| Arc::clone(&state.snapshot))
            .map_err(|_| McpRegistryError::PublicationUnavailable)
    }

    fn current_state(
        &self,
    ) -> Result<(Arc<AdmittedRegistrySnapshot>, McpRegistryStatus), McpRegistryError> {
        self.state
            .read()
            .map(|state| (Arc::clone(&state.snapshot), state.status.clone()))
            .map_err(|_| McpRegistryError::PublicationUnavailable)
    }

    fn record_candidate_failure(
        &self,
        request_id: &str,
        server: &McpServerName,
        failure: &CandidateFailure,
        predecessor: &Arc<AdmittedRegistrySnapshot>,
    ) -> Result<(), McpRegistryError> {
        if let Some(authorization) = &failure.authorization {
            self.journal
                .append(McpRegistryEvent::AuthorizationRequired {
                    server: server.to_string(),
                    issuer: authorization.issuer.clone(),
                    protected_resource: authorization.protected_resource.clone(),
                    scopes: authorization.scopes.clone(),
                    request_correlation: request_id.to_owned(),
                })?;
        }
        let retained = predecessor
            .servers
            .contains_key(server)
            .then(|| predecessor.generation.to_string());
        self.journal.append(McpRegistryEvent::RefreshFailed {
            request_id: request_id.to_owned(),
            cause: failure.sanitized_cause().to_owned(),
            retained_generation: retained,
        })?;
        Ok(())
    }

    fn publish(
        &self,
        request_id: &str,
        config_revision: u64,
        predecessor: Arc<AdmittedRegistrySnapshot>,
        servers: BTreeMap<McpServerName, Arc<AdmittedServerSlice>>,
        status_overrides: BTreeMap<McpServerName, McpServerStatus>,
    ) -> Result<McpRegistryPublication, McpRegistryError> {
        let number = match predecessor.number.checked_add(1) {
            Some(number) => number,
            None => {
                return self.fail_publication(
                    request_id,
                    "registry generation space exhausted".to_owned(),
                    &predecessor,
                    servers.keys(),
                );
            }
        };
        let generation: Arc<str> = Arc::from(format!("generation-{number}"));
        let stable_fingerprint: Arc<str> = match stable_capture_fingerprint(&servers) {
            Ok(fingerprint) => Arc::from(fingerprint),
            Err(error) => {
                return self.fail_publication(
                    request_id,
                    error.to_string(),
                    &predecessor,
                    servers.keys(),
                );
            }
        };
        let mut captured_tools = Vec::<Arc<McpCapturedTool>>::new();
        for (name, server) in &servers {
            for definition in server.tools.iter() {
                let tool = match definition.capture(
                    name.as_str(),
                    &generation,
                    &stable_fingerprint,
                    Arc::clone(&server.admission),
                    Arc::clone(&server.factory),
                ) {
                    Ok(tool) => tool,
                    Err(error) => {
                        return self.fail_publication(
                            request_id,
                            error.to_string(),
                            &predecessor,
                            servers.keys(),
                        );
                    }
                };
                captured_tools.push(tool);
            }
        }
        captured_tools.sort_by_key(|tool| tool.descriptor().name);
        let effect_tools = captured_tools
            .iter()
            .cloned()
            .map(|tool| -> Arc<dyn Tool> { tool })
            .collect::<Vec<_>>();
        let tools = effect_tools
            .iter()
            .cloned()
            .map(RegisteredTool::Effect)
            .collect::<Vec<_>>();
        if let Err(error) = CapturedToolCatalogue::new(generation.to_string(), tools.clone()) {
            return self.fail_publication(
                request_id,
                error.to_string(),
                &predecessor,
                servers.keys(),
            );
        }
        let contexts = match capture_contexts(&stable_fingerprint, &servers) {
            Ok(contexts) => Arc::new(contexts),
            Err(error) => {
                return self.fail_publication(
                    request_id,
                    error.to_string(),
                    &predecessor,
                    servers.keys(),
                );
            }
        };
        let captured_servers = servers
            .values()
            .map(|server| McpCapturedServer {
                admission: Arc::clone(&server.admission),
                identity: server.identity.clone(),
                provenance: server.provenance.clone(),
                capabilities: server.capabilities.clone(),
                instructions: server.instructions.clone(),
                cache: server.cache.clone(),
                collapsed_bootstrap: server.collapsed_bootstrap,
                factory: Arc::clone(&server.factory),
            })
            .collect::<Vec<_>>();
        let instructions = captured_instructions(&captured_servers);
        let capture = McpRegistryCapture {
            inner: Arc::new(McpRegistryCaptureInner {
                generation: Arc::clone(&generation),
                stable_fingerprint,
                tools: effect_tools.into(),
                captured_tools: captured_tools.into(),
                contexts,
                servers: captured_servers.into(),
                instructions: instructions.into(),
            }),
        };
        let snapshot = Arc::new(AdmittedRegistrySnapshot {
            number,
            generation: Arc::clone(&generation),
            config_revision,
            servers,
            tools: tools.into(),
            capture,
        });
        let publication = publication_of(&snapshot);
        self.journal.append(McpRegistryEvent::SnapshotPublished {
            generation: generation.to_string(),
            predecessor: (predecessor.number > 0).then(|| predecessor.generation.to_string()),
            servers: publication.admitted_servers.clone(),
            descriptor_digests: descriptor_digests(&snapshot.tools),
        })?;
        let mut state = self
            .state
            .write()
            .map_err(|_| McpRegistryError::PublicationUnavailable)?;
        state.status = status_from_snapshot(&snapshot, status_overrides, Instant::now());
        let captures = state
            .captures
            .entry(Arc::clone(&snapshot.capture.inner.stable_fingerprint))
            .or_default();
        captures.retain(|capture| capture.strong_count() > 0);
        captures.push(Arc::downgrade(&snapshot.capture.inner));
        state.snapshot = snapshot;
        Ok(publication)
    }

    fn fail_publication<'a>(
        &self,
        request_id: &str,
        _cause: String,
        predecessor: &Arc<AdmittedRegistrySnapshot>,
        attempted_servers: impl Iterator<Item = &'a McpServerName>,
    ) -> Result<McpRegistryPublication, McpRegistryError> {
        let retained_generation =
            (predecessor.number > 0).then(|| predecessor.generation.to_string());
        self.journal.append(McpRegistryEvent::RefreshFailed {
            request_id: request_id.to_owned(),
            cause: "registry publication failed".to_owned(),
            retained_generation,
        })?;

        let attempted_servers = attempted_servers.cloned().collect::<Vec<_>>();
        let mut state = self
            .state
            .write()
            .map_err(|_| McpRegistryError::PublicationUnavailable)?;
        state.status = failed_publication_status(predecessor, attempted_servers, Instant::now());
        Err(McpRegistryError::InvalidCandidate(
            "registry publication failed".to_owned(),
        ))
    }
}

impl McpCaptureResolver for McpRegistryController {
    fn resolve_capture(
        &self,
        stable_fingerprint: &str,
    ) -> Result<McpRegistryCapture, McpRegistryError> {
        resolve_capture_in_state(&self.state, stable_fingerprint)
    }
}

fn publication_of(snapshot: &Arc<AdmittedRegistrySnapshot>) -> McpRegistryPublication {
    let mut admitted_tools = snapshot
        .tools
        .iter()
        .map(|tool| tool.descriptor().name)
        .collect::<Vec<_>>();
    admitted_tools.sort();
    McpRegistryPublication {
        generation: Arc::clone(&snapshot.generation),
        predecessor: (snapshot.number > 1)
            .then(|| Arc::from(format!("generation-{}", snapshot.number.saturating_sub(1)))),
        config_revision: snapshot.config_revision,
        admitted_servers: snapshot.servers.keys().map(ToString::to_string).collect(),
        admitted_tools,
        capture: snapshot.capture.clone(),
    }
}

fn descriptor_digests(tools: &[RegisteredTool]) -> Vec<String> {
    let mut digests = tools
        .iter()
        .map(|tool| {
            let descriptor = tool.descriptor();
            let mut hash = Sha256::new();
            hash.update(descriptor.name.as_bytes());
            hash.update([0]);
            hash.update(descriptor.description.as_bytes());
            hash.update([0]);
            hash.update(descriptor.parameters.to_string().as_bytes());
            format!("sha256:{:x}", hash.finalize())
        })
        .collect::<Vec<_>>();
    digests.sort();
    digests
}

fn stable_capture_fingerprint(
    servers: &BTreeMap<McpServerName, Arc<AdmittedServerSlice>>,
) -> Result<String, McpRegistryError> {
    let mut fingerprint = StableCaptureHasher::new();
    fingerprint.count("server-count", servers.len());
    for (name, server) in servers {
        fingerprint.field("server", name.as_str().as_bytes());
        fingerprint.field("candidate-identity", server.identity.as_str().as_bytes());
        fingerprint.provenance(&server.provenance);
        fingerprint.server_admission(&server.admission)?;
        fingerprint.field("capability-tools", &[u8::from(server.capabilities.tools)]);
        fingerprint.field(
            "capability-resources",
            &[u8::from(server.capabilities.resources)],
        );
        fingerprint.field(
            "capability-prompts",
            &[u8::from(server.capabilities.prompts)],
        );
        fingerprint.optional_str("cache-source-revision", server.cache.source_revision());
        fingerprint.field(
            "collapsed-bootstrap",
            &[u8::from(server.collapsed_bootstrap.is_some())],
        );

        let mut descriptors = server.descriptors.iter().collect::<Vec<_>>();
        descriptors.sort_by(|left, right| left.name.cmp(&right.name));
        fingerprint.count("descriptor-count", descriptors.len());
        for descriptor in descriptors {
            fingerprint.field("descriptor-name", descriptor.name.as_bytes());
            fingerprint.field("descriptor-description", descriptor.description.as_bytes());
            fingerprint.json("descriptor-parameters", &descriptor.parameters);
        }

        let mut definitions = server.tools.iter().collect::<Vec<_>>();
        definitions.sort_by(|left, right| left.name().cmp(right.name()));
        fingerprint.count("operation-count", definitions.len());
        for definition in definitions {
            fingerprint.field("operation-name", definition.name().as_bytes());
            fingerprint.field(
                "operation-schema",
                definition.admission_schema_fingerprint().as_bytes(),
            );
        }

        let mut resources = server.resources.iter().collect::<Vec<_>>();
        resources.sort_by(|left, right| left.uri.cmp(&right.uri));
        fingerprint.count("resource-count", resources.len());
        for resource in resources {
            let value = serde_json::to_value(resource)
                .map_err(|error| McpRegistryError::InvalidCandidate(error.to_string()))?;
            fingerprint.json("resource", &value);
        }

        let mut prompts = server.prompts.iter().collect::<Vec<_>>();
        prompts.sort_by(|left, right| left.name.cmp(&right.name));
        fingerprint.count("prompt-count", prompts.len());
        for prompt in prompts {
            let value = serde_json::to_value(prompt)
                .map_err(|error| McpRegistryError::InvalidCandidate(error.to_string()))?;
            fingerprint.json("prompt", &value);
        }
    }
    Ok(fingerprint.finish())
}

struct StableCaptureHasher(Sha256);

impl StableCaptureHasher {
    fn new() -> Self {
        let mut hash = Sha256::new();
        hash.update(b"ego:mcp-registry-capture:v1");
        Self(hash)
    }

    fn field(&mut self, name: &str, value: &[u8]) {
        self.bytes(name.as_bytes());
        self.bytes(value);
    }

    fn bytes(&mut self, value: &[u8]) {
        self.0.update(value.len().to_string().as_bytes());
        self.0.update([0]);
        self.0.update(value);
    }

    fn count(&mut self, name: &str, value: usize) {
        self.field(name, value.to_string().as_bytes());
    }

    fn optional_str(&mut self, name: &str, value: Option<&str>) {
        self.field(name, &[u8::from(value.is_some())]);
        if let Some(value) = value {
            self.field(name, value.as_bytes());
        }
    }

    fn provenance(&mut self, provenance: &McpConfigProvenance) {
        match provenance {
            McpConfigProvenance::Layer { source } => {
                self.field("provenance-kind", b"layer");
                self.field("provenance-source", source.to_string().as_bytes());
            }
            McpConfigProvenance::Profile { source, name } => {
                self.field("provenance-kind", b"profile");
                self.field("provenance-source", source.to_string().as_bytes());
                self.field("provenance-profile", name.as_bytes());
            }
        }
    }

    fn server_admission(&mut self, admission: &McpServerAdmission) -> Result<(), McpRegistryError> {
        match admission.config() {
            McpServerConfig::Stdio(config) => {
                self.field("transport", b"stdio");
                self.field("command", config.command.as_bytes());
                self.count("argument-count", config.args.len());
                for argument in &config.args {
                    self.field("argument", argument.as_bytes());
                }
                self.field("cwd-present", &[u8::from(config.cwd.is_some())]);
                if let Some(cwd) = &config.cwd {
                    self.field("cwd", cwd.as_os_str().as_encoded_bytes());
                }
                let mut inherited = config
                    .inherit_env
                    .iter()
                    .map(|name| name.as_str())
                    .collect::<Vec<_>>();
                inherited.sort();
                self.count("inherit-env-count", inherited.len());
                for name in inherited {
                    self.field("inherit-env", name.as_bytes());
                }
                let environment = admission.stdio_environment().ok_or_else(|| {
                    McpRegistryError::InvalidCandidate(
                        "admitted MCP stdio server has no stdio material".to_owned(),
                    )
                })?;
                self.count("environment-count", environment.len());
                for binding in environment {
                    let binding = binding
                        .explicit_binding()
                        .map_err(|error| McpRegistryError::InvalidCandidate(error.to_string()))?;
                    self.field("environment-name", binding.name().to_string().as_bytes());
                    self.field(
                        "environment-value",
                        binding.expose_value().expose_secret_bytes(),
                    );
                }
            }
            McpServerConfig::Http(config) => {
                self.field("transport", b"http");
                self.field("url", config.url.to_string().as_bytes());
                self.field(
                    "http-auth",
                    match config.auth {
                        ego_core::config::McpHttpAuth::None => b"none",
                        ego_core::config::McpHttpAuth::Oauth => b"oauth",
                    },
                );
                self.field(
                    "allow-insecure-loopback",
                    &[u8::from(config.allow_insecure_loopback)],
                );
                let headers = admission.http_headers().ok_or_else(|| {
                    McpRegistryError::InvalidCandidate(
                        "admitted MCP HTTP server has no HTTP material".to_owned(),
                    )
                })?;
                self.count("header-count", headers.len());
                for header in headers {
                    self.field("header-name", header.name().as_bytes());
                    self.field("header-value", header.value().expose_secret_bytes());
                }
            }
            McpServerConfig::Acp { server_id } => {
                self.field("transport", b"acp");
                self.field("server-id", server_id.as_bytes());
            }
        }
        Ok(())
    }

    fn json(&mut self, name: &str, value: &serde_json::Value) {
        self.bytes(name.as_bytes());
        self.json_value(value);
    }

    fn json_value(&mut self, value: &serde_json::Value) {
        match value {
            serde_json::Value::Null => self.field("json-type", b"null"),
            serde_json::Value::Bool(value) => {
                self.field("json-type", b"boolean");
                self.field("json-boolean", &[u8::from(*value)]);
            }
            serde_json::Value::Number(value) => {
                self.field("json-type", b"number");
                self.field("json-number", value.to_string().as_bytes());
            }
            serde_json::Value::String(value) => {
                self.field("json-type", b"string");
                self.field("json-string", value.as_bytes());
            }
            serde_json::Value::Array(values) => {
                self.field("json-type", b"array");
                self.count("json-length", values.len());
                for value in values {
                    self.json_value(value);
                }
            }
            serde_json::Value::Object(values) => {
                self.field("json-type", b"object");
                self.count("json-length", values.len());
                let mut fields = values.iter().collect::<Vec<_>>();
                fields.sort_by_key(|(name, _)| (*name).clone());
                for (name, value) in fields {
                    self.field("json-field", name.as_bytes());
                    self.json_value(value);
                }
            }
        }
    }

    fn finish(self) -> String {
        format!("sha256:{:x}", self.0.finalize())
    }
}

fn validate_candidate_contents(
    capabilities: &McpCapabilities,
    tools: &[GeneratorToolDescriptor],
    resources: &[Resource],
    prompts: &[Prompt],
) -> Result<(), McpRegistryError> {
    if (!tools.is_empty() && !capabilities.has_tools())
        || (!resources.is_empty() && !capabilities.has_resources())
        || (!prompts.is_empty() && !capabilities.has_prompts())
    {
        return Err(McpRegistryError::InvalidCandidate(
            "MCP candidate content exceeds its advertised capabilities".to_owned(),
        ));
    }
    let mut tool_names = std::collections::BTreeSet::new();
    for tool in tools {
        if tool.name.trim().is_empty()
            || !tool.parameters.is_object()
            || !tool_names.insert(tool.name.as_str())
        {
            return Err(McpRegistryError::InvalidCandidate(
                "MCP candidate contains an invalid or duplicate tool".to_owned(),
            ));
        }
    }
    let mut resource_uris = std::collections::BTreeSet::new();
    for resource in resources {
        if resource.uri.trim().is_empty()
            || resource.name.trim().is_empty()
            || !resource_uris.insert(resource.uri.as_str())
        {
            return Err(McpRegistryError::InvalidCandidate(
                "MCP candidate contains an invalid or duplicate resource".to_owned(),
            ));
        }
    }
    let mut prompt_names = std::collections::BTreeSet::new();
    for prompt in prompts {
        if prompt.name.trim().is_empty()
            || !prompt_names.insert(prompt.name.as_str())
            || prompt.arguments.as_ref().is_some_and(|arguments| {
                let mut names = std::collections::BTreeSet::new();
                arguments.iter().any(|argument| {
                    argument.name.trim().is_empty() || !names.insert(&argument.name)
                })
            })
        {
            return Err(McpRegistryError::InvalidCandidate(
                "MCP candidate contains an invalid or duplicate prompt".to_owned(),
            ));
        }
    }
    Ok(())
}

fn capture_contexts(
    capture: &str,
    servers: &BTreeMap<McpServerName, Arc<AdmittedServerSlice>>,
) -> Result<McpCapturedContextSource, McpRegistryError> {
    let mut operations = Vec::new();
    for (server_name, server) in servers {
        for resource in server.resources.iter() {
            let descriptor = descriptor_fingerprint(resource)?;
            let revision = server
                .cache
                .source_revision()
                .unwrap_or(&descriptor)
                .to_owned();
            let entry = ContextEntry::resource(
                server_name.to_string(),
                resource.uri.clone(),
                resource.name.clone(),
                ContextSensitivity::Restricted,
                CacheScope::Private,
                revision,
            )
            .and_then(|entry| entry.with_admission_capture(capture))
            .map_err(|error| McpRegistryError::InvalidCandidate(error.to_string()))?;
            let subject = McpSubject::Resource {
                server: server_name.to_string(),
                uri: resource.uri.clone(),
            };
            let admission = McpAdmissionFingerprint::new(
                capture,
                format!("{server_name}/{}", resource.uri),
                descriptor,
            )
            .map_err(|error| McpRegistryError::InvalidCandidate(error.to_string()))?;
            operations.push(McpCapturedContextOperation {
                entry,
                subject,
                operation: McpOperationKind::ReadResource,
                admission,
                server_admission: Arc::clone(&server.admission),
                factory: Arc::clone(&server.factory),
            });
        }
        for prompt in server.prompts.iter() {
            let descriptor = descriptor_fingerprint(prompt)?;
            let revision = server
                .cache
                .source_revision()
                .unwrap_or(&descriptor)
                .to_owned();
            let arguments = prompt
                .arguments
                .as_deref()
                .unwrap_or_default()
                .iter()
                .map(|argument| argument.name.clone())
                .collect();
            let entry = ContextEntry::prompt(
                server_name.to_string(),
                prompt.name.clone(),
                prompt.description.clone().unwrap_or_default(),
                arguments,
                ContextSensitivity::Restricted,
                CacheScope::Private,
                revision,
            )
            .and_then(|entry| entry.with_admission_capture(capture))
            .map_err(|error| McpRegistryError::InvalidCandidate(error.to_string()))?;
            let subject = McpSubject::Prompt {
                server: server_name.to_string(),
                name: prompt.name.clone(),
            };
            let admission = McpAdmissionFingerprint::new(
                capture,
                format!("{server_name}/{}", prompt.name),
                descriptor,
            )
            .map_err(|error| McpRegistryError::InvalidCandidate(error.to_string()))?;
            operations.push(McpCapturedContextOperation {
                entry,
                subject,
                operation: McpOperationKind::GetPrompt,
                admission,
                server_admission: Arc::clone(&server.admission),
                factory: Arc::clone(&server.factory),
            });
        }
    }
    McpCapturedContextSource::new(operations)
}

fn descriptor_fingerprint(value: &impl serde::Serialize) -> Result<String, McpRegistryError> {
    let bytes = serde_json::to_vec(value)
        .map_err(|error| McpRegistryError::InvalidCandidate(error.to_string()))?;
    Ok(format!("sha256:{:x}", Sha256::digest(bytes)))
}

#[cfg(test)]
mod collapsed_mcp_red {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use futures::future::BoxFuture;
    use rmcp::model::Prompt;
    use serde_json::{Value, json};

    use ego_core::config::{McpConfigProvenance, McpServerConfig, Source};
    use ego_core::generator::types::ToolDescriptor;
    use ego_core::mcp::ToolExecution;
    use ego_core::permit::{CanonPath, GuardedIo, Op, PreparedCall, ToolError};
    use ego_core::turn::{McpRegistryEvent, McpRegistryJournal, McpRegistryJournalError};

    use super::*;

    const SERVER: &str = "collapsed";
    fn descriptor(name: &str, schema: Value) -> ToolDescriptor {
        ToolDescriptor {
            name: name.to_owned(),
            description: format!("{name} fixture"),
            parameters: schema,
        }
    }

    fn management() -> Vec<ToolDescriptor> {
        vec![
            descriptor(
                "search_tools",
                json!({
                    "type":"object",
                    "properties":{"query":{"type":"string"},"limit":{"type":"integer","minimum":1}},
                    "required":["query"],
                    "additionalProperties":false
                }),
            ),
            descriptor(
                "get_tool_schema",
                json!({
                    "type":"object",
                    "properties":{"tool_name":{"type":"string"}},
                    "required":["tool_name"],
                    "additionalProperties":false
                }),
            ),
            descriptor(
                "call_tool",
                json!({
                    "type":"object",
                    "properties":{"tool_name":{"type":"string"},"arguments":{"type":"object"}},
                    "required":["tool_name","arguments"],
                    "additionalProperties":false
                }),
            ),
        ]
    }

    fn admission(name: &str) -> Arc<McpServerAdmission> {
        Arc::new(
            McpServerAdmission::new(
                name.parse().expect("server name"),
                McpServerConfig::stdio(format!("mcp-{name}"), Vec::new(), None, Vec::new())
                    .expect("stdio config"),
                crate::McpTransportMaterial::stdio(Vec::new()),
            )
            .expect("admission"),
        )
    }

    #[derive(Default)]
    struct Factory;

    impl McpOperationFactory for Factory {
        fn prepare(
            &self,
            _admission: Arc<McpServerAdmission>,
            _operation: crate::McpOperation,
        ) -> Result<PreparedCall, ToolError> {
            Ok(PreparedCall::new(
                "collapsed-test",
                Op::Read {
                    path: CanonPath::new("/fixture"),
                },
            ))
        }

        fn execute<'a, 'io>(
            &'a self,
            _admission: Arc<McpServerAdmission>,
            _call: PreparedCall,
            _io: &'a dyn GuardedIo<'io>,
            _generation: Arc<str>,
        ) -> BoxFuture<'a, Result<ToolExecution, ToolError>>
        where
            'io: 'a,
        {
            Box::pin(async {
                Ok(ToolExecution::Complete(
                    ego_core::permit::ToolResult::builtin("collapsed-test"),
                ))
            })
        }
    }

    enum Outcome {
        Candidate(McpAdmittedCandidate),
    }

    #[derive(Default)]
    struct Admission {
        outcomes: Mutex<VecDeque<Outcome>>,
    }

    impl Admission {
        fn push(&self, outcome: Outcome) {
            self.outcomes.lock().expect("outcomes").push_back(outcome);
        }
    }

    impl McpCandidateAdmission for Admission {
        fn admit(
            &self,
            _request: McpCandidateRequest,
        ) -> McpFuture<'static, Result<McpAdmittedCandidate, McpCandidateError>> {
            let outcome = self.outcomes.lock().expect("outcomes").pop_front();
            Box::pin(async move {
                match outcome.expect("candidate outcome") {
                    Outcome::Candidate(candidate) => Ok(candidate),
                }
            })
        }
    }

    #[derive(Default)]
    struct Journal {
        events: Mutex<Vec<McpRegistryEvent>>,
        revision: Mutex<u64>,
    }

    impl McpRegistryJournal for Journal {
        fn append(&self, event: McpRegistryEvent) -> Result<(), McpRegistryJournalError> {
            self.events.lock().expect("events").push(event);
            Ok(())
        }

        fn replay(&self) -> Result<Vec<McpRegistryEvent>, McpRegistryJournalError> {
            Ok(self.events.lock().expect("events").clone())
        }

        fn begin_refresh(
            &self,
            request_id: String,
            trigger: String,
            server_scope: Option<String>,
        ) -> Result<u64, McpRegistryJournalError> {
            let mut revision = self.revision.lock().expect("revision");
            *revision += 1;
            self.events
                .lock()
                .expect("events")
                .push(McpRegistryEvent::RefreshRequested {
                    request_id,
                    trigger,
                    server_scope,
                    config_revision: *revision,
                });
            Ok(*revision)
        }
    }

    fn candidate(
        server_admission: Arc<McpServerAdmission>,
        catalogue: Option<McpCollapsedCatalogue>,
    ) -> McpAdmittedCandidate {
        McpAdmittedCandidate::new(
            Arc::clone(&server_admission),
            McpCandidateIdentity::validated(server_admission.name().as_str()),
            McpCandidateInventory::new(
                McpCapabilities::tools(),
                None,
                management(),
                Vec::new(),
                Vec::<Prompt>::new(),
                McpCacheMetadata::default(),
                catalogue,
            ),
            Arc::new(Factory),
        )
        .expect("candidate")
    }

    fn refresh(admission: &Arc<McpServerAdmission>) -> McpRegistryRefresh {
        McpRegistryRefresh::new(
            vec![McpRegistryCandidate::ready(
                Arc::clone(admission),
                McpConfigProvenance::Layer {
                    source: Source::User,
                },
            )],
            RegistryTrigger::Reload,
            RegistryScope::All,
        )
        .expect("refresh")
    }

    fn remove_refresh(admission: &Arc<McpServerAdmission>) -> McpRegistryRefresh {
        McpRegistryRefresh::new(
            Vec::new(),
            RegistryTrigger::Remove,
            RegistryScope::Server(admission.name().clone()),
        )
        .expect("remove refresh")
    }

    async fn bootstrap_named(
        name: &str,
    ) -> (
        Arc<McpRegistryController>,
        Arc<McpServerAdmission>,
        McpRegistryPublication,
    ) {
        let server_admission = admission(name);
        let candidate_admission = Arc::new(Admission::default());
        candidate_admission.push(Outcome::Candidate(candidate(
            Arc::clone(&server_admission),
            None,
        )));
        let journal = Arc::new(Journal::default());
        let controller = McpRegistryController::new(
            Arc::clone(&journal) as Arc<dyn McpRegistryJournal>,
            Arc::clone(&candidate_admission) as Arc<dyn McpCandidateAdmission>,
        )
        .expect("controller");
        let publication = controller
            .reconcile(refresh(&server_admission))
            .await
            .expect("bootstrap");
        (controller, server_admission, publication)
    }

    async fn bootstrap() -> (
        Arc<McpRegistryController>,
        Arc<McpServerAdmission>,
        McpRegistryPublication,
    ) {
        bootstrap_named(SERVER).await
    }

    fn fingerprint_for(server: &str, capture: &McpRegistryCapture) -> McpAdmissionFingerprint {
        let lookup_schema = management()
            .into_iter()
            .find(|tool| tool.name == "get_tool_schema")
            .expect("get_tool_schema descriptor");
        let lookup_schema = crate::dynamic::schema_fingerprint(&lookup_schema.parameters)
            .expect("lookup schema fingerprint");
        McpAdmissionFingerprint::new(
            capture.stable_fingerprint(),
            format!("{server}/get_tool_schema"),
            lookup_schema,
        )
        .expect("lookup fingerprint")
    }

    fn fingerprint(capture: &McpRegistryCapture) -> McpAdmissionFingerprint {
        fingerprint_for(SERVER, capture)
    }

    fn schema_definition(name: &str, schema: Value) -> Value {
        json!({
            "name": name,
            "description": format!("{name} definition"),
            "inputSchema": schema
        })
    }

    fn command(
        capture: &McpRegistryCapture,
        generation: &str,
        requested: &str,
        structured_content: Value,
    ) -> McpToolSchemaCommand {
        McpToolSchemaCommand::new(
            capture.servers()[0].clone(),
            generation.to_owned(),
            fingerprint(capture),
            requested.to_owned(),
            structured_content,
        )
    }

    fn weather_schema() -> Value {
        json!({
            "type":"object",
            "properties":{"city":{"type":"string"}},
            "required":["city"],
            "additionalProperties":false
        })
    }

    #[tokio::test(flavor = "current_thread")]
    async fn collapsed_bootstrap_publishes_management_without_call_tool() {
        let bootstrap_contract =
            McpCollapsedBootstrap::new(management()).expect("collapsed bootstrap");
        assert_eq!(
            bootstrap_contract.tool_names().collect::<Vec<_>>(),
            vec!["get_tool_schema", "search_tools"]
        );
        let (controller, _admission, publication) = bootstrap().await;
        assert_eq!(publication.generation(), "generation-1");
        assert_eq!(
            publication.admitted_tools(),
            &["get_tool_schema".to_owned(), "search_tools".to_owned()]
        );
        assert!(
            !controller
                .capture()
                .expect("capture")
                .tools()
                .iter()
                .any(|tool| tool.descriptor().name == "call_tool")
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn exact_schema_lookup_publishes_n_plus_one_and_preserves_old_capture() {
        let (controller, _admission, first) = bootstrap().await;
        let old_capture = first.capture().clone();
        let command = command(
            first.capture(),
            "generation-1",
            "weather",
            schema_definition("weather", weather_schema()),
        );
        let second = controller
            .reconcile_tool_schema(command)
            .await
            .expect("schema reconciliation");
        assert_eq!(second.generation(), "generation-2");
        assert_eq!(second.predecessor(), Some("generation-1"));
        assert!(
            second
                .admitted_tools()
                .iter()
                .any(|name| name == "call_tool")
        );
        assert!(
            !old_capture
                .tools()
                .iter()
                .any(|tool| tool.descriptor().name == "call_tool")
        );
        assert!(
            second
                .capture()
                .servers()
                .iter()
                .all(|server| server.admission().name().as_str() == SERVER)
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn repeated_exact_schema_replaces_and_merges_only_requested_tools_deterministically() {
        let (controller, _admission, first) = bootstrap().await;
        let first = controller
            .reconcile_tool_schema(command(
                first.capture(),
                "generation-1",
                "weather",
                schema_definition("weather", weather_schema()),
            ))
            .await
            .expect("first schema lookup");
        let second = controller
            .reconcile_tool_schema(command(
                first.capture(),
                "generation-2",
                "calendar",
                schema_definition(
                    "calendar",
                    json!({"type":"object","additionalProperties":false}),
                ),
            ))
            .await
            .expect("second schema lookup");
        let third = controller
            .reconcile_tool_schema(command(
                second.capture(),
                "generation-3",
                "weather",
                schema_definition(
                    "weather",
                    json!({
                        "type":"object",
                        "properties":{"city":{"type":"string"},"units":{"type":"string"}},
                        "required":["city","units"],
                        "additionalProperties":false
                    }),
                ),
            ))
            .await
            .expect("replacement schema lookup");
        assert_eq!(first.generation(), "generation-2");
        assert_eq!(second.generation(), "generation-3");
        assert_eq!(third.generation(), "generation-4");
        let snapshot = controller.current_snapshot().expect("snapshot");
        let server = snapshot
            .servers
            .get(&SERVER.parse().expect("server name"))
            .expect("server");
        let execute = server
            .tools
            .iter()
            .find(|tool| tool.name() == "call_tool")
            .expect("call_tool wrapper");
        let catalogue = execute.collapsed_catalogue().expect("catalogue");
        assert_eq!(
            catalogue.tool_names().collect::<Vec<_>>(),
            vec!["call_tool", "calendar", "weather"]
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn stale_or_invalid_schema_is_rejected_without_publication() {
        let cases = [
            ("empty definition", "weather", json!({}), "generation-1"),
            (
                "stale generation",
                "weather",
                schema_definition("weather", weather_schema()),
                "generation-0",
            ),
            (
                "missing requested",
                "calendar",
                schema_definition("weather", weather_schema()),
                "generation-1",
            ),
            (
                "duplicate descriptor",
                "weather",
                json!({"name":"weather","description":"weather definition","inputSchema":weather_schema(),"extra":true}),
                "generation-1",
            ),
            (
                "management descriptor",
                "search_tools",
                schema_definition("search_tools", json!({"type":"object"})),
                "generation-1",
            ),
            (
                "lookup management descriptor",
                "get_tool_schema",
                schema_definition("get_tool_schema", json!({"type":"object"})),
                "generation-1",
            ),
            (
                "dispatch management descriptor",
                "call_tool",
                schema_definition("call_tool", json!({"type":"object"})),
                "generation-1",
            ),
            (
                "unexpected descriptor",
                "weather",
                schema_definition("calendar", json!({"type":"object"})),
                "generation-1",
            ),
            (
                "invalid schema",
                "weather",
                schema_definition("weather", json!({"type":7})),
                "generation-1",
            ),
            (
                "protocol error",
                "weather",
                json!({"isError":true,"content":[{"type":"text","text":"failed"}]}),
                "generation-1",
            ),
        ];
        for (label, requested, content, generation) in cases {
            let (controller, _admission, first) = bootstrap().await;
            let before = controller.publication();
            let command = command(first.capture(), generation, requested, content);
            assert!(
                controller.reconcile_tool_schema(command).await.is_err(),
                "{label} was accepted"
            );
            assert_eq!(
                controller.publication().generation(),
                before.generation(),
                "{label}"
            );
            assert_eq!(
                controller.publication().admitted_tools(),
                before.admitted_tools(),
                "{label}"
            );
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn stale_server_and_lookup_fingerprint_are_rejected_without_publication() {
        let (controller, _admission, first) = bootstrap().await;
        let (_other_controller, _other_admission, other) = bootstrap_named("other").await;
        let stale = McpToolSchemaCommand::new(
            other.capture().servers()[0].clone(),
            "generation-1".to_owned(),
            fingerprint_for("other", other.capture()),
            "weather".to_owned(),
            schema_definition("weather", weather_schema()),
        );
        assert!(controller.reconcile_tool_schema(stale).await.is_err());
        let before = controller.publication();
        let stale = McpToolSchemaCommand::new(
            first.capture().servers()[0].clone(),
            "generation-1".to_owned(),
            McpAdmissionFingerprint::new(
                "generation-1",
                format!("{SERVER}/get_tool_schema"),
                "sha256:stale",
            )
            .expect("stale fingerprint"),
            "weather".to_owned(),
            schema_definition("weather", weather_schema()),
        );
        assert!(controller.reconcile_tool_schema(stale).await.is_err());
        assert_eq!(controller.publication().generation(), before.generation());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn removal_withdraws_collapsed_tools_while_invalid_refresh_keeps_last_good() {
        let (controller, admission, first) = bootstrap().await;
        let before = controller.publication();
        let invalid = command(
            first.capture(),
            "generation-1",
            "weather",
            json!({"name":"weather","description":"weather definition","inputSchema":{"type":7}}),
        );
        assert!(controller.reconcile_tool_schema(invalid).await.is_err());
        assert_eq!(controller.publication().generation(), before.generation());
        let removed = controller
            .reconcile(remove_refresh(&admission))
            .await
            .expect("remove refresh");
        assert!(removed.admitted_tools().is_empty());
        assert!(removed.capture().tools().is_empty());
    }
}

#[cfg(test)]
mod candidate_detail_critic {
    use super::*;

    fn message(cause: &str) -> String {
        CandidateFailure::admission(McpCandidateError::Invalid {
            cause: cause.to_owned(),
        })
        .diagnostic_message()
    }

    fn detail(cause: &str) -> String {
        let fixed = failure_message(McpFailure::InvalidValue);
        message(cause)
            .strip_prefix(fixed)
            .and_then(|rest| rest.strip_prefix(": "))
            .unwrap_or_default()
            .to_owned()
    }

    #[test]
    fn multibyte_cause_is_cut_on_a_character_boundary_at_the_bound() {
        // catches: a byte-based cut that splits a 4-byte scalar or keeps 256 bytes.
        let cause = "😀".repeat(300);
        let out = detail(&cause);
        assert_eq!(out.chars().count(), MAX_DIAGNOSTIC_DETAIL_CHARS);
        assert!(out.chars().all(|c| c == '😀'));
    }

    #[test]
    fn cause_at_the_bound_is_kept_whole_and_one_past_it_loses_one_character() {
        // catches: off-by-one in the bound.
        let exact = "x".repeat(MAX_DIAGNOSTIC_DETAIL_CHARS);
        assert_eq!(detail(&exact), exact);
        let over = "x".repeat(MAX_DIAGNOSTIC_DETAIL_CHARS + 1);
        assert_eq!(detail(&over), exact);
    }

    #[test]
    fn ascii_control_characters_never_reach_the_diagnostic() {
        // catches: newline / ESC / NUL from a foreign server reshaping the host's line.
        let out = message("bad\nforged: line\r\u{1b}[31mred\u{0}\u{7f}\u{85}end");
        assert!(out.chars().all(|c| !c.is_control()), "{out:?}");
    }

    #[test]
    fn unicode_line_breaks_and_bidi_overrides_never_reach_the_diagnostic() {
        // catches: `char::is_control` (Cc only) letting U+2028/U+2029 split the
        // line and U+202E reorder the text the host displays.
        let out = message("a\u{2028}b\u{2029}c\u{202e}d\u{2066}e\u{200b}f");
        for forbidden in ['\u{2028}', '\u{2029}', '\u{202e}', '\u{2066}'] {
            assert!(!out.contains(forbidden), "{forbidden:?} survived: {out:?}");
        }
    }

    #[test]
    fn empty_or_blank_cause_adds_no_dangling_separator() {
        // catches: "<fixed cause>: " with nothing after the colon.
        let fixed = failure_message(McpFailure::InvalidValue);
        for cause in ["", "   ", "\n\n\n", "\t\r\n"] {
            assert_eq!(message(cause), fixed, "cause {cause:?}");
        }
    }

    #[test]
    fn leading_control_flood_does_not_consume_the_bound() {
        // catches: 300 leading newlines turning into 256 blanks and hiding the real cause.
        let cause = format!("{}resources/list missing envelope", "\n".repeat(300));
        assert!(
            message(&cause).contains("resources/list"),
            "{:?}",
            message(&cause)
        );
    }

    #[test]
    fn secret_values_are_masked_in_every_form() {
        // catches: a backend error echoing credentials into the published diagnostic.
        for (cause, secret) in [
            ("backend-secret=do-not-publish", "do-not-publish"),
            ("bad api_key=abc123 retry", "abc123"),
            ("TOKEN=Zz9 failed", "Zz9"),
            ("GET /x?client_secret=s3&mode=1", "s3"),
            (
                "auth failed: Authorization: Bearer eyJhbGci.payload.sig",
                "eyJhbGci",
            ),
            ("authorization: BEARER abc", "abc"),
            ("key=\"two words\" end", "two words"),
        ] {
            let out = detail(cause);
            assert!(!out.contains(secret), "{cause:?} leaked: {out:?}");
            assert!(out.contains(SECRET_MASK), "{cause:?} not masked: {out:?}");
        }
        assert_eq!(
            detail("backend-secret=do-not-publish"),
            "backend-secret=***"
        );
        assert_eq!(detail("x?a=1&api_key=k&b=2"), "x?a=1&api_key=***");
    }

    #[test]
    fn non_secret_detail_is_kept_verbatim() {
        // catches: over-broad masking hiding the real cause (bearer/key words, other pairs).
        for cause in [
            "duplicate tool identity",
            "mode=fast retries=3",
            "bearer token required",
            "empty key=",
        ] {
            assert_eq!(detail(cause), cause);
        }
    }

    #[test]
    fn secret_straddling_the_cut_never_leaks_a_prefix() {
        // catches: truncating before masking, leaving the head of a secret visible.
        let cause = format!("{} api_key=SUPERSECRETVALUE", "a".repeat(240));
        let out = detail(&cause);
        assert!(!out.contains("SUPER"), "{out:?}");
        assert!(out.ends_with("api_key=***"), "{out:?}");
    }

    #[test]
    fn json_escaped_inside_a_string_is_masked_with_its_structure_kept() {
        // catches: `\"token\":\"x\"` inside a JSON string leaking, or the mask eating the
        // text after the escaped value.
        assert_eq!(
            detail(r#"body {\"token\":\"LEAK\"} rejected"#),
            r#"body {\"token\":***} rejected"#
        );
    }

    #[test]
    fn digest_quoted_params_with_blanks_are_masked_to_the_last_one() {
        // catches: the parameter loop stopping inside `realm="my realm"`, leaking `response=`.
        assert_eq!(
            detail(r#"Authorization: Digest username="u", realm="my realm", response="LEAK" end"#),
            "Authorization: *** end"
        );
    }

    #[test]
    fn every_default_ignorable_or_format_mark_is_dropped_before_matching() {
        // catches: a name split by a mark outside a hand-picked list (grapheme joiner,
        // variation selector, tag character, Hangul filler, Arabic number sign).
        for mark in [
            '\u{034f}',
            '\u{fe0f}',
            '\u{e0041}',
            '\u{180e}',
            '\u{3164}',
            '\u{0600}',
            '\u{115f}',
        ] {
            assert_eq!(detail(&format!("to{mark}ken=LEAK done")), "token=*** done");
        }
    }

    #[test]
    fn bracketed_and_tuple_names_are_masked() {
        // catches: `params[token]=x` and Debug tuples `("x-api-key", "x")` leaking.
        assert_eq!(detail("params[token]=LEAK&a=1"), "params[token]=***");
        assert_eq!(
            detail(r#"header ("x-api-key", "LEAK") rejected"#),
            r#"header ("x-api-key", ***) rejected"#
        );
    }

    #[test]
    fn quoted_credential_with_a_blank_after_a_scheme_is_masked_whole() {
        // catches: the credential ending at its first blank and leaking the second word.
        assert_eq!(
            detail(r#"Authorization: Bearer "abc LEAK" rejected"#),
            "Authorization: *** rejected"
        );
    }

    #[test]
    fn unterminated_private_key_header_masks_to_the_end() {
        // catches: a header without closing dashes publishing the key body.
        assert_eq!(
            detail("bad -----BEGIN RSA PRIVATE KEY MIIEvLEAK"),
            "bad ***"
        );
        assert_eq!(
            detail("see -----BEGIN CERTIFICATE----- ok"),
            "see -----BEGIN CERTIFICATE----- ok"
        );
    }

    #[test]
    fn empty_value_masks_nothing_after_the_blank_but_a_spaced_value_is_masked() {
        // catches: `key= mode=fast` masking the unrelated next pair; and, in the other
        // direction, the pair check hiding `token: YWJjZA==` (base64 padding is no pair).
        assert_eq!(detail("empty key= mode=fast"), "empty key= mode=fast");
        assert_eq!(detail("token: YWJjZA== end"), "token: *** end");
        assert_eq!(detail("api_key = LEAK end"), "api_key = *** end");
    }

    #[test]
    fn joiners_after_non_ascii_text_are_kept_but_never_split_a_secret_name() {
        // catches: dropping ZWJ/ZWNJ/VS16 from emoji and Persian text, and, the other way,
        // keeping a joiner that splits `token` after an ASCII letter.
        assert_eq!(detail("failed 👨\u{200d}👩 ok"), "failed 👨\u{200d}👩 ok");
        assert_eq!(detail("می\u{200c}خواهد"), "می\u{200c}خواهد");
        assert_eq!(detail("❤\u{fe0f} ok"), "❤\u{fe0f} ok");
        assert_eq!(detail("to\u{200d}ken=LEAK"), "token=***");
        assert_eq!(detail("é\u{200d}token=LEAK"), "é\u{200d}token=***");
    }

    #[test]
    fn json_text_and_lists_naming_secret_words_are_kept_verbatim() {
        // catches: the tuple rule masking the next member of an error body or list.
        for cause in [
            r#"{"error":"invalid token","code":401}"#,
            r#"missing properties ["key", "name"]"#,
        ] {
            assert_eq!(detail(cause), cause);
        }
    }

    #[test]
    fn name_value_records_mask_the_value_member() {
        // catches: `{"name":"x-api-key","value":"v"}` and `Header { name, value }` leaking.
        assert_eq!(
            detail(r#"{"name":"x-api-key","value":"LEAK","k":1}"#),
            r#"{"name":"x-api-key","value":***,"k":1}"#
        );
        assert_eq!(
            detail(r#"Header { name: "x-api-key", value: "LEAK" }"#),
            r#"Header { name: "x-api-key", value: *** }"#
        );
        assert_eq!(
            detail(r#"{"name":"content-type","value":"json"}"#),
            r#"{"name":"content-type","value":"json"}"#
        );
    }

    #[test]
    fn escaped_json_of_any_depth_masks_the_value_and_keeps_the_tail() {
        // catches: one-level-only `\"` handling and an embedded `\\\"` ending the value.
        assert_eq!(
            detail(r#"o {\\\"token\\\":\\\"LEAK\\\"} end"#),
            r#"o {\\\"token\\\":***} end"#
        );
        assert_eq!(
            detail(r#"o {\"password\":\"pa\\\"LEAK\"} end"#),
            r#"o {\"password\":***} end"#
        );
    }

    #[test]
    fn nested_object_or_array_under_a_secret_name_is_masked_whole() {
        // catches: only the opening brace being masked.
        assert_eq!(
            detail(r#"{ "password": { "value": "LEAK", "b": ["x"] } } ok"#),
            r#"{ "password": *** } ok"#
        );
        assert_eq!(detail(r#"token=["a","}"] ok"#), "token=*** ok");
    }
}

#[cfg(test)]
mod secret_mask_critic {
    use super::*;

    fn detail(cause: &str) -> String {
        bounded_detail(cause).unwrap_or_default()
    }

    fn assert_hidden(cause: &str, secret: &str) {
        let out = detail(cause);
        assert!(
            !out.contains(secret),
            "{cause:?} leaked {secret:?}: {out:?}"
        );
    }

    #[test]
    fn json_quoted_token_value_is_hidden() {
        // catches: masking only `name=value`, so a JSON body `{"token":"x"}` echoed by the backend leaks.
        assert_hidden(r#"bad body {"token":"abc123XYZ"}"#, "abc123XYZ");
        assert_hidden(r#"bad body {"api_key": "abc123XYZ"}"#, "abc123XYZ");
    }

    #[test]
    fn colon_header_style_secret_is_hidden() {
        // catches: only `=` is recognised; `x-api-key: v` / `token: v` leak.
        assert_hidden("sent x-api-key: abc123XYZ failed", "abc123XYZ");
        assert_hidden("token: abc123XYZ rejected", "abc123XYZ");
    }

    #[test]
    fn password_values_are_hidden() {
        // catches: name list lacks password/passwd/pwd, so `password=hunter2` leaks.
        assert_hidden("login password=hunter2 failed", "hunter2");
        assert_hidden("login passwd=hunter2 failed", "hunter2");
    }

    #[test]
    fn url_userinfo_password_is_hidden() {
        // catches: credentials embedded in a URL (`https://user:pw@host`) are published.
        assert_hidden(
            "cannot reach https://svc:hunter2@host.example/mcp",
            "hunter2",
        );
    }

    #[test]
    fn blanks_around_equals_do_not_defeat_the_mask() {
        // catches: `api_key = v` leaves the name empty before `=` and the value unmasked.
        assert_hidden("bad api_key = abc123XYZ end", "abc123XYZ");
    }

    #[test]
    fn non_bearer_authorization_schemes_are_hidden() {
        // catches: only the Bearer scheme is masked; Basic/Token credentials leak.
        assert_hidden("Authorization: Basic dXNlcjpwYXNz", "dXNlcjpwYXNz");
        assert_hidden("Authorization: Token abc123XYZ", "abc123XYZ");
    }

    #[test]
    fn authorization_without_blank_before_scheme_is_hidden() {
        // catches: `Authorization:Bearer v` is one word, so the two-word lookup misses it.
        assert_hidden("Authorization:Bearer abc123XYZ", "abc123XYZ");
    }

    #[test]
    fn json_quoted_authorization_bearer_is_hidden() {
        // catches: `"Authorization": "Bearer v"` keeps a quote on the scheme word and escapes the mask.
        assert_hidden(
            r#"headers {"Authorization": "Bearer abc123XYZ"}"#,
            "abc123XYZ",
        );
    }

    #[test]
    fn escaped_quote_inside_quoted_value_does_not_end_the_mask() {
        // catches: value ends at the first quote even when escaped, leaking the tail.
        assert_hidden(r#"token="ab\"TAILSECRET" end"#, "TAILSECRET");
    }

    #[test]
    fn scheme_word_after_equals_does_not_shield_the_credential() {
        // catches: `token=Bearer v` masks only the word `Bearer` and leaks `v`.
        assert_hidden("token=Bearer abc123XYZ", "abc123XYZ");
    }

    #[test]
    fn zero_width_char_inside_the_name_does_not_defeat_the_mask() {
        // catches: U+200B (format char, not blanked) splits `token` so the suffix test fails.
        assert_hidden("to\u{200b}ken=abc123XYZ", "abc123XYZ");
    }

    #[test]
    fn pem_private_key_body_is_hidden() {
        // catches: no rule for -----BEGIN ... PRIVATE KEY----- blocks.
        assert_hidden(
            "bad -----BEGIN PRIVATE KEY----- MIIEvQIBADANBgkq -----END PRIVATE KEY-----",
            "MIIEvQIBADANBgkq",
        );
    }

    #[test]
    fn semicolon_inside_secret_value_does_not_leak_the_tail() {
        // catches: `;` ends the value, so `token=ab;cdTAIL` publishes the tail.
        assert_hidden("token=ab;cdTAIL", "cdTAIL");
    }

    #[test]
    fn bearer_secret_straddling_the_cut_never_leaks_a_prefix() {
        // catches: truncation before masking for the Bearer path.
        let cause = format!(
            "{} Authorization: Bearer SUPERSECRETVALUE",
            "a ".repeat(120)
        );
        let out = detail(&cause);
        assert!(!out.contains("SUPER"), "{out:?}");
    }

    #[test]
    fn base64_padding_in_value_is_fully_masked() {
        // catches: inner `=` padding restarts parsing and leaves part of the value visible.
        let out = detail("token=YWJjZA== done");
        assert_eq!(out, "token=*** done");
    }

    #[test]
    fn repeated_secrets_are_all_masked() {
        // catches: only the first occurrence is masked.
        let out = detail("a api_key=ONE b token=TWO c Authorization: Bearer THREE");
        for s in ["ONE", "TWO", "THREE"] {
            assert!(!out.contains(s), "{out:?}");
        }
    }
}

#[cfg(test)]
mod secret_mask_critic_round_two {
    use super::*;

    fn detail(cause: &str) -> String {
        bounded_detail(cause).unwrap_or_default()
    }

    fn assert_hidden(cause: &str, secret: &str) {
        let out = detail(cause);
        assert!(
            !out.contains(secret),
            "{cause:?} leaked {secret:?}: {out:?}"
        );
    }

    #[test]
    fn delimiter_characters_inside_an_unquoted_secret_do_not_leak_the_tail() {
        // catches: word_end stops at , & ) } ] so `token=ab,cdTAIL` publishes `cdTAIL`.
        for cause in [
            "token=ab,cdTAIL",
            "token=ab&cdTAIL",
            "token=ab)cdTAIL",
            "token=ab}cdTAIL",
            "token=ab]cdTAIL",
            "Authorization: Bearer ab,cdTAIL",
            "password=p@ss)cdTAIL",
        ] {
            assert_hidden(cause, "cdTAIL");
        }
    }

    #[test]
    fn pgp_private_key_block_is_hidden() {
        // catches: the PEM rule requires the label to end in `PRIVATE KEY`, so `PRIVATE KEY BLOCK` leaks.
        assert_hidden(
            "bad -----BEGIN PGP PRIVATE KEY BLOCK----- lQOYBGBODYSECRET -----END PGP PRIVATE KEY BLOCK-----",
            "lQOYBGBODYSECRET",
        );
    }

    #[test]
    fn url_userinfo_without_password_is_hidden() {
        // catches: `https://<token>@host` (GitHub-style token-as-user) is kept because no colon is present.
        assert_hidden(
            "clone https://ghp_ABCDEF123456@github.com/x failed",
            "ghp_ABCDEF123456",
        );
    }

    #[test]
    fn bidi_mark_or_nul_inside_the_name_does_not_defeat_the_mask() {
        // catches: bidi/control chars become blanks, splitting `token` into `to ken` so the suffix test fails.
        assert_hidden("to\u{200e}ken=abc123XYZ", "abc123XYZ");
        assert_hidden("tok\u{0}en=abc123XYZ", "abc123XYZ");
        assert_hidden("to\u{2063}ken=abc123XYZ", "abc123XYZ");
    }

    #[test]
    fn sigv4_authorization_header_is_hidden() {
        // catches: a scheme containing digits is not an "alphabetic scheme", so only it is masked and the signature leaks.
        assert_hidden(
            "Authorization: AWS4-HMAC-SHA256 Credential=AKIAEXAMPLE/20260101/x, Signature=deadbeef0123",
            "deadbeef0123",
        );
    }

    #[test]
    fn colon_after_a_key_word_in_prose_is_masked_by_design() {
        // catches: a silent change of the accepted trade-off: over-masking beats a leak
        // for `key: value`, so `duplicate key: command` is masked.
        assert_eq!(detail("duplicate key: command"), "duplicate key: ***");
    }

    #[test]
    fn hostile_long_input_stays_bounded() {
        // catches: super-linear scans or an unbounded result on a hostile cause.
        for unit in [
            "-----BEGIN ",
            "a://",
            "key:",
            "token=\"\\",
            "k=",
            "authorization: a ",
        ] {
            let out = detail(&unit.repeat(40_000));
            assert!(
                out.chars().count() <= MAX_DIAGNOSTIC_DETAIL_CHARS,
                "{unit:?}"
            );
        }
    }
}

#[cfg(test)]
mod secret_mask_critic_round_three {
    use super::*;

    fn detail(cause: &str) -> String {
        bounded_detail(cause).unwrap_or_default()
    }

    fn assert_hidden(cause: &str, secret: &str) {
        let out = detail(cause);
        assert!(
            !out.contains(secret),
            "{cause:?} leaked {secret:?}: {out:?}"
        );
    }

    #[test]
    fn json_encoded_inside_a_string_still_hides_the_value() {
        // catches: a backslash before the closing quote of the name (`\"token\":`) ends the
        // name scan empty, so a JSON body echoed inside a JSON string leaks.
        assert_hidden(r#"body {\"token\":\"LEAKESC\"} rejected"#, "LEAKESC");
        assert_hidden(r#"body {\"api_key\": \"LEAKESC\"}"#, "LEAKESC");
    }

    #[test]
    fn digest_params_with_spaces_inside_quotes_do_not_leak_later_params() {
        // catches: the parameter loop breaks on `realm"` (no `=`), so `response=` leaks.
        assert_hidden(
            r#"Authorization: Digest username="u", realm="my realm", response="LEAKRESP""#,
            "LEAKRESP",
        );
    }

    #[test]
    fn invisible_marks_missing_from_the_drop_list_do_not_split_the_name() {
        // catches: is_invisible lists only some format characters; a combining grapheme
        // joiner, variation selector, tag character or Mongolian vowel separator splits
        // `token` and the value is published.
        for mark in ['\u{034f}', '\u{fe0f}', '\u{e0041}', '\u{180e}', '\u{3164}'] {
            let cause = format!("to{mark}ken=LEAKINV done");
            assert_hidden(&cause, "LEAKINV");
        }
    }

    #[test]
    fn bracketed_and_tuple_forms_are_hidden() {
        // catches: only `name=`/`name:` separators are recognised; `headers[token]=x` and
        // Rust Debug tuples `("x-api-key", "x")` leak.
        assert_hidden("params[token]=LEAKBR&a=1", "LEAKBR");
        assert_hidden(r#"header ("x-api-key", "LEAKTUP") rejected"#, "LEAKTUP");
        assert_hidden(r#"Header("authorization", "Bearer LEAKTUP")"#, "LEAKTUP");
    }

    #[test]
    fn quoted_bearer_credential_with_a_space_does_not_leak_its_tail() {
        // catches: after a scheme the credential ends at the first blank, so a quoted
        // credential with a space leaks the second word.
        assert_hidden(r#"Authorization: Bearer "abc LEAKSP" rejected"#, "LEAKSP");
    }

    #[test]
    fn unterminated_private_key_header_does_not_publish_the_body() {
        // catches: a header cut before its closing dashes is not recognised and the key body is kept.
        assert_hidden(
            "-----BEGIN PRIVATE KEY MIIEvQIBADANBgkqLEAKBODY",
            "LEAKBODY",
        );
    }

    #[test]
    fn empty_value_does_not_mask_the_next_pair() {
        // catches: `key= mode=fast` skips the blank and masks the unrelated next word.
        assert_eq!(detail("empty key= mode=fast"), "empty key= mode=fast");
    }

    #[test]
    fn urls_without_userinfo_are_kept_verbatim() {
        // catches: userinfo masking firing on `@` in a path/query or on a host port.
        for cause in [
            "GET http://localhost:8080/mcp failed",
            "see https://example.com/a@b now",
            "see https://example.com?x=a@b now",
            "connect tcp://127.0.0.1:9 refused",
        ] {
            assert_eq!(detail(cause), cause);
        }
    }
}

#[cfg(test)]
mod secret_mask_critic_round_four {
    use super::*;

    fn detail(cause: &str) -> String {
        bounded_detail(cause).unwrap_or_default()
    }

    fn assert_hidden(cause: &str, secret: &str) {
        let out = detail(cause);
        assert!(
            !out.contains(secret),
            "{cause:?} leaked {secret:?}: {out:?}"
        );
    }

    #[test]
    fn json_error_bodies_whose_message_ends_in_a_secret_word_are_kept() {
        // catches: the tuple rule `"<name>", <next>` firing on any quoted string that ends in
        // `token`/`key` and replacing the next JSON member name with `***`.
        for cause in [
            r#"{"error":"invalid token","code":401}"#,
            r#"{"message":"missing api key","code":400}"#,
            r#"{"field":"api_key","message":"required"}"#,
        ] {
            assert_eq!(detail(cause), cause);
        }
    }

    #[test]
    fn string_lists_that_name_a_secret_word_are_kept() {
        // catches: `["key", "name"]` / `["authorization", "content-type"]` losing their second
        // element to the comma-separated tuple rule.
        for cause in [
            r#"missing properties ["key", "name"]"#,
            r#"allowed headers ["authorization", "content-type"]"#,
        ] {
            assert_eq!(detail(cause), cause);
        }
    }

    #[test]
    fn name_value_header_records_hide_the_value() {
        // catches: the tuple rule masking the `"value"` member name instead of its value, so the
        // secret itself stays in `{"name":"x-api-key","value":"..."}` / `Header { name, value }`.
        assert_hidden(r#"{"name":"x-api-key","value":"LEAKREC"}"#, "LEAKREC");
        assert_hidden(
            r#"Header { name: "x-api-key", value: "LEAKREC" }"#,
            "LEAKREC",
        );
    }

    #[test]
    fn json_escaped_twice_hides_the_value() {
        // catches: `\\\"token\\\"` (JSON inside a JSON string inside a JSON string) not
        // recognised as a name, because only one escaped quote is stripped.
        assert_hidden(
            r#"outer {\\\"token\\\":\\\"LEAKDEEP\\\"} rejected"#,
            "LEAKDEEP",
        );
    }

    #[test]
    fn escaped_quote_inside_an_escaped_value_does_not_end_it() {
        // catches: in `\"pa\\\"TAIL\"` the scan reads `\\` then `\"` as the closing quote and
        // publishes everything after the embedded quote.
        assert_hidden(
            r#"body {\"password\":\"pa\\\"LEAKTAIL\"} rejected"#,
            "LEAKTAIL",
        );
        assert_hidden(
            r#"{\"authorization\":\"Digest username=\\\"u\\\", response=\\\"LEAKDIG\\\"\"}"#,
            "LEAKDIG",
        );
    }

    #[test]
    fn pretty_printed_object_under_a_secret_name_is_hidden() {
        // catches: whitespace collapse turns `"password": {\n "value": "x" }` into a value
        // starting with `{`; only that brace is masked and the member below leaks.
        assert_hidden(r#"{ "password": { "value": "LEAKNEST" } }"#, "LEAKNEST");
    }

    #[test]
    fn invisible_table_range_boundaries_are_all_dropped() {
        // catches: an off-by-one or typo at the edge of a range in the invisible table, which
        // lets one mark split a secret name.
        for mark in [
            '\u{00ad}',
            '\u{0605}',
            '\u{06dd}',
            '\u{070f}',
            '\u{0891}',
            '\u{08e2}',
            '\u{1160}',
            '\u{17b4}',
            '\u{180b}',
            '\u{180f}',
            '\u{200b}',
            '\u{200f}',
            '\u{202a}',
            '\u{202e}',
            '\u{2060}',
            '\u{2064}',
            '\u{2066}',
            '\u{206f}',
            '\u{fe00}',
            '\u{feff}',
            '\u{ffa0}',
            '\u{fff9}',
            '\u{fffb}',
            '\u{110bd}',
            '\u{110cd}',
            '\u{13430}',
            '\u{1343f}',
            '\u{1bca0}',
            '\u{1bca3}',
            '\u{1d173}',
            '\u{1d17a}',
            '\u{e0001}',
            '\u{e0100}',
            '\u{e01ef}',
        ] {
            let cause = format!("to{mark}ken=LEAKMARK done");
            assert_hidden(&cause, "LEAKMARK");
        }
    }

    #[test]
    fn private_key_headers_cut_at_every_point_hide_the_body() {
        // catches: the unterminated-header branch keeping text for a header cut mid-fence,
        // right at its end, or followed directly by the body.
        for cause in [
            "-----BEGIN PRIVATE KEY",
            "-----BEGIN PRIVATE KEY--",
            "-----BEGIN PRIVATE KEY-----",
            "-----BEGIN PRIVATE KEY----- LEAKPEM",
            "x -----BEGIN OPENSSH PRIVATE KEY----- LEAKPEM -----END",
        ] {
            assert_hidden(cause, "LEAKPEM");
            assert!(
                detail(cause).ends_with("***"),
                "{cause:?}: {:?}",
                detail(cause)
            );
        }
    }

    #[test]
    fn separator_followed_only_by_blanks_masks_nothing_and_does_not_panic() {
        // catches: the pair look-ahead indexing past the end when `key=` is followed by blanks
        // to the end of the text (mask_secrets gets collapsed text, but may end in a blank
        // after an earlier mask).
        assert_eq!(mask_secrets("key= "), "key= ");
        assert_eq!(mask_secrets("token: "), "token: ");
        assert_eq!(detail("key= "), "key=");
    }
}

#[cfg(test)]
mod secret_mask_critic_round_five {
    use super::*;
    use std::time::{Duration, Instant};

    fn detail(cause: &str) -> String {
        bounded_detail(cause).unwrap_or_default()
    }

    fn assert_hidden(cause: &str, secret: &str) {
        let out = detail(cause);
        assert!(
            !out.contains(secret),
            "{cause:?} leaked {secret:?}: {out:?}"
        );
    }

    fn assert_kept(cause: &str, kept: &str) {
        let out = detail(cause);
        assert!(out.contains(kept), "{cause:?} lost {kept:?}: {out:?}");
    }

    #[test]
    fn long_inputs_are_masked_in_linear_time() {
        // catches: `name:` followed by an unclosed or nested bracket makes `names_secret` rescan
        // (and copy) the rest of the text at every `name:`, so n members cost n squared.
        let cases = [
            ("unclosed objects", "name:{".repeat(10_000)),
            ("unclosed arrays", "name:[".repeat(10_000)),
            (
                "nested objects",
                format!("{}1{}", r#"{"name":"#.repeat(12_000), "}".repeat(12_000)),
            ),
            ("blank key pairs", "key= ".repeat(50_000)),
            (
                "authorization chain",
                "authorization: Bearer ".repeat(20_000),
            ),
            ("tuple heads", r#"("token", "#.repeat(20_000)),
        ];
        for (label, cause) in cases {
            let started = Instant::now();
            let _ = bounded_detail(&cause);
            let elapsed = started.elapsed();
            assert!(
                elapsed < Duration::from_secs(3),
                "{label}: {elapsed:?} for {} chars",
                cause.len()
            );
        }
    }

    #[test]
    fn every_prefix_and_suffix_of_a_dense_sample_is_masked_without_panicking() {
        // catches: an index past the end in the look-ahead, escape-run, bracket or record scans
        // when the text is cut at any point.
        let sample = format!(
            "\u{e9}\u{200d}{}",
            r#"{\"name\":\"x-api-key\",\"value\":\\\"a\\\\\"b\",\"token\":{\"a\":[\"b\"]},("key", 'v')} Authorization: Digest a="b c", d=\"e\" name: token value: [x {y 'z \\\\"#
        );
        let cuts = sample
            .char_indices()
            .map(|(index, _)| index)
            .chain(std::iter::once(sample.len()));
        for cut in cuts {
            let _ = bounded_detail(&sample[..cut]);
            let _ = bounded_detail(&sample[cut..]);
        }
    }

    #[test]
    fn name_value_record_with_an_auth_scheme_word_as_name_hides_the_value() {
        // catches: `names_secret` reading `name=token value=X` as the scheme `token` plus its
        // credential `value=X`, so the content is not a single name and `value` stays public.
        for cause in [
            "param name=token value=LEAKREC",
            "param name: token value: LEAKREC",
            "param name=bearer value=LEAKREC",
        ] {
            assert_hidden(cause, "LEAKREC");
        }
    }

    #[test]
    fn key_value_records_hide_the_value_member() {
        // catches: `{"Key":"db_password","Value":"X"}` (AWS tags, Postman, env lists): `key` is
        // itself a secret suffix, so only its value is masked and the sibling `Value` leaks.
        for cause in [
            r#"{"key":"API_TOKEN","value":"LEAKKV"}"#,
            r#"{"Key":"db_password","Value":"LEAKKV"}"#,
            r#"[{"Key":"Environment","Value":"prod"},{"Key":"api_key","Value":"LEAKKV"}]"#,
        ] {
            assert_hidden(cause, "LEAKKV");
        }
    }

    #[test]
    fn name_value_record_hides_the_value_and_keeps_other_records() {
        // catches: the record rule masking every `value` (or none) instead of the one that
        // follows a secret-looking `name`.
        let cause = r#"[{"name":"token","value":"LEAKR1"},{"name":"mode","value":"fast"}]"#;
        assert_hidden(cause, "LEAKR1");
        assert_kept(cause, r#""value":"fast""#);
        let escaped = r#"{\\\"name\\\":\\\"x-api-key\\\",\\\"value\\\":\\\"LEAKR2\\\"}"#;
        assert_hidden(escaped, "LEAKR2");
    }

    #[test]
    fn a_name_marker_does_not_outlive_its_record() {
        // catches: `record_secret` staying set after `{"name":"token"}` (no `value` member), so
        // the `value` of an unrelated later object is published as `***`.
        assert_kept(
            r#"[{"name":"token","type":"string"},{"id":7,"value":"public"}]"#,
            "public",
        );
    }

    #[test]
    fn pretty_printed_debug_tuples_hide_the_value() {
        // catches: `opens_tuple` requiring `("name"` with no blank, so `{:#?}` output (whitespace
        // collapsed to `( "x-api-key", "X", )`) falls back to text and leaks.
        for cause in [
            "(\n    \"x-api-key\",\n    \"LEAKPT\",\n)",
            "Header(\n    \"authorization\",\n    \"Bearer LEAKPT\",\n)",
            "[\n    (\n        \"x-api-key\",\n        \"LEAKPT\",\n    ),\n]",
        ] {
            assert_hidden(cause, "LEAKPT");
        }
    }

    #[test]
    fn python_bytes_tuples_hide_the_value() {
        // catches: `opens_tuple` not seeing `(b'x-api-key'`, so ASGI/requests style header lists
        // `[(b'authorization', b'Bearer X')]` fall back to text and leak.
        for cause in [
            r#"[(b'x-api-key', b'LEAKBT')]"#,
            r#"[(b'authorization', b'Bearer LEAKBT')]"#,
        ] {
            assert_hidden(cause, "LEAKBT");
        }
    }

    #[test]
    fn debug_tuples_hide_the_value_and_lists_stay_text() {
        // catches: the tuple rule dropped (leak) or widened back to every quoted pair (lists
        // lose their second element).
        for cause in [
            r#"Some(("x-api-key", "LEAKT"))"#,
            r#"[("accept", "*/*"), ("x-api-key", "LEAKT")]"#,
            r#"(\"x-api-key\", \"LEAKT\")"#,
            r#"[('Authorization', 'Bearer LEAKT')]"#,
        ] {
            assert_hidden(cause, "LEAKT");
        }
        for cause in [
            r#"missing properties ["key", "name"]"#,
            r#"{"error":"invalid token","code":401}"#,
        ] {
            assert_eq!(detail(cause), cause);
        }
    }

    #[test]
    fn brackets_inside_json_strings_of_an_escaped_nested_value_do_not_close_it() {
        // catches: `bracket_end` skipping the escaped quotes of a JSON body embedded in a string,
        // so it never sees `"x}y"` as a string and closes the value at the `}` inside it.
        let cause = r#"{\"token\":{\"a\":\"x}y\",\"b\":\"LEAKBR\"},\"user\":\"bob\"} done"#;
        assert_hidden(cause, "LEAKBR");
        assert_kept(cause, "bob");
    }

    #[test]
    fn brackets_inside_single_quoted_strings_of_a_nested_value_do_not_close_it() {
        // catches: `bracket_end` tracking only double quotes, so a Python dict repr
        // `{'token': {'a': 'x}y', 'b': 'X'}}` closes at the `}` inside the string.
        let cause = r#"{'token': {'a': 'x}y', 'b': 'LEAKSQ'}, 'user': 'bob'} done"#;
        assert_hidden(cause, "LEAKSQ");
        assert_kept(cause, "bob");
    }

    #[test]
    fn balanced_nested_values_are_masked_whole_and_the_text_after_them_is_kept() {
        // catches: masking only the first brace or word of a nested value, or swallowing the
        // siblings that follow it.
        for cause in [
            r#"{"token":{"a":{"b":"LEAKN1"},"c":["LEAKN2","LEAKN3"]},"user":"bob"} done"#,
            r#"{"token":{"a":"x}y","b":"LEAKN4"},"user":"bob"} done"#,
            r#"{\"token\":{\"a\":{\"b\":\"LEAKN5\"}},\"user\":\"bob\"} done"#,
        ] {
            let out = detail(cause);
            assert!(!out.contains("LEAKN"), "{cause:?} leaked: {out:?}");
            assert!(
                out.contains("bob") && out.ends_with("done"),
                "{cause:?} lost its siblings: {out:?}"
            );
        }
        assert_hidden("token: {truncated LEAKN6", "LEAKN6");
    }

    #[test]
    fn embedded_escaped_quotes_stay_inside_the_secret_at_every_depth() {
        // catches: a closing quote picked from an inner escaped quote (`LEAKA\"LEAKB`), which
        // publishes the tail of the secret; depth 0, 1 and 2.
        for cause in [
            r#"password="LEAKA\"LEAKB" user=bob"#,
            r#"password='LEAKA\'LEAKB' user=bob"#,
            r#"{\"password\":\"LEAKA\\\"LEAKB\",\"user\":\"bob\"}"#,
            r#"{\\\"password\\\":\\\"LEAKA\\\\\\\"LEAKB\\\",\\\"user\\\":\\\"bob\\\"}"#,
        ] {
            let out = detail(cause);
            assert!(!out.contains("LEAK"), "{cause:?} leaked: {out:?}");
            assert!(out.contains("bob"), "{cause:?} lost its sibling: {out:?}");
        }
    }

    #[test]
    fn joiners_after_ascii_never_split_a_secret_name_and_joiners_in_text_survive() {
        // catches: keeping a joiner or variation selector after ASCII (splits `to<ZWJ>ken`), or
        // dropping it after a non-ASCII character (breaks emoji and Persian text).
        for joiner in ['\u{200c}', '\u{200d}', '\u{fe0f}', '\u{e0100}'] {
            for cause in [
                format!("to{joiner}ken=LEAKJ"),
                format!("api_{joiner}key: LEAKJ"),
                format!("to\u{200b}{joiner}ken=LEAKJ"),
                format!("\u{e9}{joiner}token=LEAKJ"),
            ] {
                assert_hidden(&cause, "LEAKJ");
            }
        }
        assert_eq!(
            detail("\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467} token=LEAKJ"),
            "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467} token=***"
        );
        assert_eq!(
            detail("\u{645}\u{6cc}\u{200c}\u{62e}\u{648}\u{627}\u{647}\u{645}"),
            "\u{645}\u{6cc}\u{200c}\u{62e}\u{648}\u{627}\u{647}\u{645}"
        );
    }

    #[test]
    fn ordinary_pairs_and_records_without_a_secret_name_are_kept() {
        // catches: over-masking from the round-five rules (bracket values, records, tuples).
        for cause in [
            "duplicate tool identity: search",
            "mode=fast retries=3",
            r#"{"name":"mode","value":"fast"}"#,
            r#"{"name":"theme","value":"dark","extra":{"value":1}}"#,
            "name: web value: 42",
        ] {
            assert_eq!(detail(cause), cause);
        }
    }
}

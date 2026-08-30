//! Crate-private discovery and immutable admission of `SKILL.md` files.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use ego_core::config::{ConfigLayer, Sourced, Trust, resolve};
use ego_core::event::hook::HookLifecycle;
use ego_core::event::skill::{SkillInvocationId, SkillSourceLocation};
use ego_core::hook::{HookConfigEntry, HookHandlerConfig, HookMatcher};
use ego_core::permit::rule::Rule;

use crate::yaml_envelope::{self, MAX_FILE_BYTES, Node};

const PORTABLE_TOOLS: &[&str] = &[
    "ask_question",
    "bash",
    "edit",
    "glob",
    "grep",
    "read",
    "write",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum SkillSourceAuthority {
    System,
    User,
    Project,
}

#[derive(Clone, Debug)]
pub(crate) struct SkillDiscovery {
    system: PathBuf,
    user: PathBuf,
    project: PathBuf,
    trusted_project: bool,
}

impl SkillDiscovery {
    pub(crate) fn new(
        system: PathBuf,
        user: PathBuf,
        project: PathBuf,
        trusted_project: bool,
    ) -> Self {
        Self {
            system,
            user,
            project,
            trusted_project,
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct SkillDescriptor {
    name: String,
    description: String,
    source: SkillSourceAuthority,
    source_path: PathBuf,
    source_digest: String,
    allowed_tools: Vec<String>,
    agent: Option<String>,
}

impl fmt::Debug for SkillDescriptor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SkillDescriptor")
            .field("name", &self.name)
            .field("description", &self.description)
            .field("source", &self.source)
            .field("source_path", &self.source_path)
            .field("source_digest", &self.source_digest)
            .field("agent", &self.agent)
            .finish_non_exhaustive()
    }
}

impl SkillDescriptor {
    pub(crate) fn name(&self) -> &str {
        &self.name
    }
    pub(crate) fn source(&self) -> SkillSourceAuthority {
        self.source
    }
    pub(crate) fn agent(&self) -> Option<&str> {
        self.agent.as_deref()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SkillDiagnostic {
    name: String,
    source: SkillSourceAuthority,
    source_path: PathBuf,
    message: String,
    collision: bool,
}

impl SkillDiagnostic {
    pub(crate) fn name(&self) -> &str {
        &self.name
    }
    pub(crate) fn source(&self) -> SkillSourceAuthority {
        self.source
    }
    pub(crate) fn source_path(&self) -> &Path {
        &self.source_path
    }
    pub(crate) fn message(&self) -> &str {
        &self.message
    }
    pub(crate) fn is_collision(&self) -> bool {
        self.collision
    }
}

impl fmt::Display for SkillDiagnostic {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "skill `{}` at {}: {}",
            self.name,
            self.source_path.display(),
            self.message
        )
    }
}

#[derive(Debug)]
pub(crate) struct SkillCatalogueError(String);

impl fmt::Display for SkillCatalogueError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct SkillCatalogue {
    descriptors: Vec<SkillDescriptor>,
    diagnostics: Vec<SkillDiagnostic>,
    snapshot_digest: String,
}

impl SkillCatalogue {
    pub(crate) fn discover(
        discovery: SkillDiscovery,
        fixed_actions: &[&str],
    ) -> Result<Self, SkillCatalogueError> {
        let candidate = Candidate::build(discovery, fixed_actions, None)?;
        Ok(Self {
            snapshot_digest: candidate.digest(),
            descriptors: candidate.descriptors,
            diagnostics: candidate.diagnostics,
        })
    }

    pub(crate) fn discover_with_profile_layers(
        discovery: SkillDiscovery,
        fixed_actions: &[&str],
        profile_layers: &[Sourced<ConfigLayer>],
    ) -> Result<Self, SkillCatalogueError> {
        let candidate = Candidate::build(discovery, fixed_actions, Some(profile_layers))?;
        Ok(Self {
            snapshot_digest: candidate.digest(),
            descriptors: candidate.descriptors,
            diagnostics: candidate.diagnostics,
        })
    }

    pub(crate) fn reload(
        &mut self,
        discovery: SkillDiscovery,
        fixed_actions: &[&str],
    ) -> Result<(), SkillCatalogueError> {
        let candidate = Candidate::build(discovery, fixed_actions, None)?;
        self.publish_reload(candidate);
        Ok(())
    }

    pub(crate) fn reload_with_profile_layers(
        &mut self,
        discovery: SkillDiscovery,
        fixed_actions: &[&str],
        profile_layers: &[Sourced<ConfigLayer>],
    ) -> Result<(), SkillCatalogueError> {
        let candidate = Candidate::build(discovery, fixed_actions, Some(profile_layers))?;
        self.publish_reload(candidate);
        Ok(())
    }

    fn publish_reload(&mut self, candidate: Candidate) {
        if candidate.diagnostics.is_empty() {
            self.snapshot_digest = candidate.digest();
            self.descriptors = candidate.descriptors;
        }
        self.diagnostics = candidate.diagnostics;
    }

    pub(crate) fn descriptors(&self) -> &[SkillDescriptor] {
        &self.descriptors
    }
    pub(crate) fn diagnostics(&self) -> &[SkillDiagnostic] {
        &self.diagnostics
    }
    pub(crate) fn descriptor(&self, name: &str) -> Option<&SkillDescriptor> {
        self.descriptors
            .iter()
            .find(|descriptor| descriptor.name == name)
    }
    pub(crate) fn snapshot_digest(&self) -> &str {
        &self.snapshot_digest
    }
}

struct Candidate {
    descriptors: Vec<SkillDescriptor>,
    diagnostics: Vec<SkillDiagnostic>,
}

impl Candidate {
    fn build(
        discovery: SkillDiscovery,
        fixed_actions: &[&str],
        profile_layers: Option<&[Sourced<ConfigLayer>]>,
    ) -> Result<Self, SkillCatalogueError> {
        let mut fixed_unique = BTreeSet::new();
        for action in fixed_actions {
            if action.is_empty() || !fixed_unique.insert(*action) {
                return Err(SkillCatalogueError(format!(
                    "fixed action vocabulary contains invalid or duplicate name `{action}`"
                )));
            }
        }
        let profile_trust = if discovery.trusted_project {
            Trust::Trusted
        } else {
            Trust::Untrusted
        };
        let mut descriptors = Vec::new();
        let mut diagnostics = Vec::new();
        for (source, root, enabled) in [
            (SkillSourceAuthority::System, discovery.system, true),
            (SkillSourceAuthority::User, discovery.user, true),
            (
                SkillSourceAuthority::Project,
                discovery.project,
                discovery.trusted_project,
            ),
        ] {
            discover_root(source, &root, enabled, &mut descriptors, &mut diagnostics);
        }

        if let Some(profile_layers) = profile_layers {
            descriptors.retain(|descriptor| {
                let Some(profile) = descriptor.agent.as_deref() else {
                    return true;
                };
                match resolve(profile_layers, profile_trust, Some(profile)) {
                    Ok(_) => true,
                    Err(error) => {
                        diagnostics.push(SkillDiagnostic {
                            name: descriptor.name.clone(),
                            source: descriptor.source,
                            source_path: descriptor.source_path.clone(),
                            message: format!(
                                "field `agent` must select an existing profile: {error}"
                            ),
                            collision: false,
                        });
                        false
                    }
                }
            });
        }

        let fixed = fixed_unique;
        let mut occurrences: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for (index, descriptor) in descriptors.iter().enumerate() {
            occurrences
                .entry(descriptor.name.clone())
                .or_default()
                .push(index);
        }
        let mut rejected = BTreeSet::new();
        for (name, indices) in occurrences {
            if indices.len() > 1 || fixed.contains(name.as_str()) {
                for index in indices {
                    rejected.insert(index);
                    let descriptor = &descriptors[index];
                    diagnostics.push(SkillDiagnostic {
                        name: name.clone(),
                        source: descriptor.source,
                        source_path: descriptor.source_path.clone(),
                        message: if fixed.contains(name.as_str()) {
                            format!("collides with fixed action `{name}`")
                        } else {
                            format!("collides with another admitted skill named `{name}`")
                        },
                        collision: true,
                    });
                }
            }
        }
        descriptors = descriptors
            .into_iter()
            .enumerate()
            .filter_map(|(index, descriptor)| (!rejected.contains(&index)).then_some(descriptor))
            .collect();
        descriptors.sort_by(|left, right| left.name.cmp(&right.name));
        diagnostics.sort_by(|left, right| {
            left.name
                .cmp(&right.name)
                .then_with(|| left.source.cmp(&right.source))
                .then_with(|| left.source_path.cmp(&right.source_path))
        });
        Ok(Self {
            descriptors,
            diagnostics,
        })
    }

    fn digest(&self) -> String {
        let mut canonical = String::new();
        for descriptor in &self.descriptors {
            canonical.push_str(&descriptor.name);
            canonical.push('\0');
            canonical.push_str(match descriptor.source {
                SkillSourceAuthority::System => "system",
                SkillSourceAuthority::User => "user",
                SkillSourceAuthority::Project => "project",
            });
            canonical.push('\0');
            canonical.push_str(&descriptor.source_digest);
            canonical.push('\n');
        }
        ego_core::digest::sha256_hex(canonical.as_bytes())
    }
}

fn discover_root(
    source: SkillSourceAuthority,
    root: &Path,
    enabled: bool,
    descriptors: &mut Vec<SkillDescriptor>,
    diagnostics: &mut Vec<SkillDiagnostic>,
) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    let mut paths = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path().join("SKILL.md"))
        .filter(|path| path.is_file())
        .collect::<Vec<_>>();
    paths.sort();
    for path in paths {
        let directory_name = path
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            .unwrap_or("<invalid>")
            .to_owned();
        if !enabled {
            diagnostics.push(diagnostic(
                directory_name,
                source,
                path,
                "project skill is unavailable because the workspace is untrusted",
            ));
            continue;
        }
        match load_skill(source, root, &path, &directory_name) {
            Ok(descriptor) => descriptors.push(descriptor),
            Err(message) => diagnostics.push(diagnostic(directory_name, source, path, message)),
        }
    }
}

fn load_skill(
    source: SkillSourceAuthority,
    root: &Path,
    requested_path: &Path,
    directory_name: &str,
) -> Result<SkillDescriptor, String> {
    let canonical_root = fs::canonicalize(root)
        .map_err(|error| format!("skill root could not be resolved: {error}"))?;
    let path = fs::canonicalize(requested_path)
        .map_err(|error| format!("could not be resolved: {error}"))?;
    if !path.starts_with(&canonical_root) {
        return Err("resolves outside its canonical skill root".to_owned());
    }
    let mut file = fs::File::open(&path).map_err(|error| format!("could not be read: {error}"))?;
    if file
        .metadata()
        .map_err(|error| format!("could not be read: {error}"))?
        .len()
        > MAX_FILE_BYTES
    {
        return Err(format!("exceeds {MAX_FILE_BYTES} bytes"));
    }
    let mut bytes = Vec::new();
    file.by_ref()
        .take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("could not be read: {error}"))?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(format!("exceeds {MAX_FILE_BYTES} bytes"));
    }
    let text = String::from_utf8(bytes).map_err(|_| "is not valid UTF-8".to_owned())?;
    let source_digest = ego_core::digest::sha256_hex(text.as_bytes());
    let (header, _body) = yaml_envelope::split(&text, false)?;
    let mut entries = yaml_envelope::mapping(header)?;
    validate_field_names(&entries)?;

    let name = take_required_string(&mut entries, "name")?;
    validate_skill_name(&name)?;
    if name != directory_name {
        return Err(format!(
            "field `name` must equal containing directory `{directory_name}`"
        ));
    }
    let description = take_required_string(&mut entries, "description")?;
    if description.chars().count() > 1024 {
        return Err("field `description` exceeds 1024 characters".to_owned());
    }

    let allowed_tools = take_tool_scalar(&mut entries, "allowed-tools")?.unwrap_or_default();
    let _disallowed_tools = take_tool_scalar(&mut entries, "disallowed-tools")?.unwrap_or_default();
    let _ = take_optional_string(&mut entries, "license")?;
    let compatibility = take_optional_string(&mut entries, "compatibility")?;
    if compatibility
        .as_deref()
        .is_some_and(|value| value.chars().count() > 500)
    {
        return Err("field `compatibility` exceeds 500 characters".to_owned());
    }
    for key in ["when_to_use", "argument-hint", "model", "effort"] {
        let _ = take_optional_string(&mut entries, key)?;
    }
    let user_invocable = take_optional_bool(&mut entries, "user-invocable")?.unwrap_or(true);
    let model_invocable =
        !take_optional_bool(&mut entries, "disable-model-invocation")?.unwrap_or(false);
    if !user_invocable && !model_invocable {
        return Err(
            "fields `user-invocable` and `disable-model-invocation` make the skill unreachable"
                .to_owned(),
        );
    }
    let arguments = take_string_sequence(&mut entries, "arguments")?;
    if let Some(argument) = arguments
        .iter()
        .find(|argument| !valid_identifier(argument))
    {
        return Err(format!(
            "field `arguments` contains invalid identifier `{argument}`"
        ));
    }
    let _ = take_string_sequence(&mut entries, "paths")?;
    let _ = take_string_mapping(&mut entries, "metadata")?;

    let context = take_optional_string(&mut entries, "context")?;
    if context.as_deref().is_some_and(|value| value != "fork") {
        return Err("field `context` supports only exact value `fork`".to_owned());
    }
    let agent = take_optional_string(&mut entries, "agent")?;
    if agent.is_some() && context.as_deref() != Some("fork") {
        return Err("field `agent` requires `context: fork`".to_owned());
    }
    if let Some(agent) = &agent {
        validate_profile_name(agent).map_err(|message| format!("field `agent` {message}"))?;
    }
    if take_optional_bool(&mut entries, "background")? == Some(true) {
        return Err(
            "field `background` is unavailable until its final owner is connected".to_owned(),
        );
    }
    if entries.contains_key("hooks") {
        return Err("field `hooks` is unavailable until its final owner is connected".to_owned());
    }
    if entries.contains_key("shell") {
        return Err("field `shell` is unsupported".to_owned());
    }
    debug_assert!(entries.is_empty(), "the closed field set is fully consumed");

    Ok(SkillDescriptor {
        name,
        description,
        source,
        source_path: path,
        source_digest,
        allowed_tools,
        agent,
    })
}

fn validate_field_names(entries: &BTreeMap<String, Node>) -> Result<(), String> {
    for key in entries.keys() {
        if key == "allowed_tools" {
            return Err(
                "field `allowed_tools` is unsupported; use exact field `allowed-tools`".to_owned(),
            );
        }
        if !matches!(
            key.as_str(),
            "name"
                | "description"
                | "license"
                | "compatibility"
                | "metadata"
                | "allowed-tools"
                | "when_to_use"
                | "argument-hint"
                | "arguments"
                | "disable-model-invocation"
                | "user-invocable"
                | "disallowed-tools"
                | "model"
                | "effort"
                | "paths"
                | "context"
                | "agent"
                | "background"
                | "hooks"
                | "shell"
        ) {
            return Err(format!("contains unknown or unsupported field `{key}`"));
        }
    }
    Ok(())
}

fn take_required_string(entries: &mut BTreeMap<String, Node>, key: &str) -> Result<String, String> {
    take_optional_string(entries, key)?
        .ok_or_else(|| format!("is missing required string field `{key}`"))
}

fn take_optional_string(
    entries: &mut BTreeMap<String, Node>,
    key: &str,
) -> Result<Option<String>, String> {
    let Some(node) = entries.remove(key) else {
        return Ok(None);
    };
    yaml_envelope::scalar(node)
        .map(Some)
        .ok_or_else(|| format!("field `{key}` must be a non-empty string"))
}

fn take_optional_bool(
    entries: &mut BTreeMap<String, Node>,
    key: &str,
) -> Result<Option<bool>, String> {
    let Some(node) = entries.remove(key) else {
        return Ok(None);
    };
    match node {
        Node::Scalar { value, plain: true } if value == "true" => Ok(Some(true)),
        Node::Scalar { value, plain: true } if value == "false" => Ok(Some(false)),
        _ => Err(format!("field `{key}` must be a boolean")),
    }
}

fn take_string_sequence(
    entries: &mut BTreeMap<String, Node>,
    key: &str,
) -> Result<Vec<String>, String> {
    let Some(node) = entries.remove(key) else {
        return Ok(Vec::new());
    };
    let Node::Sequence(nodes) = node else {
        return Err(format!("field `{key}` must be a sequence of strings"));
    };
    let mut values = Vec::new();
    let mut unique = BTreeSet::new();
    for node in nodes {
        let Some(value) = yaml_envelope::scalar(node) else {
            return Err(format!("field `{key}` must contain only non-empty strings"));
        };
        if !unique.insert(value.clone()) {
            return Err(format!("field `{key}` contains duplicate `{value}`"));
        }
        values.push(value);
    }
    Ok(values)
}

fn take_string_mapping(
    entries: &mut BTreeMap<String, Node>,
    key: &str,
) -> Result<BTreeMap<String, String>, String> {
    let Some(node) = entries.remove(key) else {
        return Ok(BTreeMap::new());
    };
    let Node::Mapping(nodes) = node else {
        return Err(format!("field `{key}` must be a string mapping"));
    };
    nodes
        .into_iter()
        .map(|(key_node, value_node)| {
            let Some(map_key) = yaml_envelope::scalar(key_node) else {
                return Err(format!("field `{key}` keys must be non-empty strings"));
            };
            let Some(value) = yaml_envelope::scalar(value_node) else {
                return Err(format!("field `{key}` values must be non-empty strings"));
            };
            Ok((map_key, value))
        })
        .collect()
}

fn take_tool_scalar(
    entries: &mut BTreeMap<String, Node>,
    key: &str,
) -> Result<Option<Vec<String>>, String> {
    let Some(node) = entries.remove(key) else {
        return Ok(None);
    };
    let value = yaml_envelope::scalar(node)
        .ok_or_else(|| format!("field `{key}` must be one non-empty space-separated scalar"))?;
    let tools = value.split(' ').collect::<Vec<_>>();
    if tools.is_empty()
        || tools.iter().any(|tool| tool.is_empty())
        || tools.join(" ") != value
        || value.contains(',')
    {
        return Err(format!(
            "field `{key}` must be one non-empty space-separated scalar"
        ));
    }
    let mut unique = BTreeSet::new();
    for tool in &tools {
        if !PORTABLE_TOOLS.contains(tool) {
            return Err(format!("field `{key}` contains unknown tool `{tool}`"));
        }
        if !unique.insert(*tool) {
            return Err(format!("field `{key}` contains duplicate tool `{tool}`"));
        }
    }
    Ok(Some(tools.into_iter().map(str::to_owned).collect()))
}

fn validate_skill_name(name: &str) -> Result<(), String> {
    if name.is_empty()
        || name.len() > 64
        || name.starts_with('-')
        || name.ends_with('-')
        || name.contains("--")
        || !name.chars().all(|character| {
            character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
        })
    {
        return Err(format!("field `name` has invalid skill name `{name}`"));
    }
    Ok(())
}

fn validate_profile_name(name: &str) -> Result<(), &'static str> {
    if name.is_empty()
        || !name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
    {
        return Err("must name an existing profile with a canonical identifier");
    }
    Ok(())
}

fn valid_identifier(name: &str) -> bool {
    let mut characters = name.chars();
    matches!(characters.next(), Some(first) if first == '_' || first.is_ascii_alphabetic())
        && characters.all(|character| character == '_' || character.is_ascii_alphanumeric())
}

fn diagnostic(
    name: String,
    source: SkillSourceAuthority,
    source_path: PathBuf,
    message: impl Into<String>,
) -> SkillDiagnostic {
    SkillDiagnostic {
        name,
        source,
        source_path,
        message: message.into(),
        collision: false,
    }
}

/// Project the closed invocation-scoped hook envelope without reading a skill body.
pub(crate) fn project_skill_hooks(
    header: &str,
    logical_location: &str,
    invocation: SkillInvocationId,
) -> Result<Vec<HookConfigEntry>, String> {
    let location = SkillSourceLocation::parse(logical_location)
        .map_err(|error| format!("invalid skill hook source location: {error}"))?;
    let mut root = yaml_envelope::mapping(header)?;
    let version = root
        .remove("version")
        .ok_or_else(|| "skill hook envelope is missing required integer `version: 1`".to_owned())?;
    match version {
        Node::Scalar { value, plain: true } if value == "1" => {}
        _ => return Err("skill hook envelope `version` must be the integer 1".to_owned()),
    }
    for key in root.keys() {
        if key != "hooks" {
            return Err(format!("unsupported skill hook envelope field `{key}`"));
        }
    }
    let Some(hooks) = root.remove("hooks") else {
        return Ok(Vec::new());
    };
    let Node::Sequence(hooks) = hooks else {
        return Err("skill hook field `hooks` must be a sequence".to_owned());
    };
    hooks
        .into_iter()
        .enumerate()
        .map(|(ordinal, hook)| {
            Ok(project_hook_entry(hook, ordinal)?.bind_skill(location.as_str(), invocation))
        })
        .collect()
}

fn project_hook_entry(node: Node, ordinal: usize) -> Result<HookConfigEntry, String> {
    let mut fields = hook_mapping(node, &format!("hooks[{ordinal}]"))?;
    reject_unknown_hook_fields(&fields, &["event", "matcher", "once", "handler"])?;
    let event = hook_required_string(&mut fields, "event")?;
    let event =
        HookLifecycle::parse(&event).ok_or_else(|| format!("unsupported hook event `{event}`"))?;
    let matcher = hook_optional_string(&mut fields, "matcher")?
        .map(project_hook_matcher)
        .transpose()?;
    let once = hook_optional_bool(&mut fields, "once")?.unwrap_or(false);
    let handler = fields
        .remove("handler")
        .ok_or_else(|| "skill hook is missing required field `handler`".to_owned())?;
    let handler = project_hook_handler(handler)?;
    Ok(HookConfigEntry::new(event, matcher, handler).with_once(once))
}

fn project_hook_matcher(value: String) -> Result<HookMatcher, String> {
    if let Ok(rule) = Rule::load(&value) {
        return Ok(HookMatcher::Permission(rule));
    }
    if value.is_empty()
        || value.len() > 4096
        || value.chars().any(char::is_control)
        || (value.starts_with('/') && value.ends_with('/'))
    {
        return Err(format!("invalid exact hook matcher `{value}`"));
    }
    Ok(HookMatcher::Exact(value))
}

fn project_hook_handler(node: Node) -> Result<HookHandlerConfig, String> {
    let mut fields = hook_mapping(node, "handler")?;
    let handler_type = hook_required_string(&mut fields, "type")?;
    match handler_type.as_str() {
        "command" => {
            reject_unknown_hook_fields(&fields, &["command", "args", "timeout"])?;
            Ok(HookHandlerConfig::Command {
                command: hook_required_string(&mut fields, "command")?,
                args: hook_optional_strings(&mut fields, "args")?.unwrap_or_default(),
                timeout_seconds: hook_optional_timeout(&mut fields)?,
            })
        }
        "http" => {
            reject_unknown_hook_fields(&fields, &["url", "headers", "allowedEnvVars", "timeout"])?;
            Ok(HookHandlerConfig::Http {
                url: hook_required_string(&mut fields, "url")?,
                headers: hook_optional_string_map(&mut fields, "headers")?.unwrap_or_default(),
                allowed_env_vars: hook_optional_strings(&mut fields, "allowedEnvVars")?
                    .unwrap_or_default(),
                timeout_seconds: hook_optional_timeout(&mut fields)?,
            })
        }
        "mcp_tool" => {
            reject_unknown_hook_fields(&fields, &["server", "tool", "timeout"])?;
            Ok(HookHandlerConfig::McpTool {
                server: hook_required_string(&mut fields, "server")?,
                tool: hook_required_string(&mut fields, "tool")?,
                timeout_seconds: hook_optional_timeout(&mut fields)?,
            })
        }
        "prompt" => {
            reject_unknown_hook_fields(&fields, &["timeout"])?;
            Ok(HookHandlerConfig::Prompt {
                timeout_seconds: hook_optional_timeout(&mut fields)?,
            })
        }
        _ => Err(format!("unsupported hook handler `{handler_type}`")),
    }
}

fn hook_mapping(node: Node, field: &str) -> Result<BTreeMap<String, Node>, String> {
    let Node::Mapping(entries) = node else {
        return Err(format!("skill hook field `{field}` must be a mapping"));
    };
    entries
        .into_iter()
        .map(|(key, value)| match key {
            Node::Scalar { value: key, .. } => Ok((key, value)),
            _ => Err(format!("skill hook field `{field}` has a non-string key")),
        })
        .collect()
}

fn reject_unknown_hook_fields(
    fields: &BTreeMap<String, Node>,
    allowed: &[&str],
) -> Result<(), String> {
    if let Some(field) = fields
        .keys()
        .find(|field| !allowed.contains(&field.as_str()))
    {
        return Err(format!("unsupported skill hook field `{field}`"));
    }
    Ok(())
}

fn hook_required_string(
    fields: &mut BTreeMap<String, Node>,
    field: &str,
) -> Result<String, String> {
    hook_optional_string(fields, field)?
        .ok_or_else(|| format!("skill hook is missing required field `{field}`"))
}

fn hook_optional_string(
    fields: &mut BTreeMap<String, Node>,
    field: &str,
) -> Result<Option<String>, String> {
    let Some(value) = fields.remove(field) else {
        return Ok(None);
    };
    yaml_envelope::scalar(value)
        .map(Some)
        .ok_or_else(|| format!("skill hook field `{field}` must be a non-empty string"))
}

fn hook_optional_bool(
    fields: &mut BTreeMap<String, Node>,
    field: &str,
) -> Result<Option<bool>, String> {
    let Some(value) = fields.remove(field) else {
        return Ok(None);
    };
    match value {
        Node::Scalar { value, plain: true } if value == "true" => Ok(Some(true)),
        Node::Scalar { value, plain: true } if value == "false" => Ok(Some(false)),
        _ => Err(format!("skill hook field `{field}` must be a boolean")),
    }
}

fn hook_optional_strings(
    fields: &mut BTreeMap<String, Node>,
    field: &str,
) -> Result<Option<Vec<String>>, String> {
    let Some(value) = fields.remove(field) else {
        return Ok(None);
    };
    let Node::Sequence(values) = value else {
        return Err(format!(
            "skill hook field `{field}` must be a string sequence"
        ));
    };
    values
        .into_iter()
        .map(|value| {
            yaml_envelope::scalar(value)
                .ok_or_else(|| format!("skill hook field `{field}` must contain non-empty strings"))
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

fn hook_optional_string_map(
    fields: &mut BTreeMap<String, Node>,
    field: &str,
) -> Result<Option<Vec<(String, String)>>, String> {
    let Some(value) = fields.remove(field) else {
        return Ok(None);
    };
    let Node::Mapping(values) = value else {
        return Err(format!(
            "skill hook field `{field}` must be a string mapping"
        ));
    };
    values
        .into_iter()
        .map(|(name, value)| {
            let name = yaml_envelope::scalar(name)
                .ok_or_else(|| format!("skill hook field `{field}` has a non-string name"))?;
            let value = yaml_envelope::scalar(value)
                .ok_or_else(|| format!("skill hook field `{field}` has a non-string value"))?;
            Ok((name, value))
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

fn hook_optional_timeout(fields: &mut BTreeMap<String, Node>) -> Result<Option<u64>, String> {
    let Some(value) = fields.remove("timeout") else {
        return Ok(None);
    };
    let Node::Scalar { value, plain: true } = value else {
        return Err("skill hook field `timeout` must be an integer".to_owned());
    };
    let seconds = value
        .parse::<u64>()
        .map_err(|_| "skill hook field `timeout` must be an integer".to_owned())?;
    if !(1..=3600).contains(&seconds) {
        return Err("skill hook field `timeout` must be between 1 and 3600 seconds".to_owned());
    }
    Ok(Some(seconds))
}

#[cfg(test)]
mod tests;

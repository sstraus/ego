//! Read-only process projection of one admitted hook catalogue.

use ego_core::config::Trust;
use ego_core::event::skill::SkillSourceAuthority;
use ego_core::hook::{HookCatalogue, HookHandlerConfig, HookMatcher, HookSource, HookSpec};
use ego_core::run::RunTermination;
use serde::Serialize;

use crate::cli::{HooksArgs, UsageError};
use crate::config;
use crate::output::{Diagnostics, Stdout, empty_listing};

const HOOK_LISTING_VERSION: u8 = 1;

/// What an empty catalogue means, shared by `ego hooks ls` and `/hooks`.
///
/// There is no `ego hooks add`: hooks are a config table by design, so the
/// command that adds one is the config editor.
pub(crate) const EMPTY_HOOKS: &str = "no hooks are admitted";

/// Admit and render the one non-interactive system/user hook snapshot.
pub fn execute(args: &HooksArgs, out: &Stdout) -> RunTermination {
    match execute_inner(args, out) {
        Ok(()) => RunTermination::Done,
        Err(termination) => termination,
    }
}

fn execute_inner(args: &HooksArgs, out: &Stdout) -> Result<(), RunTermination> {
    let layers = config::hook_listing_layers().map_err(usage)?;
    let catalogue = HookCatalogue::admit_typed(&layers, &[], Trust::Untrusted)
        .map_err(|error| usage(UsageError(error.to_string())))?;
    let rendered = render(&catalogue, args.json).map_err(|error| RunTermination::Failed {
        message: format!("the hook catalogue could not be rendered: {error}"),
    })?;
    // An empty text listing has no rows, so it has nothing to put on stdout;
    // the explanation belongs on the diagnostic channel. `--json` already
    // answers with one complete object carrying an empty array.
    if rendered.is_empty() {
        out.note(&empty_listing(EMPTY_HOOKS, "ego config edit"));
    } else {
        out.line(&rendered);
    }
    Ok(())
}

fn usage(error: UsageError) -> RunTermination {
    RunTermination::UsageError {
        message: error.to_string(),
    }
}

/// Project and render exactly one immutable catalogue capture.
pub(crate) fn render(catalogue: &HookCatalogue, json: bool) -> Result<String, serde_json::Error> {
    let listing = HookListing::from_catalogue(catalogue);
    if json {
        serde_json::to_string(&listing)
    } else {
        Ok(listing.render_text())
    }
}

#[derive(Debug, Serialize)]
struct HookListing {
    v: u8,
    generation: String,
    hooks: Vec<HookListingRow>,
}

impl HookListing {
    fn from_catalogue(catalogue: &HookCatalogue) -> Self {
        Self {
            v: HOOK_LISTING_VERSION,
            generation: catalogue.generation().to_owned(),
            hooks: catalogue
                .specs()
                .iter()
                .map(HookListingRow::from_spec)
                .collect(),
        }
    }

    fn render_text(&self) -> String {
        self.hooks
            .iter()
            .map(HookListingRow::render_text)
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[derive(Debug, Serialize)]
struct HookListingRow {
    id: String,
    activation: HookListingActivation,
    lifecycle: &'static str,
    matcher: Option<String>,
    handler_kind: &'static str,
    source_authority: &'static str,
    logical_location: String,
    digest: String,
    ordinal: u32,
    enabled: bool,
    refusal_reason: Option<String>,
}

impl HookListingRow {
    fn from_spec(spec: &HookSpec) -> Self {
        let source = spec.source();
        Self {
            id: spec.id().to_string(),
            activation: HookListingActivation::from_source(&source),
            lifecycle: spec.event_name(),
            matcher: spec.matcher().map(matcher_text),
            handler_kind: handler_kind(spec.handler()),
            source_authority: source_authority(&source),
            logical_location: spec.location().as_str().to_owned(),
            digest: spec.content_digest().to_string(),
            ordinal: spec
                .ordinal()
                .expect("an admitted hook always has a source-local ordinal"),
            enabled: true,
            refusal_reason: None,
        }
    }

    fn render_text(&self) -> String {
        format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            self.id,
            self.activation.text(),
            self.lifecycle,
            self.matcher.as_deref().unwrap_or("-"),
            self.handler_kind,
            self.source_authority,
            self.logical_location,
            self.digest,
            self.ordinal,
            self.enabled,
            self.refusal_reason.as_deref().unwrap_or("-")
        )
    }
}

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum HookListingActivation {
    Persistent,
    Skill { invocation: String },
}

impl HookListingActivation {
    fn from_source(source: &HookSource) -> Self {
        match source {
            HookSource::Skill { invocation, .. } => Self::Skill {
                invocation: invocation.to_string(),
            },
            HookSource::System | HookSource::User | HookSource::Project => Self::Persistent,
        }
    }

    fn text(&self) -> String {
        match self {
            Self::Persistent => "persistent".to_owned(),
            Self::Skill { invocation } => format!("skill:{invocation}"),
        }
    }
}

fn matcher_text(matcher: &HookMatcher) -> String {
    match matcher {
        HookMatcher::Permission(rule) => rule.to_string(),
        HookMatcher::Exact(value) => value.clone(),
    }
}

fn handler_kind(handler: &HookHandlerConfig) -> &'static str {
    match handler {
        HookHandlerConfig::Command { .. } => "command",
        HookHandlerConfig::Http { .. } => "http",
        HookHandlerConfig::McpTool { .. } => "mcp_tool",
        HookHandlerConfig::Prompt { .. } => "prompt",
    }
}

fn source_authority(source: &HookSource) -> &'static str {
    match source {
        HookSource::System => "system",
        HookSource::User => "user",
        HookSource::Project => "project",
        HookSource::Skill { authority, .. } => match authority {
            SkillSourceAuthority::System => "system",
            SkillSourceAuthority::User => "user",
            SkillSourceAuthority::Project => "project",
        },
    }
}

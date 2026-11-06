//! What a project may ask the owner for.
//!
//! A project layer cannot widen its own authority: `roots`, `exec_paths` and
//! `tool(<server>/*)` rules stay owner-only. `[grants]` is the one place a
//! project proposes them. The proposal is part of the executable surface the
//! owner's workspace trust answer covers, so one answer approves exactly this
//! text, a changed proposal asks again, and an untrusted workspace drops the
//! whole layer. Administrator ceilings bind approved grants like any other root.

use std::path::PathBuf;

use serde::Deserialize;

use super::error::ConfigError;
use super::layer::rules;
use super::source::Source;
use crate::permit::RootCfg;
use crate::permit::rule::Rule;

/// The `[grants]` table of a project or local layer.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectGrants {
    /// Extra folders, added to the owner's roots.
    #[serde(default)]
    pub roots: Vec<RootCfg>,
    /// Extra executable and readable folders, added to the owner's.
    #[serde(default)]
    pub exec_paths: Vec<PathBuf>,
    /// MCP tool allow rules, named or `tool(<server>/*)`.
    #[serde(default, deserialize_with = "rules")]
    pub allow: Vec<Rule>,
}

impl ProjectGrants {
    pub(super) fn validate(&self, source: Source) -> Result<(), ConfigError> {
        let invalid = |message: String| ConfigError::Invalid { source, message };
        if !matches!(source, Source::Project | Source::Local) {
            return Err(invalid(
                "`[grants]` is a project proposal; the owner's own layers declare roots, `exec_paths` and permissions directly"
                    .to_owned(),
            ));
        }
        for root in &self.roots {
            if !root.path.is_absolute() && !root.path.starts_with("~") {
                return Err(invalid(
                    "grants roots paths must be absolute or start with ~/".to_owned(),
                ));
            }
            if root.consent.is_some() {
                return Err(invalid(
                    "a grants root cannot carry a consent receipt".to_owned(),
                ));
            }
        }
        if let Some(path) = self.exec_paths.iter().find(|path| !path.is_absolute()) {
            return Err(invalid(format!(
                "grants exec root {} is not absolute",
                path.display()
            )));
        }
        if let Some(rule) = self
            .allow
            .iter()
            .find(|rule| !matches!(rule, Rule::Tool { .. }))
        {
            return Err(invalid(format!(
                "grants may propose only MCP tool rules, not {rule:?}"
            )));
        }
        Ok(())
    }
}

//! Approvals owned by one live session, never persisted as rules: exact commands,
//! each MCP server tool whatever its arguments, and each host of a native GET
//! whatever its path.
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use super::{Op, PreparedCall};

/// Shared owner decisions for fresh policy captures of the same session.
/// Only the engine can record an approval, after the human answers Allow.
#[derive(Clone, Default)]
pub struct SessionApprovals(
    Arc<Mutex<HashSet<[u8; 32]>>>,
    Arc<Mutex<Vec<String>>>,
    Arc<Mutex<HashMap<&'static str, usize>>>,
);

/// Most recent human answers kept for the judge.
const PRECEDENTS_KEPT: usize = 50;

/// Approvals of one program family after which ego offers to allow the whole family.
const FAMILY_OFFER_AFTER: usize = 2;

/// Programs whose commands the owner can sensibly allow as a family. Excluded: shells and
/// interpreters (they wrap anything) and tools whose subcommands publish or destroy
/// (`git`, `gh`, `docker`, `kubectl`).
const FAMILY_PROGRAMS: &[&str] = &["cargo", "npm", "pnpm", "yarn", "go"];

impl SessionApprovals {
    fn command(call: &PreparedCall) -> bool {
        !call.ops().is_empty() && call.ops().iter().all(|op| matches!(op, Op::Spawn { .. }))
    }

    /// The key a call is remembered under: its exact fingerprint for a command,
    /// its argument-free tool identity for an MCP tool call.
    fn key(call: &PreparedCall) -> Option<[u8; 32]> {
        if Self::command(call) {
            Some(call.authority_fingerprint())
        } else {
            call.mcp_tool_fingerprint()
                .or_else(|| call.web_host_fingerprint())
        }
    }

    /// The one program every command of `call` runs, when it is an offerable family.
    fn family(call: &PreparedCall) -> Option<&'static str> {
        if !Self::command(call) {
            return None;
        }
        let mut found: Option<&'static str> = None;
        for op in call.ops() {
            let Op::Spawn { spec } = op else {
                return None;
            };
            let commands = super::shell::static_commands(spec.argv()).or_else(|| {
                spec.argv()
                    .iter()
                    .map(|word| word.to_str().map(str::to_owned))
                    .collect::<Option<Vec<_>>>()
                    .map(|argv| vec![argv])
            })?;
            for command in commands {
                let program = command.first()?.rsplit('/').next()?;
                let program = FAMILY_PROGRAMS
                    .iter()
                    .copied()
                    .find(|name| *name == program)?;
                if found.is_some_and(|existing| existing != program) {
                    return None;
                }
                found = Some(program);
            }
        }
        found
    }

    /// The family to offer for `call`: approved at least [`FAMILY_OFFER_AFTER`] times already.
    pub(super) fn family_offer(&self, call: &PreparedCall) -> Option<&'static str> {
        let family = Self::family(call)?;
        let count = *self
            .2
            .lock()
            .expect("session families poisoned")
            .get(family)
            .unwrap_or(&0);
        (count >= FAMILY_OFFER_AFTER).then_some(family)
    }

    pub(super) fn contains(&self, call: &PreparedCall) -> bool {
        Self::key(call).is_some_and(|key| {
            self.0
                .lock()
                .expect("session approvals poisoned")
                .contains(&key)
        })
    }

    /// Remember what the human answered, so the judge never re-litigates it.
    pub(super) fn record_answer(&self, call: &PreparedCall, allowed: bool, always: bool) {
        let what = call
            .ops()
            .iter()
            .map(|op| match op {
                Op::Spawn { spec } => {
                    let argv: Vec<String> = spec
                        .argv()
                        .iter()
                        .map(|word| word.to_string_lossy().into_owned())
                        .collect();
                    format!("run {}", super::shell::observe(&argv).skeleton)
                }
                other => format!(
                    "{} {}",
                    other.name(),
                    other.subject(&super::CanonPath::new("/"))
                ),
            })
            .collect::<Vec<_>>()
            .join("; ");
        let answer = match (allowed, always) {
            (true, true) => "the user allowed for the session",
            (true, false) => "the user allowed once",
            (false, _) => "the user refused",
        };
        if allowed && let Some(family) = Self::family(call) {
            *self
                .2
                .lock()
                .expect("session families poisoned")
                .entry(family)
                .or_insert(0) += 1;
        }
        let mut precedents = self.1.lock().expect("session precedents poisoned");
        if precedents.len() == PRECEDENTS_KEPT {
            precedents.remove(0);
        }
        precedents.push(format!("{answer}: {what}"));
    }

    pub(crate) fn precedents(&self) -> Vec<String> {
        self.1.lock().expect("session precedents poisoned").clone()
    }

    pub(super) fn record(&self, call: &PreparedCall) {
        if let Some(key) = Self::key(call) {
            self.0
                .lock()
                .expect("session approvals poisoned")
                .insert(key);
        }
    }
}

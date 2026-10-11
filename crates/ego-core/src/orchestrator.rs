//! Orchestration contracts; the OS adapter owns external effects.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Operator-selected orchestration authority; observation never mutates peers.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    Observe,
    Active,
}

/// Configuration uses the existing layer/profile precedence.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub mode: Mode,
}

/// One durable spawn receipt in the existing tool-completion session row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpawnReceipt {
    pub session_id: String,
    pub name: String,
    pub cwd: std::path::PathBuf,
    pub prompt: String,
    pub spawned_at_ms: u64,
}

/// Native active requests have no shell interpolation or arbitrary argv.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum ActiveRequest {
    Spawn {
        role: String,
        name: String,
        cwd: String,
        brief: String,
    },
    Say {
        session_id: String,
        message: String,
    },
    Consult {
        role: String,
        name: String,
        cwd: String,
        brief: String,
    },
    WorktreeCreate {
        path: String,
        branch: String,
    },
    Land {
        path: String,
        branch: String,
        message: String,
    },
}

/// Limit from the coordinator plan's model-context budget, measured in bytes.
pub const MAX_CHILD_LINE_BYTES: usize = 500;

/// An optional workspace subtree selection.
#[derive(Debug, Default, Clone)]
pub struct Filter {
    pub root: Option<std::path::PathBuf>,
}

/// A single bounded, human-readable child row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildLine(String);

impl ChildLine {
    pub fn new(id: &str, name: &str, state: &str, cwd: &str) -> Self {
        let text = format!("{id} {name} [{state}] {cwd}");
        Self(bounded_line(&text))
    }

    pub fn text(&self) -> &str {
        &self.0
    }
}

/// Current tracked children and unread result-file paths, without mutation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Brief {
    pub children: Vec<ChildLine>,
    pub unread_results: Vec<String>,
}

/// Keep one terminal-safe row inside the plan's UTF-8 byte budget.
pub fn bounded_line(text: &str) -> String {
    let mut text: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if text.len() > MAX_CHILD_LINE_BYTES {
        let end = text.floor_char_boundary(MAX_CHILD_LINE_BYTES - "…".len());
        text.truncate(end);
        text.push('…');
    }
    text
}

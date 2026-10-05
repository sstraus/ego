//! Read-only orchestration projections; the OS adapter owns their acquisition.

use futures::future::BoxFuture;

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

/// The coordinator's read-only external effect boundary.
pub trait Orchestrator: Send + Sync {
    fn status<'a>(&'a self, filter: &'a Filter) -> BoxFuture<'a, Result<Vec<ChildLine>, String>>;
    fn brief<'a>(&'a self, filter: &'a Filter) -> BoxFuture<'a, Result<Brief, String>>;
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

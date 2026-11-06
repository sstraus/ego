//! Opt-in language-server code intelligence: the `lsp` tool.
//!
//! Off unless the owner turns it on (`lsp = true` in user configuration or
//! the `lsp` row of `ego setup`). When on, the model can ask a language
//! server for the definition of a symbol, its references, or the diagnostics
//! of one file. See `CLI.md` "Language-server code intelligence".
//!
//! Pure here: the request contract and the bounded answer text. The servers
//! run behind [`CodeIntel`], whose only real implementation lives in
//! `ego-tools` and starts each server through the run's guarded
//! [`DuplexSpawner`], so a server runs under the same confinement as an MCP
//! stdio server or a command.

use std::sync::Arc;

use futures::future::BoxFuture;
use serde::Deserialize;

use crate::permit::DuplexSpawner;

/// The tool name the model sees.
pub const TOOL_NAME: &str = "lsp";

/// The most locations or diagnostics one answer lists.
pub const MAX_ITEMS: usize = 50;

/// The largest answer text, in bytes.
pub const MAX_BYTES: usize = 16 * 1024;

/// The longest source line or diagnostic message shown, in characters.
pub const MAX_LINE_CHARS: usize = 200;

/// What the model asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    Definition,
    References,
    Diagnostics,
}

/// A position in a file: 1-based line, 1-based column counted in characters
/// (Unicode scalar values), the way the `read` tool shows a line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Position {
    pub line: u32,
    pub column: u32,
}

/// One validated `lsp` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeIntelRequest {
    pub operation: Operation,
    /// As the model wrote it: relative to the workspace, or absolute.
    pub path: String,
    /// Present for definition and references; absent for diagnostics.
    pub position: Option<Position>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Wire {
    operation: Operation,
    path: String,
    line: Option<u32>,
    column: Option<u32>,
}

impl CodeIntelRequest {
    pub fn parse(value: serde_json::Value) -> Result<Self, String> {
        let wire: Wire = serde_json::from_value(value)
            .map_err(|error| format!("lsp was called with invalid arguments: {error}"))?;
        if wire.path.trim().is_empty() {
            return Err("lsp needs a file path".into());
        }
        let position = match (wire.operation, wire.line, wire.column) {
            (Operation::Diagnostics, None, None) => None,
            (Operation::Diagnostics, _, _) => {
                return Err("diagnostics take a file only; omit line and column".into());
            }
            (_, Some(line), Some(column)) if line >= 1 && column >= 1 => {
                Some(Position { line, column })
            }
            (_, Some(_), Some(_)) => return Err("line and column start at 1".into()),
            _ => return Err("definition and references need line and column".into()),
        };
        Ok(Self {
            operation: wire.operation,
            path: wire.path,
            position,
        })
    }
}

/// One place a server named, already in the model's units.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    /// Relative to the workspace when inside it, else absolute.
    pub path: String,
    pub line: u32,
    pub column: u32,
    /// The source line, only for a file inside the workspace.
    pub text: Option<String>,
}

/// One diagnostic of the asked file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub line: u32,
    pub column: u32,
    /// `error`, `warning`, `information` or `hint` when the server says.
    pub severity: Option<&'static str>,
    pub source: Option<String>,
    pub code: Option<String>,
    pub message: String,
}

/// What a server answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    Locations(Vec<Location>),
    /// `published` is false when the server sent no diagnostics for the file
    /// before the wait ended, which is not the same as "no problems".
    Diagnostics {
        published: bool,
        items: Vec<Diagnostic>,
    },
}

/// The language servers of one session.
pub trait CodeIntel: Send + Sync {
    /// Answer `request`, starting the server for the file's language through
    /// `spawner` when it is not running yet. The error is the text the model
    /// sees.
    fn query(
        &self,
        request: CodeIntelRequest,
        spawner: Arc<dyn DuplexSpawner>,
    ) -> BoxFuture<'static, Result<Answer, String>>;
}

/// The answer text, bounded by [`MAX_ITEMS`] and [`MAX_BYTES`].
pub fn render(request: &CodeIntelRequest, answer: &Answer) -> String {
    let (header, lines): (String, Vec<String>) = match answer {
        Answer::Locations(locations) => {
            let what = match request.operation {
                Operation::Definition => "definition",
                Operation::References => "references",
                Operation::Diagnostics => "locations",
            };
            let at = request
                .position
                .map(|at| format!("{}:{}:{}", request.path, at.line, at.column))
                .unwrap_or_else(|| request.path.clone());
            if locations.is_empty() {
                return format!(
                    "no {what} found for {at}. A server that is still indexing can answer nothing; try again later or use read and exec."
                );
            }
            (
                format!("{what} of {at}: {}", count(locations.len(), "location")),
                locations
                    .iter()
                    .map(|location| {
                        let place =
                            format!("{}:{}:{}", location.path, location.line, location.column);
                        match &location.text {
                            Some(text) => format!("{place}: {}", cut(text.trim())),
                            None => place,
                        }
                    })
                    .collect(),
            )
        }
        Answer::Diagnostics { published, items } => {
            if !published {
                return format!(
                    "the language server published no diagnostics for {} before the wait ended; this does not mean the file has no problems",
                    request.path
                );
            }
            if items.is_empty() {
                return format!("{}: no diagnostics", request.path);
            }
            (
                format!("{}: {}", request.path, count(items.len(), "diagnostic")),
                items
                    .iter()
                    .map(|item| {
                        let mut line = format!("{}:{}", item.line, item.column);
                        if let Some(severity) = item.severity {
                            line.push(' ');
                            line.push_str(severity);
                        }
                        let tag = match (&item.source, &item.code) {
                            (Some(source), Some(code)) => Some(format!("{source} {code}")),
                            (Some(one), None) | (None, Some(one)) => Some(one.clone()),
                            (None, None) => None,
                        };
                        if let Some(tag) = tag {
                            line.push_str(&format!(" [{tag}]"));
                        }
                        line.push_str(": ");
                        line.push_str(&cut(&item
                            .message
                            .split_whitespace()
                            .collect::<Vec<_>>()
                            .join(" ")));
                        line
                    })
                    .collect(),
            )
        }
    };
    let mut text = header;
    let mut shown = 0;
    for line in &lines {
        // Leave room for the omission line.
        if shown == MAX_ITEMS || text.len() + 1 + line.len() > MAX_BYTES - 64 {
            break;
        }
        text.push('\n');
        text.push_str(line);
        shown += 1;
    }
    if shown < lines.len() {
        text.push_str(&format!("\n{} more omitted", lines.len() - shown));
    }
    text
}

fn count(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("1 {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

fn cut(text: &str) -> String {
    match text.char_indices().nth(MAX_LINE_CHARS) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_owned(),
    }
}

pub fn descriptor() -> crate::generator::types::ToolDescriptor {
    crate::generator::types::ToolDescriptor {
        name: TOOL_NAME.into(),
        description: format!(
            "Ask the language server of a source file. definition and references take the 1-based line and column (in characters) of a symbol and list where it is defined or used, with the source line; diagnostics lists the errors and warnings of one file. Prefer this to searching text when you look for a symbol. At most {MAX_ITEMS} entries are listed."
        ),
        parameters: serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["operation", "path"],
            "properties": {
                "operation": {"type": "string", "enum": ["definition", "references", "diagnostics"]},
                "path": {"type": "string", "minLength": 1, "description": "A file in the workspace, relative to it or absolute."},
                "line": {"type": "integer", "minimum": 1, "description": "definition and references: the 1-based line of the symbol."},
                "column": {"type": "integer", "minimum": 1, "description": "definition and references: the 1-based column of the symbol, in characters."}
            }
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn references() -> CodeIntelRequest {
        CodeIntelRequest {
            operation: Operation::References,
            path: "src/lib.rs".into(),
            position: Some(Position { line: 4, column: 9 }),
        }
    }

    fn location(index: usize, text: String) -> Location {
        Location {
            path: format!("src/file_{index}.rs"),
            line: index as u32 + 1,
            column: 1,
            text: Some(text),
        }
    }

    // Catches: a references answer for a widely used symbol filling the
    // context with every use, past the documented entry limit.
    #[test]
    fn a_long_answer_lists_at_most_the_item_limit_and_counts_the_rest() {
        let locations = (0..120)
            .map(|index| location(index, "use".into()))
            .collect();
        let text = render(&references(), &Answer::Locations(locations));
        assert_eq!(text.lines().count(), 1 + MAX_ITEMS + 1, "{text}");
        assert!(text.ends_with("\n70 more omitted"), "{text}");
    }

    // Catches: the byte limit applied only to the item count, so 50 long
    // lines still make an answer far over the documented size.
    #[test]
    fn long_source_lines_are_cut_and_the_answer_stays_under_the_byte_limit() {
        let long = "x".repeat(10_000);
        let locations = (0..MAX_ITEMS)
            .map(|index| location(index, long.clone()))
            .collect();
        let text = render(&references(), &Answer::Locations(locations));
        assert!(text.len() <= MAX_BYTES, "{} bytes", text.len());
        let first = text.lines().nth(1).unwrap();
        assert!(first.chars().count() < MAX_LINE_CHARS + 40, "{first}");
        assert!(first.ends_with('…'), "{first}");
    }

    // Catches: a server that never published diagnostics reported as a clean
    // file, so the model trusts code that does not build.
    #[test]
    fn unpublished_diagnostics_are_not_reported_as_a_clean_file() {
        let request = CodeIntelRequest {
            operation: Operation::Diagnostics,
            path: "main.go".into(),
            position: None,
        };
        let silent = render(
            &request,
            &Answer::Diagnostics {
                published: false,
                items: vec![],
            },
        );
        assert!(
            silent.contains("does not mean the file has no problems"),
            "{silent}"
        );
        let clean = render(
            &request,
            &Answer::Diagnostics {
                published: true,
                items: vec![],
            },
        );
        assert_eq!(clean, "main.go: no diagnostics");
    }

    // Catches: definition sent to a server without a position, or a
    // 0-based column taken as the documented 1-based one.
    #[test]
    fn positions_are_required_and_one_based() {
        let parse = |value| CodeIntelRequest::parse(value);
        assert!(parse(serde_json::json!({"operation": "definition", "path": "a.rs"})).is_err());
        assert!(
            parse(serde_json::json!({"operation": "definition", "path": "a.rs", "line": 0, "column": 1}))
                .is_err()
        );
        assert!(
            parse(serde_json::json!({"operation": "diagnostics", "path": "a.rs", "line": 1, "column": 1}))
                .is_err()
        );
        assert_eq!(
            parse(serde_json::json!({"operation": "references", "path": "a.rs", "line": 3, "column": 7}))
                .unwrap()
                .position,
            Some(Position { line: 3, column: 7 })
        );
    }
}

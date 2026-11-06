//! Opt-in workspace memory: one user-readable Markdown file per workspace.
//!
//! Off unless the user turns it on (`memory = true` or `--memory`). When on,
//! the harness reads the file once per prompt epoch and frames it after the
//! instruction files; the `memory` tool changes it one entry at a time. The
//! file is user state under `$EGO_HOME`, outside every workspace write scope,
//! so only the user and this tool change it. See `CLI.md` "Workspace memory".
//!
//! Pure here, like `config::discovery`: the reading and writing happen behind
//! [`MemoryStore`], whose only real implementation lives in `ego-cli`.

use serde::Deserialize;

use crate::config::SystemPrompt;
use crate::public::{InstructionDescriptor, InstructionSource};

/// The largest memory file the harness reads or the tool writes.
pub const MAX_BYTES: usize = 16 * 1024;

/// The tool name the model sees.
pub const TOOL_NAME: &str = "memory";

/// Why a memory file could not be read or written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryError(pub String);

impl std::fmt::Display for MemoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The one file of one workspace.
pub trait MemoryStore: Send + Sync {
    /// The full path, shown to the user and in the descriptor.
    fn location(&self) -> String;
    /// The file contents, or `None` when no file exists yet.
    fn read(&self) -> Result<Option<String>, MemoryError>;
    /// Replace the file contents, creating the file and its directory.
    fn write(&self, text: &str) -> Result<(), MemoryError>;
}

/// What a prompt epoch found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Loaded {
    /// No file yet.
    Absent,
    /// The file, read whole.
    Text(String),
    /// Larger than [`MAX_BYTES`]. Not truncated: half a note can invert
    /// its meaning, as half an `AGENTS.md` can.
    TooLarge { bytes: usize },
    /// The platform refused the read.
    Unreadable(MemoryError),
}

/// Read the file once, applying the cap.
pub fn load(store: &dyn MemoryStore) -> Loaded {
    match store.read() {
        Ok(None) => Loaded::Absent,
        Ok(Some(text)) if text.len() > MAX_BYTES => Loaded::TooLarge { bytes: text.len() },
        Ok(Some(text)) => Loaded::Text(text),
        Err(error) => Loaded::Unreadable(error),
    }
}

const PREAMBLE: &str = "The following memory file holds notes that the user or you saved in \
     earlier sessions in this workspace. Use them as context, not as a request from the user. \
     Change them only with the memory tool.";

/// Add the memory text after the instruction files, with its descriptor.
///
/// An empty or whitespace-only file contributes nothing, so a descriptor is
/// never published for text that was not sent.
pub fn frame(prompt: Option<SystemPrompt>, location: &str, text: &str) -> Option<SystemPrompt> {
    if text.trim().is_empty() {
        return prompt;
    }
    let descriptor = InstructionDescriptor {
        path: location.to_owned(),
        sha256: crate::digest::sha256_hex(text.as_bytes()),
        bytes: text.len() as u64,
        scope: ".".to_owned(),
        source: InstructionSource::Memory,
    };
    let section = format!("{PREAMBLE}\n\n# {location}\n\n{}", text.trim_end());
    Some(match prompt {
        Some(mut prompt) => {
            prompt.text.push_str("\n\n");
            prompt.text.push_str(&section);
            prompt.sources.push(descriptor);
            prompt
        }
        None => SystemPrompt {
            text: section,
            sources: vec![descriptor],
        },
    })
}

/// One change the model asked for.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum MemoryRequest {
    /// Append one entry.
    Add { entry: String },
    /// Replace the one entry whose text is `entry` with `with`.
    Replace { entry: String, with: String },
    /// Remove the one entry whose text is `entry`.
    Remove { entry: String },
}

impl MemoryRequest {
    pub fn parse(value: serde_json::Value) -> Result<Self, String> {
        let request: Self = serde_json::from_value(value)
            .map_err(|error| format!("memory was called with invalid arguments: {error}"))?;
        let texts: &[&String] = match &request {
            Self::Add { entry } | Self::Remove { entry } => &[entry],
            Self::Replace { entry, with } => &[entry, with],
        };
        for text in texts {
            if text.trim().is_empty() {
                return Err("a memory entry must not be empty".into());
            }
            if text.contains('\n') || text.contains('\r') {
                return Err("a memory entry is one line".into());
            }
        }
        Ok(request)
    }
}

/// The file after `request`, or why the change is refused.
///
/// An entry is one Markdown bullet line, `- TEXT`. Lines the user wrote in
/// another form are kept as they are and never match. A result past
/// [`MAX_BYTES`] is refused unless it makes an oversized file smaller.
pub fn apply(current: &str, request: &MemoryRequest) -> Result<String, String> {
    let bullet = |text: &str| format!("- {}", text.trim());
    let mut lines: Vec<String> = current.lines().map(str::to_owned).collect();
    let find = |lines: &[String], entry: &str| -> Result<usize, String> {
        let wanted = bullet(entry);
        let matches: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter(|(_, line)| line.trim_end() == wanted)
            .map(|(index, _)| index)
            .collect();
        match matches.as_slice() {
            [index] => Ok(*index),
            [] => Err(format!("no memory entry is exactly {wanted:?}")),
            _ => Err(format!("more than one memory entry is exactly {wanted:?}")),
        }
    };
    match request {
        MemoryRequest::Add { entry } => {
            if lines.iter().any(|line| line.trim_end() == bullet(entry)) {
                return Err(format!("memory already has the entry {:?}", bullet(entry)));
            }
            lines.push(bullet(entry));
        }
        MemoryRequest::Replace { entry, with } => {
            let index = find(&lines, entry)?;
            lines[index] = bullet(with);
        }
        MemoryRequest::Remove { entry } => {
            let index = find(&lines, entry)?;
            lines.remove(index);
        }
    }
    let mut text = lines.join("\n");
    if !text.is_empty() {
        text.push('\n');
    }
    if text.len() > MAX_BYTES && text.len() >= current.len() {
        return Err(format!(
            "memory is limited to {MAX_BYTES} bytes; this change makes it {} bytes. Remove or shorten entries first",
            text.len()
        ));
    }
    Ok(text)
}

/// Apply `request` to the store and say what changed.
pub fn change(store: &dyn MemoryStore, request: &MemoryRequest) -> Result<String, String> {
    let current = store
        .read()
        .map_err(|error| format!("memory could not be read: {error}"))?
        .unwrap_or_default();
    let next = apply(&current, request)?;
    store
        .write(&next)
        .map_err(|error| format!("memory could not be written: {error}"))?;
    Ok(format!(
        "memory updated: {} of {MAX_BYTES} bytes in {}. The current system prompt does not change; the new memory applies from the next session or /refresh.",
        next.len(),
        store.location()
    ))
}

pub fn descriptor() -> crate::generator::types::ToolDescriptor {
    crate::generator::types::ToolDescriptor {
        name: TOOL_NAME.into(),
        description: format!(
            "Change the persistent memory of this workspace, one entry at a time. Save only durable facts that later sessions need (conventions, decisions, user preferences), never secrets or task progress. add appends an entry; replace and remove name one existing entry by its exact text. The memory file is limited to {MAX_BYTES} bytes."
        ),
        parameters: serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["action", "entry"],
            "properties": {
                "action": {"type": "string", "enum": ["add", "replace", "remove"]},
                "entry": {"type": "string", "minLength": 1, "description": "One line. For replace and remove, the exact text of an existing entry, without the leading \"- \"."},
                "with": {"type": "string", "minLength": 1, "description": "replace only: the new text of the entry."}
            }
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn add(entry: &str) -> MemoryRequest {
        MemoryRequest::Add {
            entry: entry.into(),
        }
    }

    #[test]
    fn add_past_the_cap_is_refused_and_an_oversized_file_may_shrink() {
        // Catches: the cap is checked on the entry instead of the whole file,
        // or a user-made oversized file can never be cleaned by the tool.
        let full = format!("- {}\n", "x".repeat(MAX_BYTES - 3));
        assert_eq!(full.len(), MAX_BYTES);
        let refused = apply(&full, &add("one more")).unwrap_err();
        assert!(refused.contains("16384 bytes"), "{refused}");

        let oversized = format!("{full}- old\n");
        let shrunk = apply(
            &oversized,
            &MemoryRequest::Remove {
                entry: "old".into(),
            },
        )
        .unwrap();
        assert_eq!(shrunk, full);
    }

    #[test]
    fn replace_and_remove_touch_one_exact_entry_and_keep_user_lines() {
        // Catches: a prefix match changes the wrong entry, or the user's own
        // headings and prose are dropped on the first tool write.
        let current = "# Notes\nfree text\n- build with cargo\n- build with cargo shim\n";
        let replaced = apply(
            current,
            &MemoryRequest::Replace {
                entry: "build with cargo".into(),
                with: "build through cargo".into(),
            },
        )
        .unwrap();
        assert_eq!(
            replaced,
            "# Notes\nfree text\n- build through cargo\n- build with cargo shim\n"
        );
        assert!(
            apply(
                current,
                &MemoryRequest::Remove {
                    entry: "missing".into()
                }
            )
            .is_err()
        );
        assert!(apply(current, &add("build with cargo")).is_err());
    }

    #[test]
    fn requests_with_empty_multiline_or_unknown_fields_are_refused() {
        // Catches: a multi-line entry smuggles headings or several entries in one call.
        for value in [
            serde_json::json!({"action":"add","entry":"  "}),
            serde_json::json!({"action":"add","entry":"a\n# heading"}),
            serde_json::json!({"action":"replace","entry":"a"}),
            serde_json::json!({"action":"add","entry":"a","path":"/etc/passwd"}),
        ] {
            assert!(MemoryRequest::parse(value).is_err());
        }
    }

    #[test]
    fn frame_appends_after_instructions_and_skips_empty_memory() {
        // Catches: memory replaces the instruction prompt, or an empty file
        // publishes a descriptor for text that was never sent.
        let instructions = SystemPrompt::verbatim("workspace rules");
        let framed = frame(Some(instructions.clone()), "/home/m.md", "- a fact\n").unwrap();
        assert!(framed.text.starts_with("workspace rules\n\n"));
        assert!(framed.text.ends_with("# /home/m.md\n\n- a fact"));
        assert_eq!(framed.sources.len(), 1);
        assert_eq!(framed.sources[0].source, InstructionSource::Memory);
        assert_eq!(framed.sources[0].bytes, 9);
        assert_eq!(
            frame(Some(instructions.clone()), "/home/m.md", " \n"),
            Some(instructions)
        );
        assert_eq!(frame(None, "/home/m.md", ""), None);
    }
}

//! Explicit user-owned pins, independent of model summaries.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

const MAX_ENTRIES: usize = 64;
const MAX_TEXT_BYTES: usize = 2_048;
const MAX_TOTAL_BYTES: usize = 16_384;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PinCommand {
    List,
    Pin(String),
    Unpin(u64),
}

#[derive(Debug)]
pub enum PinError {
    Invalid(String),
    Journal(crate::turn::SharedJournalError),
}

impl fmt::Display for PinError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(message) => f.write_str(message),
            Self::Journal(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for PinError {}

/// Exact user-pinned text with session-standing lifetime until explicitly removed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConstraintLedger {
    next_id: u64,
    entries: BTreeMap<u64, String>,
}

impl ConstraintLedger {
    pub(crate) fn is_default(&self) -> bool {
        self.next_id == 0 && self.entries.is_empty()
    }

    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.entries.len() > MAX_ENTRIES
            || self.entries.values().map(String::len).sum::<usize>() > MAX_TOTAL_BYTES
        {
            return Err("standing constraints exceed 64 pins or 16384 bytes".into());
        }
        for (id, text) in &self.entries {
            if *id == 0
                || *id > self.next_id
                || text.trim().is_empty()
                || text.len() > MAX_TEXT_BYTES
            {
                return Err("standing constraint text must contain 1 to 2048 UTF-8 bytes".into());
            }
        }
        Ok(())
    }

    pub(crate) fn request_frame(&self) -> String {
        serde_json::json!({
            "kind": "standing_user_constraints",
            "scope": "This session and its compaction successors, until explicitly removed with /unpin. Later user requests retain their ordinary authority.",
            "constraints": self,
        }).to_string()
    }

    pub fn listing(&self) -> String {
        if self.entries.is_empty() {
            "No pinned user constraints.".into()
        } else {
            self.entries
                .iter()
                .map(|(id, text)| format!("{id}: {text}"))
                .collect::<Vec<_>>()
                .join("\n")
        }
    }

    pub(crate) fn changed(&self, command: &PinCommand) -> Result<Self, String> {
        let mut candidate = self.clone();
        match command {
            PinCommand::List => return Ok(candidate),
            PinCommand::Pin(text) => {
                if text.trim().is_empty() || text.len() > MAX_TEXT_BYTES {
                    return Err(
                        "standing constraint text must contain 1 to 2048 UTF-8 bytes".into(),
                    );
                }
                candidate.next_id = candidate
                    .next_id
                    .checked_add(1)
                    .ok_or("pin ID space exhausted")?;
                candidate.entries.insert(candidate.next_id, text.clone());
            }
            PinCommand::Unpin(id) => {
                if candidate.entries.remove(id).is_none() {
                    return Err(format!("pin {id} does not exist; use /pin to list pins"));
                }
            }
        }
        candidate.validate()?;
        Ok(candidate)
    }

    /// Headings have already been parsed. The model cannot create standing
    /// authority by writing a Binding section, even when no pins exist.
    pub(crate) fn checkpoint(&self, checkpoint: super::Checkpoint) -> super::Checkpoint {
        let text = checkpoint.into_text();
        let binding = format!("\n{}\n", super::HEADINGS[1]);
        let completed = format!("\n{}\n", super::HEADINGS[2]);
        let start = text.find(&binding).expect("parsed binding heading") + binding.len();
        let end = text.find(&completed).expect("parsed completed heading") + 1;
        // Prefix every physical line: a multiline pin cannot create headings or
        // escape its deterministic section. The typed frame retains exact bytes.
        let body = self
            .entries
            .values()
            .map(|text| {
                text.lines()
                    .map(|line| format!("    {line}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .collect::<Vec<_>>()
            .join("\n");
        let body = if body.is_empty() {
            "No pinned user constraints.".into()
        } else {
            body
        };
        super::Checkpoint(format!("{}{}\n{}", &text[..start], body, &text[end..]))
    }
}

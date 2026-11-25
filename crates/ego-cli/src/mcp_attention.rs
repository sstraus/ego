//! Show the "MCP server needs attention" startup note once per failing server.
//!
//! An unused server that stays broken would otherwise repeat the same note at every
//! launch. The ledger remembers each diagnostic line the last launch showed, so a
//! server that fails for a new reason is announced again, and a server that
//! recovered is forgotten: a later failure with the same cause is announced again.

use std::collections::BTreeSet;
use std::path::PathBuf;

const LEDGER_FILE: &str = "mcp-attention-seen";

pub(crate) struct AttentionLedger {
    path: Option<PathBuf>,
}

impl AttentionLedger {
    pub(crate) fn in_home(home: Option<PathBuf>) -> Self {
        Self {
            path: home.map(|home| home.join(LEDGER_FILE)),
        }
    }

    /// The diagnostics not yet shown, recorded as shown. A ledger that cannot be
    /// read or written reports everything: a repeated note beats a hidden failure.
    pub(crate) fn unseen(&self, errors: &[String]) -> Vec<String> {
        let Some(path) = &self.path else {
            return errors.to_vec();
        };
        let seen: BTreeSet<String> = std::fs::read_to_string(path)
            .map(|text| text.lines().map(str::to_owned).collect())
            .unwrap_or_default();
        let fresh: Vec<String> = errors
            .iter()
            .filter(|error| !seen.contains(error.as_str()))
            .cloned()
            .collect();
        // The ledger is exactly the diagnostics of this launch, so a diagnostic
        // that disappears is announced again when it returns.
        let current: BTreeSet<String> = errors.iter().cloned().collect();
        if current != seen {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(path, current.into_iter().collect::<Vec<_>>().join("\n"));
        }
        fresh
    }
}

/// The server name inside one `MCP server 'NAME': cause` diagnostic.
pub(crate) fn server_name(error: &str) -> Option<&str> {
    let rest = error.strip_prefix("MCP server '")?;
    rest.split_once('\'').map(|(name, _)| name)
}

/// Count affected servers, rather than individual errors, in stable name order.
pub(crate) fn startup_notice(errors: &[String], width: usize) -> String {
    let names = errors
        .iter()
        .map(|error| server_name(error).unwrap_or("unidentified"))
        .collect::<BTreeSet<_>>();
    let mut preview = names.iter().take(3).copied().collect::<Vec<_>>().join(", ");
    if names.len() > 3 {
        preview.push_str(&format!(" · +{} more", names.len() - 3));
    }
    let grammar = if names.len() == 1 {
        "server needs"
    } else {
        "servers need"
    };
    crate::run::startup_layout::row(
        "! mcp",
        &format!("{} {grammar} attention: {preview} · /mcp", names.len()),
        width,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches: the note repeating at every launch for an unused broken server.
    #[test]
    fn a_diagnostic_is_shown_once_across_launches() {
        let home = tempfile::tempdir().unwrap();
        let errors = vec!["MCP server 'a': offline".to_owned()];
        let first = AttentionLedger::in_home(Some(home.path().to_path_buf()));
        assert_eq!(first.unseen(&errors), errors);
        let second = AttentionLedger::in_home(Some(home.path().to_path_buf()));
        assert!(second.unseen(&errors).is_empty());
        let changed = vec!["MCP server 'a': invalid".to_owned()];
        assert_eq!(second.unseen(&changed), changed);
    }

    /// Catches: a server that recovered and failed again with the same cause
    /// staying hidden forever.
    #[test]
    fn a_recovered_server_is_announced_again_when_it_fails_again() {
        let home = tempfile::tempdir().unwrap();
        let errors = vec!["MCP server 'a': offline".to_owned()];
        let ledger = AttentionLedger::in_home(Some(home.path().to_path_buf()));
        assert_eq!(ledger.unseen(&errors), errors);
        assert!(ledger.unseen(&[]).is_empty());
        assert_eq!(ledger.unseen(&errors), errors);
    }

    // Catches: duplicate errors inflate server totals or overflow the three-name preview.
    #[test]
    fn startup_notice_deduplicates_and_bounds_server_names() {
        let errors = [
            "MCP server 'd': offline",
            "MCP server 'a': invalid",
            "MCP server 'b': offline",
            "MCP server 'a': timeout",
            "MCP server 'c': offline",
        ]
        .map(str::to_owned);
        for width in [120, 80, 60, 40] {
            let shown = startup_notice(&errors, width);
            let prose = shown.split_whitespace().collect::<Vec<_>>().join(" ");
            assert!(
                prose.contains("4 servers need attention: a, b, c · +1 more · /mcp"),
                "{shown}"
            );
            assert!(
                shown
                    .lines()
                    .all(|line| crate::render::Line::plain(line).width() <= width)
            );
            assert!(!shown.contains("offline") && !shown.contains("timeout"));
        }
    }

    #[test]
    fn name_is_read_from_the_diagnostic() {
        assert_eq!(server_name("MCP server 'fx': offline"), Some("fx"));
        assert_eq!(server_name("something else"), None);
    }
}

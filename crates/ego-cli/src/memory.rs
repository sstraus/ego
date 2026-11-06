//! The file behind the opt-in workspace memory (`ego_core::memory`).
//!
//! One Markdown file per workspace under `$EGO_HOME/memory/`. The name is the
//! workspace directory name plus a digest prefix of the whole canonical path,
//! so two workspaces named alike cannot share a file.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use ego_core::memory::{MemoryError, MemoryStore};

/// The memory file of `root` under `home`.
pub(crate) fn file_for(home: &Path, root: &Path) -> PathBuf {
    let digest = ego_core::digest::sha256_hex(root.to_string_lossy().as_bytes());
    let name: String = root
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let name = if name.is_empty() { "root" } else { &name };
    home.join("memory")
        .join(format!("{name}-{}.md", &digest[..12]))
}

/// The store for this run, or `None` when memory is off for it.
///
/// Off means no store at all, so nothing can read or write the file:
/// `memory` unset, `--bare`, or no `$EGO_HOME`.
pub(crate) fn store(
    enabled: bool,
    bare: bool,
    home: Option<&Path>,
    root: &Path,
) -> Option<Arc<dyn MemoryStore>> {
    if !enabled || bare {
        return None;
    }
    Some(Arc::new(FileStore {
        path: file_for(home?, root),
    }))
}

/// What `/context` prints: each source in the system prompt, then the memory.
pub(crate) fn context_text(
    system: Option<&ego_core::config::SystemPrompt>,
    memory: Option<&dyn MemoryStore>,
) -> String {
    use ego_core::memory::{Loaded, MAX_BYTES};
    let mut lines = vec!["context: system prompt sources".to_owned()];
    match system
        .map(|prompt| prompt.sources.as_slice())
        .unwrap_or_default()
    {
        [] => lines.push("  none".to_owned()),
        sources => lines.extend(
            sources
                .iter()
                .map(|source| format!("  {} ({} bytes)", source.path, source.bytes)),
        ),
    }
    lines.push(match memory {
        None => "memory: off".to_owned(),
        Some(store) => {
            let location = store.location();
            match ego_core::memory::load(store) {
                Loaded::Absent => format!("memory: on, no file yet ({location})"),
                Loaded::Text(text) => {
                    format!("memory: on, {} of {MAX_BYTES} bytes ({location})", text.len())
                }
                Loaded::TooLarge { bytes } => format!(
                    "memory: on, {bytes} bytes is over the {MAX_BYTES} byte limit, so it is not used ({location})"
                ),
                Loaded::Unreadable(error) => format!("memory: on, unreadable: {error} ({location})"),
            }
        }
    });
    lines.join("\n")
}

struct FileStore {
    path: PathBuf,
}

impl MemoryStore for FileStore {
    fn location(&self) -> String {
        self.path.display().to_string()
    }

    fn read(&self) -> Result<Option<String>, MemoryError> {
        match std::fs::read(&self.path) {
            Ok(bytes) => String::from_utf8(bytes)
                .map(Some)
                .map_err(|_| MemoryError("the memory file is not UTF-8 text".into())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(MemoryError(error.to_string())),
        }
    }

    fn write(&self, text: &str) -> Result<(), MemoryError> {
        let fail = |error: std::io::Error| MemoryError(error.to_string());
        let dir = self.path.parent().expect("the memory file has a directory");
        std::fs::create_dir_all(dir).map_err(fail)?;
        // Same directory, so the rename is atomic: a reader sees the old or
        // the new file, never half of one.
        let staged = self.path.with_extension("md.tmp");
        std::fs::write(&staged, text).map_err(fail)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o600))
                .map_err(fail)?;
        }
        std::fs::rename(&staged, &self.path).map_err(fail)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_gives_no_store_and_touches_no_file() {
        // Catches: memory off (or --bare) still resolves a store that a later
        // read or write could reach.
        let home = tempfile::tempdir().unwrap();
        let root = Path::new("/work/app");
        assert!(store(false, false, Some(home.path()), root).is_none());
        assert!(store(true, true, Some(home.path()), root).is_none());
        assert!(store(true, false, None, root).is_none());
        assert!(!home.path().join("memory").exists());
    }

    #[test]
    fn context_shows_memory_state_and_size() {
        // Catches: /context omits the memory, or reports the cap as the size.
        assert!(context_text(None, None).ends_with("memory: off"));
        let home = tempfile::tempdir().unwrap();
        let store = store(true, false, Some(home.path()), Path::new("/w")).unwrap();
        assert!(context_text(None, Some(store.as_ref())).contains("on, no file yet"));
        store.write("- one\n").unwrap();
        let text = context_text(None, Some(store.as_ref()));
        assert!(text.contains("on, 6 of 16384 bytes"), "{text}");
        assert!(text.contains(&store.location()), "{text}");
    }

    #[test]
    fn workspaces_with_the_same_name_get_different_files() {
        // Catches: two checkouts called `app` share one memory.
        let home = Path::new("/h");
        let one = file_for(home, Path::new("/a/app"));
        let two = file_for(home, Path::new("/b/app"));
        assert_ne!(one, two);
        assert!(one.starts_with("/h/memory"));
        assert!(
            one.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("app-")
        );
    }

    #[test]
    fn write_then_read_round_trips_and_replaces_whole() {
        // Catches: a failed rename leaves a stale temp file, or write appends
        // instead of replacing.
        let home = tempfile::tempdir().unwrap();
        let store = store(true, false, Some(home.path()), Path::new("/w")).unwrap();
        assert_eq!(store.read().unwrap(), None);
        store.write("- one\n- two\n").unwrap();
        store.write("- one\n").unwrap();
        assert_eq!(store.read().unwrap().as_deref(), Some("- one\n"));
        let entries: Vec<_> = std::fs::read_dir(home.path().join("memory"))
            .unwrap()
            .collect();
        assert_eq!(entries.len(), 1);
    }
}

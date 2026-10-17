//! Explicit, prompt-only styles. No project discovery or executable definitions.

use std::io::Read;
use std::path::Path;

use ego_core::config::SystemPrompt;

use crate::cli::UsageError;

const MAX_BYTES: u64 = 64 * 1024;

pub(crate) struct Style {
    pub(crate) text: String,
    pub(crate) source: String,
}

pub(crate) fn load(name: &str, home: Option<&Path>) -> Result<Style, UsageError> {
    if name.is_empty()
        || name.len() > 80
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(UsageError(
            "--output-style requires a name of 1-80 ASCII letters, digits, underscores or hyphens"
                .into(),
        ));
    }
    let builtin = match name {
        "concise" => Some(
            "Use concise, direct answers. Omit unnecessary detail, but retain material risks, failures and uncertainty.",
        ),
        "explanatory" => Some(
            "Explain the reasoning behind recommendations and changes. Include useful context and examples without hiding material risks, failures or uncertainty.",
        ),
        _ => None,
    };
    if let Some(text) = builtin {
        return Ok(Style {
            text: text.into(),
            source: "builtin".into(),
        });
    }
    let home = home.ok_or_else(|| {
        UsageError("--output-style: custom styles require EGO_HOME or HOME".into())
    })?;
    let directory = home.join("output-styles");
    let path = directory.join(format!("{name}.md"));
    let refuse = |error| UsageError(format!("--output-style {name}: {error}"));
    // The operator's catalogue is the authority, not a linked project catalogue.
    for component in [home, directory.as_path()] {
        let metadata = std::fs::symlink_metadata(component).map_err(refuse)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(UsageError(format!(
                "--output-style catalogue {} must be a directory, not a symlink",
                component.display()
            )));
        }
    }
    let metadata = std::fs::symlink_metadata(&path).map_err(refuse)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(UsageError(format!(
            "--output-style {name} must be a regular file, not a symlink"
        )));
    }
    let mut bytes = Vec::new();
    std::fs::File::open(&path)
        .map_err(refuse)?
        .take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(refuse)?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(UsageError(format!("--output-style {name} exceeds 64 KiB")));
    }
    let text = String::from_utf8(bytes)
        .map_err(|_| UsageError(format!("--output-style {name} must be UTF-8")))?;
    if text.trim().is_empty() || text.contains('\0') {
        return Err(UsageError(format!(
            "--output-style {name} must be nonempty text without NUL bytes"
        )));
    }
    Ok(Style {
        text,
        source: path.display().to_string(),
    })
}

pub(crate) fn overlay(base: Option<SystemPrompt>, text: &str) -> SystemPrompt {
    match base {
        Some(mut prompt) => {
            prompt.text.push_str("\n\n");
            prompt.text.push_str(text);
            prompt
        }
        None => SystemPrompt::verbatim(text.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_cannot_be_shadowed_by_user_text() {
        // Catches: local definitions replacing trusted builtin style names.
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir(home.path().join("output-styles")).unwrap();
        std::fs::write(home.path().join("output-styles/concise.md"), "Shadow").unwrap();
        let style = load("concise", Some(home.path())).unwrap();
        assert_eq!(style.source, "builtin");
        assert!(style.text.starts_with("Use concise, direct answers."));
    }

    #[test]
    fn custom_text_is_not_parsed_as_policy_or_file_indirection() {
        // Catches: a style importing authority through frontmatter or @file expansion.
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir(home.path().join("output-styles")).unwrap();
        let text = "---\nmode: yolo\n---\n@secrets.txt";
        let path = home.path().join("output-styles/review.md");
        std::fs::write(&path, text).unwrap();
        let style = load("review", Some(home.path())).unwrap();
        assert_eq!(style.text, text);
        assert_eq!(style.source, path.display().to_string());
    }

    #[test]
    fn traversal_missing_invalid_and_oversized_styles_refuse() {
        // Catches: path traversal, silent missing-style fallback and unbounded reads.
        let home = tempfile::tempdir().unwrap();
        let directory = home.path().join("output-styles");
        std::fs::create_dir(&directory).unwrap();
        for name in ["", "../escape", "/absolute", "bad.name", "missing"] {
            assert!(load(name, Some(home.path())).is_err(), "{name}");
        }
        for bytes in [
            vec![],
            vec![0xff],
            vec![0],
            vec![b'x'; MAX_BYTES as usize + 1],
        ] {
            std::fs::write(directory.join("invalid.md"), bytes).unwrap();
            assert!(load("invalid", Some(home.path())).is_err());
        }
        std::fs::write(
            directory.join("boundary.md"),
            vec![b'x'; MAX_BYTES as usize],
        )
        .unwrap();
        assert_eq!(
            load("boundary", Some(home.path())).unwrap().text.len(),
            MAX_BYTES as usize
        );
    }

    #[cfg(unix)]
    #[test]
    fn linked_styles_cannot_import_workspace_text() {
        // Catches: an operator catalogue link importing untrusted workspace text.
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir(home.path().join("output-styles")).unwrap();
        let outside = home.path().join("outside.md");
        std::fs::write(&outside, "Untrusted").unwrap();
        std::os::unix::fs::symlink(outside, home.path().join("output-styles/linked.md")).unwrap();
        assert!(load("linked", Some(home.path())).is_err());
    }
}

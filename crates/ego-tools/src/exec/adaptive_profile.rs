//! Adaptive Seatbelt policy. Every path remains encoded SBPL data.

use super::{ProfileError, ProfileInput, path_literal};
use crate::exec::adaptive::floor;
use ego_core::permit::SandboxAccess;
use std::collections::BTreeSet;
use std::path::Path;

pub(super) fn render(input: &ProfileInput<'_>) -> Result<String, ProfileError> {
    let perimeter = input
        .adaptive
        .ok_or(ProfileError::MissingAdaptivePerimeter)?;
    let mut writable = vec![input.workspace, input.private_scratch];
    writable.extend(perimeter.writable.iter().map(|p| p.as_path()));
    writable.extend(
        input
            .grants
            .iter()
            .filter(|g| g.access == SandboxAccess::Write)
            .map(|g| g.path.as_path()),
    );
    let mut out = String::from("(allow file-read* file-test-existence)\n(allow process-exec*)\n");
    out.push_str("(deny file-write* (require-not (require-any\n");
    for path in &writable {
        subpath(&mut out, path)?;
    }
    out.push_str(")))\n(allow file-write*\n");
    for path in writable {
        subpath(&mut out, path)?;
    }
    out.push_str(")\n");
    // Denies take precedence over broad read/write rules, including human grants.
    out.push_str("(deny file-read* file-write* process-exec*\n");
    for path in &perimeter.credentials {
        subpath(&mut out, path.as_path())?;
    }
    subpath(&mut out, &input.private_scratch.join("credential-probe"))?;
    out.push_str(")\n(deny file-write*\n");
    for path in &perimeter.controls {
        subpath(&mut out, path.as_path())?;
    }
    // Selector files stay protected; a new fixture .git directory is allowed.
    out.push_str(&format!(
        "  (require-all (regex #\"{}\") (require-not (vnode-type DIRECTORY)))\n",
        floor::GIT_ENTRY
    ));
    // A captured Git root has config and hooks controls even when absent.
    // Cover nested submodules/worktrees beneath separately named metadata too.
    for root in floor::metadata_roots(perimeter) {
        rooted_regex(&mut out, root, floor::metadata_control())?;
    }
    out.push_str(")\n(deny file-write-unlink\n");
    // Protect structural boundaries from relocation, while leaving transient
    // operation directories and objects/refs cleanup available to Git.
    for root in floor::metadata_roots(perimeter) {
        out.push_str("  (require-all\n");
        rooted_regex(&mut out, root, floor::metadata_boundary())?;
        out.push_str("    (vnode-type DIRECTORY)\n    (require-not\n");
        rooted_regex(&mut out, root, floor::METADATA_MODULE_TRANSIENT)?;
        out.push_str("    ))\n");
    }
    // Subpath denies protect contents, but a rename of any parent would move
    // those contents out from under their captured names. Deny only unlink
    // on each ancestor entry; operational descendants remain writable.
    let ancestors = perimeter
        .controls
        .iter()
        .chain(&perimeter.credentials)
        .flat_map(|path| path.as_path().ancestors().skip(1))
        .collect::<BTreeSet<_>>();
    for path in ancestors {
        out.push_str("  (literal ");
        out.push_str(&path_literal(path)?);
        out.push_str(")\n");
    }
    out.push_str(")\n");
    out.push_str("(deny file-read* file-write* process-exec* (require-all\n");
    out.push_str(&format!(
        "  (regex #\"{}\")\n  (require-not (subpath ",
        floor::ENV
    ));
    out.push_str(&path_literal(input.workspace)?);
    out.push_str("))))\n");
    Ok(out)
}

fn rooted_regex(out: &mut String, root: &Path, suffix: &str) -> Result<(), ProfileError> {
    // SBPL `#"..."` is raw: regex backslashes must remain unchanged, but a
    // double quote cannot be represented inside it even with a backslash.
    path_literal(root)?;
    if root.as_os_str().as_encoded_bytes().contains(&b'"') {
        return Err(ProfileError::UnrepresentablePath(super::path_label(root)));
    }
    let pattern = floor::rooted_pattern(root, suffix);
    out.push_str(&format!("  (regex #\"{pattern}\")\n"));
    Ok(())
}

fn subpath(out: &mut String, path: &Path) -> Result<(), ProfileError> {
    out.push_str("  (subpath ");
    out.push_str(&path_literal(path)?);
    out.push_str(")\n");
    Ok(())
}

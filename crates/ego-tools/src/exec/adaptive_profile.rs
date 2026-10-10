//! Adaptive Seatbelt policy. Every path remains encoded SBPL data.

use super::{ProfileError, ProfileInput, path_literal};
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
    out.push_str(
        "  (regex #\"(^|/)[.][gG][iI][tT]$\")\n\
           (regex #\"(^|/)[.][gG][iI][tT]/(.*/)?(config([.]worktree)?|hooks|commondir|gitdir|info/attributes)(/|$)\")\n",
    );
    // A captured Git root has config and hooks controls even when absent.
    // Cover nested submodules/worktrees beneath separately named metadata too.
    for control in &perimeter.controls {
        let path = control.as_path();
        if path.file_name().is_some_and(|name| name == "config")
            && let Some(root) = path.parent()
            && perimeter
                .controls
                .iter()
                .any(|other| other.as_path() == root.join("hooks"))
        {
            out.push_str("  (require-all\n");
            subpath(&mut out, root)?;
            out.push_str("    (regex #\"/(config([.]worktree)?|hooks|commondir|gitdir|info/attributes)(/|$)\"))\n");
        }
    }
    out.push_str(")\n(deny file-write-unlink\n");
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
    out.push_str("  (regex #\"(^|/)[.]env[^/]*(/|$)\")\n  (require-not (subpath ");
    out.push_str(&path_literal(input.workspace)?);
    out.push_str("))))\n");
    Ok(out)
}

fn subpath(out: &mut String, path: &Path) -> Result<(), ProfileError> {
    out.push_str("  (subpath ");
    out.push_str(&path_literal(path)?);
    out.push_str(")\n");
    Ok(())
}

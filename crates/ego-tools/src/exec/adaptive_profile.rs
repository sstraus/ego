//! Adaptive Seatbelt policy. Every path remains encoded SBPL data.

use super::{ProfileError, ProfileInput, path_literal};
use ego_core::permit::SandboxAccess;
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
        "  (regex #\"(^|/)[.]git/(config([.]worktree)?|hooks|commondir|gitdir)(/|$)\")\n)\n",
    );
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

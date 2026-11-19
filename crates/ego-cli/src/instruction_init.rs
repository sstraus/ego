//! Reviewed project instruction initialization.
use ego_core::config::{Effective, Mode, Trust};
use ego_core::permit::{CanonPath, Classification, Op, PolicyEngine, PreparedCall};
use std::path::{Path, PathBuf};
const LIMIT: usize = 64 * 1024;

const INITIAL: &str = "# Project instructions\n\n- Read the relevant source and tests before changing behavior.\n- Make the smallest change that meets the request.\n- Run the relevant checks and report incomplete validation.\n";

/// Captures exactly what the operator reviews. Existing files are never rewritten.
#[derive(Debug)]
pub struct InitProposal {
    root: PathBuf,
    agents: Option<String>,
    link_exists: bool,
}

impl InitProposal {
    pub fn prepare(root: &Path) -> Result<Self, String> {
        let agents_path = root.join("AGENTS.md");
        let agents = match std::fs::symlink_metadata(&agents_path) {
            Ok(meta) if meta.is_file() && meta.len() <= LIMIT as u64 => {
                Some(std::fs::read_to_string(&agents_path).map_err(|_| "cannot read AGENTS.md")?)
            }
            Ok(_) => return Err("AGENTS.md must be a regular UTF-8 file of at most 64 KiB".into()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(_) => return Err("cannot inspect AGENTS.md".into()),
        };
        let claude = root.join("CLAUDE.md");
        let link_exists = match std::fs::symlink_metadata(&claude) {
            Ok(meta) if meta.file_type().is_symlink() && std::fs::read_link(&claude).ok().as_deref() == Some(Path::new("AGENTS.md")) => true,
            Ok(_) => return Err("CLAUDE.md is independent or points elsewhere; reconcile it with AGENTS.md before using '/init'".into()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
            Err(_) => return Err("cannot inspect CLAUDE.md".into()),
        };
        Ok(Self {
            root: root.to_path_buf(),
            agents,
            link_exists,
        })
    }

    pub fn diff(&self) -> String {
        let mut text = String::new();
        if self.agents.is_none() {
            text.push_str("--- /dev/null\n+++ AGENTS.md\n");
            for line in INITIAL.lines() {
                text.push_str(&format!("+{line}\n"));
            }
        } else {
            text.push_str("AGENTS.md exists: preserve its exact content.\n");
        }
        if !self.link_exists {
            text.push_str("--- /dev/null\n+++ CLAUDE.md (relative symlink)\n+AGENTS.md\n");
        }
        text
    }

    pub fn apply(
        &self,
        admitted: bool,
        confirm: &dyn Fn(String) -> bool,
    ) -> Result<String, String> {
        if !admitted {
            return Err(
                "instruction initialization denied by effective write policy or workspace trust"
                    .into(),
            );
        }
        if self.agents.is_some() && self.link_exists {
            return Ok("Project instructions already exist; no changes.".into());
        }
        if !confirm(ego_core::permit::terminal_safe(&format!(
            "{}\nReview this proposal. Apply project instructions?",
            self.diff()
        ))) {
            return Ok("Initialization declined; no writes.".into());
        }
        let current = Self::prepare(&self.root)?;
        if current.agents != self.agents || current.link_exists != self.link_exists {
            return Err("instructions changed after review; propose again".into());
        }
        if self.agents.is_none() {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(self.root.join("AGENTS.md"))
                .map_err(|_| "AGENTS.md could not be created; no existing file was overwritten")?;
            file.write_all(INITIAL.as_bytes())
                .map_err(|_| "AGENTS.md write failed; inspect partial initialization")?;
        }
        if !self.link_exists {
            create_link(&self.root.join("CLAUDE.md")).map_err(|_| "CLAUDE.md symlink failed; AGENTS.md is preserved; inspect partial initialization")?;
        }
        Ok("Project instructions created. Load them with '/refresh'; ego will ask for trust if needed.".into())
    }
}

#[cfg(unix)]
fn create_link(path: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink("AGENTS.md", path)
}
#[cfg(windows)]
fn create_link(path: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_file("AGENTS.md", path)
}

pub fn init_admitted(effective: &Effective, root: &Path, trust: Trust) -> bool {
    // This is an explicit operator control-file workflow, not a proposal for
    // general directory authority (setup's control-file floor still applies
    // to setup). The native tool/exec sandbox does not perform these writes.
    trust == Trust::Trusted
        && policy_admitted(
            effective,
            root,
            &[root.join("AGENTS.md"), root.join("CLAUDE.md")],
        )
}

fn policy_admitted(effective: &Effective, root: &Path, paths: &[PathBuf]) -> bool {
    if effective.mode == Some(Mode::Plan)
        || effective.sandbox == Some(ego_core::permit::Confinement::Ro)
    {
        return false;
    }
    let workspace = CanonPath::new(root);
    let Ok(roots) = crate::run::profile_roots::admit(effective, &workspace) else {
        return false;
    };
    let policy = PolicyEngine::new(
        workspace,
        effective.mode.unwrap_or(Mode::Default),
        effective.permissions.clone(),
    )
    .with_profile_roots(roots);
    paths.iter().all(|path| {
        !matches!(
            policy.classify(PreparedCall::new(
                "write",
                Op::Write {
                    path: CanonPath::new(path),
                    change: ego_core::permit::Change::Whole(Vec::new())
                }
            )),
            Classification::Settled(Err(_))
        )
    })
}

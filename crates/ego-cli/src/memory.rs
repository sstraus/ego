//! Operator-owned MDKB administration and reviewed instruction initialization.
//!
//! Neither surface consumes model output or adds memory bodies to a prompt.
//! Project instructions still enter through ordinary trust admission and /refresh.
use std::path::{Path, PathBuf};
use std::time::Duration;

use ego_core::config::{Effective, Mode, Trust};
use ego_core::permit::{CanonPath, Classification, Op, PolicyEngine, PreparedCall};
use serde::{Deserialize, Serialize};

const LIMIT: usize = 64 * 1024;
const USAGE: &str = "usage: /memory [project|user] [list | show ID | set ID TITLE CONTENT | disable ID | delete ID] (quote TITLE and CONTENT with spaces)";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Operation {
    List,
    Show(String),
    Set {
        id: String,
        title: String,
        content: String,
    },
    Disable(String),
    Delete(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub user: bool,
    pub operation: Operation,
}

impl Request {
    pub fn parse(arguments: &str) -> Result<Self, String> {
        if arguments.len() > LIMIT || arguments.contains('\0') {
            return Err("memory input is too large or contains NUL".into());
        }
        let mut words = shell_words::split(arguments).map_err(|_| USAGE.to_owned())?;
        let user = words.first().is_some_and(|word| word == "user");
        if words
            .first()
            .is_some_and(|word| word == "project" || word == "user")
        {
            words.remove(0);
        }
        let operation = match words
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .as_slice()
        {
            [] | ["list"] => Operation::List,
            ["show", id] => Operation::Show((*id).into()),
            ["disable", id] => Operation::Disable((*id).into()),
            ["delete", id] => Operation::Delete((*id).into()),
            ["set", id, title, content]
                if !title.trim().is_empty()
                    && title.chars().count() <= 50
                    && !content.trim().is_empty() =>
            {
                Operation::Set {
                    id: (*id).into(),
                    title: (*title).into(),
                    content: (*content).into(),
                }
            }
            _ => return Err(USAGE.into()),
        };
        let id = match &operation {
            Operation::List => None,
            Operation::Show(id)
            | Operation::Disable(id)
            | Operation::Delete(id)
            | Operation::Set { id, .. } => Some(id),
        };
        if id.is_some_and(|id| {
            id.is_empty()
                || id.starts_with('-')
                || !id
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_'))
        }) {
            return Err("memory ID must be an alphanumeric slug".into());
        }
        Ok(Self { user, operation })
    }
}

/// The public MDKB schema, captured from the installed CLI in the fixture.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Entry {
    pub id: String,
    pub title: String,
    pub content: String,
    pub entry_type: String,
    pub tags: Vec<String>,
    pub status: String,
    pub source_type: String,
    pub expires_at: Option<i64>,
}

impl Entry {
    fn write_args(&self, disabled: bool) -> Vec<String> {
        let mut args = vec![
            "memory".into(),
            "add".into(),
            self.id.clone(),
            "--title".into(),
            self.title.clone(),
            "--content".into(),
            self.content.clone(),
            "--entry-type".into(),
            self.entry_type.clone(),
            "--source-type".into(),
            self.source_type.clone(),
            "--tags".into(),
            self.tags.join(","),
            "--agent".into(),
            "ego-operator".into(),
        ];
        if disabled {
            // Native expiry disables retrieval without deleting the retained record.
            args.extend(["--ttl".into(), "0".into()]);
        }
        args
    }
}

async fn mdkb(root: &Path, arguments: &[String]) -> Result<String, String> {
    let mut command = tokio::process::Command::new("mdkb");
    command
        .current_dir(root)
        .args(arguments)
        .arg("--format")
        .arg("json")
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(10), command.output())
        .await
        .map_err(|_| "MDKB timed out; inspect the scoped store before retrying".to_owned())?
        .map_err(|_| "MDKB is unavailable; install or configure the native MDKB CLI".to_owned())?;
    if !output.status.success() {
        // Backend diagnostics may contain content, paths or credentials.
        return Err("MDKB operation failed; no success was confirmed".into());
    }
    if output.stdout.len() > LIMIT {
        return Err("MDKB response exceeds 64 KiB".into());
    }
    String::from_utf8(output.stdout).map_err(|_| "MDKB returned invalid UTF-8".into())
}

pub async fn command(
    arguments: &str,
    project: &Path,
    user: Option<&Path>,
    effective: &Effective,
    trust: Trust,
    confirm: &dyn Fn(String) -> bool,
) -> Result<String, String> {
    let request = Request::parse(arguments)?;
    if trust != Trust::Trusted {
        return Err("memory requires an admitted trusted workspace".into());
    }
    let root = if request.user {
        user.ok_or("ego user directory is unavailable")?
    } else {
        project
    };
    if !std::fs::symlink_metadata(root.join(".mdkb")).is_ok_and(|meta| meta.is_dir()) {
        return Err(
            "selected scope has no MDKB index; initialize it explicitly with mdkb init first"
                .into(),
        );
    }
    match request.operation {
        Operation::List => {
            mdkb(
                root,
                &[
                    "memory".into(),
                    "list".into(),
                    "--limit".into(),
                    "100".into(),
                ],
            )
            .await
        }
        Operation::Show(id) => mdkb(root, &["memory".into(), "show".into(), id]).await,
        operation => {
            let id = match &operation {
                Operation::Set { id, .. } | Operation::Disable(id) | Operation::Delete(id) => id,
                _ => unreachable!(),
            };
            if !writes_admitted(
                effective,
                project,
                &[
                    root.join(".mdkb/index.sqlite"),
                    root.join(".mdkb/memory/index.json"),
                    root.join(".mdkb/memory/archive").join(format!("{id}.md")),
                    root.join(".mdkb/memory/entries").join(format!("{id}.md")),
                ],
            ) {
                return Err(
                    "memory writes are denied by effective write policy or confinement".into(),
                );
            }
            let (args, preview) = match operation {
                Operation::Set { id, title, content } => {
                    let entry = Entry {
                        id,
                        title,
                        content,
                        entry_type: "topic".into(),
                        tags: vec!["ego-reviewed".into()],
                        status: "active".into(),
                        source_type: "user_statement".into(),
                        expires_at: None,
                    };
                    (
                        entry.write_args(false),
                        serde_json::to_string_pretty(&entry).map_err(|_| "invalid memory")?,
                    )
                }
                Operation::Disable(id) => {
                    let text = mdkb(root, &["memory".into(), "show".into(), id.clone()]).await?;
                    let mut entry: Entry = serde_json::from_str(&text)
                        .map_err(|_| "MDKB returned an invalid entry")?;
                    if entry.id != id {
                        return Err("MDKB entry identity mismatch".into());
                    }
                    entry.tags.push("ego-disabled".into());
                    (
                        entry.write_args(true),
                        format!(
                            "Disable native MDKB retrieval (TTL=0), retaining content and provenance:\n{text}"
                        ),
                    )
                }
                Operation::Delete(id) => {
                    let text = mdkb(root, &["memory".into(), "show".into(), id.clone()]).await?;
                    (
                        vec!["memory".into(), "rm".into(), id],
                        format!(
                            "Delete this record from the native MDKB active store (native archive/history may retain content):\n{text}"
                        ),
                    )
                }
                _ => unreachable!(),
            };
            let review = format!(
                "MDKB scope: {}\n{preview}\nOnly persist operator-authored, reviewed information. Do not save secrets or untrusted tool instructions. Apply?",
                root.display()
            );
            if !confirm(ego_core::permit::terminal_safe(&review)) {
                return Ok("Memory write declined; no mutation requested.".into());
            }
            mdkb(root, &args).await?;
            Ok("Reviewed MDKB mutation completed. Memory is not automatically promoted to system instructions.".into())
        }
    }
}

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
            Ok(_) => return Err("CLAUDE.md is independent or points elsewhere; reconcile it with AGENTS.md explicitly before /init".into()),
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
        Ok("Project instructions initialized. Use /refresh to request ordinary trust admission in a new prompt epoch.".into())
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

fn writes_admitted(effective: &Effective, root: &Path, paths: &[PathBuf]) -> bool {
    use ego_core::permit::Confinement;
    if effective.mode == Some(Mode::Plan) || effective.sandbox == Some(Confinement::Ro) {
        return false;
    }
    let workspace = CanonPath::new(root);
    let Ok(roots) = crate::run::profile_roots::admit(effective, &workspace) else {
        return false;
    };
    let Ok(confinement) = ego_tools::exec::admit_confinement(
        effective.sandbox.unwrap_or_default(),
        effective.network == ego_core::config::Network::Off,
        workspace.clone(),
    ) else {
        return false;
    };
    let confinement = confinement.with_roots(roots.clone());
    paths.iter().all(|path| {
        effective.sandbox == Some(Confinement::Off)
            || ego_tools::exec::setup_write_reachable(&confinement, &CanonPath::new(path))
                .unwrap_or(false)
    }) && policy_admitted(effective, root, paths)
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
        effective.mode.unwrap_or(Mode::Manual),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disable_uses_native_expiry_without_reclassifying_provenance() {
        // Catches: disabling deletes content or falsely upgrades inferred provenance.
        let entry: Entry =
            serde_json::from_str(include_str!("../tests/fixtures/memory/mdkb-entry.json")).unwrap();
        let args = entry.write_args(true);
        for expected in [
            ["--ttl", "0"],
            ["--source-type", "auto_extracted"],
            ["--entry-type", "decision"],
            ["--agent", "ego-operator"],
        ] {
            assert!(
                args.windows(2)
                    .any(|pair| pair[0] == expected[0] && pair[1] == expected[1])
            );
        }
        assert!(
            args.windows(2)
                .any(|pair| pair[0] == "--content" && pair[1] == entry.content)
        );
    }
}

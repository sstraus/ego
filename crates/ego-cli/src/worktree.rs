//! Explicit operator-owned repository lifecycle; never automatic exit cleanup.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::cli::{Args, SessionChoice, WorktreeAction};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    pub version: u32,
    pub id: Uuid,
    pub common_dir: PathBuf,
    pub source: PathBuf,
    pub source_ref: String,
    pub initial_head: String,
    pub branch: String,
    pub worktree: PathBuf,
    pub state: ReceiptState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReceiptState {
    Pending,
    Ready,
    Removed,
}

/// The effect boundary shared by ordinary Git and the existing TUIC owner.
/// Hosted receipts remain owned by TUIC, not adopted into the local store.
pub trait Repository {
    fn create(
        &self,
        source: &Path,
        branch: &str,
    ) -> Result<(PathBuf, String, Option<File>), String>;
}

pub struct LocalRepository {
    home: PathBuf,
}

impl LocalRepository {
    pub fn new(home: PathBuf) -> Self {
        Self { home }
    }

    fn directory(&self) -> PathBuf {
        self.home.join("worktrees")
    }

    fn lock(&self) -> Result<fd_lock::RwLock<File>, String> {
        let dir = self.directory();
        fs::create_dir_all(&dir).map_err(error)?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        Ok(fd_lock::RwLock::new(
            options.open(dir.join("lifecycle.lock")).map_err(error)?,
        ))
    }

    fn lease(&self, id: Uuid) -> Result<File, String> {
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options
            .open(self.directory().join(format!("{id}.active")))
            .map_err(error)?;
        file.try_lock()
            .map_err(|_| "worktree is in use by another ego process".to_owned())?;
        Ok(file)
    }

    fn existing_lease(&self, directory: &Path) -> Result<Option<File>, String> {
        let directory = fs::canonicalize(directory).map_err(error)?;
        let checkouts = self.directory().join("checkouts");
        let Ok(relative) = directory.strip_prefix(checkouts) else {
            return Ok(None);
        };
        let Some(id) = relative
            .components()
            .next()
            .and_then(|part| part.as_os_str().to_str())
        else {
            return Ok(None);
        };
        let id = Uuid::parse_str(id).map_err(error)?;
        let mut lock = self.lock()?;
        let _guard = lock.try_write().map_err(error)?;
        let receipt = self.read(id)?;
        if receipt.state == ReceiptState::Removed {
            return Err("owned worktree receipt has been removed".into());
        }
        self.lease(id).map(Some)
    }

    fn publish(&self, receipt: &Receipt) -> Result<(), String> {
        let dir = self.directory();
        let mut file = tempfile::NamedTempFile::new_in(&dir).map_err(error)?;
        serde_json::to_writer_pretty(&mut file, receipt).map_err(error)?;
        file.write_all(b"\n").map_err(error)?;
        file.as_file().sync_all().map_err(error)?;
        file.persist(dir.join(format!("{}.json", receipt.id)))
            .map_err(error)?;
        File::open(dir).and_then(|f| f.sync_all()).map_err(error)
    }

    pub fn read(&self, id: Uuid) -> Result<Receipt, String> {
        let path = self.directory().join(format!("{id}.json"));
        let receipt: Receipt =
            serde_json::from_slice(&fs::read(path).map_err(error)?).map_err(error)?;
        if receipt.version != 1 || receipt.id != id {
            return Err("unknown or mismatched worktree receipt".into());
        }
        if receipt.worktree != self.directory().join("checkouts").join(id.to_string()) {
            return Err("receipt checkout does not match its owned location".into());
        }
        Ok(receipt)
    }

    pub fn cleanup(&self, id: Uuid) -> Result<Receipt, String> {
        let mut lock = self.lock()?;
        let _guard = lock.try_write().map_err(error)?;
        let mut receipt = self.read(id)?;
        let _lease = self.lease(id)?;
        if receipt.state == ReceiptState::Removed {
            return Ok(receipt);
        }
        if !receipt.worktree.try_exists().map_err(error)? {
            // Removal may have completed before the final receipt was durable.
            // Do not remove/prune anything during this recovery observation.
            let registered = git(&receipt.source, &["worktree", "list", "--porcelain"])?;
            if registered
                .lines()
                .any(|line| line.strip_prefix("worktree ") == receipt.worktree.to_str())
            {
                return Err(
                    "checkout is missing but Git still registers it; recovery needs inspection"
                        .into(),
                );
            }
            receipt.state = ReceiptState::Removed;
            self.publish(&receipt)?;
            return Ok(receipt);
        }
        // Git's own worktree index and the checkout's metadata must agree with
        // the receipt. A moved/replaced directory is not ours to delete.
        if fs::canonicalize(&receipt.worktree).map_err(error)? != receipt.worktree {
            return Err("checkout path was replaced by a symlink; retaining worktree".into());
        }
        let common = common_dir(&receipt.worktree)?;
        if common != receipt.common_dir {
            return Err("checkout Git identity changed; retaining worktree".into());
        }
        let branch = git(&receipt.worktree, &["symbolic-ref", "--quiet", "HEAD"])?;
        if branch != format!("refs/heads/{}", receipt.branch) {
            return Err("checkout branch changed; retaining worktree".into());
        }
        let status = git(
            &receipt.worktree,
            &[
                "status",
                "--porcelain=v1",
                "--untracked-files=all",
                "--ignored=matching",
                "--ignore-submodules=none",
            ],
        )?;
        if !status.is_empty() {
            return Err("worktree has uncommitted changes; cleanup refused".into());
        }
        let head = git(&receipt.worktree, &["rev-parse", "--verify", "HEAD"])?;
        git(
            &receipt.worktree,
            &["merge-base", "--is-ancestor", &head, &receipt.source_ref],
        )
        .map_err(|_| "worktree has unmerged commits; cleanup refused".to_owned())?;
        if head != receipt.initial_head {
            let upstream = git(
                &receipt.worktree,
                &[
                    "rev-parse",
                    "--verify",
                    &format!(
                        "{}@{{upstream}}",
                        receipt
                            .source_ref
                            .strip_prefix("refs/heads/")
                            .unwrap_or(&receipt.source_ref)
                    ),
                ],
            )
            .map_err(|_| "no upstream proof for new commits; cleanup refused".to_owned())?;
            git(
                &receipt.worktree,
                &["merge-base", "--is-ancestor", &head, &upstream],
            )
            .map_err(|_| "worktree has unpushed commits; cleanup refused".to_owned())?;
        }
        // Ignore no files and never pass --force. Git rechecks dirty state and
        // refuses worktrees containing submodules rather than discarding them.
        git(
            &receipt.worktree,
            &[
                "worktree",
                "remove",
                receipt.worktree.to_str().ok_or("non-UTF-8 checkout")?,
            ],
        )?;
        receipt.state = ReceiptState::Removed;
        self.publish(&receipt)?;
        Ok(receipt)
    }
}

impl Repository for LocalRepository {
    fn create(
        &self,
        source: &Path,
        branch: &str,
    ) -> Result<(PathBuf, String, Option<File>), String> {
        validate_branch(source, branch)?;
        let mut lock = self.lock()?;
        let _guard = lock.try_write().map_err(error)?;
        let source = fs::canonicalize(source).map_err(error)?;
        let common_dir = common_dir(&source)?;
        let head = git(&source, &["rev-parse", "--verify", "HEAD"])?;
        let source_ref =
            git(&source, &["symbolic-ref", "--quiet", "HEAD"]).unwrap_or_else(|_| head.clone());
        let id = Uuid::new_v4();
        let worktree = self.directory().join("checkouts").join(id.to_string());
        fs::create_dir_all(worktree.parent().ok_or("missing checkout parent")?).map_err(error)?;
        let mut receipt = Receipt {
            version: 1,
            id,
            common_dir,
            source,
            source_ref,
            initial_head: head,
            branch: branch.to_owned(),
            worktree,
            state: ReceiptState::Pending,
        };
        // Intent survives Git failure, process death and uncertain publication.
        self.publish(&receipt)?;
        let lease = self.lease(id)?;
        git(
            &receipt.source,
            &[
                "worktree",
                "add",
                "-b",
                branch,
                receipt.worktree.to_str().ok_or("non-UTF-8 checkout")?,
                &receipt.initial_head,
            ],
        )
        .map_err(|message| format!("{message}; retained receipt {id}"))?;
        receipt.state = ReceiptState::Ready;
        self.publish(&receipt)
            .map_err(|message| format!("{message}; retain checkout and inspect receipt {id}"))?;
        let text = serde_json::to_string(&receipt).map_err(error)?;
        Ok((receipt.worktree, text, Some(lease)))
    }
}

pub struct TuicRepository;

impl Repository for TuicRepository {
    fn create(
        &self,
        source: &Path,
        branch: &str,
    ) -> Result<(PathBuf, String, Option<File>), String> {
        validate_branch(source, branch)?;
        let request = serde_json::json!({
            "action": "worktree_create", "path": fs::canonicalize(source).map_err(error)?,
            "branch": branch, "spawn_session": false
        });
        let output = Command::new("tuic")
            .args(["mcp", "repo", &request.to_string()])
            .stdin(Stdio::null())
            .output()
            .map_err(error)?;
        if !output.status.success() {
            return Err(format!(
                "TUIC worktree creation failed: {}; no local fallback",
                output.status
            ));
        }
        let text = String::from_utf8(output.stdout).map_err(error)?;
        let value: serde_json::Value = serde_json::from_str(&text).map_err(error)?;
        let path = value["worktree_path"]
            .as_str()
            .ok_or("TUIC returned no worktree_path; retain host ownership and inspect TUIC")?;
        let path = fs::canonicalize(path).map_err(error)?;
        if common_dir(&path)? != common_dir(source)?
            || git(&path, &["symbolic-ref", "--quiet", "HEAD"])? != format!("refs/heads/{branch}")
        {
            return Err(
                "TUIC checkout does not match requested repository and branch; inspect TUIC".into(),
            );
        }
        Ok((path, text, None))
    }
}

fn validate_branch(source: &Path, branch: &str) -> Result<(), String> {
    if branch.is_empty() || branch.starts_with('-') || branch == "HEAD" {
        return Err("--worktree requires an explicit new branch name".into());
    }
    git(source, &["check-ref-format", "--branch", branch])?;
    // --branch accepts @{-1} shorthand. It is not exact branch consent.
    if branch.contains("@{") {
        return Err("worktree branch must be literal, not a checkout shorthand".into());
    }
    Ok(())
}

fn common_dir(source: &Path) -> Result<PathBuf, String> {
    let path = git(
        source,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    fs::canonicalize(path).map_err(error)
}

fn git_command(source: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(source)
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "submodule.recurse=false",
            "-c",
            "core.fsmonitor=false",
        ])
        .env_remove("GIT_DIR")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env("GIT_NO_LAZY_FETCH", "1")
        .stdin(Stdio::null());
    command
}

fn git(source: &Path, args: &[&str]) -> Result<String, String> {
    let mut command = git_command(source);
    {
        // A checkout/status filter is arbitrary code, and can also fetch content.
        // Disable configured smudge/process drivers rather than running an
        // external system merely to create an isolated checkout.
        let filters = git_command(source)
            .args([
                "config",
                "--name-only",
                "--get-regexp",
                r"^filter\..*\.(clean|smudge|process|required)$",
            ])
            .output()
            .map_err(error)?;
        if !filters.status.success() && filters.status.code() != Some(1) {
            return Err("cannot inspect checkout filter configuration".into());
        }
        for key in String::from_utf8(filters.stdout).map_err(error)?.lines() {
            let value = if key.ends_with(".required") {
                "false"
            } else {
                ""
            };
            command.arg("-c").arg(format!("{key}={value}"));
        }
    }
    let output = command.args(args).output().map_err(error)?;
    if !output.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args[0],
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    String::from_utf8(output.stdout)
        .map(|s| s.trim_end_matches(['\r', '\n']).to_owned())
        .map_err(error)
}

fn error(error: impl std::fmt::Display) -> String {
    error.to_string()
}

fn home() -> Result<PathBuf, String> {
    let path = std::env::var_os("EGO_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".ego")))
        .ok_or("worktree lifecycle requires EGO_HOME or HOME")?;
    fs::create_dir_all(&path).map_err(error)?;
    fs::canonicalize(path).map_err(error)
}

fn hosted() -> bool {
    std::env::var_os("TUIC_SOCKET").is_some_and(|s| !s.is_empty())
}

/// Run before trust/config/provider admission: all subsequent consumers use
/// the actual checkout directory, not the source repository's directory.
pub fn launch(args: &mut Args) -> Result<Option<File>, String> {
    let source = args
        .directory
        .clone()
        .map(Ok)
        .unwrap_or_else(std::env::current_dir)
        .map_err(error)?;
    let Some(branch) = args.worktree.take() else {
        return if hosted() {
            Ok(None)
        } else {
            LocalRepository::new(home()?).existing_lease(&source)
        };
    };
    if !matches!(args.session, SessionChoice::New) {
        return Err("--worktree requires a new unnamed session".into());
    }
    let adapter: Box<dyn Repository> = if hosted() {
        Box::new(TuicRepository)
    } else {
        Box::new(LocalRepository::new(home()?))
    };
    let (path, receipt, lease) = adapter.create(&source, &branch)?;
    eprintln!("Retained worktree receipt: {receipt}");
    args.directory = Some(path);
    Ok(lease)
}

pub fn execute(action: &WorktreeAction) -> Result<String, String> {
    if hosted() {
        return Err("TUIC owns hosted worktrees; use its repo lifecycle and receipts".into());
    }
    let repository = LocalRepository::new(home()?);
    let receipt = match action {
        WorktreeAction::Show { receipt } => repository.read(*receipt)?,
        WorktreeAction::Cleanup { receipt } => repository.cleanup(*receipt)?,
    };
    serde_json::to_string_pretty(&receipt).map_err(error)
}

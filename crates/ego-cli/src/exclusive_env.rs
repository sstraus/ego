//! The environment of an exclusive read-only ACP session.
//!
//! `ego acp --exclusive-read-only` must not leave a push credential, an SSH agent socket or any
//! other ambient variable reachable. The process rebuilds its environment from an allow-list at
//! start, before the runtime spawns a thread: it replaces itself with the same command line under
//! a cleared environment. The check is of the environment as it is, not of a marker, so an
//! inherited marker cannot skip the clearing, and the second start finds nothing to remove.

use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::process::ExitCode;

use ego_ai::provider::CredentialStrategy;

use crate::cli::{self, Invocation};

/// Platform values every session needs. Credentials are never here.
const BASE: &[&str] = &[
    "PATH",
    "HOME",
    "EGO_HOME",
    "TMPDIR",
    "TEMP",
    "TMP",
    "LANG",
    "TZ",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "HTTPS_PROXY",
    "HTTP_PROXY",
    "NO_PROXY",
    "ALL_PROXY",
    "https_proxy",
    "http_proxy",
    "no_proxy",
    "all_proxy",
    "SYSTEMROOT",
    "WINDIR",
    "COMSPEC",
    "PATHEXT",
    "USERPROFILE",
    "APPDATA",
    "LOCALAPPDATA",
    "PROGRAMDATA",
    // macOS adds this variable to every process it starts, so a restart finds it again.
    "__CF_USER_TEXT_ENCODING",
    RESTARTED,
];

/// Set by the restart so that a second pass which still finds a foreign variable refuses the
/// launch instead of replacing the process again, forever. It is kept, never trusted: a pass
/// that sees it and a dirty environment fails.
const RESTARTED: &str = "EGO_EXCLUSIVE_RESTARTED";

/// Windows compares names without case; elsewhere they are exact.
fn normal(name: &OsStr) -> String {
    let text = name.to_string_lossy();
    if cfg!(windows) {
        text.to_uppercase()
    } else {
        text.into_owned()
    }
}

/// The names this session keeps.
pub(crate) struct Keep(BTreeSet<String>);

impl Keep {
    pub(crate) fn new<'a>(extra: impl IntoIterator<Item = &'a str>) -> Self {
        Self(
            BASE.iter()
                .copied()
                .chain(extra)
                .map(|name| normal(OsStr::new(name)))
                .collect(),
        )
    }

    fn keeps(&self, name: &OsStr) -> bool {
        let text = normal(name);
        self.0.contains(&text) || text.starts_with("LC_")
    }

    /// The variables of `env` that stay, and whether any other variable was present.
    pub(crate) fn filter(
        &self,
        env: Vec<(OsString, OsString)>,
    ) -> (Vec<(OsString, OsString)>, bool) {
        let total = env.len();
        let kept: Vec<_> = env
            .into_iter()
            .filter(|(name, _)| self.keeps(name))
            .collect();
        let dirty = kept.len() != total;
        (kept, dirty)
    }
}

/// The credential variables of one provider row.
pub(crate) fn credential_vars(strategy: &CredentialStrategy) -> Vec<String> {
    match strategy {
        CredentialStrategy::None | CredentialStrategy::OAuth { .. } => Vec::new(),
        CredentialStrategy::BearerFromEnv { var }
        | CredentialStrategy::BedrockSigv4 { var, .. }
        | CredentialStrategy::HeaderFromEnv { var, .. }
        | CredentialStrategy::OAuthOrEnv { var, .. } => vec![var.clone()],
        CredentialStrategy::GoogleCloud {
            project_var,
            location_var,
            ..
        } => vec![project_var.clone(), location_var.clone()],
        CredentialStrategy::Anthropic {
            api_key_var,
            oauth_var,
            ..
        } => vec![api_key_var.clone(), oauth_var.clone()],
    }
}

/// The credential variables of the provider that `model` names, and no other provider's.
fn provider_vars(model: &str) -> Vec<String> {
    let provider = model.split('/').next().unwrap_or_default();
    crate::model::rows(&Default::default())
        .unwrap_or_default()
        .into_iter()
        .find(|row| row.id() == provider)
        .map(|row| credential_vars(&row.credential))
        .unwrap_or_default()
}

/// Rebuild the environment of an exclusive read-only `ego acp` and start again under it.
///
/// `None` means this process may go on: the policy is off, or the environment already holds only
/// allowed names. `Some` is the exit of a refused launch, or of the process that replaced this one.
pub fn enforce(argv: Vec<String>) -> Option<ExitCode> {
    let Ok(Invocation::Acp(args)) = cli::parse(argv) else {
        return None;
    };
    if !args.shape.exclusive_read_only {
        return None;
    }
    let model = crate::run::workspace(args.directory.as_ref())
        .ok()
        .and_then(|root| {
            crate::run::protocol_effective(&root, args.session_dir.clone(), args.shape.clone()).ok()
        })
        .and_then(|effective| effective.model);
    let provider = model
        .as_ref()
        .map(|model| provider_vars(&model.0))
        .unwrap_or_default();
    let keep = Keep::new(
        provider
            .iter()
            .map(String::as_str)
            .chain(args.env_allow.iter().map(String::as_str)),
    );
    let (kept, dirty) = keep.filter(std::env::vars_os().collect());
    if !dirty {
        return None;
    }
    if std::env::var_os(RESTARTED).is_some() {
        let leftover: Vec<String> = std::env::vars_os()
            .map(|(name, _)| name)
            .filter(|name| !keep.keeps(name))
            .map(|name| name.to_string_lossy().into_owned())
            .collect();
        eprintln!("ego: the environment could not be cleared; these names remain: {leftover:?}");
        return Some(ExitCode::from(1));
    }
    Some(restart(kept))
}

fn restart(kept: Vec<(OsString, OsString)>) -> ExitCode {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(error) => {
            eprintln!("ego: the executable path could not be resolved: {error}");
            return ExitCode::from(1);
        }
    };
    let mut command = std::process::Command::new(exe);
    command
        .args(std::env::args_os().skip(1))
        .env_clear()
        .envs(kept)
        .env(RESTARTED, "1");
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let error = command.exec();
        eprintln!("ego: the cleared environment could not be installed: {error}");
        ExitCode::from(1)
    }
    #[cfg(not(unix))]
    {
        match command.status() {
            Ok(status) => ExitCode::from(status.code().map_or(1, |code| code as u8)),
            Err(error) => {
                eprintln!("ego: the cleared environment could not be installed: {error}");
                ExitCode::from(1)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        pairs
            .iter()
            .map(|(name, value)| (OsString::from(name), OsString::from(value)))
            .collect()
    }

    #[test]
    fn ambient_credentials_do_not_survive_and_the_selected_key_does() {
        let keep = Keep::new(["OPENROUTER_API_KEY"]);
        let (kept, dirty) = keep.filter(env(&[
            ("PATH", "/bin"),
            ("LC_ALL", "C"),
            ("OPENROUTER_API_KEY", "k"),
            ("OPENAI_API_KEY", "other-provider"),
            ("GH_TOKEN", "t"),
            ("SSH_AUTH_SOCK", "/s"),
            ("GIT_ASKPASS", "/a"),
            ("TUIC_SESSION", "x"),
        ]));
        let names: Vec<_> = kept
            .iter()
            .map(|(name, _)| name.to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["PATH", "LC_ALL", "OPENROUTER_API_KEY"]);
        assert!(dirty);
    }

    #[test]
    fn a_clean_environment_needs_no_restart() {
        let keep = Keep::new([]);
        assert!(!keep.filter(env(&[("PATH", "/bin"), ("HOME", "/h")])).1);
    }

    #[test]
    fn the_credential_variables_are_those_of_one_row() {
        let strategy = CredentialStrategy::Anthropic {
            provider: "anthropic".into(),
            api_key_var: "ANTHROPIC_API_KEY".into(),
            oauth_var: "ANTHROPIC_OAUTH_TOKEN".into(),
        };
        assert_eq!(
            credential_vars(&strategy),
            ["ANTHROPIC_API_KEY", "ANTHROPIC_OAUTH_TOKEN"]
        );
        assert!(credential_vars(&CredentialStrategy::None).is_empty());
    }
}

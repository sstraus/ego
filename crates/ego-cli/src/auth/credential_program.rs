//! Execute credential programs and decode their secret output.

use super::*;

/// Run the two-verb credential program without ever logging stdout.
pub async fn run_program(
    program: &Path,
    verb: &str,
    current: Option<&AuthEntry>,
) -> Result<AuthEntry, AuthError> {
    let commands = OsCredentialCommand;
    run_program_scoped(
        program,
        verb,
        current,
        &commands,
        &CancelToken::new(),
        None,
        PROGRAM_TIMEOUT,
    )
    .await
}

pub(super) async fn run_configured_command_scoped(
    command: &str,
    verb: &str,
    current: Option<&AuthEntry>,
    commands: &dyn CredentialCommandPort,
    cancel: &CancelToken,
    run_deadline: Option<Instant>,
    timeout: Duration,
) -> Result<AuthEntry, AuthError> {
    let mut words = shell_words::split(command).map_err(|err| AuthError::MissingCredential {
        program: command.to_string(),
        reason: format!("cannot parse command: {err}"),
    })?;
    if words.is_empty() {
        return Err(AuthError::MissingCredential {
            program: command.to_string(),
            reason: "command is empty".to_string(),
        });
    }
    let program = PathBuf::from(words.remove(0));
    run_program_parts(
        &program,
        &words,
        command,
        verb,
        current,
        CredentialCommandScope {
            commands,
            cancel,
            run_deadline,
            timeout,
        },
    )
    .await
}

pub(super) async fn run_program_scoped(
    program: &Path,
    verb: &str,
    current: Option<&AuthEntry>,
    commands: &dyn CredentialCommandPort,
    cancel: &CancelToken,
    run_deadline: Option<Instant>,
    timeout: Duration,
) -> Result<AuthEntry, AuthError> {
    run_program_parts(
        program,
        &[],
        &program.display().to_string(),
        verb,
        current,
        CredentialCommandScope {
            commands,
            cancel,
            run_deadline,
            timeout,
        },
    )
    .await
}

pub(super) async fn run_program_parts(
    program: &Path,
    arguments: &[String],
    label: &str,
    verb: &str,
    current: Option<&AuthEntry>,
    scope: CredentialCommandScope<'_>,
) -> Result<AuthEntry, AuthError> {
    let mut argv = Vec::with_capacity(arguments.len() + 2);
    argv.push(program.as_os_str().to_os_string());
    argv.extend(arguments.iter().map(OsString::from));
    argv.push(OsString::from(verb));
    let stdin = current
        .map(|entry| {
            let encoded = encode_entry(entry).map_err(|reason| missing_label(label, reason))?;
            serde_json::to_vec(&encoded)
                .map(SecretMaterial::new)
                .map_err(|error| missing_label(label, error.to_string()))
        })
        .transpose()?;
    let stdout = scope
        .commands
        .run(CredentialCommandRequest {
            argv,
            stdin,
            cancel: scope.cancel.clone(),
            run_deadline: scope.run_deadline,
            timeout: scope.timeout,
        })
        .await
        .map_err(|failure| match failure {
            failure @ CredentialCommandFailure::Exit { .. } => {
                missing_label(label, failure.to_string())
            }
            CredentialCommandFailure::Io(reason) => missing_label(label, reason),
            failure => AuthError::CredentialCommand {
                program: label.to_string(),
                failure,
            },
        })?;
    parse_program_output(label, stdout.expose_secret_bytes())
}

pub(super) fn parse_program_output(program: &str, stdout: &[u8]) -> Result<AuthEntry, AuthError> {
    let text = std::str::from_utf8(stdout)
        .map_err(|err| missing_label(program, err.to_string()))?
        .trim();
    if text.is_empty() {
        return Err(missing_label(program, "printed an empty credential"));
    }
    #[derive(Deserialize)]
    struct Blob {
        access: String,
        #[serde(default)]
        refresh: Option<String>,
        #[serde(default)]
        expires: Option<u64>,
    }
    match serde_json::from_str::<Blob>(text) {
        Ok(blob) => Ok(AuthEntry::OAuth {
            access: blob.access.into(),
            refresh: blob.refresh.map(Into::into),
            expires: blob.expires,
        }),
        Err(_) if text.starts_with('{') => {
            Err(missing_label(program, "printed invalid credential JSON"))
        }
        Err(_) => Ok(AuthEntry::ApiKey { key: text.into() }),
    }
}

pub(super) fn missing_label(program: &str, reason: impl Into<String>) -> AuthError {
    AuthError::MissingCredential {
        program: program.to_string(),
        reason: reason.into(),
    }
}

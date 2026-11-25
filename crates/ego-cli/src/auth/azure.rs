//! Azure OpenAI Entra credential acquisition and storage binding.

use super::{AuthEntry, AuthError, now_ms};
use ego_core::config::ConnectionConfig;

const ENTRA_SCOPE: &str = "https://ai.azure.com/.default";

/// Add the admitted resource and platform scope to both login and refresh.
pub(super) fn scoped_command(
    account: &ConnectionConfig,
    command: Option<&str>,
) -> Result<String, AuthError> {
    let resource = account
        .azure_resource
        .as_ref()
        .filter(|resource| resource.is_azure_openai_v1())
        .ok_or_else(changed_binding)?;
    let command = command.ok_or_else(missing_command)?;
    let mut argv = shell_words::split(command).map_err(|_| missing_command())?;
    if !argv
        .first()
        .is_some_and(|program| std::path::Path::new(program).is_absolute())
    {
        return Err(missing_command());
    }
    for argument in argv.iter().skip(1) {
        let option = argument
            .split_once('=')
            .map_or(argument.as_str(), |(name, _)| name);
        if matches!(option, "--resource" | "--scope" | "--") {
            return Err(AuthError::InvalidConfig(format!(
                "remove {option} from auth.azure-openai.credential_command; Ego supplies the admitted resource and scope"
            )));
        }
    }
    argv.extend([
        "--resource".to_owned(),
        resource.as_url().as_str().to_owned(),
        "--scope".to_owned(),
        ENTRA_SCOPE.to_owned(),
    ]);
    Ok(shell_words::join(argv))
}

/// The normalized command contains the exact admitted resource and scope.
pub(super) fn binding(command: &str) -> String {
    ego_core::digest::sha256_hex(command.as_bytes())
}

pub(super) fn bind_entry(mut entry: AuthEntry, binding: String) -> Result<AuthEntry, AuthError> {
    if let AuthEntry::OAuth {
        expires: Some(expires),
        access,
        scope_binding,
        ..
    } = &mut entry
        && *expires > now_ms()
        && !access.expose_secret_bytes().is_empty()
    {
        *scope_binding = Some(binding);
        return Ok(entry);
    }
    Err(AuthError::InvalidConfig(
        "azure-openai credential command must return an Entra access token and a future expires timestamp in milliseconds".into(),
    ))
}

pub(super) fn missing_command() -> AuthError {
    AuthError::InvalidConfig(
        "configure auth.azure-openai.credential_command with an absolute executable path".into(),
    )
}

pub(super) fn changed_binding() -> AuthError {
    AuthError::InvalidConfig(
        "saved OAuth credential has a different resource or command binding; renew the selected account".into(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // Catches: a configured option overrides the admitted resource or scope,
    // or an option terminator prevents the helper from parsing either flag.
    #[test]
    fn azure_reserved_options_refuse_before_helper_dispatch() {
        let account: ConnectionConfig = serde_json::from_value(serde_json::json!({
            "name": "Azure",
            "vendor": "azure-openai",
            "access": "oauth",
            "azure_resource": "https://tenant.openai.azure.com/openai/v1/"
        }))
        .unwrap();
        let executable = std::env::current_exe().unwrap();
        let executable = shell_words::join([executable.to_str().unwrap()]);
        for reserved in [
            "--resource different",
            "--resource=different",
            "'--scope' different",
            "--scope=different",
            "--",
        ] {
            let configured = format!("{executable} {reserved}");
            let error = scoped_command(&account, Some(&configured))
                .expect_err("reserved argument must refuse before dispatch");
            assert!(
                error
                    .to_string()
                    .contains("auth.azure-openai.credential_command")
            );
            assert!(error.to_string().contains("remove"));
        }
        let allowed = format!("{executable} --tenant example");
        assert!(scoped_command(&account, Some(&allowed)).is_ok());
    }
}

use axum::{extract::State, Json};
use serde::{Deserialize, Serialize};

use crate::{error::ApiError, state::AppState};

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SecurityProfile {
    #[default]
    Default,
    Loose,
    IWantToNukeMyServer,
}

impl SecurityProfile {
    pub fn allows_unrestricted_shell(self) -> bool {
        matches!(self, Self::Loose | Self::IWantToNukeMyServer)
    }
}

#[derive(Debug, Serialize)]
pub struct SecurityProfileResponse {
    profile: SecurityProfile,
    unrestricted_shell: bool,
    general_command_api: bool,
    passwordless_sudo_expected: bool,
    default_command_allowlist: Vec<String>,
}

pub async fn get_security_profile(State(state): State<AppState>) -> Json<SecurityProfileResponse> {
    let profile = state.config.security.profile;
    Json(SecurityProfileResponse {
        profile,
        unrestricted_shell: profile.allows_unrestricted_shell(),
        general_command_api: profile.allows_unrestricted_shell(),
        passwordless_sudo_expected: profile == SecurityProfile::IWantToNukeMyServer,
        default_command_allowlist: state.config.security.default_command_allowlist.clone(),
    })
}

pub fn validate_default_shell(command: &str, allowlist: &[String]) -> Result<Vec<String>, ApiError> {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return Err(ApiError::BadRequest("command must not be empty".into()));
    }
    if trimmed.len() > 16_384 {
        return Err(ApiError::BadRequest("command is too large".into()));
    }

    const FORBIDDEN: [&str; 16] = [
        ";", "&&", "||", "|", ">", "<", "`", "$(", "${", "\n", "\r", "\\\n", "*", "?", "[", "]",
    ];
    if FORBIDDEN.iter().any(|needle| trimmed.contains(needle)) {
        return Err(ApiError::Forbidden(
            "default profile raw commands cannot use shell metacharacters or chaining".into(),
        ));
    }

    let argv = trimmed
        .split_ascii_whitespace()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let program = argv
        .first()
        .ok_or_else(|| ApiError::BadRequest("command must contain a program".into()))?;
    if !allowlist.iter().any(|allowed| allowed == program) {
        return Err(ApiError::Forbidden(format!(
            "program {program:?} is not allowed by the default security profile"
        )));
    }
    Ok(argv)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allowlist() -> Vec<String> {
        vec!["ls".into(), "cat".into()]
    }

    #[test]
    fn default_accepts_allowlisted_simple_command() {
        assert_eq!(
            validate_default_shell("ls -la", &allowlist()).unwrap(),
            vec!["ls", "-la"]
        );
    }

    #[test]
    fn default_rejects_shell_chaining() {
        assert!(validate_default_shell("ls; id", &allowlist()).is_err());
        assert!(validate_default_shell("ls | cat", &allowlist()).is_err());
    }

    #[test]
    fn default_rejects_non_allowlisted_program() {
        assert!(validate_default_shell("bash -c id", &allowlist()).is_err());
    }
}

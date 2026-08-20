use serde::{Deserialize, Serialize};

use crate::error::ApiError;

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

pub fn validate_default_shell(command: &str, allowlist: &[String]) -> Result<Vec<String>, ApiError> {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return Err(ApiError::BadRequest("command must not be empty".into()));
    }
    if trimmed.len() > 16_384 {
        return Err(ApiError::BadRequest("command is too large".into()));
    }

    // Default profile intentionally does not provide shell grammar. This blocks
    // chaining, redirection, substitution, globbing tricks, and multiline scripts.
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
        vec!["git".into(), "ls".into()]
    }

    #[test]
    fn default_accepts_allowlisted_simple_command() {
        assert_eq!(
            validate_default_shell("git status --short", &allowlist()).unwrap(),
            vec!["git", "status", "--short"]
        );
    }

    #[test]
    fn default_rejects_shell_chaining() {
        assert!(validate_default_shell("ls; id", &allowlist()).is_err());
        assert!(validate_default_shell("git status | cat", &allowlist()).is_err());
    }

    #[test]
    fn default_rejects_non_allowlisted_program() {
        assert!(validate_default_shell("bash -c id", &allowlist()).is_err());
    }
}

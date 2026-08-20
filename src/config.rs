use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::security::SecurityProfile;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    #[serde(default)] pub server: ServerConfig,
    #[serde(default)] pub auth: AuthConfig,
    #[serde(default)] pub security: SecurityConfig,
    #[serde(default)] pub limits: LimitsConfig,
    #[serde(default)] pub lsp: LspConfig,
    #[serde(default)] pub workspaces: BTreeMap<String, WorkspaceConfig>,
    #[serde(default)] pub user_files: BTreeMap<String, UserFilesConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ServerConfig {
    #[serde(default = "default_listen")]
    pub listen: String,
}
impl Default for ServerConfig {
    fn default() -> Self { Self { listen: default_listen() } }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AuthConfig {
    #[serde(default = "default_api_key_env")]
    pub api_key_env: String,
}
impl Default for AuthConfig {
    fn default() -> Self { Self { api_key_env: default_api_key_env() } }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SecurityConfig {
    #[serde(default)]
    pub profile: SecurityProfile,
    #[serde(default = "default_command_allowlist")]
    pub default_command_allowlist: Vec<String>,
}
impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            profile: SecurityProfile::Default,
            default_command_allowlist: default_command_allowlist(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LimitsConfig {
    #[serde(default = "default_max_body_bytes")] pub max_body_bytes: usize,
    #[serde(default = "default_max_in_flight")] pub max_in_flight: usize,
    #[serde(default = "default_command_timeout")] pub command_timeout_seconds: u64,
    #[serde(default = "default_max_command_timeout")] pub max_command_timeout_seconds: u64,
    #[serde(default = "default_command_output_bytes")] pub command_output_bytes: usize,
    #[serde(default = "default_max_jobs")] pub max_jobs: usize,
    #[serde(default = "default_job_retention_seconds")] pub job_retention_seconds: u64,
}
impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            max_body_bytes: default_max_body_bytes(),
            max_in_flight: default_max_in_flight(),
            command_timeout_seconds: default_command_timeout(),
            max_command_timeout_seconds: default_max_command_timeout(),
            command_output_bytes: default_command_output_bytes(),
            max_jobs: default_max_jobs(),
            job_retention_seconds: default_job_retention_seconds(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct LspConfig {
    #[serde(default)]
    pub servers: BTreeMap<String, LspServerConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LspServerConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub argv: Vec<String>,
    #[serde(default)]
    pub extensions: Vec<String>,
    pub language_id: Option<String>,
    pub initialization_options: Option<Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WorkspaceConfig {
    pub root: PathBuf,
    #[serde(default)] pub capabilities: Capabilities,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Capabilities {
    #[serde(default)] pub fs_read: bool,
    #[serde(default)] pub fs_write: bool,
    #[serde(default)] pub commands: bool,
    #[serde(default)] pub git_read: bool,
    #[serde(default)] pub git_write: bool,
    #[serde(default)] pub git_network: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct UserFilesConfig {
    pub root: PathBuf,
    #[serde(default)] pub read: bool,
    #[serde(default)] pub write: bool,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let raw = fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(), source,
        })?;
        let config: Self = toml::from_str(&raw).map_err(ConfigError::Parse)?;
        config.validate()?;
        Ok(config)
    }

    pub fn api_key(&self) -> Result<String, ConfigError> {
        let value = env::var(&self.auth.api_key_env)
            .map_err(|_| ConfigError::MissingSecret(self.auth.api_key_env.clone()))?;
        if value.len() < 32 || value.trim() != value || value.chars().any(char::is_control) {
            return Err(ConfigError::Invalid(format!(
                "API key from {} must be at least 32 characters and contain no surrounding whitespace or control characters",
                self.auth.api_key_env
            )));
        }
        Ok(value)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        if self.server.listen.trim().is_empty() {
            return Err(ConfigError::Invalid("server.listen must not be empty".into()));
        }
        if self.auth.api_key_env.trim().is_empty() {
            return Err(ConfigError::Invalid("auth.api_key_env must not be empty".into()));
        }
        if self.security.default_command_allowlist.is_empty() {
            return Err(ConfigError::Invalid("security.default_command_allowlist must not be empty".into()));
        }
        for program in &self.security.default_command_allowlist {
            if program.trim().is_empty() || program.starts_with('-') || program.contains('/') || program.chars().any(char::is_whitespace) {
                return Err(ConfigError::Invalid(format!("invalid default command allowlist entry {program:?}")));
            }
        }
        for (name, server) in &self.lsp.servers {
            if name.trim().is_empty() {
                return Err(ConfigError::Invalid("LSP server names must not be empty".into()));
            }
            if server.argv.is_empty() || server.argv[0].trim().is_empty() {
                return Err(ConfigError::Invalid(format!("LSP server {name:?} must define a non-empty argv")));
            }
            if server.argv.iter().any(|arg| arg.as_bytes().contains(&0)) {
                return Err(ConfigError::Invalid(format!("LSP server {name:?} argv contains a NUL byte")));
            }
            if server.extensions.iter().any(|ext| ext.trim_matches('.').is_empty() || ext.chars().any(char::is_whitespace)) {
                return Err(ConfigError::Invalid(format!("LSP server {name:?} has an invalid extension mapping")));
            }
        }
        if self.limits.max_body_bytes < 1024 || self.limits.max_body_bytes > 64 * 1024 * 1024 {
            return Err(ConfigError::Invalid("limits.max_body_bytes must be between 1024 and 67108864".into()));
        }
        if self.limits.max_in_flight == 0 || self.limits.max_in_flight > 1024 {
            return Err(ConfigError::Invalid("limits.max_in_flight must be between 1 and 1024".into()));
        }
        if self.limits.command_timeout_seconds == 0
            || self.limits.max_command_timeout_seconds == 0
            || self.limits.command_timeout_seconds > self.limits.max_command_timeout_seconds
            || self.limits.max_command_timeout_seconds > 3600
        {
            return Err(ConfigError::Invalid("command timeout limits are invalid or exceed one hour".into()));
        }
        if self.limits.command_output_bytes == 0 || self.limits.command_output_bytes > 16 * 1024 * 1024 {
            return Err(ConfigError::Invalid("limits.command_output_bytes must be between 1 and 16777216".into()));
        }
        if self.limits.max_jobs == 0 || self.limits.max_jobs > 4096 {
            return Err(ConfigError::Invalid("limits.max_jobs must be between 1 and 4096".into()));
        }
        validate_roots("workspace", self.workspaces.iter().map(|(name, cfg)| (name, &cfg.root)))?;
        validate_roots("user-files mount", self.user_files.iter().map(|(name, cfg)| (name, &cfg.root)))?;
        Ok(())
    }
}

fn validate_roots<'a>(kind: &str, roots: impl Iterator<Item = (&'a String, &'a PathBuf)>) -> Result<(), ConfigError> {
    for (name, root) in roots {
        if name.trim().is_empty() {
            return Err(ConfigError::Invalid(format!("{kind} names must not be empty")));
        }
        if !root.is_absolute() {
            return Err(ConfigError::Invalid(format!("{kind} {name:?} root must be an absolute path")));
        }
    }
    Ok(())
}

fn default_listen() -> String { "127.0.0.1:8787".into() }
fn default_api_key_env() -> String { "TUXBRIDGE_API_KEY".into() }
fn default_max_body_bytes() -> usize { 10 * 1024 * 1024 }
fn default_max_in_flight() -> usize { 32 }
fn default_command_timeout() -> u64 { 120 }
fn default_max_command_timeout() -> u64 { 900 }
fn default_command_output_bytes() -> usize { 2 * 1024 * 1024 }
fn default_max_jobs() -> usize { 128 }
fn default_job_retention_seconds() -> u64 { 3600 }
fn default_true() -> bool { true }
fn default_command_allowlist() -> Vec<String> {
    ["pwd", "ls", "cat", "head", "tail", "wc", "grep", "stat", "du", "df", "uname", "id", "whoami"]
        .into_iter().map(str::to_owned).collect()
}

#[derive(Debug)]
pub enum ConfigError {
    Read { path: PathBuf, source: std::io::Error },
    Parse(toml::de::Error),
    MissingSecret(String),
    Invalid(String),
}
impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Read { path, source } => write!(f, "failed to read {}: {source}", path.display()),
            Self::Parse(source) => write!(f, "failed to parse configuration: {source}"),
            Self::MissingSecret(name) => write!(f, "required API key environment variable {name} is missing"),
            Self::Invalid(message) => write!(f, "invalid configuration: {message}"),
        }
    }
}
impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Read { source, .. } => Some(source),
            Self::Parse(source) => Some(source),
            Self::MissingSecret(_) | Self::Invalid(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn rejects_relative_workspace_root() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(file, "[workspaces.demo]\nroot = \"relative/path\"").unwrap();
        let err = Config::load(file.path()).unwrap_err();
        assert!(err.to_string().contains("absolute path"));
    }

    #[test]
    fn applies_defaults() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(file, "[workspaces.demo]\nroot = \"/tmp\"").unwrap();
        let config = Config::load(file.path()).unwrap();
        assert_eq!(config.server.listen, "127.0.0.1:8787");
        assert_eq!(config.auth.api_key_env, "TUXBRIDGE_API_KEY");
        assert_eq!(config.limits.max_in_flight, 32);
        assert_eq!(config.security.profile, SecurityProfile::Default);
        assert!(config.security.default_command_allowlist.iter().any(|v| v == "ls"));
        assert!(!config.security.default_command_allowlist.iter().any(|v| v == "python3"));
        assert!(config.lsp.servers.is_empty());
    }
}

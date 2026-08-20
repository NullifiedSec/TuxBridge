use std::{collections::BTreeMap, env, fs, path::{Path, PathBuf}};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub auth: AuthConfig,
    #[serde(default)]
    pub workspaces: BTreeMap<String, WorkspaceConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ServerConfig {
    #[serde(default = "default_listen")]
    pub listen: String,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: default_listen(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AuthConfig {
    #[serde(default = "default_api_key_env")]
    pub api_key_env: String,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            api_key_env: default_api_key_env(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WorkspaceConfig {
    pub root: PathBuf,
    #[serde(default)]
    pub capabilities: Capabilities,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Capabilities {
    #[serde(default)]
    pub fs_read: bool,
    #[serde(default)]
    pub fs_write: bool,
    #[serde(default)]
    pub commands: bool,
    #[serde(default)]
    pub git_read: bool,
    #[serde(default)]
    pub git_write: bool,
    #[serde(default)]
    pub git_network: bool,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let raw = fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        let config: Self = toml::from_str(&raw).map_err(ConfigError::Parse)?;
        config.validate()?;
        Ok(config)
    }

    pub fn api_key(&self) -> Result<String, ConfigError> {
        let value = env::var(&self.auth.api_key_env)
            .map_err(|_| ConfigError::MissingSecret(self.auth.api_key_env.clone()))?;
        if value.trim().is_empty() {
            return Err(ConfigError::MissingSecret(self.auth.api_key_env.clone()));
        }
        Ok(value)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        if self.server.listen.trim().is_empty() {
            return Err(ConfigError::Invalid("server.listen must not be empty".into()));
        }

        for (name, workspace) in &self.workspaces {
            if name.trim().is_empty() {
                return Err(ConfigError::Invalid("workspace names must not be empty".into()));
            }
            if !workspace.root.is_absolute() {
                return Err(ConfigError::Invalid(format!(
                    "workspace {name:?} root must be an absolute path"
                )));
            }
        }

        Ok(())
    }
}

fn default_listen() -> String {
    "127.0.0.1:8787".into()
}

fn default_api_key_env() -> String {
    "TUXBRIDGE_API_KEY".into()
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
            Self::MissingSecret(name) => write!(f, "required API key environment variable {name} is missing or empty"),
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
    fn applies_server_and_auth_defaults() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(file, "[workspaces.demo]\nroot = \"/tmp\"").unwrap();

        let config = Config::load(file.path()).unwrap();
        assert_eq!(config.server.listen, "127.0.0.1:8787");
        assert_eq!(config.auth.api_key_env, "TUXBRIDGE_API_KEY");
    }
}

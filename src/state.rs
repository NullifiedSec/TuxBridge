use std::sync::Arc;

use crate::config::{Config, ConfigError};

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    api_key: Arc<str>,
}

impl AppState {
    pub fn new(config: Config) -> Result<Self, ConfigError> {
        let api_key = config.api_key()?;
        Ok(Self {
            config: Arc::new(config),
            api_key: Arc::from(api_key),
        })
    }

    pub fn api_key(&self) -> &str {
        &self.api_key
    }
}

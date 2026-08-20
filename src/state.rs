use std::sync::Arc;

use crate::{
    command::JobStore,
    config::{Config, ConfigError},
};

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub jobs: JobStore,
    api_key: Arc<str>,
}

impl AppState {
    pub fn new(config: Config) -> Result<Self, ConfigError> {
        let api_key = config.api_key()?;
        Ok(Self {
            config: Arc::new(config),
            jobs: JobStore::default(),
            api_key: Arc::from(api_key),
        })
    }

    pub fn api_key(&self) -> &str {
        &self.api_key
    }
}

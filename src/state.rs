use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use tokio::sync::Semaphore;

use crate::{
    command::JobStore,
    config::{Config, ConfigError},
};

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub jobs: JobStore,
    pub request_gate: Arc<Semaphore>,
    request_sequence: Arc<AtomicU64>,
    api_key: Arc<str>,
}

impl AppState {
    pub fn new(config: Config) -> Result<Self, ConfigError> {
        let api_key = config.api_key()?;
        let jobs = JobStore::new(
            config.limits.max_jobs,
            config.limits.job_retention_seconds,
        );
        let request_gate = Arc::new(Semaphore::new(config.limits.max_in_flight));

        Ok(Self {
            config: Arc::new(config),
            jobs,
            request_gate,
            request_sequence: Arc::new(AtomicU64::new(0)),
            api_key: Arc::from(api_key),
        })
    }

    pub fn api_key(&self) -> &str {
        &self.api_key
    }

    pub fn next_request_id(&self) -> String {
        let sequence = self.request_sequence.fetch_add(1, Ordering::Relaxed) + 1;
        format!("tb-{sequence:016x}")
    }
}

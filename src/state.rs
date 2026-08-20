use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use tokio::sync::Semaphore;

use crate::{
    approvals::ApprovalStore,
    audit::AuditStore,
    command::JobStore,
    config::{AuthRole, Config, ConfigError},
    events::EventHub,
    sessions::SessionStore,
};

#[derive(Debug, Clone)]
pub struct PrincipalCredential {
    pub name: Arc<str>,
    pub key: Arc<str>,
    pub roles: Arc<[AuthRole]>,
}

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub jobs: JobStore,
    pub audit: AuditStore,
    pub events: EventHub,
    pub sessions: SessionStore,
    pub approvals: ApprovalStore,
    pub request_gate: Arc<Semaphore>,
    pub principals: Arc<[PrincipalCredential]>,
    request_sequence: Arc<AtomicU64>,
}

impl AppState {
    pub fn new(config: Config) -> Result<Self, ConfigError> {
        let principals = config
            .auth_principals()?
            .into_iter()
            .map(|principal| PrincipalCredential {
                name: Arc::from(principal.name),
                key: Arc::from(principal.key),
                roles: Arc::from(principal.roles),
            })
            .collect::<Vec<_>>();
        let jobs = JobStore::new(
            config.limits.max_jobs,
            config.limits.job_retention_seconds,
        );
        let request_gate = Arc::new(Semaphore::new(config.limits.max_in_flight));

        Ok(Self {
            config: Arc::new(config),
            jobs,
            audit: AuditStore::default(),
            events: EventHub::default(),
            sessions: SessionStore::default(),
            approvals: ApprovalStore::default(),
            request_gate,
            principals: Arc::from(principals),
            request_sequence: Arc::new(AtomicU64::new(0)),
        })
    }

    pub fn next_request_id(&self) -> String {
        let sequence = self.request_sequence.fetch_add(1, Ordering::Relaxed) + 1;
        format!("tb-{sequence:016x}")
    }
}

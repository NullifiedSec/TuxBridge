use std::{collections::VecDeque, sync::Arc, time::{SystemTime, UNIX_EPOCH}};

use axum::{extract::State, Json};
use serde::Serialize;
use tokio::sync::Mutex;

use crate::state::AppState;

const MAX_AUDIT_EVENTS: usize = 1024;

#[derive(Debug, Clone, Serialize)]
pub struct AuditEvent {
    pub sequence: u64,
    pub timestamp_unix_ms: u128,
    pub request_id: String,
    pub method: String,
    pub path: String,
    pub status: u16,
    pub duration_ms: u128,
}

#[derive(Clone, Default)]
pub struct AuditStore {
    inner: Arc<Mutex<VecDeque<AuditEvent>>>,
}

impl AuditStore {
    pub async fn push(&self, event: AuditEvent) {
        let mut events = self.inner.lock().await;
        events.push_back(event);
        while events.len() > MAX_AUDIT_EVENTS {
            events.pop_front();
        }
    }

    pub async fn snapshot(&self) -> Vec<AuditEvent> {
        self.inner.lock().await.iter().cloned().collect()
    }
}

pub async fn list_events(State(state): State<AppState>) -> Json<Vec<AuditEvent>> {
    Json(state.audit.snapshot().await)
}

pub fn unix_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

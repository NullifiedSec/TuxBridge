use std::{collections::VecDeque, convert::Infallible, sync::{Arc, atomic::{AtomicU64, Ordering}}, time::{SystemTime, UNIX_EPOCH}};

use axum::{extract::State, response::sse::{Event, KeepAlive, Sse}};
use serde::Serialize;
use serde_json::Value;
use tokio::sync::{Mutex, broadcast};
use tokio_stream::{StreamExt, wrappers::BroadcastStream};

use crate::state::AppState;

const EVENT_RING: usize = 2048;

#[derive(Debug, Clone, Serialize)]
pub struct AgentEvent {
    pub sequence: u64,
    pub timestamp_unix_ms: u128,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    pub summary: String,
    #[serde(skip_serializing_if = "Value::is_null")]
    pub data: Value,
}

#[derive(Clone)]
pub struct EventHub {
    tx: broadcast::Sender<AgentEvent>,
    ring: Arc<Mutex<VecDeque<AgentEvent>>>,
    next: Arc<AtomicU64>,
}

impl Default for EventHub {
    fn default() -> Self {
        let (tx, _) = broadcast::channel(1024);
        Self { tx, ring: Arc::new(Mutex::new(VecDeque::new())), next: Arc::new(AtomicU64::new(0)) }
    }
}

impl EventHub {
    pub async fn emit(&self, kind: impl Into<String>, workspace: Option<&str>, summary: impl Into<String>, data: Value) {
        let event = AgentEvent {
            sequence: self.next.fetch_add(1, Ordering::Relaxed) + 1,
            timestamp_unix_ms: SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis(),
            kind: kind.into(),
            workspace: workspace.map(str::to_owned),
            summary: summary.into(),
            data,
        };
        {
            let mut ring = self.ring.lock().await;
            ring.push_back(event.clone());
            while ring.len() > EVENT_RING { ring.pop_front(); }
        }
        let _ = self.tx.send(event);
    }

    pub async fn snapshot(&self) -> Vec<AgentEvent> {
        self.ring.lock().await.iter().cloned().collect()
    }

    fn subscribe(&self) -> broadcast::Receiver<AgentEvent> { self.tx.subscribe() }
}

pub async fn list_events(State(state): State<AppState>) -> axum::Json<Vec<AgentEvent>> {
    axum::Json(state.events.snapshot().await)
}

pub async fn stream_events(State(state): State<AppState>) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let stream = BroadcastStream::new(state.events.subscribe()).filter_map(|item| match item {
        Ok(event) => serde_json::to_string(&event).ok().map(|json| Ok(Event::default().event(event.kind).data(json))),
        Err(_) => None,
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

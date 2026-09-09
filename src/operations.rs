use std::{
    collections::HashMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::{
    Json,
    extract::{Path as AxumPath, State},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tempfile::NamedTempFile;
use tokio::{sync::Mutex, time};

use crate::{config::OperationPolicy, error::ApiError, state::AppState};

const OPERATIONS_DIR: &str = "operations";
const MAX_WAIT_MS: u64 = 25_000;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OperationStatus {
    PendingApproval,
    Executing,
    Completed,
    Failed,
    Denied,
    Blocked,
}

impl OperationStatus {
    fn terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Denied | Self::Blocked
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperationRecord {
    pub id: String,
    pub session: String,
    pub workspace: String,
    pub tool: String,
    pub args: Value,
    pub policy: OperationPolicy,
    pub status: OperationStatus,
    pub created_at_unix_ms: u128,
    pub updated_at_unix_ms: u128,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone)]
pub struct OperationStore {
    inner: Arc<Mutex<HashMap<String, OperationRecord>>>,
    root: Arc<PathBuf>,
    next: Arc<AtomicU64>,
}

#[derive(Debug, Deserialize)]
pub struct WaitOperationRequest {
    pub session: String,
    pub operation_id: String,
    pub timeout_ms: Option<u64>,
}

impl OperationStore {
    pub fn open(state_root: &Path) -> Result<Self, String> {
        let root = state_root.join(OPERATIONS_DIR);
        fs::create_dir_all(&root)
            .map_err(|e| format!("failed to create operation state directory: {e}"))?;
        let mut items = HashMap::new();
        let mut max_sequence = 0u64;
        for entry in
            fs::read_dir(&root).map_err(|e| format!("failed to read operation state: {e}"))?
        {
            let entry = entry.map_err(|e| format!("failed to inspect operation state: {e}"))?;
            if !entry.file_type().map_err(|e| e.to_string())?.is_file() {
                continue;
            }
            if entry.path().extension().and_then(|v| v.to_str()) != Some("json") {
                continue;
            }
            let bytes = fs::read(entry.path()).map_err(|e| e.to_string())?;
            let mut record: OperationRecord = serde_json::from_slice(&bytes).map_err(|e| {
                format!("failed to parse operation {}: {e}", entry.path().display())
            })?;
            if matches!(record.status, OperationStatus::Executing) {
                record.status = OperationStatus::Failed;
                record.error = Some(
                    "TuxBridge restarted while the operation was executing; execution was not replayed automatically".into(),
                );
                record.updated_at_unix_ms = now_ms();
            }
            if let Some(sequence) = record
                .id
                .strip_prefix("op-")
                .and_then(|v| u64::from_str_radix(v, 16).ok())
            {
                max_sequence = max_sequence.max(sequence);
            }
            items.insert(record.id.clone(), record);
        }
        for record in items.values() {
            if record.status == OperationStatus::Failed
                && record
                    .error
                    .as_deref()
                    .is_some_and(|error| error.contains("not replayed automatically"))
            {
                persist_record(&root, record).map_err(|error| error.to_string())?;
            }
        }
        Ok(Self {
            inner: Arc::new(Mutex::new(items)),
            root: Arc::new(root),
            next: Arc::new(AtomicU64::new(max_sequence)),
        })
    }

    pub async fn create(
        &self,
        session: String,
        workspace: String,
        tool: String,
        args: Value,
        policy: OperationPolicy,
    ) -> Result<OperationRecord, ApiError> {
        let id = format!("op-{:016x}", self.next.fetch_add(1, Ordering::Relaxed) + 1);
        let status = match policy {
            OperationPolicy::Automatic => OperationStatus::Executing,
            OperationPolicy::Confirm => OperationStatus::PendingApproval,
            OperationPolicy::Blocked => OperationStatus::Blocked,
        };
        let now = now_ms();
        let record = OperationRecord {
            id: id.clone(),
            session,
            workspace,
            tool,
            args,
            policy,
            status,
            created_at_unix_ms: now,
            updated_at_unix_ms: now,
            result: None,
            error: None,
        };
        self.persist(&record)?;
        self.inner.lock().await.insert(id, record.clone());
        Ok(record)
    }

    pub async fn get(&self, id: &str) -> Result<OperationRecord, ApiError> {
        self.inner
            .lock()
            .await
            .get(id)
            .cloned()
            .ok_or_else(|| ApiError::NotFound(format!("operation {id:?} not found")))
    }
    pub async fn list(&self) -> Vec<OperationRecord> {
        let mut values = self
            .inner
            .lock()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        values.sort_by_key(|v| v.created_at_unix_ms);
        values
    }

    pub async fn begin_approved(&self, id: &str) -> Result<OperationRecord, ApiError> {
        let mut items = self.inner.lock().await;
        let record = items
            .get_mut(id)
            .ok_or_else(|| ApiError::NotFound(format!("operation {id:?} not found")))?;
        if record.status != OperationStatus::PendingApproval {
            return Err(ApiError::Conflict(
                "operation is not waiting for approval".into(),
            ));
        }
        record.status = OperationStatus::Executing;
        record.updated_at_unix_ms = now_ms();
        self.persist(record)?;
        Ok(record.clone())
    }

    pub async fn deny(&self, id: &str) -> Result<OperationRecord, ApiError> {
        let mut items = self.inner.lock().await;
        let record = items
            .get_mut(id)
            .ok_or_else(|| ApiError::NotFound(format!("operation {id:?} not found")))?;
        if record.status != OperationStatus::PendingApproval {
            return Err(ApiError::Conflict(
                "operation is not waiting for approval".into(),
            ));
        }
        record.status = OperationStatus::Denied;
        record.updated_at_unix_ms = now_ms();
        self.persist(record)?;
        Ok(record.clone())
    }

    pub async fn finish(
        &self,
        id: &str,
        outcome: Result<Value, ApiError>,
    ) -> Result<OperationRecord, ApiError> {
        let mut items = self.inner.lock().await;
        let record = items
            .get_mut(id)
            .ok_or_else(|| ApiError::NotFound(format!("operation {id:?} not found")))?;
        match outcome {
            Ok(value) => {
                record.status = OperationStatus::Completed;
                record.result = Some(value);
                record.error = None;
            }
            Err(error) => {
                record.status = OperationStatus::Failed;
                record.result = None;
                record.error = Some(error.to_string());
            }
        }
        record.updated_at_unix_ms = now_ms();
        self.persist(record)?;
        Ok(record.clone())
    }

    fn persist(&self, record: &OperationRecord) -> Result<(), ApiError> {
        persist_record(self.root.as_ref(), record)
    }
}

pub async fn list_operations(State(state): State<AppState>) -> Json<Vec<OperationRecord>> {
    Json(state.operations.list().await)
}

pub async fn approve(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<OperationRecord>, ApiError> {
    let record = state.operations.begin_approved(&id).await?;
    let (canonical, workspace) = state.sessions.active_context(&record.session).await?;
    if canonical != record.session || workspace != record.workspace {
        return Err(ApiError::Conflict(
            "operation session/workspace binding changed".into(),
        ));
    }
    state
        .events
        .emit(
            "operation.approved",
            Some(&record.workspace),
            format!("approved {}", record.tool),
            serde_json::json!({
                "session": record.session.clone(),
                "operation_id": record.id.clone(),
                "tool": record.tool.clone()
            }),
        )
        .await;
    let outcome = crate::agent_ops::execute_frozen(&state, &record).await;
    let final_record = state.operations.finish(&id, outcome).await?;
    state
        .events
        .emit(
            "operation.finished",
            Some(&final_record.workspace),
            format!(
                "{} finished as {:?}",
                final_record.tool, final_record.status
            ),
            serde_json::json!({
                "session": final_record.session.clone(),
                "operation_id": final_record.id.clone(),
                "tool": final_record.tool.clone(),
                "status": final_record.status
            }),
        )
        .await;
    Ok(Json(final_record))
}

pub async fn deny(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<OperationRecord>, ApiError> {
    let record = state.operations.deny(&id).await?;
    state
        .events
        .emit(
            "operation.denied",
            Some(&record.workspace),
            format!("denied {}", record.tool),
            serde_json::json!({
                "session": record.session.clone(),
                "operation_id": record.id.clone(),            "tool": record.tool.clone()
            }),
        )
        .await;
    Ok(Json(record))
}

pub async fn wait_for_operation(
    State(state): State<AppState>,
    Json(request): Json<WaitOperationRequest>,
) -> Result<Json<OperationRecord>, ApiError> {
    let (canonical, _) = state.sessions.active_context(&request.session).await?;
    let timeout = request.timeout_ms.unwrap_or(20_000).clamp(0, MAX_WAIT_MS);
    let started = time::Instant::now();
    loop {
        let record = state.operations.get(&request.operation_id).await?;
        if record.session != canonical {
            return Err(ApiError::Forbidden(
                "operation does not belong to this session".into(),
            ));
        }
        if record.status.terminal() || started.elapsed() >= Duration::from_millis(timeout) {
            return Ok(Json(record));
        }
        time::sleep(Duration::from_millis(100)).await;
    }
}

fn persist_record(root: &Path, record: &OperationRecord) -> Result<(), ApiError> {
    let mut temp = NamedTempFile::new_in(root).map_err(map_io)?;
    serde_json::to_writer_pretty(temp.as_file_mut(), record)
        .map_err(|e| ApiError::Internal(format!("failed to serialize operation state: {e}")))?;
    temp.write_all(b"\n").map_err(map_io)?;
    temp.as_file_mut().sync_all().map_err(map_io)?;
    temp.persist(root.join(format!("{}.json", record.id)))
        .map_err(|e| map_io(e.error))?;
    Ok(())
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn map_io(error: std::io::Error) -> ApiError {
    ApiError::Internal(format!("operation persistence failed: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn pending_operation_survives_reopen_with_frozen_args() {
        let directory = tempfile::tempdir().unwrap();
        let store = OperationStore::open(directory.path()).unwrap();
        let args =
            serde_json::json!({"session":"demo--task--abc12","remote":"origin","branch":"main"});
        let record = store
            .create(
                "demo--task--abc12".into(),
                "demo".into(),
                "gitSync.push".into(),
                args.clone(),
                OperationPolicy::Confirm,
            )
            .await
            .unwrap();
        assert_eq!(record.status, OperationStatus::PendingApproval);
        drop(store);

        let reopened = OperationStore::open(directory.path()).unwrap();
        let recovered = reopened.get(&record.id).await.unwrap();
        assert_eq!(recovered.status, OperationStatus::PendingApproval);
        assert_eq!(recovered.args, args);
    }

    #[tokio::test]
    async fn executing_operation_is_not_replayed_after_restart() {
        let directory = tempfile::tempdir().unwrap();
        let store = OperationStore::open(directory.path()).unwrap();
        let record = store
            .create(
                "demo--task--abc12".into(),
                "demo".into(),
                "runCommand".into(),
                serde_json::json!({"session":"demo--task--abc12","argv":["pwd"]}),
                OperationPolicy::Automatic,
            )
            .await
            .unwrap();
        assert_eq!(record.status, OperationStatus::Executing);
        drop(store);

        let reopened = OperationStore::open(directory.path()).unwrap();
        let recovered = reopened.get(&record.id).await.unwrap();
        assert_eq!(recovered.status, OperationStatus::Failed);
        assert!(
            recovered
                .error
                .unwrap()
                .contains("not replayed automatically")
        );
    }
}

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs,
    path::Path,
    sync::atomic::Ordering,
};

use crate::{error::ApiError, state::AppState};

use super::*;
use super::support::{
    digest, ensure_active, map_io, now_ms, readable_session_ref, resolve_key, safe_existing,
    validate_session, validate_text, view_record,
};

impl SessionStore {
    pub(super) async fn create(
        &self,
        workspace: &str,
        title: String,
        baseline: Option<SessionBaseline>,
    ) -> Result<String, ApiError> {
        let mut sessions = self.inner.lock().await;
        if sessions
            .values()
            .filter(|session| matches!(session.status, SessionStatus::Active))
            .count()
            >= MAX_SESSIONS
        {
            return Err(ApiError::Conflict(format!(
                "active coding session limit of {MAX_SESSIONS} reached"
            )));
        }

        let id = loop {
            let sequence = self.next.fetch_add(1, Ordering::Relaxed) + 1;
            let candidate = readable_session_ref(workspace, &title, sequence);
            if !sessions.contains_key(&candidate) {
                break candidate;
            }
        };
        let now = now_ms();
        let record = SessionRecord {
            id: id.clone(),
            workspace: workspace.into(),
            title,
            summary: None,
            plan: None,
            result_summary: None,
            created_at_unix_ms: now,
            updated_at_unix_ms: now,
            status: SessionStatus::Active,
            baseline,
            todos: Vec::new(),
            next_todo: 0,
            files: BTreeMap::new(),
            bytes: 0,
        };
        self.persistence.save_session(&record)?;
        sessions.insert(id.clone(), record);
        Ok(id)
    }

    pub async fn capture_change(
        &self,
        id: &str,
        workspace: &str,
        relative_path: &str,
        before: &[u8],
        after: &[u8],
    ) -> Result<(), ApiError> {
        let mut sessions = self.inner.lock().await;
        let key = resolve_key(&sessions, id)?;
        let session = sessions
            .get_mut(&key)
            .ok_or_else(|| ApiError::NotFound(format!("coding session {id:?} not found")))?;
        validate_session(session, workspace)?;
        let previous = session.clone();

        if let Some(existing) = session.files.get_mut(relative_path) {
            existing.after_sha256 = digest(after);
            session.updated_at_unix_ms = now_ms();
            if let Err(error) = self.persistence.save_session(session) {
                *session = previous;
                return Err(error);
            }
            return Ok(());
        }
        if session.files.len() >= MAX_FILES_PER_SESSION {
            return Err(ApiError::Conflict(format!(
                "session file limit of {MAX_FILES_PER_SESSION} reached"
            )));
        }
        if session.bytes.saturating_add(before.len()) > MAX_SNAPSHOT_BYTES {
            return Err(ApiError::Conflict(
                "session rollback snapshot budget exceeded".into(),
            ));
        }

        let before_sha256 = digest(before);
        self.persistence
            .write_snapshot(&session.id, &before_sha256, before)?;
        session.bytes += before.len();
        session.files.insert(
            relative_path.into(),
            FileSnapshot {
                before_sha256,
                after_sha256: digest(after),
                before_bytes: before.len(),
            },
        );
        session.updated_at_unix_ms = now_ms();
        if let Err(error) = self.persistence.save_session(session) {
            *session = previous;
            return Err(error);
        }
        Ok(())
    }

    pub async fn workspace_for(&self, session_ref: &str) -> Result<String, ApiError> {
        let sessions = self.inner.lock().await;
        let key = resolve_key(&sessions, session_ref)?;
        let session = sessions
            .get(&key)
            .ok_or_else(|| ApiError::NotFound(format!("coding session {session_ref:?} not found")))?;
        if !matches!(session.status, SessionStatus::Active) {
            return Err(ApiError::Conflict("coding session is not active".into()));
        }
        Ok(session.workspace.clone())
    }

    pub(super) async fn checkpoint(
        &self,
        id: &str,
        workspace: &str,
        root: &Path,
        paths: &[String],
    ) -> Result<(), ApiError> {
        for relative in paths {
            let path = safe_existing(root, relative)?;
            let bytes = fs::read(&path).map_err(map_io)?;
            self.capture_change(id, workspace, relative, &bytes, &bytes)
                .await?;
        }
        Ok(())
    }

    pub(super) async fn refresh(
        &self,
        id: &str,
        workspace: &str,
        root: &Path,
        paths: &[String],
    ) -> Result<(), ApiError> {
        let mut sessions = self.inner.lock().await;
        let key = resolve_key(&sessions, id)?;
        let session = sessions
            .get_mut(&key)
            .ok_or_else(|| ApiError::NotFound(format!("coding session {id:?} not found")))?;
        validate_session(session, workspace)?;
        let previous = session.clone();
        for relative in paths {
            let snapshot = session.files.get_mut(relative).ok_or_else(|| {
                ApiError::Conflict(format!(
                    "{relative:?} was not checkpointed in this session"
                ))
            })?;
            let path = safe_existing(root, relative)?;
            snapshot.after_sha256 = digest(&fs::read(path).map_err(map_io)?);
        }
        session.updated_at_unix_ms = now_ms();
        if let Err(error) = self.persistence.save_session(session) {
            *session = previous;
            return Err(error);
        }
        Ok(())
    }

    pub(super) async fn views(&self, state: &AppState) -> Vec<SessionView> {
        let sessions = self.inner.lock().await;
        let mut views = sessions
            .values()
            .map(|session| view_record(state, session))
            .collect::<Vec<_>>();
        views.sort_by_key(|view| view.created_at_unix_ms);
        views
    }

    pub(super) async fn view(&self, state: &AppState, reference: &str) -> Result<SessionView, ApiError> {
        let sessions = self.inner.lock().await;
        let key = resolve_key(&sessions, reference)?;
        let session = sessions
            .get(&key)
            .ok_or_else(|| ApiError::NotFound(format!("coding session {reference:?} not found")))?;
        Ok(view_record(state, session))
    }

    pub(super) async fn set_summary(&self, reference: &str, summary: String) -> Result<(), ApiError> {
        validate_text("summary", &summary, MAX_SUMMARY_BYTES)?;
        let mut sessions = self.inner.lock().await;
        let key = resolve_key(&sessions, reference)?;
        let session = sessions
            .get_mut(&key)
            .ok_or_else(|| ApiError::NotFound(format!("coding session {reference:?} not found")))?;
        ensure_active(session)?;
        let previous = session.clone();
        session.summary = Some(summary);
        session.updated_at_unix_ms = now_ms();
        if let Err(error) = self.persistence.save_session(session) {
            *session = previous;
            return Err(error);
        }
        Ok(())
    }

    pub(super) async fn set_plan(&self, reference: &str, plan: String) -> Result<(), ApiError> {
        validate_text("plan", &plan, MAX_PLAN_BYTES)?;
        let mut sessions = self.inner.lock().await;
        let key = resolve_key(&sessions, reference)?;
        let session = sessions
            .get_mut(&key)
            .ok_or_else(|| ApiError::NotFound(format!("coding session {reference:?} not found")))?;
        ensure_active(session)?;
        let previous = session.clone();
        session.plan = Some(plan);
        session.updated_at_unix_ms = now_ms();
        if let Err(error) = self.persistence.save_session(session) {
            *session = previous;
            return Err(error);
        }
        Ok(())
    }

    pub(super) async fn complete(
        &self,
        reference: &str,
        result_summary: Option<String>,
    ) -> Result<(), ApiError> {
        if let Some(summary) = result_summary.as_deref() {
            validate_text("result_summary", summary, MAX_SUMMARY_BYTES)?;
        }
        let mut sessions = self.inner.lock().await;
        let key = resolve_key(&sessions, reference)?;
        let session = sessions
            .get_mut(&key)
            .ok_or_else(|| ApiError::NotFound(format!("coding session {reference:?} not found")))?;
        ensure_active(session)?;
        let previous = session.clone();
        session.result_summary = result_summary;
        session.status = SessionStatus::Finalized;
        session.updated_at_unix_ms = now_ms();
        if let Err(error) = self.persistence.save_session(session) {
            *session = previous;
            return Err(error);
        }
        Ok(())
    }

    pub(super) async fn mark_rolled_back(&self, reference: &str) -> Result<(), ApiError> {
        let mut sessions = self.inner.lock().await;
        let key = resolve_key(&sessions, reference)?;
        let session = sessions
            .get_mut(&key)
            .ok_or_else(|| ApiError::NotFound(format!("coding session {reference:?} not found")))?;
        ensure_active(session)?;
        let previous = session.clone();
        session.status = SessionStatus::RolledBack;
        session.updated_at_unix_ms = now_ms();
        if let Err(error) = self.persistence.save_session(session) {
            *session = previous;
            return Err(error);
        }
        Ok(())
    }

    pub(super) fn read_snapshot(&self, session: &str, sha256: &str) -> Result<Vec<u8>, ApiError> {
        self.persistence.read_snapshot(session, sha256)
    }

    pub(super) fn snapshot_available(&self, session: &str, sha256: &str) -> bool {
        self.persistence.snapshot_available(session, sha256)
    }

    pub(super) async fn todos(&self, request: TodoRequest) -> Result<TodoResponse, ApiError> {
        let mut sessions = self.inner.lock().await;
        let reference = match &request {
            TodoRequest::List { session }
            | TodoRequest::Add { session, .. }
            | TodoRequest::Update { session, .. }
            | TodoRequest::Remove { session, .. }
            | TodoRequest::Reorder { session, .. } => session.clone(),
        };
        let key = resolve_key(&sessions, &reference)?;
        let session = sessions.get_mut(&key).ok_or_else(|| {
            ApiError::NotFound(format!("coding session {reference:?} not found"))
        })?;
        let previous = session.clone();
        let mut changed = false;

        match request {
            TodoRequest::List { .. } => {}
            TodoRequest::Add { text, status, .. } => {
                ensure_active(session)?;
                validate_text("todo text", &text, MAX_TODO_BYTES)?;
                if session.todos.len() >= MAX_TODOS {
                    return Err(ApiError::Conflict(format!(
                        "session todo limit of {MAX_TODOS} reached"
                    )));
                }
                session.next_todo += 1;
                session.todos.push(TodoItem {
                    id: format!("todo-{:04}", session.next_todo),
                    text,
                    status: status.unwrap_or(TodoStatus::Pending),
                });
                session.updated_at_unix_ms = now_ms();
                changed = true;
            }
            TodoRequest::Update {
                id, text, status, ..
            } => {
                ensure_active(session)?;
                if text.is_none() && status.is_none() {
                    return Err(ApiError::BadRequest(
                        "todo update must change text or status".into(),
                    ));
                }
                if let Some(value) = text.as_deref() {
                    validate_text("todo text", value, MAX_TODO_BYTES)?;
                }
                let item = session.todos.iter_mut().find(|item| item.id == id).ok_or_else(|| {
                    ApiError::NotFound(format!("todo {id:?} not found in session"))
                })?;
                if let Some(text) = text {
                    item.text = text;
                }
                if let Some(status) = status {
                    item.status = status;
                }
                session.updated_at_unix_ms = now_ms();
                changed = true;
            }
            TodoRequest::Remove { id, .. } => {
                ensure_active(session)?;
                let original = session.todos.len();
                session.todos.retain(|item| item.id != id);
                if session.todos.len() == original {
                    return Err(ApiError::NotFound(format!(
                        "todo {id:?} not found in session"
                    )));
                }
                session.updated_at_unix_ms = now_ms();
                changed = true;
            }
            TodoRequest::Reorder { ids, .. } => {
                ensure_active(session)?;
                let current = session
                    .todos
                    .iter()
                    .map(|item| item.id.clone())
                    .collect::<BTreeSet<_>>();
                let requested = ids.iter().cloned().collect::<BTreeSet<_>>();
                if ids.len() != session.todos.len()
                    || requested.len() != ids.len()
                    || requested != current
                {
                    return Err(ApiError::BadRequest(
                        "todo reorder must contain every current todo id exactly once".into(),
                    ));
                }
                let positions = ids
                    .into_iter()
                    .enumerate()
                    .map(|(index, id)| (id, index))
                    .collect::<HashMap<_, _>>();
                session
                    .todos
                    .sort_by_key(|item| positions.get(&item.id).copied().unwrap_or(usize::MAX));
                session.updated_at_unix_ms = now_ms();
                changed = true;
            }
        }

        if changed {
            if let Err(error) = self.persistence.save_session(session) {
                *session = previous;
                return Err(error);
            }
        }

        Ok(TodoResponse {
            session: session.id.clone(),
            todos: session.todos.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn session_state_survives_store_reopen() {
        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::open(directory.path()).unwrap();
        let session = store
            .create("demo", "Persist agent work".into(), None)
            .await
            .unwrap();
        store
            .set_summary(&session, "Fix the persistent session flow".into())
            .await
            .unwrap();
        store
            .set_plan(&session, "Inspect, edit, verify, review diff".into())
            .await
            .unwrap();
        store
            .todos(TodoRequest::Add {
                session: session.clone(),
                text: "Implement persistence".into(),
                status: Some(TodoStatus::InProgress),
            })
            .await
            .unwrap();
        store
            .capture_change(&session, "demo", "src/lib.rs", b"before", b"after")
            .await
            .unwrap();
        drop(store);

        let reopened = SessionStore::open(directory.path()).unwrap();
        assert_eq!(reopened.workspace_for(&session).await.unwrap(), "demo");
        let sessions = reopened.inner.lock().await;
        let record = sessions.get(&session).unwrap();
        assert_eq!(record.summary.as_deref(), Some("Fix the persistent session flow"));
        assert_eq!(
            record.plan.as_deref(),
            Some("Inspect, edit, verify, review diff")
        );
        assert_eq!(record.todos.len(), 1);
        assert_eq!(record.todos[0].status, TodoStatus::InProgress);
        let snapshot = record.files.get("src/lib.rs").unwrap();
        let snapshot_sha = snapshot.before_sha256.clone();
        drop(sessions);
        assert_eq!(
            reopened.read_snapshot(&session, &snapshot_sha).unwrap(),
            b"before"
        );
    }
}

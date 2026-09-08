use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, atomic::AtomicU64},
};

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::config::Capabilities;

mod agent;
mod legacy;
mod store;
mod support;

pub use agent::{
    choose_session, complete_session, find_workspaces, select_workspace, set_plan, set_summary, todo,
};
pub use legacy::{checkpoint, create_session, finalize, get_session, list_sessions, refresh, rollback};

const MAX_SESSIONS: usize = 64;
const MAX_FILES_PER_SESSION: usize = 128;
const MAX_SNAPSHOT_BYTES: usize = 32 * 1024 * 1024;
const MAX_SESSION_TITLE_BYTES: usize = 160;
const MAX_SUMMARY_BYTES: usize = 32 * 1024;
const MAX_PLAN_BYTES: usize = 64 * 1024;
const MAX_TODOS: usize = 256;
const MAX_TODO_BYTES: usize = 2 * 1024;

#[derive(Clone, Default)]
pub struct SessionStore {
    inner: Arc<Mutex<HashMap<String, SessionRecord>>>,
    next: Arc<AtomicU64>,
}

struct SessionRecord {
    id: String,
    workspace: String,
    title: String,
    summary: Option<String>,
    plan: Option<String>,
    result_summary: Option<String>,
    created_at_unix_ms: u128,
    updated_at_unix_ms: u128,
    status: SessionStatus,
    baseline: Option<SessionBaseline>,
    todos: Vec<TodoItem>,
    next_todo: u64,
    files: BTreeMap<String, FileSnapshot>,
    bytes: usize,
}

struct FileSnapshot {
    before: Vec<u8>,
    before_sha256: String,
    after_sha256: String,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum SessionStatus {
    Active,
    Finalized,
    RolledBack,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionBaseline {
    branch: Option<String>,
    head: Option<String>,
    dirty: Option<bool>,
    changes: Vec<SessionBaselineChange>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionBaselineChange {
    index: char,
    worktree: char,
    path: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Done,
    Blocked,
    Skipped,
}

#[derive(Debug, Clone, Serialize)]
pub struct TodoItem {
    id: String,
    text: String,
    status: TodoStatus,
}

#[derive(Debug, Deserialize)]
pub struct CreateSessionRequest {
    workspace: String,
    title: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct TrackFilesRequest {
    paths: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct FindWorkspacesRequest {
    query: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SelectWorkspaceRequest {
    workspace: String,
    session_title: String,
}

#[derive(Debug, Deserialize)]
pub struct ChooseSessionRequest {
    session: String,
}

#[derive(Debug, Deserialize)]
pub struct SetSummaryRequest {
    session: String,
    summary: String,
}

#[derive(Debug, Deserialize)]
pub struct SetPlanRequest {
    session: String,
    plan: String,
}

#[derive(Debug, Deserialize)]
pub struct CompleteSessionRequest {
    session: String,
    result_summary: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum TodoRequest {
    List {
        session: String,
    },
    Add {
        session: String,
        text: String,
        status: Option<TodoStatus>,
    },
    Update {
        session: String,
        id: String,
        text: Option<String>,
        status: Option<TodoStatus>,
    },
    Remove {
        session: String,
        id: String,
    },
    Reorder {
        session: String,
        ids: Vec<String>,
    },
}

#[derive(Debug, Serialize)]
pub struct AgentWorkspaceSummary {
    id: String,
    name: String,
    root: String,
    exists: bool,
    capabilities: Capabilities,
    git: Option<SessionBaseline>,
}

#[derive(Debug, Serialize)]
pub struct FindWorkspacesResponse {
    workspaces: Vec<AgentWorkspaceSummary>,
}

#[derive(Debug, Serialize)]
pub struct SessionView {
    id: String,
    session: String,
    workspace: String,
    title: String,
    summary: Option<String>,
    plan: Option<String>,
    result_summary: Option<String>,
    created_at_unix_ms: u128,
    updated_at_unix_ms: u128,
    status: SessionStatus,
    baseline: Option<SessionBaseline>,
    todos: Vec<TodoItem>,
    files: Vec<SessionFileView>,
    resume_warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct SessionFileView {
    path: String,
    before_sha256: String,
    after_sha256: String,
    current_sha256: Option<String>,
    rollback_safe: bool,
}

#[derive(Debug, Serialize)]
pub struct TodoResponse {
    session: String,
    todos: Vec<TodoItem>,
}

#[derive(Debug, Serialize)]
pub struct RollbackResponse {
    id: String,
    session: String,
    restored: Vec<String>,
    skipped: Vec<String>,
}

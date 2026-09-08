use std::{
    collections::{BTreeMap, HashMap},
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex as StdMutex},
};

use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

use crate::error::ApiError;

use super::{
    FileSnapshot, SessionBaseline, SessionRecord, SessionStatus, TodoItem, MAX_FILES_PER_SESSION,
    MAX_SNAPSHOT_BYTES, MAX_TODOS,
};
use super::support::digest;

const SESSION_SCHEMA_VERSION: u32 = 1;
const SESSION_MANIFEST: &str = "session.json";
const SESSIONS_DIR: &str = "sessions";
const SNAPSHOTS_DIR: &str = "snapshots";

#[derive(Clone)]
pub(super) struct SessionPersistence {
    root: Option<Arc<PathBuf>>,
    memory_snapshots: Arc<StdMutex<HashMap<String, Vec<u8>>>>,
}

#[derive(Debug, Serialize, Deserialize)]
struct PersistedSession {
    schema_version: u32,
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
    files: BTreeMap<String, PersistedFileSnapshot>,
}

#[derive(Debug, Serialize, Deserialize)]
struct PersistedFileSnapshot {
    before_sha256: String,
    after_sha256: String,
    before_bytes: usize,
}

impl SessionPersistence {
    pub(super) fn disabled() -> Self {
        Self {
            root: None,
            memory_snapshots: Arc::new(StdMutex::new(HashMap::new())),
        }
    }

    pub(super) fn open(state_root: &Path) -> Result<Self, String> {
        let root = state_root.join(SESSIONS_DIR);
        fs::create_dir_all(&root).map_err(|error| {
            format!(
                "failed to create session state directory {}: {error}",
                root.display()
            )
        })?;
        Ok(Self {
            root: Some(Arc::new(root)),
            memory_snapshots: Arc::new(StdMutex::new(HashMap::new())),
        })
    }

    pub(super) fn load_sessions(&self) -> Result<HashMap<String, SessionRecord>, String> {
        let Some(root) = self.root.as_ref() else {
            return Ok(HashMap::new());
        };
        let mut sessions = HashMap::new();
        let entries = fs::read_dir(root.as_ref()).map_err(|error| {
            format!(
                "failed to read session state directory {}: {error}",
                root.display()
            )
        })?;
        for entry in entries {
            let entry = entry.map_err(|error| format!("failed to read persisted session: {error}"))?;
            let file_type = entry
                .file_type()
                .map_err(|error| format!("failed to inspect persisted session: {error}"))?;
            if !file_type.is_dir() {
                continue;
            }
            let manifest_path = entry.path().join(SESSION_MANIFEST);
            if !manifest_path.is_file() {
                continue;
            }
            let manifest = read_manifest(&manifest_path)?;
            let directory_name = entry.file_name().to_string_lossy().into_owned();
            if manifest.id != directory_name {
                return Err(format!(
                    "persisted session id {:?} does not match directory {:?}",
                    manifest.id, directory_name
                ));
            }
            let record = manifest.into_record()?;
            if sessions.insert(record.id.clone(), record).is_some() {
                return Err(format!("duplicate persisted session {directory_name:?}"));
            }
        }
        Ok(sessions)
    }

    pub(super) fn save_session(&self, session: &SessionRecord) -> Result<(), ApiError> {
        let Some(_) = self.root.as_ref() else {
            return Ok(());
        };
        let directory = self.session_dir(&session.id)?;
        fs::create_dir_all(&directory).map_err(map_state_io)?;
        let manifest = PersistedSession::from_record(session);
        let mut temp = NamedTempFile::new_in(&directory).map_err(map_state_io)?;
        serde_json::to_writer_pretty(temp.as_file_mut(), &manifest)
            .map_err(|error| ApiError::Internal(format!("failed to serialize session state: {error}")))?;
        temp.write_all(b"\n").map_err(map_state_io)?;
        temp.as_file_mut().flush().map_err(map_state_io)?;
        temp.as_file().sync_all().map_err(map_state_io)?;
        let target = directory.join(SESSION_MANIFEST);
        temp.persist(&target)
            .map_err(|error| map_state_io(error.error))?;
        sync_directory(&directory).map_err(map_state_io)?;
        Ok(())
    }

    pub(super) fn write_snapshot(
        &self,
        session: &str,
        sha256: &str,
        bytes: &[u8],
    ) -> Result<(), ApiError> {
        validate_sha256(sha256)?;
        if digest(bytes) != sha256 {
            return Err(ApiError::Internal(
                "rollback snapshot hash does not match its content".into(),
            ));
        }
        if self.root.is_none() {
            let key = snapshot_memory_key(session, sha256);
            let mut snapshots = self
                .memory_snapshots
                .lock()
                .map_err(|_| ApiError::Internal("in-memory snapshot store is poisoned".into()))?;
            if let Some(existing) = snapshots.get(&key) {
                if digest(existing) != sha256 {
                    return Err(ApiError::Internal(
                        "in-memory rollback snapshot is corrupt".into(),
                    ));
                }
            } else {
                snapshots.insert(key, bytes.to_vec());
            }
            return Ok(());
        }
        let directory = self.session_dir(session)?.join(SNAPSHOTS_DIR);
        fs::create_dir_all(&directory).map_err(map_state_io)?;
        let target = directory.join(format!("{sha256}.bin"));
        if target.exists() {
            let existing = fs::read(&target).map_err(map_state_io)?;
            if digest(&existing) != sha256 {
                return Err(ApiError::Internal(format!(
                    "persisted rollback snapshot {} is corrupt",
                    target.display()
                )));
            }
            return Ok(());
        }
        let mut temp = NamedTempFile::new_in(&directory).map_err(map_state_io)?;
        temp.write_all(bytes).map_err(map_state_io)?;
        temp.as_file_mut().flush().map_err(map_state_io)?;
        temp.as_file().sync_all().map_err(map_state_io)?;
        match temp.persist_noclobber(&target) {
            Ok(_) => {}
            Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
                let existing = fs::read(&target).map_err(map_state_io)?;
                if digest(&existing) != sha256 {
                    return Err(ApiError::Internal(format!(
                        "persisted rollback snapshot {} is corrupt",
                        target.display()
                    )));
                }
            }
            Err(error) => return Err(map_state_io(error.error)),
        }
        sync_directory(&directory).map_err(map_state_io)?;
        Ok(())
    }

    pub(super) fn read_snapshot(&self, session: &str, sha256: &str) -> Result<Vec<u8>, ApiError> {
        validate_sha256(sha256)?;
        if self.root.is_none() {
            let key = snapshot_memory_key(session, sha256);
            let snapshots = self
                .memory_snapshots
                .lock()
                .map_err(|_| ApiError::Internal("in-memory snapshot store is poisoned".into()))?;
            return snapshots.get(&key).cloned().ok_or_else(|| {
                ApiError::Internal("rollback snapshot is missing from in-memory state".into())
            });
        }
        let path = self.snapshot_path(session, sha256)?;
        let mut file = File::open(&path).map_err(map_state_io)?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).map_err(map_state_io)?;
        if digest(&bytes) != sha256 {
            return Err(ApiError::Internal(format!(
                "persisted rollback snapshot {} failed its SHA-256 check",
                path.display()
            )));
        }
        Ok(bytes)
    }

    pub(super) fn snapshot_available(&self, session: &str, sha256: &str) -> bool {
        if validate_sha256(sha256).is_err() {
            return false;
        }
        if self.root.is_none() {
            return self
                .memory_snapshots
                .lock()
                .ok()
                .is_some_and(|snapshots| snapshots.contains_key(&snapshot_memory_key(session, sha256)));
        }
        self.snapshot_path(session, sha256)
            .ok()
            .is_some_and(|path| path.is_file())
    }

    fn session_dir(&self, session: &str) -> Result<PathBuf, ApiError> {
        let root = self
            .root
            .as_ref()
            .ok_or_else(|| ApiError::Internal("session persistence is disabled".into()))?;
        validate_session_component(session)?;
        Ok(root.join(session))
    }

    fn snapshot_path(&self, session: &str, sha256: &str) -> Result<PathBuf, ApiError> {
        Ok(self
            .session_dir(session)?
            .join(SNAPSHOTS_DIR)
            .join(format!("{sha256}.bin")))
    }
}

impl PersistedSession {
    fn from_record(session: &SessionRecord) -> Self {
        Self {
            schema_version: SESSION_SCHEMA_VERSION,
            id: session.id.clone(),
            workspace: session.workspace.clone(),
            title: session.title.clone(),
            summary: session.summary.clone(),
            plan: session.plan.clone(),
            result_summary: session.result_summary.clone(),
            created_at_unix_ms: session.created_at_unix_ms,
            updated_at_unix_ms: session.updated_at_unix_ms,
            status: session.status,
            baseline: session.baseline.clone(),
            todos: session.todos.clone(),
            next_todo: session.next_todo,
            files: session
                .files
                .iter()
                .map(|(path, snapshot)| {
                    (
                        path.clone(),
                        PersistedFileSnapshot {
                            before_sha256: snapshot.before_sha256.clone(),
                            after_sha256: snapshot.after_sha256.clone(),
                            before_bytes: snapshot.before_bytes,
                        },
                    )
                })
                .collect(),
        }
    }

    fn into_record(self) -> Result<SessionRecord, String> {
        if self.schema_version != SESSION_SCHEMA_VERSION {
            return Err(format!(
                "unsupported persisted session schema version {} for {:?}",
                self.schema_version, self.id
            ));
        }
        validate_session_component_string(&self.id)?;
        if self.files.len() > MAX_FILES_PER_SESSION {
            return Err(format!(
                "persisted session {:?} exceeds file limit",
                self.id
            ));
        }
        if self.todos.len() > MAX_TODOS {
            return Err(format!(
                "persisted session {:?} exceeds todo limit",
                self.id
            ));
        }
        let bytes = self
            .files
            .values()
            .try_fold(0usize, |total, file| total.checked_add(file.before_bytes))
            .ok_or_else(|| format!("persisted session {:?} snapshot size overflow", self.id))?;
        if bytes > MAX_SNAPSHOT_BYTES {
            return Err(format!(
                "persisted session {:?} exceeds rollback snapshot budget",
                self.id
            ));
        }
        for snapshot in self.files.values() {
            validate_sha256_string(&snapshot.before_sha256)?;
            validate_sha256_string(&snapshot.after_sha256)?;
        }
        Ok(SessionRecord {
            id: self.id,
            workspace: self.workspace,
            title: self.title,
            summary: self.summary,
            plan: self.plan,
            result_summary: self.result_summary,
            created_at_unix_ms: self.created_at_unix_ms,
            updated_at_unix_ms: self.updated_at_unix_ms,
            status: self.status,
            baseline: self.baseline,
            todos: self.todos,
            next_todo: self.next_todo,
            files: self
                .files
                .into_iter()
                .map(|(path, snapshot)| {
                    (
                        path,
                        FileSnapshot {
                            before_sha256: snapshot.before_sha256,
                            after_sha256: snapshot.after_sha256,
                            before_bytes: snapshot.before_bytes,
                        },
                    )
                })
                .collect(),
            bytes,
        })
    }
}

fn read_manifest(path: &Path) -> Result<PersistedSession, String> {
    let bytes = fs::read(path)
        .map_err(|error| format!("failed to read persisted session {}: {error}", path.display()))?;
    serde_json::from_slice(&bytes)
        .map_err(|error| format!("failed to parse persisted session {}: {error}", path.display()))
}

fn validate_session_component(session: &str) -> Result<(), ApiError> {
    validate_session_component_string(session).map_err(ApiError::Internal)
}

fn validate_session_component_string(session: &str) -> Result<(), String> {
    if session.is_empty()
        || session == "."
        || session == ".."
        || session.contains('/')
        || session.contains('\\')
        || session.chars().any(char::is_control)
    {
        return Err(format!("invalid persisted session identifier {session:?}"));
    }
    Ok(())
}

fn validate_sha256(sha256: &str) -> Result<(), ApiError> {
    validate_sha256_string(sha256).map_err(ApiError::Internal)
}

fn validate_sha256_string(sha256: &str) -> Result<(), String> {
    if sha256.len() != 64 || !sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(format!("invalid persisted SHA-256 {sha256:?}"));
    }
    Ok(())
}

fn snapshot_memory_key(session: &str, sha256: &str) -> String {
    format!("{session}/{sha256}")
}

fn sync_directory(path: &Path) -> std::io::Result<()> {
    File::open(path)?.sync_all()
}

fn map_state_io(error: std::io::Error) -> ApiError {
    ApiError::Internal(format!("session persistence failed: {error}"))
}

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex as StdMutex},
};

use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

use crate::error::ApiError;

use super::support::digest;
use super::{
    FileSnapshot, MAX_FILES_PER_SESSION, MAX_SNAPSHOT_BYTES, MAX_TODOS, SessionBaseline,
    SessionRecord, SessionStatus, TodoItem,
};

const SESSION_SCHEMA_VERSION: u32 = 1;
const SESSION_MANIFEST: &str = "session.json";
const SESSIONS_DIR: &str = "sessions";
const SNAPSHOTS_DIR: &str = "snapshots";
const QUARANTINE_DIR: &str = "_quarantine";

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
        for entry in fs::read_dir(root.as_ref()).map_err(|error| {
            format!(
                "failed to read session state directory {}: {error}",
                root.display()
            )
        })? {
            let entry =
                entry.map_err(|error| format!("failed to read persisted session: {error}"))?;
            let file_type = entry.file_type().map_err(|error| {
                format!(
                    "failed to inspect persisted session {}: {error}",
                    entry.path().display()
                )
            })?;
            if !file_type.is_dir() || entry.file_name() == QUARANTINE_DIR {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let manifest_path = entry.path().join(SESSION_MANIFEST);
            let loaded = read_manifest(&manifest_path).and_then(|manifest| {
                if manifest.id != name {
                    return Err(format!(
                        "persisted session id {:?} does not match directory {:?}",
                        manifest.id, name
                    ));
                }
                manifest.into_record()
            });
            let record = match loaded {
                Ok(record) => record,
                Err(error) => {
                    eprintln!("tuxbridge: quarantining persisted session {name:?}: {error}");
                    quarantine_session(root, &entry.path(), &name, &error)?;
                    continue;
                }
            };
            if sessions.insert(record.id.clone(), record).is_some() {
                return Err(format!("duplicate persisted session {name:?}"));
            }
        }
        self.garbage_collect_snapshots(&sessions)?;
        Ok(sessions)
    }

    fn garbage_collect_snapshots(
        &self,
        sessions: &HashMap<String, SessionRecord>,
    ) -> Result<(), String> {
        let Some(root) = self.root.as_ref() else {
            return Ok(());
        };
        for (session_id, session) in sessions {
            let directory = root.join(session_id).join(SNAPSHOTS_DIR);
            if !directory.is_dir() {
                continue;
            }
            let referenced = session
                .files
                .values()
                .map(|snapshot| format!("{}.bin", snapshot.before_sha256))
                .collect::<HashSet<_>>();
            for entry in fs::read_dir(&directory)
                .map_err(|error| format!("failed to scan {}: {error}", directory.display()))?
            {
                let entry =
                    entry.map_err(|error| format!("failed to inspect snapshot: {error}"))?;
                if !entry
                    .file_type()
                    .map_err(|error| error.to_string())?
                    .is_file()
                {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().into_owned();
                if !referenced.contains(&name) {
                    fs::remove_file(entry.path()).map_err(|error| {
                        format!(
                            "failed to remove orphan snapshot {}: {error}",
                            entry.path().display()
                        )
                    })?;
                }
            }
            sync_directory(&directory).map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    pub(super) fn save_session(&self, session: &SessionRecord) -> Result<(), ApiError> {
        let Some(_) = self.root.as_ref() else {
            return Ok(());
        };
        let directory = self.session_dir(&session.id)?;
        fs::create_dir_all(&directory).map_err(map_state_io)?;
        let manifest = PersistedSession::from_record(session);
        let mut temp = NamedTempFile::new_in(&directory).map_err(map_state_io)?;
        serde_json::to_writer_pretty(temp.as_file_mut(), &manifest).map_err(|error| {
            ApiError::Internal(format!("failed to serialize session state: {error}"))
        })?;
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
            return self.memory_snapshots.lock().ok().is_some_and(|snapshots| {
                snapshots.contains_key(&snapshot_memory_key(session, sha256))
            });
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

fn quarantine_session(
    root: &Path,
    session_dir: &Path,
    name: &str,
    reason: &str,
) -> Result<(), String> {
    let quarantine = root.join(QUARANTINE_DIR);
    fs::create_dir_all(&quarantine).map_err(|error| {
        format!(
            "failed to create quarantine directory {}: {error}",
            quarantine.display()
        )
    })?;
    let mut suffix = 0u64;
    let target = loop {
        let candidate = quarantine.join(format!("{name}-{}-{suffix}", std::process::id()));
        if !candidate.exists() {
            break candidate;
        }
        suffix += 1;
    };
    fs::rename(session_dir, &target).map_err(|error| {
        format!(
            "failed to quarantine corrupt session {}: {error}",
            session_dir.display()
        )
    })?;
    fs::write(
        target.join("QUARANTINE_REASON.txt"),
        format!(
            "{reason}
"
        ),
    )
    .map_err(|error| format!("failed to record quarantine reason: {error}"))?;
    sync_directory(&quarantine).map_err(|error| error.to_string())?;
    Ok(())
}

fn read_manifest(path: &Path) -> Result<PersistedSession, String> {
    let bytes = fs::read(path).map_err(|error| {
        format!(
            "failed to read persisted session {}: {error}",
            path.display()
        )
    })?;
    serde_json::from_slice(&bytes).map_err(|error| {
        format!(
            "failed to parse persisted session {}: {error}",
            path.display()
        )
    })
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sessions::SessionStore;

    #[test]
    fn corrupt_manifest_is_quarantined_without_blocking_startup() {
        let directory = tempfile::tempdir().unwrap();
        let sessions = directory.path().join(SESSIONS_DIR);
        let broken = sessions.join("demo--broken--abc12");
        fs::create_dir_all(&broken).unwrap();
        fs::write(broken.join(SESSION_MANIFEST), b"{ definitely not json").unwrap();

        let persistence = SessionPersistence::open(directory.path()).unwrap();
        let loaded = persistence.load_sessions().unwrap();
        assert!(loaded.is_empty());

        let quarantine = sessions.join(QUARANTINE_DIR);
        let entries = fs::read_dir(&quarantine)
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(entries.len(), 1);
        let quarantined = entries[0].path();
        assert!(quarantined.join("QUARANTINE_REASON.txt").is_file());
        assert!(!broken.exists());
    }

    #[tokio::test]
    async fn reopen_removes_only_unreferenced_snapshot_blobs() {
        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::open(directory.path()).unwrap();
        let session = store
            .create("demo", "GC snapshots".into(), None)
            .await
            .unwrap();
        store
            .capture_change(&session, "demo", "src/lib.rs", b"before", b"after")
            .await
            .unwrap();

        let sessions = store.inner.lock().await;
        let record = sessions.get(&session).unwrap();
        let referenced = record
            .files
            .get("src/lib.rs")
            .unwrap()
            .before_sha256
            .clone();
        drop(sessions);
        drop(store);

        let snapshot_dir = directory
            .path()
            .join(SESSIONS_DIR)
            .join(&session)
            .join(SNAPSHOTS_DIR);
        let orphan = snapshot_dir.join(format!("{}.bin", "0".repeat(64)));
        fs::write(&orphan, b"orphan").unwrap();

        let reopened = SessionStore::open(directory.path()).unwrap();
        assert!(!orphan.exists());
        assert_eq!(
            reopened.read_snapshot(&session, &referenced).unwrap(),
            b"before"
        );
    }
}

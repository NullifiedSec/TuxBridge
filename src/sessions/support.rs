use std::{
    collections::HashMap,
    fs,
    io::Write,
    path::{Component, Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;

use crate::{error::ApiError, state::AppState};

use super::*;

pub(super) fn validate_session(session: &SessionRecord, workspace: &str) -> Result<(), ApiError> {
    if session.workspace != workspace {
        return Err(ApiError::Forbidden(
            "coding session belongs to another workspace".into(),
        ));
    }
    ensure_active(session)
}

pub(super) fn ensure_active(session: &SessionRecord) -> Result<(), ApiError> {
    if !matches!(session.status, SessionStatus::Active) {
        return Err(ApiError::Conflict("coding session is not active".into()));
    }
    Ok(())
}

pub(super) fn view_record(state: &AppState, session: &SessionRecord) -> SessionView {
    let root = state
        .config
        .workspaces
        .get(&session.workspace)
        .and_then(|workspace| fs::canonicalize(&workspace.root).ok());
    let files = session
        .files
        .iter()
        .map(|(path, file)| {
            let current_sha256 = root
                .as_ref()
                .and_then(|root| safe_existing(root, path).ok())
                .and_then(|path| fs::read(path).ok())
                .map(|bytes| digest(&bytes));
            let snapshot_available = state
                .sessions
                .snapshot_available(&session.id, &file.before_sha256);
            let rollback_safe = snapshot_available
                && current_sha256.as_deref() == Some(file.after_sha256.as_str());
            SessionFileView {
                path: path.clone(),
                before_sha256: file.before_sha256.clone(),
                after_sha256: file.after_sha256.clone(),
                current_sha256,
                rollback_safe,
            }
        })
        .collect::<Vec<_>>();

    let mut resume_warnings = Vec::new();
    for (path, file) in &session.files {
        let snapshot_available = state
            .sessions
            .snapshot_available(&session.id, &file.before_sha256);
        if !snapshot_available {
            resume_warnings.push(format!(
                "rollback snapshot for {path} is missing from TuxBridge state"
            ));
        } else if files
            .iter()
            .find(|view| view.path == *path)
            .is_some_and(|view| !view.rollback_safe)
        {
            resume_warnings.push(format!(
                "{path} changed after this session last wrote it"
            ));
        }
    }
    if let (Some(baseline), Some(root)) = (&session.baseline, root.as_ref()) {
        if let Some(current) = git_snapshot(root) {
            if baseline.head.is_some() && current.head != baseline.head {
                resume_warnings.push(format!(
                    "workspace HEAD moved from {} to {} since the session started",
                    baseline.head.as_deref().unwrap_or("unknown"),
                    current.head.as_deref().unwrap_or("unknown")
                ));
            }
        }
    }

    SessionView {
        id: session.id.clone(),
        session: session.id.clone(),
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
        files,
        resume_warnings,
    }
}

pub(super) fn resolve_key(
    sessions: &HashMap<String, SessionRecord>,
    reference: &str,
) -> Result<String, ApiError> {
    let reference = reference.trim();
    if reference.is_empty() {
        return Err(ApiError::BadRequest("session reference must not be empty".into()));
    }
    if sessions.contains_key(reference) {
        return Ok(reference.to_owned());
    }
    if reference.contains("--") {
        return Err(ApiError::NotFound(format!(
            "coding session {reference:?} not found"
        )));
    }
    let suffix = format!("--{reference}");
    let matches = sessions
        .keys()
        .filter(|key| key.ends_with(&suffix))
        .cloned()
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [] => Err(ApiError::NotFound(format!(
            "coding session {reference:?} not found"
        ))),
        [only] => Ok(only.clone()),
        _ => Err(ApiError::Conflict(format!(
            "session reference {reference:?} is ambiguous: {}",
            matches.join(", ")
        ))),
    }
}

pub(super) fn readable_session_ref(workspace: &str, title: &str, sequence: u64) -> String {
    let workspace = slug(workspace, 32, "workspace");
    let title = slug(title, 48, "session");
    let entropy = (now_ms() as u64).rotate_left(17) ^ sequence.wrapping_mul(0x9e3779b97f4a7c15);
    format!("{workspace}--{title}--{}", base36_suffix(entropy, 5))
}

fn slug(value: &str, max_len: usize, fallback: &str) -> String {
    let mut result = String::new();
    let mut pending_dash = false;
    for character in value.chars() {
        if character.is_ascii_alphanumeric() {
            if pending_dash && !result.is_empty() && result.len() < max_len {
                result.push('-');
            }
            pending_dash = false;
            if result.len() >= max_len {
                break;
            }
            result.push(character.to_ascii_lowercase());
        } else if !result.is_empty() {
            pending_dash = true;
        }
    }
    result.truncate(max_len);
    while result.ends_with('-') {
        result.pop();
    }
    if result.is_empty() {
        fallback.to_owned()
    } else {
        result
    }
}

fn base36_suffix(mut value: u64, length: usize) -> String {
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut output = vec![b'0'; length];
    for index in (0..length).rev() {
        output[index] = DIGITS[(value % 36) as usize];
        value /= 36;
    }
    String::from_utf8(output).expect("base36 digits are UTF-8")
}

pub(super) fn validate_text(label: &str, value: &str, max_bytes: usize) -> Result<(), ApiError> {
    if value.trim().is_empty() {
        return Err(ApiError::BadRequest(format!("{label} must not be empty")));
    }
    if value.len() > max_bytes {
        return Err(ApiError::BadRequest(format!(
            "{label} exceeds the {max_bytes}-byte limit"
        )));
    }
    Ok(())
}

pub(super) fn git_snapshot(root: &Path) -> Option<SessionBaseline> {
    if !root.is_dir() {
        return None;
    }
    let inside = run_git_optional(root, &["rev-parse", "--is-inside-work-tree"])?;
    if inside != "true" {
        return None;
    }
    let branch = run_git_optional(root, &["branch", "--show-current"])
        .filter(|value| !value.is_empty());
    let head = run_git_optional(root, &["rev-parse", "HEAD"])
        .filter(|value| !value.is_empty());
    let status = run_git_optional(root, &["status", "--porcelain=v1"])?;
    let changes = status
        .lines()
        .filter_map(parse_baseline_status_line)
        .collect::<Vec<_>>();
    Some(SessionBaseline {
        branch,
        head,
        dirty: Some(!changes.is_empty()),
        changes,
    })
}

fn parse_baseline_status_line(line: &str) -> Option<SessionBaselineChange> {
    let bytes = line.as_bytes();
    if bytes.len() < 3 {
        return None;
    }
    Some(SessionBaselineChange {
        index: bytes[0] as char,
        worktree: bytes[1] as char,
        path: line[3..].to_owned(),
    })
}

fn run_git_optional(root: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

pub(super) fn safe_existing(root: &Path, relative: &str) -> Result<PathBuf, ApiError> {
    let path = Path::new(relative);
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(ApiError::BadRequest("session path escapes workspace".into()));
    }
    let raw = root.join(path);
    if fs::symlink_metadata(&raw).map_err(map_io)?.file_type().is_symlink() {
        return Err(ApiError::Forbidden(
            "session rollback through symlinks is forbidden".into(),
        ));
    }
    let canonical = fs::canonicalize(raw).map_err(map_io)?;
    if !canonical.starts_with(root) || !canonical.is_file() {
        return Err(ApiError::Forbidden("session path escapes workspace".into()));
    }
    Ok(canonical)
}

pub(super) fn atomic_write(
    path: &Path,
    content: &[u8],
    permissions: fs::Permissions,
) -> Result<(), ApiError> {
    let parent = path
        .parent()
        .ok_or_else(|| ApiError::BadRequest("file has no parent".into()))?;
    let mut temp = NamedTempFile::new_in(parent).map_err(map_io)?;
    temp.write_all(content).map_err(map_io)?;
    temp.as_file().sync_all().map_err(map_io)?;
    temp.as_file().set_permissions(permissions).map_err(map_io)?;
    temp.persist(path).map_err(|error| map_io(error.error))?;
    Ok(())
}

pub(super) fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub(super) fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

pub(super) fn map_io(error: std::io::Error) -> ApiError {
    match error.kind() {
        std::io::ErrorKind::NotFound => ApiError::NotFound(error.to_string()),
        std::io::ErrorKind::PermissionDenied => ApiError::Forbidden(error.to_string()),
        _ => ApiError::Internal(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap};

    use super::super::{SessionRecord, SessionStatus};
    use super::*;

    #[test]
    fn session_refs_are_readable_and_bounded() {
        let reference = readable_session_ref(
            "damasquino-loyalty-app",
            "Fix notification localization without touching Firebase config",
            7,
        );
        assert!(reference.starts_with(
            "damasquino-loyalty-app--fix-notification-localization-without-touching-f--"
        ));
        assert_eq!(reference.rsplit("--").next().unwrap().len(), 5);
    }

    #[test]
    fn short_session_refs_resolve_when_unique() {
        let mut sessions = HashMap::new();
        let id = "demo--fix-thing--a1b2c".to_owned();
        sessions.insert(
            id.clone(),
            SessionRecord {
                id: id.clone(),
                workspace: "demo".into(),
                title: "Fix thing".into(),
                summary: None,
                plan: None,
                result_summary: None,
                created_at_unix_ms: 0,
                updated_at_unix_ms: 0,
                status: SessionStatus::Active,
                baseline: None,
                todos: Vec::new(),
                next_todo: 0,
                files: BTreeMap::new(),
                bytes: 0,
            },
        );
        assert_eq!(resolve_key(&sessions, "a1b2c").unwrap(), id);
    }

    #[test]
    fn slug_normalizes_agent_facing_refs() {
        assert_eq!(slug(" Hello, World! ", 32, "fallback"), "hello-world");
        assert_eq!(slug("***", 32, "fallback"), "fallback");
    }
}

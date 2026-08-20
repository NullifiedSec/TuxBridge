use std::{
    fs::{self, File},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
};

use axum::{extract::State, Json};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;

use crate::{config::UserFilesConfig, error::ApiError, state::AppState};

const MAX_FILE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Serialize)]
pub struct MountSummary {
    name: String,
    root: String,
    exists: bool,
    read: bool,
    write: bool,
}

#[derive(Debug, Deserialize)]
pub struct PathRequest {
    mount: String,
    #[serde(default)]
    path: String,
}

#[derive(Debug, Deserialize)]
pub struct ReadRequest {
    mount: String,
    path: String,
    max_bytes: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct WriteRequest {
    mount: String,
    path: String,
    mode: WriteMode,
    content: String,
    expected_sha256: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WriteMode {
    Create,
    Replace,
}

#[derive(Debug, Deserialize)]
pub struct PatchRequest {
    mount: String,
    path: String,
    expected_sha256: String,
    old: String,
    new: String,
}

#[derive(Debug, Serialize)]
pub struct Entry {
    name: String,
    path: String,
    kind: &'static str,
    size: u64,
}

#[derive(Debug, Serialize)]
pub struct StatResponse {
    path: String,
    absolute: String,
    kind: &'static str,
    size: u64,
    readonly: bool,
}

#[derive(Debug, Serialize)]
pub struct ReadResponse {
    path: String,
    content: String,
    size: u64,
    sha256: String,
    truncated: bool,
}

#[derive(Debug, Serialize)]
pub struct MutationResponse {
    path: String,
    size: usize,
    sha256: String,
}

pub async fn list_mounts(State(state): State<AppState>) -> Json<Vec<MountSummary>> {
    Json(
        state
            .config
            .user_files
            .iter()
            .map(|(name, mount)| MountSummary {
                name: name.clone(),
                root: mount.root.display().to_string(),
                exists: mount.root.is_dir(),
                read: mount.read,
                write: mount.write,
            })
            .collect(),
    )
}

pub async fn list_directory(
    State(state): State<AppState>,
    Json(request): Json<PathRequest>,
) -> Result<Json<Vec<Entry>>, ApiError> {
    let mount = require_mount(&state, &request.mount, false)?;
    let (root, target) = resolve_existing(mount, &request.path)?;
    if !fs::metadata(&target).map_err(map_io)?.is_dir() {
        return Err(ApiError::BadRequest("path is not a directory".into()));
    }
    let mut entries = Vec::new();
    for entry in fs::read_dir(&target).map_err(map_io)? {
        let entry = entry.map_err(map_io)?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path).map_err(map_io)?;
        entries.push(Entry {
            name: entry.file_name().to_string_lossy().into_owned(),
            path: relative(&root, &path),
            kind: kind(&metadata),
            size: metadata.len(),
        });
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(Json(entries))
}

pub async fn stat_path(
    State(state): State<AppState>,
    Json(request): Json<PathRequest>,
) -> Result<Json<StatResponse>, ApiError> {
    let mount = require_mount(&state, &request.mount, false)?;
    let (root, target) = resolve_existing(mount, &request.path)?;
    let metadata = fs::symlink_metadata(&target).map_err(map_io)?;
    Ok(Json(StatResponse {
        path: relative(&root, &target),
        absolute: target.display().to_string(),
        kind: kind(&metadata),
        size: metadata.len(),
        readonly: metadata.permissions().readonly(),
    }))
}

pub async fn read_file(
    State(state): State<AppState>,
    Json(request): Json<ReadRequest>,
) -> Result<Json<ReadResponse>, ApiError> {
    let mount = require_mount(&state, &request.mount, false)?;
    let (root, target) = resolve_existing_file(mount, &request.path)?;
    let metadata = fs::metadata(&target).map_err(map_io)?;
    let max_bytes = request.max_bytes.unwrap_or(1024 * 1024).clamp(1, MAX_FILE_BYTES);
    let mut bytes = Vec::with_capacity(max_bytes.saturating_add(1));
    File::open(&target)
        .map_err(map_io)?
        .take(max_bytes.saturating_add(1) as u64)
        .read_to_end(&mut bytes)
        .map_err(map_io)?;
    let truncated = bytes.len() > max_bytes;
    if truncated {
        bytes.truncate(max_bytes);
        if let Err(error) = std::str::from_utf8(&bytes) {
            if error.error_len().is_none() {
                bytes.truncate(error.valid_up_to());
            }
        }
    }
    let content = String::from_utf8(bytes)
        .map_err(|_| ApiError::Unsupported("file is not valid UTF-8 text".into()))?;
    Ok(Json(ReadResponse {
        path: relative(&root, &target),
        sha256: digest(content.as_bytes()),
        content,
        size: metadata.len(),
        truncated,
    }))
}

pub async fn hash_file(
    State(state): State<AppState>,
    Json(request): Json<PathRequest>,
) -> Result<Json<MutationResponse>, ApiError> {
    let mount = require_mount(&state, &request.mount, false)?;
    let (root, target) = resolve_existing_file(mount, &request.path)?;
    let bytes = read_bounded(&target)?;
    Ok(Json(MutationResponse {
        path: relative(&root, &target),
        size: bytes.len(),
        sha256: digest(&bytes),
    }))
}

pub async fn write_file(
    State(state): State<AppState>,
    Json(request): Json<WriteRequest>,
) -> Result<Json<MutationResponse>, ApiError> {
    let mount = require_mount(&state, &request.mount, true)?;
    ensure_size(request.content.len())?;
    let (root, target) = resolve_write_target(mount, &request.path)?;
    let bytes = request.content.as_bytes();

    match request.mode {
        WriteMode::Create => {
            if target.exists() {
                return Err(ApiError::Conflict("target already exists".into()));
            }
            atomic_write(&target, bytes, None, true)?;
        }
        WriteMode::Replace => {
            if !target.exists() {
                return Err(ApiError::NotFound("target does not exist".into()));
            }
            reject_symlink(&target)?;
            let current = read_bounded(&target)?;
            let expected = request.expected_sha256.as_deref().ok_or_else(|| {
                ApiError::BadRequest("expected_sha256 is required for replace mode".into())
            })?;
            verify_hash(expected, &current)?;
            let permissions = fs::metadata(&target).map_err(map_io)?.permissions();
            atomic_write(&target, bytes, Some(permissions), false)?;
        }
    }

    Ok(Json(MutationResponse {
        path: relative(&root, &target),
        size: bytes.len(),
        sha256: digest(bytes),
    }))
}

pub async fn patch_file(
    State(state): State<AppState>,
    Json(request): Json<PatchRequest>,
) -> Result<Json<MutationResponse>, ApiError> {
    if request.old.is_empty() {
        return Err(ApiError::BadRequest("old text must not be empty".into()));
    }
    let mount = require_mount(&state, &request.mount, true)?;
    let (root, target) = resolve_existing_file(mount, &request.path)?;
    reject_symlink(&target)?;
    let current = read_bounded(&target)?;
    verify_hash(&request.expected_sha256, &current)?;
    let text = String::from_utf8(current)
        .map_err(|_| ApiError::Unsupported("file is not valid UTF-8 text".into()))?;
    let count = text.match_indices(&request.old).count();
    if count != 1 {
        return Err(ApiError::Conflict(format!(
            "old text matched {count} times; targeted patch requires exactly one match"
        )));
    }
    let updated = text.replacen(&request.old, &request.new, 1);
    ensure_size(updated.len())?;
    let permissions = fs::metadata(&target).map_err(map_io)?.permissions();
    atomic_write(&target, updated.as_bytes(), Some(permissions), false)?;
    Ok(Json(MutationResponse {
        path: relative(&root, &target),
        size: updated.len(),
        sha256: digest(updated.as_bytes()),
    }))
}

fn require_mount<'a>(
    state: &'a AppState,
    name: &str,
    write: bool,
) -> Result<&'a UserFilesConfig, ApiError> {
    let mount = state
        .config
        .user_files
        .get(name)
        .ok_or_else(|| ApiError::NotFound(format!("user-files mount {name:?} is not configured")))?;
    let allowed = if write { mount.write } else { mount.read };
    if !allowed {
        return Err(ApiError::Forbidden(format!(
            "user-files mount {name:?} does not allow {}",
            if write { "writes" } else { "reads" }
        )));
    }
    Ok(mount)
}

fn resolve_existing(mount: &UserFilesConfig, requested: &str) -> Result<(PathBuf, PathBuf), ApiError> {
    let relative_path = validate_relative(requested, false)?;
    let root = fs::canonicalize(&mount.root).map_err(map_io)?;
    let target = fs::canonicalize(root.join(relative_path)).map_err(map_io)?;
    ensure_within(&root, &target)?;
    Ok((root, target))
}

fn resolve_existing_file(
    mount: &UserFilesConfig,
    requested: &str,
) -> Result<(PathBuf, PathBuf), ApiError> {
    let (root, target) = resolve_existing(mount, requested)?;
    reject_symlink(&target)?;
    if !fs::metadata(&target).map_err(map_io)?.is_file() {
        return Err(ApiError::BadRequest("path is not a regular file".into()));
    }
    Ok((root, target))
}

fn resolve_write_target(
    mount: &UserFilesConfig,
    requested: &str,
) -> Result<(PathBuf, PathBuf), ApiError> {
    let relative_path = validate_relative(requested, true)?;
    let root = fs::canonicalize(&mount.root).map_err(map_io)?;
    let raw = root.join(relative_path);
    let parent = raw
        .parent()
        .ok_or_else(|| ApiError::BadRequest("target must have a parent directory".into()))?;
    let parent = fs::canonicalize(parent).map_err(map_io)?;
    ensure_within(&root, &parent)?;
    let file_name = raw
        .file_name()
        .ok_or_else(|| ApiError::BadRequest("path must identify a file".into()))?;
    let target = parent.join(file_name);
    if target.exists() {
        reject_symlink(&target)?;
        let target = fs::canonicalize(target).map_err(map_io)?;
        ensure_within(&root, &target)?;
        Ok((root, target))
    } else {
        Ok((root, target))
    }
}

fn validate_relative(requested: &str, require_file: bool) -> Result<&Path, ApiError> {
    if require_file && requested.is_empty() {
        return Err(ApiError::BadRequest("path must not be empty".into()));
    }
    let path = Path::new(requested);
    if path.is_absolute() {
        return Err(ApiError::BadRequest("path must be relative to the configured mount".into()));
    }
    if path.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return Err(ApiError::BadRequest("path traversal is not allowed".into()));
    }
    Ok(path)
}

fn ensure_within(root: &Path, target: &Path) -> Result<(), ApiError> {
    if target.starts_with(root) {
        Ok(())
    } else {
        Err(ApiError::Forbidden("resolved path escapes configured mount".into()))
    }
}

fn reject_symlink(path: &Path) -> Result<(), ApiError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(ApiError::Forbidden("mutation through symlinks is not allowed".into()))
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(map_io(error)),
    }
}

fn read_bounded(path: &Path) -> Result<Vec<u8>, ApiError> {
    let metadata = fs::metadata(path).map_err(map_io)?;
    if metadata.len() > MAX_FILE_BYTES as u64 {
        return Err(ApiError::BadRequest(format!(
            "file exceeds user-files mutation/hash limit of {MAX_FILE_BYTES} bytes"
        )));
    }
    fs::read(path).map_err(map_io)
}

fn ensure_size(size: usize) -> Result<(), ApiError> {
    if size > MAX_FILE_BYTES {
        Err(ApiError::BadRequest(format!(
            "content exceeds user-files mutation limit of {MAX_FILE_BYTES} bytes"
        )))
    } else {
        Ok(())
    }
}

fn verify_hash(expected: &str, current: &[u8]) -> Result<(), ApiError> {
    let actual = digest(current);
    if expected.eq_ignore_ascii_case(&actual) {
        Ok(())
    } else {
        Err(ApiError::Conflict(format!(
            "file changed: expected sha256 {expected}, current sha256 {actual}"
        )))
    }
}

fn atomic_write(
    target: &Path,
    content: &[u8],
    permissions: Option<fs::Permissions>,
    no_clobber: bool,
) -> Result<(), ApiError> {
    let parent = target
        .parent()
        .ok_or_else(|| ApiError::BadRequest("target must have a parent directory".into()))?;
    let mut temp = NamedTempFile::new_in(parent).map_err(map_io)?;
    temp.write_all(content).map_err(map_io)?;
    temp.as_file().sync_all().map_err(map_io)?;
    if let Some(permissions) = permissions {
        temp.as_file().set_permissions(permissions).map_err(map_io)?;
    }
    if no_clobber {
        temp.persist_noclobber(target).map_err(|error| {
            if error.error.kind() == std::io::ErrorKind::AlreadyExists {
                ApiError::Conflict("target already exists".into())
            } else {
                map_io(error.error)
            }
        })?;
    } else {
        temp.persist(target).map_err(|error| map_io(error.error))?;
    }
    Ok(())
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

fn kind(metadata: &fs::Metadata) -> &'static str {
    if metadata.file_type().is_symlink() {
        "symlink"
    } else if metadata.is_file() {
        "file"
    } else if metadata.is_dir() {
        "directory"
    } else {
        "other"
    }
}

fn map_io(error: std::io::Error) -> ApiError {
    match error.kind() {
        std::io::ErrorKind::NotFound => ApiError::NotFound(error.to_string()),
        std::io::ErrorKind::PermissionDenied => ApiError::Forbidden(error.to_string()),
        _ => ApiError::Internal(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_traversal() {
        assert!(validate_relative("../secret", false).is_err());
    }
}

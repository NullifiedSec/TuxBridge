use std::{
    fs,
    io::Write,
    path::{Component, Path, PathBuf},
};

use axum::{extract::State, Json};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;

use crate::{config::WorkspaceConfig, error::ApiError, state::AppState};

const MAX_MUTATION_BYTES: usize = 8_388_608;

#[derive(Debug, Deserialize)]
pub struct HashRequest {
    workspace: String,
    path: String,
}

#[derive(Debug, Deserialize)]
pub struct WriteRequest {
    workspace: String,
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
    workspace: String,
    path: String,
    expected_sha256: String,
    old: String,
    new: String,
}

#[derive(Debug, Serialize)]
pub struct HashResponse {
    path: String,
    size: u64,
    sha256: String,
}

#[derive(Debug, Serialize)]
pub struct MutationResponse {
    path: String,
    size: usize,
    sha256: String,
}

pub async fn hash_file(
    State(state): State<AppState>,
    Json(request): Json<HashRequest>,
) -> Result<Json<HashResponse>, ApiError> {
    let workspace = workspace_with_capability(&state, &request.workspace, false)?;
    let (root, target) = resolve_existing_file(workspace, &request.path)?;
    let bytes = read_bounded(&target)?;
    Ok(Json(HashResponse {
        path: relative_display(&root, &target),
        size: bytes.len() as u64,
        sha256: digest(&bytes),
    }))
}

pub async fn write_file(
    State(state): State<AppState>,
    Json(request): Json<WriteRequest>,
) -> Result<Json<MutationResponse>, ApiError> {
    let workspace = workspace_with_capability(&state, &request.workspace, true)?;
    let bytes = request.content.as_bytes();
    ensure_mutation_size(bytes.len())?;
    let (root, target) = resolve_write_target(workspace, &request.path)?;

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
            verify_expected_hash(expected, &current)?;
            let permissions = fs::metadata(&target).map_err(map_io)?.permissions();
            atomic_write(&target, bytes, Some(permissions), false)?;
        }
    }

    Ok(Json(MutationResponse {
        path: relative_display(&root, &target),
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

    let workspace = workspace_with_capability(&state, &request.workspace, true)?;
    let (root, target) = resolve_existing_file(workspace, &request.path)?;
    reject_symlink(&target)?;
    let current = read_bounded(&target)?;
    verify_expected_hash(&request.expected_sha256, &current)?;
    let text = String::from_utf8(current)
        .map_err(|_| ApiError::Unsupported("file is not valid UTF-8 text".into()))?;
    let updated = apply_unique_replace(&text, &request.old, &request.new)?;
    ensure_mutation_size(updated.len())?;

    let permissions = fs::metadata(&target).map_err(map_io)?.permissions();
    atomic_write(&target, updated.as_bytes(), Some(permissions), false)?;

    Ok(Json(MutationResponse {
        path: relative_display(&root, &target),
        size: updated.len(),
        sha256: digest(updated.as_bytes()),
    }))
}

fn workspace_with_capability<'a>(
    state: &'a AppState,
    name: &str,
    write: bool,
) -> Result<&'a WorkspaceConfig, ApiError> {
    let workspace = state
        .config
        .workspaces
        .get(name)
        .ok_or_else(|| ApiError::NotFound(format!("workspace {name:?} is not configured")))?;
    let allowed = if write {
        workspace.capabilities.fs_write
    } else {
        workspace.capabilities.fs_read
    };
    if !allowed {
        return Err(ApiError::Forbidden(format!(
            "workspace {name:?} does not allow filesystem {}",
            if write { "writes" } else { "reads" }
        )));
    }
    Ok(workspace)
}

fn resolve_existing_file(
    workspace: &WorkspaceConfig,
    requested: &str,
) -> Result<(PathBuf, PathBuf), ApiError> {
    let relative = validate_relative_file_path(requested)?;
    let root = canonical_root(workspace)?;
    let raw = root.join(relative);
    reject_symlink(&raw)?;
    let target = fs::canonicalize(&raw).map_err(map_io)?;
    ensure_within(&root, &target)?;
    if !fs::metadata(&target).map_err(map_io)?.is_file() {
        return Err(ApiError::BadRequest("path is not a regular file".into()));
    }
    Ok((root, target))
}

fn resolve_write_target(
    workspace: &WorkspaceConfig,
    requested: &str,
) -> Result<(PathBuf, PathBuf), ApiError> {
    let relative = validate_relative_file_path(requested)?;
    let root = canonical_root(workspace)?;
    let target = root.join(relative);
    let parent = target
        .parent()
        .ok_or_else(|| ApiError::BadRequest("target must have a parent directory".into()))?;
    let canonical_parent = fs::canonicalize(parent).map_err(map_io)?;
    ensure_within(&root, &canonical_parent)?;

    let file_name = target
        .file_name()
        .ok_or_else(|| ApiError::BadRequest("path must identify a file".into()))?;
    let resolved = canonical_parent.join(file_name);
    if resolved.exists() {
        reject_symlink(&resolved)?;
        let canonical_target = fs::canonicalize(&resolved).map_err(map_io)?;
        ensure_within(&root, &canonical_target)?;
        Ok((root, canonical_target))
    } else {
        Ok((root, resolved))
    }
}

fn validate_relative_file_path(requested: &str) -> Result<&Path, ApiError> {
    if requested.is_empty() {
        return Err(ApiError::BadRequest("path must not be empty".into()));
    }
    let path = Path::new(requested);
    if path.is_absolute() {
        return Err(ApiError::BadRequest("path must be relative to the workspace".into()));
    }
    for component in path.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(ApiError::BadRequest(
                    "path traversal outside the workspace is not allowed".into(),
                ));
            }
        }
    }
    Ok(path)
}

fn canonical_root(workspace: &WorkspaceConfig) -> Result<PathBuf, ApiError> {
    fs::canonicalize(&workspace.root)
        .map_err(|error| ApiError::Internal(format!("failed to resolve workspace root: {error}")))
}

fn ensure_within(root: &Path, target: &Path) -> Result<(), ApiError> {
    if target.starts_with(root) {
        Ok(())
    } else {
        Err(ApiError::Forbidden("resolved path escapes workspace root".into()))
    }
}

fn reject_symlink(path: &Path) -> Result<(), ApiError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(ApiError::Forbidden(
            "filesystem mutation through symlinks is not allowed".into(),
        )),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(map_io(error)),
    }
}

fn read_bounded(path: &Path) -> Result<Vec<u8>, ApiError> {
    let metadata = fs::metadata(path).map_err(map_io)?;
    if metadata.len() > MAX_MUTATION_BYTES as u64 {
        return Err(ApiError::BadRequest(format!(
            "file exceeds mutation limit of {MAX_MUTATION_BYTES} bytes"
        )));
    }
    fs::read(path).map_err(map_io)
}

fn ensure_mutation_size(size: usize) -> Result<(), ApiError> {
    if size > MAX_MUTATION_BYTES {
        Err(ApiError::BadRequest(format!(
            "content exceeds mutation limit of {MAX_MUTATION_BYTES} bytes"
        )))
    } else {
        Ok(())
    }
}

fn verify_expected_hash(expected: &str, current: &[u8]) -> Result<(), ApiError> {
    let actual = digest(current);
    if expected.eq_ignore_ascii_case(&actual) {
        Ok(())
    } else {
        Err(ApiError::Conflict(format!(
            "file changed: expected sha256 {expected}, current sha256 {actual}"
        )))
    }
}

fn apply_unique_replace(text: &str, old: &str, new: &str) -> Result<String, ApiError> {
    let matches = text.match_indices(old).count();
    match matches {
        0 => Err(ApiError::Conflict("old text was not found".into())),
        1 => Ok(text.replacen(old, new, 1)),
        count => Err(ApiError::Conflict(format!(
            "old text matched {count} times; targeted patch requires exactly one match"
        ))),
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
            let io_error = error.error;
            if io_error.kind() == std::io::ErrorKind::AlreadyExists {
                ApiError::Conflict("target already exists".into())
            } else {
                map_io(io_error)
            }
        })?;
    } else {
        temp.persist(target)
            .map_err(|error| map_io(error.error))?;
    }
    Ok(())
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn relative_display(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
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
    fn unique_patch_rejects_ambiguous_match() {
        let error = apply_unique_replace("one one", "one", "two").unwrap_err();
        assert!(error.to_string().contains("matched 2 times"));
    }

    #[test]
    fn unique_patch_replaces_exactly_once() {
        let updated = apply_unique_replace("hello world", "world", "rust").unwrap();
        assert_eq!(updated, "hello rust");
    }

    #[test]
    fn rejects_parent_traversal() {
        let error = validate_relative_file_path("../secret").unwrap_err();
        assert!(error.to_string().contains("traversal"));
    }
}

use std::{
    fs::{self, File},
    io::Read,
    path::{Component, Path, PathBuf},
};

use axum::{extract::State, Json};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    config::WorkspaceConfig,
    error::ApiError,
    state::AppState,
};

const DEFAULT_MAX_RESULTS: usize = 100;
const HARD_MAX_RESULTS: usize = 1000;
const DEFAULT_MAX_READ_BYTES: usize = 1_048_576;
const HARD_MAX_READ_BYTES: usize = 8_388_608;

#[derive(Debug, Deserialize)]
pub struct PathRequest {
    workspace: String,
    #[serde(default)]
    path: String,
}

#[derive(Debug, Deserialize)]
pub struct ReadRequest {
    workspace: String,
    path: String,
    start_line: Option<usize>,
    end_line: Option<usize>,
    max_bytes: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct BatchReadRequest {
    workspace: String,
    files: Vec<ReadSpec>,
}

#[derive(Debug, Deserialize)]
pub struct ReadSpec {
    path: String,
    start_line: Option<usize>,
    end_line: Option<usize>,
    max_bytes: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct SearchRequest {
    workspace: String,
    #[serde(default)]
    path: String,
    query: String,
    max_results: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct DirectoryEntry {
    name: String,
    path: String,
    kind: EntryKind,
    size: u64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    File,
    Directory,
    Symlink,
    Other,
}

#[derive(Debug, Serialize)]
pub struct StatResponse {
    path: String,
    kind: EntryKind,
    size: u64,
    readonly: bool,
}

#[derive(Debug, Serialize)]
pub struct ReadResponse {
    path: String,
    content: String,
    size: u64,
    sha256: String,
    sha256_scope: &'static str,
    truncated: bool,
    start_line: usize,
    end_line: usize,
}

#[derive(Debug, Serialize)]
pub struct BatchReadResult {
    path: String,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<ReadResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SearchMatch {
    path: String,
    line: usize,
    column: usize,
    preview: String,
}

#[derive(Debug, Serialize)]
pub struct SearchResponse {
    matches: Vec<SearchMatch>,
    truncated: bool,
}

pub async fn list_directory(
    State(state): State<AppState>,
    Json(request): Json<PathRequest>,
) -> Result<Json<Vec<DirectoryEntry>>, ApiError> {
    let workspace = readable_workspace(&state, &request.workspace)?;
    let (root, target) = resolve_existing(workspace, &request.path)?;
    let metadata = fs::metadata(&target).map_err(map_io)?;
    if !metadata.is_dir() {
        return Err(ApiError::BadRequest("path is not a directory".into()));
    }

    let mut entries = Vec::new();
    for entry in fs::read_dir(&target).map_err(map_io)? {
        let entry = entry.map_err(map_io)?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path).map_err(map_io)?;
        entries.push(DirectoryEntry {
            name: entry.file_name().to_string_lossy().into_owned(),
            path: relative_display(&root, &path),
            kind: entry_kind(&metadata),
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
    let workspace = readable_workspace(&state, &request.workspace)?;
    let (root, target) = resolve_existing(workspace, &request.path)?;
    let metadata = fs::symlink_metadata(&target).map_err(map_io)?;
    Ok(Json(StatResponse {
        path: relative_display(&root, &target),
        kind: entry_kind(&metadata),
        size: metadata.len(),
        readonly: metadata.permissions().readonly(),
    }))
}

pub async fn read_file(
    State(state): State<AppState>,
    Json(request): Json<ReadRequest>,
) -> Result<Json<ReadResponse>, ApiError> {
    let workspace = readable_workspace(&state, &request.workspace)?;
    let result = read_one(
        workspace,
        ReadSpec {
            path: request.path,
            start_line: request.start_line,
            end_line: request.end_line,
            max_bytes: request.max_bytes,
        },
    )?;
    Ok(Json(result))
}

pub async fn read_files(
    State(state): State<AppState>,
    Json(request): Json<BatchReadRequest>,
) -> Result<Json<Vec<BatchReadResult>>, ApiError> {
    let workspace = readable_workspace(&state, &request.workspace)?;
    let mut results = Vec::with_capacity(request.files.len());

    for spec in request.files {
        let path = spec.path.clone();
        match read_one(workspace, spec) {
            Ok(result) => results.push(BatchReadResult {
                path,
                ok: true,
                result: Some(result),
                error: None,
            }),
            Err(error) => results.push(BatchReadResult {
                path,
                ok: false,
                result: None,
                error: Some(error.to_string()),
            }),
        }
    }

    Ok(Json(results))
}

pub async fn search_files(
    State(state): State<AppState>,
    Json(request): Json<SearchRequest>,
) -> Result<Json<SearchResponse>, ApiError> {
    if request.query.is_empty() {
        return Err(ApiError::BadRequest("query must not be empty".into()));
    }

    let workspace = readable_workspace(&state, &request.workspace)?;
    let (root, target) = resolve_existing(workspace, &request.path)?;
    let limit = request
        .max_results
        .unwrap_or(DEFAULT_MAX_RESULTS)
        .clamp(1, HARD_MAX_RESULTS);

    let mut matches = Vec::new();
    let truncated = search_path(&root, &target, &request.query, limit, &mut matches)?;
    Ok(Json(SearchResponse { matches, truncated }))
}

fn readable_workspace<'a>(state: &'a AppState, name: &str) -> Result<&'a WorkspaceConfig, ApiError> {
    let workspace = state
        .config
        .workspaces
        .get(name)
        .ok_or_else(|| ApiError::NotFound(format!("workspace {name:?} is not configured")))?;
    if !workspace.capabilities.fs_read {
        return Err(ApiError::Forbidden(format!(
            "workspace {name:?} does not allow filesystem reads"
        )));
    }
    Ok(workspace)
}

fn resolve_existing(workspace: &WorkspaceConfig, requested: &str) -> Result<(PathBuf, PathBuf), ApiError> {
    let relative = Path::new(requested);
    validate_relative(relative)?;

    let root = fs::canonicalize(&workspace.root)
        .map_err(|error| ApiError::Internal(format!("failed to resolve workspace root: {error}")))?;
    let target = fs::canonicalize(root.join(relative)).map_err(map_io)?;
    if !target.starts_with(&root) {
        return Err(ApiError::Forbidden("resolved path escapes workspace root".into()));
    }
    Ok((root, target))
}

fn validate_relative(path: &Path) -> Result<(), ApiError> {
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
    Ok(())
}

fn read_one(workspace: &WorkspaceConfig, spec: ReadSpec) -> Result<ReadResponse, ApiError> {
    if let (Some(start), Some(end)) = (spec.start_line, spec.end_line) {
        if start == 0 || end == 0 || end < start {
            return Err(ApiError::BadRequest("invalid line range".into()));
        }
    } else if spec.start_line == Some(0) || spec.end_line == Some(0) {
        return Err(ApiError::BadRequest("line numbers are 1-based".into()));
    }

    let (root, target) = resolve_existing(workspace, &spec.path)?;
    let metadata = fs::metadata(&target).map_err(map_io)?;
    if !metadata.is_file() {
        return Err(ApiError::BadRequest("path is not a regular file".into()));
    }

    let max_bytes = spec
        .max_bytes
        .unwrap_or(DEFAULT_MAX_READ_BYTES)
        .clamp(1, HARD_MAX_READ_BYTES);
    let mut file = File::open(&target).map_err(map_io)?;
    let mut returned = Vec::with_capacity(max_bytes.saturating_add(1));
    file.by_ref()
        .take(max_bytes.saturating_add(1) as u64)
        .read_to_end(&mut returned)
        .map_err(map_io)?;

    let truncated = returned.len() > max_bytes;
    if truncated {
        returned.truncate(max_bytes);
    }

    let text = match std::str::from_utf8(&returned) {
        Ok(text) => text.to_owned(),
        Err(error) if truncated && error.error_len().is_none() => {
            returned.truncate(error.valid_up_to());
            String::from_utf8(returned)
                .map_err(|_| ApiError::Unsupported("file is not valid UTF-8 text".into()))?
        }
        Err(_) => return Err(ApiError::Unsupported("file is not valid UTF-8 text".into())),
    };

    let all_lines: Vec<&str> = text.lines().collect();
    let start = spec.start_line.unwrap_or(1);
    let end = spec.end_line.unwrap_or(all_lines.len().max(1));
    let selected = if all_lines.is_empty() || start > all_lines.len() {
        String::new()
    } else {
        all_lines[(start - 1)..end.min(all_lines.len())].join("\n")
    };
    let actual_end = if selected.is_empty() {
        start.saturating_sub(1)
    } else {
        end.min(all_lines.len())
    };
    let sha256 = format!("{:x}", Sha256::digest(selected.as_bytes()));

    Ok(ReadResponse {
        path: relative_display(&root, &target),
        content: selected,
        size: metadata.len(),
        sha256,
        sha256_scope: "response_content",
        truncated,
        start_line: start,
        end_line: actual_end,
    })
}

fn search_path(
    root: &Path,
    target: &Path,
    query: &str,
    limit: usize,
    matches: &mut Vec<SearchMatch>,
) -> Result<bool, ApiError> {
    let metadata = fs::symlink_metadata(target).map_err(map_io)?;
    if metadata.file_type().is_symlink() {
        return Ok(false);
    }

    if metadata.is_dir() {
        for entry in fs::read_dir(target).map_err(map_io)? {
            let entry = entry.map_err(map_io)?;
            if search_path(root, &entry.path(), query, limit, matches)? {
                return Ok(true);
            }
        }
        return Ok(false);
    }

    if !metadata.is_file() || metadata.len() > HARD_MAX_READ_BYTES as u64 {
        return Ok(false);
    }

    let bytes = fs::read(target).map_err(map_io)?;
    let Ok(text) = std::str::from_utf8(&bytes) else {
        return Ok(false);
    };

    for (line_index, line) in text.lines().enumerate() {
        let mut offset = 0usize;
        while let Some(found) = line[offset..].find(query) {
            matches.push(SearchMatch {
                path: relative_display(root, target),
                line: line_index + 1,
                column: offset + found + 1,
                preview: line.to_owned(),
            });
            if matches.len() >= limit {
                return Ok(true);
            }
            offset += found + query.len();
        }
    }

    Ok(false)
}

fn relative_display(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

fn entry_kind(metadata: &fs::Metadata) -> EntryKind {
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        EntryKind::Symlink
    } else if file_type.is_file() {
        EntryKind::File
    } else if file_type.is_dir() {
        EntryKind::Directory
    } else {
        EntryKind::Other
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
    fn rejects_parent_components() {
        let error = validate_relative(Path::new("../secret")).unwrap_err();
        assert!(error.to_string().contains("traversal"));
    }

    #[test]
    fn rejects_absolute_paths() {
        let error = validate_relative(Path::new("/etc/passwd")).unwrap_err();
        assert!(error.to_string().contains("relative"));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_escape() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret.txt"), "secret").unwrap();
        symlink(outside.path(), root.path().join("escape")).unwrap();

        let workspace = WorkspaceConfig {
            root: root.path().to_path_buf(),
            capabilities: Default::default(),
        };
        let error = resolve_existing(&workspace, "escape/secret.txt").unwrap_err();
        assert!(error.to_string().contains("escapes workspace"));
    }
}

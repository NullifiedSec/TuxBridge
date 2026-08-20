use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Write,
    path::{Component, Path, PathBuf},
};

use axum::{extract::State, Json};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;

use crate::{config::WorkspaceConfig, error::ApiError, state::AppState};

const MAX_CODE_FILE_BYTES: usize = 8 * 1024 * 1024;
const MAX_CONTEXT_BYTES: usize = 512 * 1024;
const MAX_EDIT_FILES: usize = 64;
const MAX_EDITS_PER_FILE: usize = 256;
const MAX_REFERENCE_RESULTS: usize = 2_000;
const MAX_REFERENCE_FILE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_SYMBOLS: usize = 2_000;

#[derive(Debug, Deserialize)]
pub struct CodeContextRequest {
    workspace: String,
    path: String,
    start_line: Option<usize>,
    end_line: Option<usize>,
    context_before: Option<usize>,
    context_after: Option<usize>,
    max_bytes: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct CodeContextResponse {
    path: String,
    sha256: String,
    total_lines: usize,
    start_line: usize,
    end_line: usize,
    content: String,
    truncated: bool,
}

#[derive(Debug, Deserialize)]
pub struct CodeSymbolsRequest {
    workspace: String,
    path: String,
}

#[derive(Debug, Serialize)]
pub struct CodeSymbol {
    name: String,
    kind: String,
    line: usize,
    signature: String,
}

#[derive(Debug, Serialize)]
pub struct CodeSymbolsResponse {
    path: String,
    sha256: String,
    language: String,
    symbols: Vec<CodeSymbol>,
    truncated: bool,
}

#[derive(Debug, Deserialize)]
pub struct CodeReferencesRequest {
    workspace: String,
    identifier: String,
    path: Option<String>,
    max_results: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct CodeReference {
    path: String,
    line: usize,
    column: usize,
    preview: String,
}

#[derive(Debug, Serialize)]
pub struct CodeReferencesResponse {
    identifier: String,
    references: Vec<CodeReference>,
    files_scanned: usize,
    truncated: bool,
}

#[derive(Debug, Deserialize)]
pub struct CodeEditPlanRequest {
    workspace: String,
    files: Vec<FileEditPlan>,
    #[serde(default)]
    dry_run: bool,
}

#[derive(Debug, Deserialize)]
pub struct FileEditPlan {
    path: String,
    expected_sha256: String,
    edits: Vec<CodeEdit>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CodeEdit {
    ReplaceExact { old: String, new: String },
    ReplaceLines { start_line: usize, end_line: usize, new: String },
    InsertBefore { line: usize, new: String },
    InsertAfter { line: usize, new: String },
}

#[derive(Debug, Serialize)]
pub struct CodeEditPlanResponse {
    dry_run: bool,
    files: Vec<FileEditResult>,
}

#[derive(Debug, Serialize)]
pub struct FileEditResult {
    path: String,
    old_sha256: String,
    new_sha256: String,
    changed: bool,
    additions: usize,
    deletions: usize,
    preview: String,
}

#[derive(Debug, Deserialize)]
pub struct CodeTaskRequest {
    workspace: String,
}

#[derive(Debug, Serialize)]
pub struct CodeTask {
    id: String,
    label: String,
    argv: Vec<String>,
    source: String,
    execution_risk: String,
}

#[derive(Debug, Serialize)]
pub struct CodeTasksResponse {
    workspace: String,
    tasks: Vec<CodeTask>,
}

pub async fn code_context(
    State(state): State<AppState>,
    Json(request): Json<CodeContextRequest>,
) -> Result<Json<CodeContextResponse>, ApiError> {
    let workspace = workspace_with_capability(&state, &request.workspace, false)?;
    let (root, target) = resolve_existing_file(workspace, &request.path)?;
    let bytes = read_bounded(&target, MAX_CODE_FILE_BYTES)?;
    let sha256 = digest(&bytes);
    let text = String::from_utf8(bytes)
        .map_err(|_| ApiError::Unsupported("file is not valid UTF-8 text".into()))?;
    let lines = text.lines().collect::<Vec<_>>();
    let total_lines = lines.len();
    let requested_start = request.start_line.unwrap_or(1).max(1);
    let requested_end = request.end_line.unwrap_or(requested_start).max(requested_start);
    let start_line = requested_start.saturating_sub(request.context_before.unwrap_or(20)).max(1);
    let end_line = requested_end
        .saturating_add(request.context_after.unwrap_or(20))
        .min(total_lines.max(1));
    let max_bytes = request.max_bytes.unwrap_or(MAX_CONTEXT_BYTES).clamp(1, MAX_CONTEXT_BYTES);

    let mut content = String::new();
    let mut truncated = false;
    for line_number in start_line..=end_line {
        let line = lines.get(line_number.saturating_sub(1)).copied().unwrap_or("");
        let rendered = format!("{line_number:>6} | {line}\n");
        if content.len().saturating_add(rendered.len()) > max_bytes {
            truncated = true;
            break;
        }
        content.push_str(&rendered);
    }

    Ok(Json(CodeContextResponse {
        path: relative_display(&root, &target),
        sha256,
        total_lines,
        start_line,
        end_line,
        content,
        truncated,
    }))
}

pub async fn code_symbols(
    State(state): State<AppState>,
    Json(request): Json<CodeSymbolsRequest>,
) -> Result<Json<CodeSymbolsResponse>, ApiError> {
    let workspace = workspace_with_capability(&state, &request.workspace, false)?;
    let (root, target) = resolve_existing_file(workspace, &request.path)?;
    let bytes = read_bounded(&target, MAX_CODE_FILE_BYTES)?;
    let sha256 = digest(&bytes);
    let text = String::from_utf8(bytes)
        .map_err(|_| ApiError::Unsupported("file is not valid UTF-8 text".into()))?;
    let language = language_for(&target).to_owned();
    let mut symbols = Vec::new();
    let mut truncated = false;

    for (index, line) in text.lines().enumerate() {
        if let Some((kind, name)) = detect_symbol(&language, line) {
            symbols.push(CodeSymbol {
                name,
                kind,
                line: index + 1,
                signature: line.trim().chars().take(300).collect(),
            });
            if symbols.len() >= MAX_SYMBOLS {
                truncated = true;
                break;
            }
        }
    }

    Ok(Json(CodeSymbolsResponse {
        path: relative_display(&root, &target),
        sha256,
        language,
        symbols,
        truncated,
    }))
}

pub async fn code_references(
    State(state): State<AppState>,
    Json(request): Json<CodeReferencesRequest>,
) -> Result<Json<CodeReferencesResponse>, ApiError> {
    validate_identifier(&request.identifier)?;
    let workspace = workspace_with_capability(&state, &request.workspace, false)?;
    let root = canonical_root(workspace)?;
    let start = if let Some(path) = request.path.as_deref() {
        resolve_existing_path(&root, path)?
    } else {
        root.clone()
    };
    let max_results = request
        .max_results
        .unwrap_or(500)
        .clamp(1, MAX_REFERENCE_RESULTS);
    let mut response = CodeReferencesResponse {
        identifier: request.identifier.clone(),
        references: Vec::new(),
        files_scanned: 0,
        truncated: false,
    };
    walk_references(
        &root,
        &start,
        &request.identifier,
        max_results,
        &mut response,
    )?;
    Ok(Json(response))
}

pub async fn code_edit_plan(
    State(state): State<AppState>,
    Json(request): Json<CodeEditPlanRequest>,
) -> Result<Json<CodeEditPlanResponse>, ApiError> {
    if request.files.is_empty() || request.files.len() > MAX_EDIT_FILES {
        return Err(ApiError::BadRequest(format!(
            "files must contain between 1 and {MAX_EDIT_FILES} entries"
        )));
    }
    let workspace = workspace_with_capability(&state, &request.workspace, true)?;
    let root = canonical_root(workspace)?;
    let mut seen_paths = BTreeSet::new();
    let mut prepared = Vec::with_capacity(request.files.len());

    // Preflight every file and every hash before any mutation is attempted.
    for file in &request.files {
        if file.edits.is_empty() || file.edits.len() > MAX_EDITS_PER_FILE {
            return Err(ApiError::BadRequest(format!(
                "each file must contain between 1 and {MAX_EDITS_PER_FILE} edits"
            )));
        }
        let target = resolve_existing_from_root(&root, &file.path)?;
        let display = relative_display(&root, &target);
        if !seen_paths.insert(display.clone()) {
            return Err(ApiError::BadRequest(format!("duplicate edit target {display:?}")));
        }
        reject_symlink(&target)?;
        let current = read_bounded(&target, MAX_CODE_FILE_BYTES)?;
        let old_sha = digest(&current);
        if !file.expected_sha256.eq_ignore_ascii_case(&old_sha) {
            return Err(ApiError::Conflict(format!(
                "file changed: {display} expected sha256 {}, current sha256 {old_sha}",
                file.expected_sha256
            )));
        }
        let current_text = String::from_utf8(current)
            .map_err(|_| ApiError::Unsupported(format!("{display} is not valid UTF-8 text")))?;
        let updated = apply_edits(current_text.clone(), &file.edits)?;
        if updated.len() > MAX_CODE_FILE_BYTES {
            return Err(ApiError::BadRequest(format!(
                "updated file {display} exceeds {MAX_CODE_FILE_BYTES} bytes"
            )));
        }
        let permissions = fs::metadata(&target).map_err(map_io)?.permissions();
        prepared.push(PreparedEdit {
            target,
            display,
            original: current_text,
            updated,
            permissions,
            old_sha,
        });
    }

    let results = prepared
        .iter()
        .map(|item| edit_result(item))
        .collect::<Vec<_>>();

    if !request.dry_run {
        // All hashes have passed before this point. Each individual replacement is
        // same-directory atomic; multi-file plans are preflighted but not claimed to
        // be filesystem-transaction atomic across files.
        for item in &prepared {
            atomic_write(
                &item.target,
                item.updated.as_bytes(),
                item.permissions.clone(),
            )?;
        }
    }

    Ok(Json(CodeEditPlanResponse {
        dry_run: request.dry_run,
        files: results,
    }))
}

pub async fn discover_code_tasks(
    State(state): State<AppState>,
    Json(request): Json<CodeTaskRequest>,
) -> Result<Json<CodeTasksResponse>, ApiError> {
    let workspace = workspace_with_capability(&state, &request.workspace, false)?;
    let root = canonical_root(workspace)?;
    let mut tasks = Vec::new();

    if root.join("Cargo.toml").is_file() {
        tasks.extend([
            task("rust-check", "Rust check", &["cargo", "check", "--all-targets", "--all-features"], "Cargo.toml"),
            task("rust-test", "Rust tests", &["cargo", "test", "--all-targets", "--all-features"], "Cargo.toml"),
            task("rust-format-check", "Rust format check", &["cargo", "fmt", "--all", "--", "--check"], "Cargo.toml"),
            task("rust-clippy", "Rust Clippy", &["cargo", "clippy", "--all-targets", "--all-features", "--", "-D", "warnings"], "Cargo.toml"),
        ]);
    }
    if root.join("go.mod").is_file() {
        tasks.extend([
            task("go-test", "Go tests", &["go", "test", "./..."], "go.mod"),
            task("go-vet", "Go vet", &["go", "vet", "./..."], "go.mod"),
        ]);
    }
    if root.join("package.json").is_file() {
        let manager = if root.join("bun.lock").is_file() || root.join("bun.lockb").is_file() {
            "bun"
        } else if root.join("pnpm-lock.yaml").is_file() {
            "pnpm"
        } else if root.join("yarn.lock").is_file() {
            "yarn"
        } else {
            "npm"
        };
        if let Ok(raw) = fs::read_to_string(root.join("package.json")) {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) {
                if let Some(scripts) = value.get("scripts").and_then(|value| value.as_object()) {
                    for name in ["typecheck", "check", "lint", "test", "build", "format"] {
                        if scripts.contains_key(name) {
                            let argv = match manager {
                                "bun" => vec!["bun".into(), "run".into(), name.into()],
                                "pnpm" => vec!["pnpm".into(), "run".into(), name.into()],
                                "yarn" => vec!["yarn".into(), name.into()],
                                _ => vec!["npm".into(), "run".into(), name.into()],
                            };
                            tasks.push(CodeTask {
                                id: format!("js-{name}"),
                                label: format!("package script: {name}"),
                                argv,
                                source: "package.json".into(),
                                execution_risk: "project scripts can execute arbitrary code with the TuxBridge service user's privileges".into(),
                            });
                        }
                    }
                }
            }
        }
    }
    if root.join("pyproject.toml").is_file() {
        tasks.push(task("python-test", "Python tests", &["python3", "-m", "pytest"], "pyproject.toml"));
    }
    if root.join("composer.json").is_file() {
        tasks.push(task("composer-test", "Composer test script", &["composer", "test"], "composer.json"));
    }

    Ok(Json(CodeTasksResponse {
        workspace: request.workspace,
        tasks,
    }))
}

struct PreparedEdit {
    target: PathBuf,
    display: String,
    original: String,
    updated: String,
    permissions: fs::Permissions,
    old_sha: String,
}

fn edit_result(item: &PreparedEdit) -> FileEditResult {
    let old_lines = item.original.lines().collect::<Vec<_>>();
    let new_lines = item.updated.lines().collect::<Vec<_>>();
    let common_prefix = old_lines
        .iter()
        .zip(new_lines.iter())
        .take_while(|(left, right)| left == right)
        .count();
    let common_suffix = old_lines[common_prefix..]
        .iter()
        .rev()
        .zip(new_lines[common_prefix..].iter().rev())
        .take_while(|(left, right)| left == right)
        .count();
    let old_end = old_lines.len().saturating_sub(common_suffix);
    let new_end = new_lines.len().saturating_sub(common_suffix);
    let deleted = old_end.saturating_sub(common_prefix);
    let added = new_end.saturating_sub(common_prefix);
    let preview = render_preview(
        &old_lines,
        &new_lines,
        common_prefix,
        old_end,
        new_end,
    );

    FileEditResult {
        path: item.display.clone(),
        old_sha256: item.old_sha.clone(),
        new_sha256: digest(item.updated.as_bytes()),
        changed: item.original != item.updated,
        additions: added,
        deletions: deleted,
        preview,
    }
}

fn render_preview(
    old_lines: &[&str],
    new_lines: &[&str],
    start: usize,
    old_end: usize,
    new_end: usize,
) -> String {
    let context_start = start.saturating_sub(3);
    let old_context_end = (old_end + 3).min(old_lines.len());
    let new_context_end = (new_end + 3).min(new_lines.len());
    let mut out = String::new();
    out.push_str(&format!("@@ line {} @@\n", start + 1));
    for line in &old_lines[context_start..start] {
        out.push_str("  ");
        out.push_str(line);
        out.push('\n');
    }
    for line in &old_lines[start..old_end] {
        out.push_str("- ");
        out.push_str(line);
        out.push('\n');
    }
    for line in &new_lines[start..new_end] {
        out.push_str("+ ");
        out.push_str(line);
        out.push('\n');
    }
    let trailing = old_context_end.saturating_sub(old_end).min(new_context_end.saturating_sub(new_end));
    for offset in 0..trailing {
        if let Some(line) = new_lines.get(new_end + offset) {
            out.push_str("  ");
            out.push_str(line);
            out.push('\n');
        }
    }
    if out.len() > 32 * 1024 {
        out.truncate(32 * 1024);
        out.push_str("\n... preview truncated ...\n");
    }
    out
}

fn apply_edits(mut text: String, edits: &[CodeEdit]) -> Result<String, ApiError> {
    for edit in edits {
        text = match edit {
            CodeEdit::ReplaceExact { old, new } => {
                if old.is_empty() {
                    return Err(ApiError::BadRequest("replace_exact old text must not be empty".into()));
                }
                let count = text.match_indices(old).count();
                if count != 1 {
                    return Err(ApiError::Conflict(format!(
                        "replace_exact requires exactly one match, found {count}"
                    )));
                }
                text.replacen(old, new, 1)
            }
            CodeEdit::ReplaceLines { start_line, end_line, new } => {
                replace_lines(&text, *start_line, *end_line, new)?
            }
            CodeEdit::InsertBefore { line, new } => insert_at_line(&text, *line, new, false)?,
            CodeEdit::InsertAfter { line, new } => insert_at_line(&text, *line, new, true)?,
        };
    }
    Ok(text)
}

fn replace_lines(text: &str, start_line: usize, end_line: usize, new: &str) -> Result<String, ApiError> {
    if start_line == 0 || end_line < start_line {
        return Err(ApiError::BadRequest("invalid line range".into()));
    }
    let lines = split_preserving_newline(text);
    if end_line > lines.len() {
        return Err(ApiError::Conflict(format!(
            "line range {start_line}..={end_line} exceeds file length {}",
            lines.len()
        )));
    }
    let mut output = String::new();
    for line in &lines[..start_line - 1] {
        output.push_str(line);
    }
    output.push_str(new);
    if !new.is_empty() && !new.ends_with('\n') && end_line < lines.len() {
        output.push('\n');
    }
    for line in &lines[end_line..] {
        output.push_str(line);
    }
    Ok(output)
}

fn insert_at_line(text: &str, line: usize, new: &str, after: bool) -> Result<String, ApiError> {
    if line == 0 {
        return Err(ApiError::BadRequest("line numbers are 1-based".into()));
    }
    let lines = split_preserving_newline(text);
    if line > lines.len().max(1) {
        return Err(ApiError::Conflict(format!(
            "line {line} exceeds file length {}",
            lines.len()
        )));
    }
    let index = if after { line.min(lines.len()) } else { line.saturating_sub(1).min(lines.len()) };
    let mut output = String::new();
    for existing in &lines[..index] {
        output.push_str(existing);
    }
    output.push_str(new);
    if !new.is_empty() && !new.ends_with('\n') && index < lines.len() {
        output.push('\n');
    }
    for existing in &lines[index..] {
        output.push_str(existing);
    }
    Ok(output)
}

fn split_preserving_newline(text: &str) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    text.split_inclusive('\n').map(str::to_owned).collect()
}

fn walk_references(
    root: &Path,
    path: &Path,
    identifier: &str,
    max_results: usize,
    response: &mut CodeReferencesResponse,
) -> Result<(), ApiError> {
    if response.references.len() >= max_results {
        response.truncated = true;
        return Ok(());
    }
    let metadata = fs::symlink_metadata(path).map_err(map_io)?;
    if metadata.file_type().is_symlink() {
        return Ok(());
    }
    if metadata.is_file() {
        if metadata.len() > MAX_REFERENCE_FILE_BYTES || is_probably_binary(path) {
            return Ok(());
        }
        response.files_scanned += 1;
        let Ok(text) = fs::read_to_string(path) else {
            return Ok(());
        };
        for (line_index, line) in text.lines().enumerate() {
            for column in identifier_columns(line, identifier) {
                response.references.push(CodeReference {
                    path: relative_display(root, path),
                    line: line_index + 1,
                    column: column + 1,
                    preview: line.trim().chars().take(300).collect(),
                });
                if response.references.len() >= max_results {
                    response.truncated = true;
                    return Ok(());
                }
            }
        }
        return Ok(());
    }
    if metadata.is_dir() {
        for entry in fs::read_dir(path).map_err(map_io)? {
            let entry = entry.map_err(map_io)?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if matches!(name.as_ref(), ".git" | "node_modules" | "target" | ".nuxt" | ".next" | "dist" | "build" | "vendor") {
                continue;
            }
            walk_references(root, &entry.path(), identifier, max_results, response)?;
            if response.truncated {
                break;
            }
        }
    }
    Ok(())
}

fn identifier_columns(line: &str, identifier: &str) -> Vec<usize> {
    let mut columns = Vec::new();
    let mut offset = 0;
    while let Some(found) = line[offset..].find(identifier) {
        let start = offset + found;
        let end = start + identifier.len();
        let left_ok = line[..start]
            .chars()
            .next_back()
            .is_none_or(|ch| !is_identifier_char(ch));
        let right_ok = line[end..]
            .chars()
            .next()
            .is_none_or(|ch| !is_identifier_char(ch));
        if left_ok && right_ok {
            columns.push(start);
        }
        offset = end;
        if offset >= line.len() {
            break;
        }
    }
    columns
}

fn validate_identifier(identifier: &str) -> Result<(), ApiError> {
    if identifier.is_empty()
        || identifier.len() > 256
        || !identifier.chars().all(is_identifier_char)
        || identifier.chars().next().is_some_and(|ch| ch.is_ascii_digit())
    {
        return Err(ApiError::BadRequest("identifier must be a simple programming-language identifier".into()));
    }
    Ok(())
}

fn is_identifier_char(ch: char) -> bool {
    ch == '_' || ch.is_ascii_alphanumeric()
}

fn detect_symbol(language: &str, line: &str) -> Option<(String, String)> {
    let trimmed = line.trim_start();
    if trimmed.is_empty() || trimmed.starts_with("//") || trimmed.starts_with('#') {
        return None;
    }
    let patterns: &[(&str, &str)] = match language {
        "rust" => &[("pub fn ", "function"), ("fn ", "function"), ("pub struct ", "struct"), ("struct ", "struct"), ("pub enum ", "enum"), ("enum ", "enum"), ("trait ", "trait"), ("impl ", "impl"), ("mod ", "module")],
        "go" => &[("func ", "function"), ("type ", "type")],
        "python" => &[("async def ", "function"), ("def ", "function"), ("class ", "class")],
        "javascript" | "typescript" => &[("export function ", "function"), ("export async function ", "function"), ("function ", "function"), ("async function ", "function"), ("export class ", "class"), ("class ", "class"), ("export interface ", "interface"), ("interface ", "interface"), ("export type ", "type"), ("type ", "type")],
        "php" => &[("function ", "function"), ("class ", "class"), ("interface ", "interface"), ("trait ", "trait")],
        "java" | "kotlin" => &[("class ", "class"), ("interface ", "interface"), ("enum ", "enum"), ("fun ", "function")],
        _ => &[],
    };
    for (prefix, kind) in patterns {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            let name = rest
                .split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_'))
                .next()
                .unwrap_or("")
                .to_owned();
            if !name.is_empty() {
                return Some(((*kind).into(), name));
            }
        }
    }
    None
}

fn language_for(path: &Path) -> &'static str {
    match path.extension().and_then(|value| value.to_str()).unwrap_or("") {
        "rs" => "rust",
        "go" => "go",
        "py" => "python",
        "js" | "jsx" | "mjs" | "cjs" => "javascript",
        "ts" | "tsx" | "mts" | "cts" | "vue" => "typescript",
        "php" => "php",
        "java" => "java",
        "kt" | "kts" => "kotlin",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hpp" => "cpp",
        "sh" | "bash" => "shell",
        "toml" => "toml",
        "json" => "json",
        "yaml" | "yml" => "yaml",
        _ => "text",
    }
}

fn is_probably_binary(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|value| value.to_str()).unwrap_or("").to_ascii_lowercase().as_str(),
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "ico" | "pdf" | "zip" | "gz" | "xz" | "bz2" | "7z" | "tar" | "wasm" | "exe" | "dll" | "so" | "a" | "o" | "class" | "jar" | "lockb"
    )
}

fn task(id: &str, label: &str, argv: &[&str], source: &str) -> CodeTask {
    CodeTask {
        id: id.into(),
        label: label.into(),
        argv: argv.iter().map(|value| (*value).to_owned()).collect(),
        source: source.into(),
        execution_risk: "build, test, lint, and package-manager commands may execute project-controlled code".into(),
    }
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

fn canonical_root(workspace: &WorkspaceConfig) -> Result<PathBuf, ApiError> {
    let root = fs::canonicalize(&workspace.root)
        .map_err(|error| ApiError::Internal(format!("failed to resolve workspace root: {error}")))?;
    if !root.is_dir() {
        return Err(ApiError::BadRequest("workspace root is not a directory".into()));
    }
    Ok(root)
}

fn resolve_existing_file(workspace: &WorkspaceConfig, requested: &str) -> Result<(PathBuf, PathBuf), ApiError> {
    let root = canonical_root(workspace)?;
    let target = resolve_existing_from_root(&root, requested)?;
    if !fs::metadata(&target).map_err(map_io)?.is_file() {
        return Err(ApiError::BadRequest("path is not a regular file".into()));
    }
    Ok((root, target))
}

fn resolve_existing_from_root(root: &Path, requested: &str) -> Result<PathBuf, ApiError> {
    let relative = validate_relative_path(requested, false)?;
    let raw = root.join(relative);
    reject_symlink(&raw)?;
    let target = fs::canonicalize(&raw).map_err(map_io)?;
    ensure_within(root, &target)?;
    Ok(target)
}

fn resolve_existing_path(root: &Path, requested: &str) -> Result<PathBuf, ApiError> {
    let relative = validate_relative_path(requested, true)?;
    let raw = root.join(relative);
    let target = fs::canonicalize(&raw).map_err(map_io)?;
    ensure_within(root, &target)?;
    Ok(target)
}

fn validate_relative_path(requested: &str, allow_empty: bool) -> Result<&Path, ApiError> {
    if requested.is_empty() {
        return if allow_empty {
            Ok(Path::new("."))
        } else {
            Err(ApiError::BadRequest("path must not be empty".into()))
        };
    }
    let path = Path::new(requested);
    if path.is_absolute() {
        return Err(ApiError::BadRequest("path must be relative to the workspace".into()));
    }
    for component in path.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(ApiError::BadRequest("path traversal outside the workspace is not allowed".into()));
            }
        }
    }
    Ok(path)
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
        Ok(metadata) if metadata.file_type().is_symlink() => Err(ApiError::Forbidden("code tools do not mutate through symlinks".into())),
        Ok(_) => Ok(()),
        Err(error) => Err(map_io(error)),
    }
}

fn read_bounded(path: &Path, max: usize) -> Result<Vec<u8>, ApiError> {
    let metadata = fs::metadata(path).map_err(map_io)?;
    if metadata.len() > max as u64 {
        return Err(ApiError::BadRequest(format!("file exceeds limit of {max} bytes")));
    }
    fs::read(path).map_err(map_io)
}

fn atomic_write(target: &Path, content: &[u8], permissions: fs::Permissions) -> Result<(), ApiError> {
    let parent = target
        .parent()
        .ok_or_else(|| ApiError::BadRequest("target must have a parent directory".into()))?;
    let mut temp = NamedTempFile::new_in(parent).map_err(map_io)?;
    temp.write_all(content).map_err(map_io)?;
    temp.as_file().sync_all().map_err(map_io)?;
    temp.as_file().set_permissions(permissions).map_err(map_io)?;
    temp.persist(target).map_err(|error| map_io(error.error))?;
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
    fn identifier_boundaries_do_not_match_substrings() {
        assert_eq!(identifier_columns("foo foobar foo", "foo"), vec![0, 11]);
    }

    #[test]
    fn line_replace_is_one_based() {
        let result = replace_lines("a\nb\nc\n", 2, 2, "B\n").unwrap();
        assert_eq!(result, "a\nB\nc\n");
    }

    #[test]
    fn exact_replace_refuses_ambiguity() {
        let edits = vec![CodeEdit::ReplaceExact {
            old: "x".into(),
            new: "y".into(),
        }];
        assert!(apply_edits("x x".into(), &edits).is_err());
    }
}

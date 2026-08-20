use std::{collections::BTreeMap, path::{Path, PathBuf}, process::Stdio, time::Duration};

use axum::{extract::State, Json};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::{io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader}, process::{Child, ChildStdin, ChildStdout, Command}, time};
use url::Url;

use crate::{config::{LspServerConfig, WorkspaceConfig}, error::ApiError, state::AppState};

const MAX_LSP_MESSAGE_BYTES: usize = 16 * 1024 * 1024;
const DEFAULT_TIMEOUT_SECONDS: u64 = 30;

#[derive(Debug, Deserialize)]
pub struct PositionRequest {
    workspace: String,
    path: String,
    line: u32,
    column: u32,
}

#[derive(Debug, Deserialize)]
pub struct RenameRequest {
    workspace: String,
    path: String,
    line: u32,
    column: u32,
    new_name: String,
    #[serde(default = "default_true")]
    dry_run: bool,
}

#[derive(Debug, Deserialize)]
pub struct WorkspaceSymbolRequest {
    workspace: String,
    query: String,
}

#[derive(Debug, Deserialize)]
pub struct FormatRequest {
    workspace: String,
    path: String,
    #[serde(default = "default_true")]
    dry_run: bool,
}

#[derive(Debug, Serialize)]
pub struct LspResponse {
    server: String,
    method: String,
    result: Value,
}

#[derive(Debug, Serialize)]
pub struct LspDiagnosticsResponse {
    server: String,
    path: String,
    diagnostics: Vec<Value>,
}

#[derive(Debug, Serialize)]
pub struct LspEditPreview {
    path: String,
    expected_sha256: Option<String>,
    edits: Vec<Value>,
}

#[derive(Debug, Serialize)]
pub struct LspWorkspaceEditResponse {
    server: String,
    dry_run: bool,
    files: Vec<LspEditPreview>,
    raw_workspace_edit: Value,
}

pub async fn definition(State(state): State<AppState>, Json(req): Json<PositionRequest>) -> Result<Json<LspResponse>, ApiError> {
    position_query(&state, req, "textDocument/definition").await.map(Json)
}

pub async fn references(State(state): State<AppState>, Json(req): Json<PositionRequest>) -> Result<Json<LspResponse>, ApiError> {
    position_query_extra(&state, req, "textDocument/references", json!({"context":{"includeDeclaration":true}})).await.map(Json)
}

pub async fn hover(State(state): State<AppState>, Json(req): Json<PositionRequest>) -> Result<Json<LspResponse>, ApiError> {
    position_query(&state, req, "textDocument/hover").await.map(Json)
}

pub async fn document_symbols(State(state): State<AppState>, Json(req): Json<PositionRequest>) -> Result<Json<LspResponse>, ApiError> {
    let ctx = prepare_document(&state, &req.workspace, &req.path).await?;
    let mut client = LspProcess::start(&ctx).await?;
    let result = client.request("textDocument/documentSymbol", json!({"textDocument":{"uri":ctx.uri}})).await?;
    client.shutdown().await;
    Ok(Json(LspResponse { server: ctx.server_name, method: "textDocument/documentSymbol".into(), result }))
}

pub async fn workspace_symbols(State(state): State<AppState>, Json(req): Json<WorkspaceSymbolRequest>) -> Result<Json<LspResponse>, ApiError> {
    let workspace = readable_workspace(&state, &req.workspace)?;
    let root = std::fs::canonicalize(&workspace.root).map_err(map_io)?;
    let server = choose_workspace_server(&state, &root)?;
    let ctx = LspContext::workspace_only(req.workspace, root, server)?;
    let mut client = LspProcess::start(&ctx).await?;
    let result = client.request("workspace/symbol", json!({"query":req.query})).await?;
    client.shutdown().await;
    Ok(Json(LspResponse { server: ctx.server_name, method: "workspace/symbol".into(), result }))
}

pub async fn diagnostics(State(state): State<AppState>, Json(req): Json<PositionRequest>) -> Result<Json<LspDiagnosticsResponse>, ApiError> {
    let ctx = prepare_document(&state, &req.workspace, &req.path).await?;
    let mut client = LspProcess::start(&ctx).await?;
    client.did_open(&ctx).await?;
    let deadline = time::Instant::now() + Duration::from_secs(3);
    let mut diagnostics = Vec::new();
    while time::Instant::now() < deadline {
        match time::timeout(Duration::from_millis(250), client.read_message()).await {
            Ok(Ok(message)) => {
                if message.get("method").and_then(Value::as_str) == Some("textDocument/publishDiagnostics") {
                    if message.pointer("/params/uri").and_then(Value::as_str) == Some(ctx.uri.as_str()) {
                        if let Some(items) = message.pointer("/params/diagnostics").and_then(Value::as_array) {
                            diagnostics = items.clone();
                        }
                    }
                }
                client.answer_server_request_if_needed(&message).await?;
            }
            Ok(Err(error)) => return Err(error),
            Err(_) => {}
        }
    }
    client.shutdown().await;
    Ok(Json(LspDiagnosticsResponse { server: ctx.server_name, path: req.path, diagnostics }))
}

pub async fn rename(State(state): State<AppState>, Json(req): Json<RenameRequest>) -> Result<Json<LspWorkspaceEditResponse>, ApiError> {
    if req.new_name.trim().is_empty() || req.new_name.contains(['\n', '\r', '\0']) {
        return Err(ApiError::BadRequest("new_name must be a non-empty single-line identifier".into()));
    }
    let ctx = prepare_document(&state, &req.workspace, &req.path).await?;
    writable_workspace(&state, &req.workspace)?;
    let position = agent_position_to_lsp(&ctx.text, req.line, req.column)?;
    let mut client = LspProcess::start(&ctx).await?;
    client.did_open(&ctx).await?;
    let edit = client.request("textDocument/rename", json!({
        "textDocument":{"uri":ctx.uri},
        "position":position,
        "newName":req.new_name
    })).await?;
    client.shutdown().await;
    let files = workspace_edit_preview(&ctx.root, &edit)?;
    Ok(Json(LspWorkspaceEditResponse { server: ctx.server_name, dry_run: req.dry_run, files, raw_workspace_edit: edit }))
}

pub async fn formatting(State(state): State<AppState>, Json(req): Json<FormatRequest>) -> Result<Json<LspWorkspaceEditResponse>, ApiError> {
    let ctx = prepare_document(&state, &req.workspace, &req.path).await?;
    writable_workspace(&state, &req.workspace)?;
    let mut client = LspProcess::start(&ctx).await?;
    client.did_open(&ctx).await?;
    let edits = client.request("textDocument/formatting", json!({
        "textDocument":{"uri":ctx.uri},
        "options":{"tabSize":4,"insertSpaces":true,"trimTrailingWhitespace":true,"insertFinalNewline":true,"trimFinalNewlines":true}
    })).await?;
    client.shutdown().await;
    let synthetic = json!({"changes":{ctx.uri.clone(): edits}});
    let files = workspace_edit_preview(&ctx.root, &synthetic)?;
    Ok(Json(LspWorkspaceEditResponse { server: ctx.server_name, dry_run: req.dry_run, files, raw_workspace_edit: synthetic }))
}

async fn position_query(state: &AppState, req: PositionRequest, method: &str) -> Result<LspResponse, ApiError> {
    position_query_extra(state, req, method, json!({})).await
}

async fn position_query_extra(state: &AppState, req: PositionRequest, method: &str, extra: Value) -> Result<LspResponse, ApiError> {
    let ctx = prepare_document(state, &req.workspace, &req.path).await?;
    let position = agent_position_to_lsp(&ctx.text, req.line, req.column)?;
    let mut params = json!({"textDocument":{"uri":ctx.uri},"position":position});
    if let (Some(dst), Some(src)) = (params.as_object_mut(), extra.as_object()) {
        for (key, value) in src { dst.insert(key.clone(), value.clone()); }
    }
    let mut client = LspProcess::start(&ctx).await?;
    client.did_open(&ctx).await?;
    let result = client.request(method, params).await?;
    client.shutdown().await;
    Ok(LspResponse { server: ctx.server_name, method: method.into(), result })
}

struct LspContext {
    root: PathBuf,
    server_name: String,
    server: LspServerConfig,
    path: Option<PathBuf>,
    uri: String,
    language_id: String,
    text: String,
}

impl LspContext {
    fn workspace_only(workspace_name: String, root: PathBuf, server: (String, LspServerConfig)) -> Result<Self, ApiError> {
        let uri = Url::from_directory_path(&root).map_err(|_| ApiError::BadRequest("workspace path cannot be represented as a file URI".into()))?.to_string();
        Ok(Self { root, server_name: server.0, server: server.1, path: None, uri, language_id: workspace_name, text: String::new() })
    }
}

async fn prepare_document(state: &AppState, workspace_name: &str, requested: &str) -> Result<LspContext, ApiError> {
    let workspace = readable_workspace(state, workspace_name)?;
    let root = std::fs::canonicalize(&workspace.root).map_err(map_io)?;
    let path = safe_existing_file(&root, requested)?;
    let text = std::fs::read_to_string(&path).map_err(map_io)?;
    if text.len() > 8 * 1024 * 1024 { return Err(ApiError::BadRequest("LSP document exceeds 8 MiB".into())); }
    let (server_name, server) = choose_server(state, &path)?;
    let uri = Url::from_file_path(&path).map_err(|_| ApiError::BadRequest("file path cannot be represented as a file URI".into()))?.to_string();
    let language_id = server.language_id.clone().unwrap_or_else(|| language_id_for(&path).to_owned());
    Ok(LspContext { root, server_name, server, path: Some(path), uri, language_id, text })
}

fn choose_server(state: &AppState, path: &Path) -> Result<(String, LspServerConfig), ApiError> {
    let extension = path.extension().and_then(|v| v.to_str()).unwrap_or("").to_ascii_lowercase();
    for (name, server) in &state.config.lsp.servers {
        if server.extensions.iter().any(|item| item.trim_start_matches('.').eq_ignore_ascii_case(&extension)) {
            return Ok((name.clone(), server.clone()));
        }
    }
    builtin_server(&extension).ok_or_else(|| ApiError::Unsupported(format!("no LSP server is configured for .{extension}")))
}

fn choose_workspace_server(state: &AppState, root: &Path) -> Result<(String, LspServerConfig), ApiError> {
    for (name, server) in &state.config.lsp.servers {
        if server.enabled { return Ok((name.clone(), server.clone())); }
    }
    if root.join("Cargo.toml").exists() { return builtin_server("rs").ok_or_else(|| ApiError::Unsupported("rust-analyzer mapping unavailable".into())); }
    if root.join("go.mod").exists() { return builtin_server("go").ok_or_else(|| ApiError::Unsupported("gopls mapping unavailable".into())); }
    if root.join("tsconfig.json").exists() || root.join("package.json").exists() { return builtin_server("ts").ok_or_else(|| ApiError::Unsupported("TypeScript LSP mapping unavailable".into())); }
    Err(ApiError::Unsupported("cannot infer an LSP server for this workspace; configure [lsp.servers.*]".into()))
}

fn builtin_server(ext: &str) -> Option<(String, LspServerConfig)> {
    let (name, argv, language) = match ext {
        "rs" => ("rust-analyzer", vec!["rust-analyzer"], "rust"),
        "go" => ("gopls", vec!["gopls", "serve"], "go"),
        "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" => ("typescript-language-server", vec!["typescript-language-server", "--stdio"], if ext.starts_with('t') { "typescript" } else { "javascript" }),
        "py" => ("pyright", vec!["pyright-langserver", "--stdio"], "python"),
        "php" => ("intelephense", vec!["intelephense", "--stdio"], "php"),
        "java" => ("jdtls", vec!["jdtls"], "java"),
        "kt" | "kts" => ("kotlin-language-server", vec!["kotlin-language-server"], "kotlin"),
        _ => return None,
    };
    Some((name.into(), LspServerConfig { enabled: true, argv: argv.into_iter().map(str::to_owned).collect(), extensions: vec![ext.into()], language_id: Some(language.into()), initialization_options: None }))
}

fn language_id_for(path: &Path) -> &'static str {
    match path.extension().and_then(|v| v.to_str()).unwrap_or("") {
        "rs" => "rust", "go" => "go", "ts" | "tsx" => "typescript", "js" | "jsx" | "mjs" | "cjs" => "javascript", "py" => "python", "php" => "php", "java" => "java", "kt" | "kts" => "kotlin", _ => "plaintext"
    }
}

struct LspProcess {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl LspProcess {
    async fn start(ctx: &LspContext) -> Result<Self, ApiError> {
        if !ctx.server.enabled || ctx.server.argv.is_empty() { return Err(ApiError::Unsupported(format!("LSP server {:?} is disabled or has no argv", ctx.server_name))); }
        let mut command = Command::new(&ctx.server.argv[0]);
        command.args(&ctx.server.argv[1..]).current_dir(&ctx.root).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).kill_on_drop(true);
        let mut child = command.spawn().map_err(|error| ApiError::NotFound(format!("failed to start LSP server {}: {error}", ctx.server_name)))?;
        let stdin = child.stdin.take().ok_or_else(|| ApiError::Internal("failed to open LSP stdin".into()))?;
        let stdout = BufReader::new(child.stdout.take().ok_or_else(|| ApiError::Internal("failed to open LSP stdout".into()))?);
        let mut this = Self { child, stdin, stdout, next_id: 1 };
        let root_uri = Url::from_directory_path(&ctx.root).map_err(|_| ApiError::BadRequest("workspace root cannot be represented as URI".into()))?.to_string();
        let init = json!({
            "processId": std::process::id(),
            "clientInfo":{"name":"TuxBridge","version":env!("CARGO_PKG_VERSION")},
            "rootUri":root_uri,
            "capabilities":{
                "workspace":{"workspaceEdit":{"documentChanges":true,"resourceOperations":["create","rename","delete"]}},
                "textDocument":{
                    "synchronization":{"didSave":true,"dynamicRegistration":false},
                    "definition":{"linkSupport":true},
                    "references":{},
                    "hover":{"contentFormat":["markdown","plaintext"]},
                    "documentSymbol":{"hierarchicalDocumentSymbolSupport":true},
                    "rename":{"prepareSupport":true},
                    "publishDiagnostics":{"relatedInformation":true,"versionSupport":true},
                    "codeAction":{"dataSupport":true,"resolveSupport":{"properties":["edit","command"]}}
                },
                "general":{"positionEncodings":["utf-16"]}
            },
            "initializationOptions":ctx.server.initialization_options.clone().unwrap_or(Value::Null)
        });
        this.request("initialize", init).await?;
        this.notify("initialized", json!({})).await?;
        Ok(this)
    }

    async fn did_open(&mut self, ctx: &LspContext) -> Result<(), ApiError> {
        if ctx.path.is_none() { return Ok(()); }
        self.notify("textDocument/didOpen", json!({"textDocument":{"uri":ctx.uri,"languageId":ctx.language_id,"version":1,"text":ctx.text}})).await
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<Value, ApiError> {
        let id = self.next_id; self.next_id += 1;
        self.send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})).await?;
        let deadline = time::Instant::now() + Duration::from_secs(DEFAULT_TIMEOUT_SECONDS);
        loop {
            let remaining = deadline.saturating_duration_since(time::Instant::now());
            if remaining.is_zero() { return Err(ApiError::BadRequest(format!("LSP request {method} timed out"))); }
            let message = time::timeout(remaining, self.read_message()).await.map_err(|_| ApiError::BadRequest(format!("LSP request {method} timed out")))??;
            if message.get("id").and_then(Value::as_u64) == Some(id) && (message.get("result").is_some() || message.get("error").is_some()) {
                if let Some(error) = message.get("error") { return Err(ApiError::BadRequest(format!("LSP {method} failed: {error}"))); }
                return Ok(message.get("result").cloned().unwrap_or(Value::Null));
            }
            self.answer_server_request_if_needed(&message).await?;
        }
    }

    async fn notify(&mut self, method: &str, params: Value) -> Result<(), ApiError> { self.send(json!({"jsonrpc":"2.0","method":method,"params":params})).await }

    async fn send(&mut self, value: Value) -> Result<(), ApiError> {
        let body = serde_json::to_vec(&value).map_err(|error| ApiError::Internal(error.to_string()))?;
        let header = format!("Content-Length: {}\r\n\r\n", body.len());
        self.stdin.write_all(header.as_bytes()).await.map_err(map_io)?;
        self.stdin.write_all(&body).await.map_err(map_io)?;
        self.stdin.flush().await.map_err(map_io)
    }

    async fn read_message(&mut self) -> Result<Value, ApiError> {
        let mut content_length = None;
        loop {
            let mut line = String::new();
            let count = self.stdout.read_line(&mut line).await.map_err(map_io)?;
            if count == 0 { return Err(ApiError::Internal("LSP server closed stdout".into())); }
            if line == "\r\n" || line == "\n" { break; }
            if let Some(value) = line.strip_prefix("Content-Length:") { content_length = value.trim().parse::<usize>().ok(); }
        }
        let len = content_length.ok_or_else(|| ApiError::Internal("LSP message missing Content-Length".into()))?;
        if len > MAX_LSP_MESSAGE_BYTES { return Err(ApiError::BadRequest("LSP message exceeded 16 MiB limit".into())); }
        let mut body = vec![0u8; len];
        self.stdout.read_exact(&mut body).await.map_err(map_io)?;
        serde_json::from_slice(&body).map_err(|error| ApiError::Internal(format!("invalid LSP JSON: {error}")))
    }

    async fn answer_server_request_if_needed(&mut self, message: &Value) -> Result<(), ApiError> {
        if message.get("method").is_some() && message.get("id").is_some() {
            let id = message.get("id").cloned().unwrap_or(Value::Null);
            let method = message.get("method").and_then(Value::as_str).unwrap_or("");
            let result = match method {
                "workspace/configuration" => json!([]),
                "workspace/workspaceFolders" => Value::Null,
                "window/showMessageRequest" => Value::Null,
                _ => Value::Null,
            };
            self.send(json!({"jsonrpc":"2.0","id":id,"result":result})).await?;
        }
        Ok(())
    }

    async fn shutdown(&mut self) {
        let _ = self.request("shutdown", Value::Null).await;
        let _ = self.notify("exit", Value::Null).await;
        let _ = time::timeout(Duration::from_secs(2), self.child.wait()).await;
        let _ = self.child.kill().await;
    }
}

fn agent_position_to_lsp(text: &str, line: u32, column: u32) -> Result<Value, ApiError> {
    if line == 0 || column == 0 { return Err(ApiError::BadRequest("line and column are 1-based and must be >= 1".into())); }
    let source_line = text.lines().nth((line - 1) as usize).ok_or_else(|| ApiError::BadRequest("line is outside the document".into()))?;
    let scalar_index = (column - 1) as usize;
    let mut utf16 = 0usize;
    let mut seen = 0usize;
    for ch in source_line.chars() {
        if seen == scalar_index { break; }
        utf16 += ch.len_utf16(); seen += 1;
    }
    if seen < scalar_index { return Err(ApiError::BadRequest("column is outside the line".into())); }
    Ok(json!({"line":line - 1,"character":utf16}))
}

fn workspace_edit_preview(root: &Path, edit: &Value) -> Result<Vec<LspEditPreview>, ApiError> {
    let mut grouped: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    if let Some(changes) = edit.get("changes").and_then(Value::as_object) {
        for (uri, edits) in changes {
            if let Some(items) = edits.as_array() { grouped.entry(uri.clone()).or_default().extend(items.clone()); }
        }
    }
    if let Some(document_changes) = edit.get("documentChanges").and_then(Value::as_array) {
        for change in document_changes {
            if let (Some(uri), Some(edits)) = (change.pointer("/textDocument/uri").and_then(Value::as_str), change.get("edits").and_then(Value::as_array)) {
                grouped.entry(uri.to_owned()).or_default().extend(edits.clone());
            }
        }
    }
    let mut previews = Vec::new();
    for (uri, edits) in grouped {
        let url = Url::parse(&uri).map_err(|_| ApiError::BadRequest("LSP returned an invalid URI".into()))?;
        let path = url.to_file_path().map_err(|_| ApiError::Forbidden("LSP edit targets a non-file URI".into()))?;
        let canonical = std::fs::canonicalize(&path).map_err(map_io)?;
        if !canonical.starts_with(root) { return Err(ApiError::Forbidden("LSP edit escapes workspace root".into())); }
        let bytes = std::fs::read(&canonical).map_err(map_io)?;
        let hash = format!("{:x}", sha2::Sha256::digest(&bytes));
        previews.push(LspEditPreview { path: canonical.strip_prefix(root).unwrap_or(&canonical).to_string_lossy().into_owned(), expected_sha256: Some(hash), edits });
    }
    Ok(previews)
}

fn safe_existing_file(root: &Path, requested: &str) -> Result<PathBuf, ApiError> {
    let relative = Path::new(requested);
    if requested.is_empty() || relative.is_absolute() || relative.components().any(|c| matches!(c, std::path::Component::ParentDir | std::path::Component::RootDir | std::path::Component::Prefix(_))) {
        return Err(ApiError::BadRequest("path must be a safe workspace-relative file path".into()));
    }
    let raw = root.join(relative);
    if std::fs::symlink_metadata(&raw).map_err(map_io)?.file_type().is_symlink() { return Err(ApiError::Forbidden("LSP document cannot be a symlink".into())); }
    let canonical = std::fs::canonicalize(raw).map_err(map_io)?;
    if !canonical.starts_with(root) { return Err(ApiError::Forbidden("resolved path escapes workspace root".into())); }
    if !canonical.is_file() { return Err(ApiError::BadRequest("path is not a regular file".into())); }
    Ok(canonical)
}

fn readable_workspace<'a>(state: &'a AppState, name: &str) -> Result<&'a WorkspaceConfig, ApiError> {
    let workspace = state.config.workspaces.get(name).ok_or_else(|| ApiError::NotFound(format!("workspace {name:?} is not configured")))?;
    if !workspace.capabilities.fs_read { return Err(ApiError::Forbidden(format!("workspace {name:?} does not allow filesystem reads"))); }
    Ok(workspace)
}

fn writable_workspace<'a>(state: &'a AppState, name: &str) -> Result<&'a WorkspaceConfig, ApiError> {
    let workspace = readable_workspace(state, name)?;
    if !workspace.capabilities.fs_write { return Err(ApiError::Forbidden(format!("workspace {name:?} does not allow filesystem writes"))); }
    Ok(workspace)
}

fn default_true() -> bool { true }
fn map_io(error: std::io::Error) -> ApiError {
    match error.kind() {
        std::io::ErrorKind::NotFound => ApiError::NotFound(error.to_string()),
        std::io::ErrorKind::PermissionDenied => ApiError::Forbidden(error.to_string()),
        _ => ApiError::Internal(error.to_string()),
    }
}

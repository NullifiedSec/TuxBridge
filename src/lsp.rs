use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Component, Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use axum::{extract::State, Json};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    time,
};
use url::Url;

use crate::{
    config::{LspServerConfig, WorkspaceConfig},
    error::ApiError,
    state::AppState,
};

const MAX_LSP_MESSAGE_BYTES: usize = 16 * 1024 * 1024;
const MAX_LSP_DOCUMENT_BYTES: usize = 8 * 1024 * 1024;
const REQUEST_TIMEOUT_SECONDS: u64 = 30;
const DIAGNOSTIC_WINDOW_SECONDS: u64 = 3;

#[derive(Debug, Deserialize)]
pub struct PositionRequest {
    workspace: String,
    path: String,
    line: u32,
    column: u32,
}

#[derive(Debug, Deserialize)]
pub struct DocumentRequest {
    workspace: String,
    path: String,
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
pub struct LspEditFile {
    path: String,
    old_sha256: String,
    new_sha256: String,
    edits: usize,
    changed: bool,
}

#[derive(Debug, Serialize)]
pub struct LspWorkspaceEditResponse {
    server: String,
    dry_run: bool,
    applied: bool,
    files: Vec<LspEditFile>,
}

pub async fn definition(
    State(state): State<AppState>,
    Json(request): Json<PositionRequest>,
) -> Result<Json<LspResponse>, ApiError> {
    position_query(&state, request, "textDocument/definition", Value::Null)
        .await
        .map(Json)
}

pub async fn references(
    State(state): State<AppState>,
    Json(request): Json<PositionRequest>,
) -> Result<Json<LspResponse>, ApiError> {
    position_query(
        &state,
        request,
        "textDocument/references",
        json!({"context":{"includeDeclaration":true}}),
    )
    .await
    .map(Json)
}

pub async fn hover(
    State(state): State<AppState>,
    Json(request): Json<PositionRequest>,
) -> Result<Json<LspResponse>, ApiError> {
    position_query(&state, request, "textDocument/hover", Value::Null)
        .await
        .map(Json)
}

pub async fn document_symbols(
    State(state): State<AppState>,
    Json(request): Json<DocumentRequest>,
) -> Result<Json<LspResponse>, ApiError> {
    let context = prepare_document(&state, &request.workspace, &request.path)?;
    let mut client = LspProcess::start(&context).await?;
    client.did_open(&context).await?;
    let result = client
        .request(
            "textDocument/documentSymbol",
            json!({"textDocument":{"uri":context.uri}}),
        )
        .await?;
    client.shutdown().await;
    Ok(Json(LspResponse {
        server: context.server_name,
        method: "textDocument/documentSymbol".into(),
        result,
    }))
}

pub async fn workspace_symbols(
    State(state): State<AppState>,
    Json(request): Json<WorkspaceSymbolRequest>,
) -> Result<Json<LspResponse>, ApiError> {
    let workspace = readable_workspace(&state, &request.workspace)?;
    let root = fs::canonicalize(&workspace.root).map_err(map_io)?;
    let server = choose_workspace_server(&state, &root)?;
    let context = LspContext::workspace_only(root, server)?;
    let mut client = LspProcess::start(&context).await?;
    let result = client
        .request("workspace/symbol", json!({"query":request.query}))
        .await?;
    client.shutdown().await;
    Ok(Json(LspResponse {
        server: context.server_name,
        method: "workspace/symbol".into(),
        result,
    }))
}

pub async fn diagnostics(
    State(state): State<AppState>,
    Json(request): Json<DocumentRequest>,
) -> Result<Json<LspDiagnosticsResponse>, ApiError> {
    let context = prepare_document(&state, &request.workspace, &request.path)?;
    let mut client = LspProcess::start(&context).await?;
    client.did_open(&context).await?;

    let deadline = time::Instant::now() + Duration::from_secs(DIAGNOSTIC_WINDOW_SECONDS);
    let mut diagnostics = Vec::new();
    while time::Instant::now() < deadline {
        match time::timeout(Duration::from_millis(250), client.read_message()).await {
            Ok(Ok(message)) => {
                if message.get("method").and_then(Value::as_str)
                    == Some("textDocument/publishDiagnostics")
                    && message.pointer("/params/uri").and_then(Value::as_str)
                        == Some(context.uri.as_str())
                {
                    if let Some(items) = message
                        .pointer("/params/diagnostics")
                        .and_then(Value::as_array)
                    {
                        diagnostics = items.clone();
                    }
                }
                client.answer_server_request_if_needed(&message).await?;
            }
            Ok(Err(error)) => return Err(error),
            Err(_) => {}
        }
    }

    client.shutdown().await;
    Ok(Json(LspDiagnosticsResponse {
        server: context.server_name,
        path: request.path,
        diagnostics,
    }))
}

pub async fn rename(
    State(state): State<AppState>,
    Json(request): Json<RenameRequest>,
) -> Result<Json<LspWorkspaceEditResponse>, ApiError> {
    if request.new_name.trim().is_empty()
        || request.new_name.contains(['\n', '\r', '\0'])
    {
        return Err(ApiError::BadRequest(
            "new_name must be a non-empty single-line identifier".into(),
        ));
    }
    writable_workspace(&state, &request.workspace)?;
    let context = prepare_document(&state, &request.workspace, &request.path)?;
    let position = agent_position_to_lsp(&context.text, request.line, request.column)?;
    let mut client = LspProcess::start(&context).await?;
    client.did_open(&context).await?;
    let edit = client
        .request(
            "textDocument/rename",
            json!({
                "textDocument":{"uri":context.uri},
                "position":position,
                "newName":request.new_name
            }),
        )
        .await?;
    client.shutdown().await;
    apply_workspace_edit(
        &context.server_name,
        &context.root,
        &edit,
        request.dry_run,
    )
    .map(Json)
}

pub async fn formatting(
    State(state): State<AppState>,
    Json(request): Json<FormatRequest>,
) -> Result<Json<LspWorkspaceEditResponse>, ApiError> {
    writable_workspace(&state, &request.workspace)?;
    let context = prepare_document(&state, &request.workspace, &request.path)?;
    let mut client = LspProcess::start(&context).await?;
    client.did_open(&context).await?;
    let edits = client
        .request(
            "textDocument/formatting",
            json!({
                "textDocument":{"uri":context.uri},
                "options":{
                    "tabSize":4,
                    "insertSpaces":true,
                    "trimTrailingWhitespace":true,
                    "insertFinalNewline":true,
                    "trimFinalNewlines":true
                }
            }),
        )
        .await?;
    client.shutdown().await;
    let workspace_edit = json!({"changes":{context.uri.clone(): edits}});
    apply_workspace_edit(
        &context.server_name,
        &context.root,
        &workspace_edit,
        request.dry_run,
    )
    .map(Json)
}

async fn position_query(
    state: &AppState,
    request: PositionRequest,
    method: &str,
    extra: Value,
) -> Result<LspResponse, ApiError> {
    let context = prepare_document(state, &request.workspace, &request.path)?;
    let position = agent_position_to_lsp(&context.text, request.line, request.column)?;
    let mut params = json!({
        "textDocument":{"uri":context.uri},
        "position":position
    });
    if let (Some(target), Some(source)) = (params.as_object_mut(), extra.as_object()) {
        for (key, value) in source {
            target.insert(key.clone(), value.clone());
        }
    }
    let mut client = LspProcess::start(&context).await?;
    client.did_open(&context).await?;
    let result = client.request(method, params).await?;
    client.shutdown().await;
    Ok(LspResponse {
        server: context.server_name,
        method: method.into(),
        result,
    })
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
    fn workspace_only(
        root: PathBuf,
        server: (String, LspServerConfig),
    ) -> Result<Self, ApiError> {
        let uri = Url::from_directory_path(&root)
            .map_err(|_| ApiError::BadRequest("workspace path cannot become a file URI".into()))?
            .to_string();
        Ok(Self {
            root,
            server_name: server.0,
            server: server.1,
            path: None,
            uri,
            language_id: String::new(),
            text: String::new(),
        })
    }
}

fn prepare_document(
    state: &AppState,
    workspace_name: &str,
    requested: &str,
) -> Result<LspContext, ApiError> {
    let workspace = readable_workspace(state, workspace_name)?;
    let root = fs::canonicalize(&workspace.root).map_err(map_io)?;
    let path = safe_existing_file(&root, requested)?;
    let text = fs::read_to_string(&path).map_err(map_io)?;
    if text.len() > MAX_LSP_DOCUMENT_BYTES {
        return Err(ApiError::BadRequest(
            "LSP document exceeds the 8 MiB limit".into(),
        ));
    }
    let (server_name, server) = choose_server(state, &path)?;
    let uri = Url::from_file_path(&path)
        .map_err(|_| ApiError::BadRequest("file path cannot become a file URI".into()))?
        .to_string();
    let language_id = server
        .language_id
        .clone()
        .unwrap_or_else(|| language_id_for(&path).to_owned());
    Ok(LspContext {
        root,
        server_name,
        server,
        path: Some(path),
        uri,
        language_id,
        text,
    })
}

fn choose_server(
    state: &AppState,
    path: &Path,
) -> Result<(String, LspServerConfig), ApiError> {
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    for (name, server) in &state.config.lsp.servers {
        if server.enabled
            && server.extensions.iter().any(|item| {
                item.trim_start_matches('.')
                    .eq_ignore_ascii_case(&extension)
            })
        {
            return Ok((name.clone(), server.clone()));
        }
    }
    builtin_server(&extension).ok_or_else(|| {
        ApiError::Unsupported(format!(
            "no LSP server is configured or known for .{extension}"
        ))
    })
}

fn choose_workspace_server(
    state: &AppState,
    root: &Path,
) -> Result<(String, LspServerConfig), ApiError> {
    for (name, server) in &state.config.lsp.servers {
        if server.enabled {
            return Ok((name.clone(), server.clone()));
        }
    }
    if root.join("Cargo.toml").exists() {
        return builtin_server("rs")
            .ok_or_else(|| ApiError::Unsupported("rust-analyzer mapping unavailable".into()));
    }
    if root.join("go.mod").exists() {
        return builtin_server("go")
            .ok_or_else(|| ApiError::Unsupported("gopls mapping unavailable".into()));
    }
    if root.join("tsconfig.json").exists() || root.join("package.json").exists() {
        return builtin_server("ts").ok_or_else(|| {
            ApiError::Unsupported("TypeScript language server mapping unavailable".into())
        });
    }
    Err(ApiError::Unsupported(
        "cannot infer an LSP server for this workspace; configure [lsp.servers.*]".into(),
    ))
}

fn builtin_server(extension: &str) -> Option<(String, LspServerConfig)> {
    let (name, argv, language_id) = match extension {
        "rs" => ("rust-analyzer", vec!["rust-analyzer"], "rust"),
        "go" => ("gopls", vec!["gopls", "serve"], "go"),
        "ts" | "tsx" => (
            "typescript-language-server",
            vec!["typescript-language-server", "--stdio"],
            "typescript",
        ),
        "js" | "jsx" | "mjs" | "cjs" => (
            "typescript-language-server",
            vec!["typescript-language-server", "--stdio"],
            "javascript",
        ),
        "py" => ("pyright", vec!["pyright-langserver", "--stdio"], "python"),
        "php" => ("intelephense", vec!["intelephense", "--stdio"], "php"),
        "java" => ("jdtls", vec!["jdtls"], "java"),
        "kt" | "kts" => (
            "kotlin-language-server",
            vec!["kotlin-language-server"],
            "kotlin",
        ),
        _ => return None,
    };
    Some((
        name.into(),
        LspServerConfig {
            enabled: true,
            argv: argv.into_iter().map(str::to_owned).collect(),
            extensions: vec![extension.into()],
            language_id: Some(language_id.into()),
            initialization_options: None,
        },
    ))
}

fn language_id_for(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("")
    {
        "rs" => "rust",
        "go" => "go",
        "ts" | "tsx" => "typescript",
        "js" | "jsx" | "mjs" | "cjs" => "javascript",
        "py" => "python",
        "php" => "php",
        "java" => "java",
        "kt" | "kts" => "kotlin",
        _ => "plaintext",
    }
}

struct LspProcess {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl LspProcess {
    async fn start(context: &LspContext) -> Result<Self, ApiError> {
        if !context.server.enabled || context.server.argv.is_empty() {
            return Err(ApiError::Unsupported(format!(
                "LSP server {:?} is disabled or has no argv",
                context.server_name
            )));
        }
        let mut command = Command::new(&context.server.argv[0]);
        command
            .args(&context.server.argv[1..])
            .current_dir(&context.root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(|error| {
            ApiError::NotFound(format!(
                "failed to start LSP server {}: {error}",
                context.server_name
            ))
        })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| ApiError::Internal("failed to open LSP stdin".into()))?;
        let stdout = BufReader::new(
            child
                .stdout
                .take()
                .ok_or_else(|| ApiError::Internal("failed to open LSP stdout".into()))?,
        );
        let mut client = Self {
            child,
            stdin,
            stdout,
            next_id: 1,
        };
        let root_uri = Url::from_directory_path(&context.root)
            .map_err(|_| ApiError::BadRequest("workspace root cannot become a URI".into()))?
            .to_string();
        client
            .request(
                "initialize",
                json!({
                    "processId":std::process::id(),
                    "clientInfo":{"name":"TuxBridge","version":env!("CARGO_PKG_VERSION")},
                    "rootUri":root_uri,
                    "capabilities":{
                        "workspace":{
                            "workspaceEdit":{
                                "documentChanges":true,
                                "resourceOperations":["create","rename","delete"]
                            }
                        },
                        "textDocument":{
                            "synchronization":{"didSave":true,"dynamicRegistration":false},
                            "definition":{"linkSupport":true},
                            "references":{},
                            "hover":{"contentFormat":["markdown","plaintext"]},
                            "documentSymbol":{"hierarchicalDocumentSymbolSupport":true},
                            "rename":{"prepareSupport":true},
                            "publishDiagnostics":{"relatedInformation":true,"versionSupport":true}
                        },
                        "general":{"positionEncodings":["utf-16"]}
                    },
                    "initializationOptions":context
                        .server
                        .initialization_options
                        .clone()
                        .unwrap_or(Value::Null)
                }),
            )
            .await?;
        client.notify("initialized", json!({})).await?;
        Ok(client)
    }

    async fn did_open(&mut self, context: &LspContext) -> Result<(), ApiError> {
        if context.path.is_none() {
            return Ok(());
        }
        self.notify(
            "textDocument/didOpen",
            json!({
                "textDocument":{
                    "uri":context.uri,
                    "languageId":context.language_id,
                    "version":1,
                    "text":context.text
                }
            }),
        )
        .await
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<Value, ApiError> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({
            "jsonrpc":"2.0",
            "id":id,
            "method":method,
            "params":params
        }))
        .await?;

        let deadline = time::Instant::now() + Duration::from_secs(REQUEST_TIMEOUT_SECONDS);
        loop {
            let remaining = deadline.saturating_duration_since(time::Instant::now());
            if remaining.is_zero() {
                return Err(ApiError::BadRequest(format!(
                    "LSP request {method} timed out"
                )));
            }
            let message = time::timeout(remaining, self.read_message())
                .await
                .map_err(|_| ApiError::BadRequest(format!("LSP request {method} timed out")))??;
            if message.get("id").and_then(Value::as_u64) == Some(id)
                && (message.get("result").is_some() || message.get("error").is_some())
            {
                if let Some(error) = message.get("error") {
                    return Err(ApiError::BadRequest(format!(
                        "LSP {method} failed: {error}"
                    )));
                }
                return Ok(message.get("result").cloned().unwrap_or(Value::Null));
            }
            self.answer_server_request_if_needed(&message).await?;
        }
    }

    async fn notify(&mut self, method: &str, params: Value) -> Result<(), ApiError> {
        self.send(json!({"jsonrpc":"2.0","method":method,"params":params}))
            .await
    }

    async fn send(&mut self, message: Value) -> Result<(), ApiError> {
        let body = serde_json::to_vec(&message)
            .map_err(|error| ApiError::Internal(error.to_string()))?;
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
            if count == 0 {
                return Err(ApiError::Internal("LSP server closed stdout".into()));
            }
            if line == "\r\n" || line == "\n" {
                break;
            }
            if let Some(value) = line.strip_prefix("Content-Length:") {
                content_length = value.trim().parse::<usize>().ok();
            }
        }
        let length = content_length
            .ok_or_else(|| ApiError::Internal("LSP message missing Content-Length".into()))?;
        if length > MAX_LSP_MESSAGE_BYTES {
            return Err(ApiError::BadRequest(
                "LSP message exceeded the 16 MiB limit".into(),
            ));
        }
        let mut body = vec![0u8; length];
        self.stdout.read_exact(&mut body).await.map_err(map_io)?;
        serde_json::from_slice(&body)
            .map_err(|error| ApiError::Internal(format!("invalid LSP JSON: {error}")))
    }

    async fn answer_server_request_if_needed(
        &mut self,
        message: &Value,
    ) -> Result<(), ApiError> {
        if message.get("method").is_some() && message.get("id").is_some() {
            let id = message.get("id").cloned().unwrap_or(Value::Null);
            let result = match message.get("method").and_then(Value::as_str).unwrap_or("") {
                "workspace/configuration" => json!([]),
                "workspace/workspaceFolders" => Value::Null,
                "window/showMessageRequest" => Value::Null,
                _ => Value::Null,
            };
            self.send(json!({"jsonrpc":"2.0","id":id,"result":result}))
                .await?;
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

#[derive(Debug)]
struct PreparedFileEdit {
    path: PathBuf,
    display_path: String,
    original: String,
    updated: String,
    edit_count: usize,
    old_sha256: String,
    new_sha256: String,
}

fn apply_workspace_edit(
    server: &str,
    root: &Path,
    workspace_edit: &Value,
    dry_run: bool,
) -> Result<LspWorkspaceEditResponse, ApiError> {
    let grouped = collect_workspace_edits(workspace_edit)?;
    let mut prepared = Vec::new();

    for (uri, edits) in grouped {
        let url = Url::parse(&uri)
            .map_err(|_| ApiError::BadRequest("LSP returned an invalid edit URI".into()))?;
        let path = url
            .to_file_path()
            .map_err(|_| ApiError::Forbidden("LSP edit targets a non-file URI".into()))?;
        let canonical = fs::canonicalize(&path).map_err(map_io)?;
        if !canonical.starts_with(root) {
            return Err(ApiError::Forbidden(
                "LSP edit attempted to escape the workspace root".into(),
            ));
        }
        if fs::symlink_metadata(&canonical)
            .map_err(map_io)?
            .file_type()
            .is_symlink()
        {
            return Err(ApiError::Forbidden(
                "LSP edits through symlinks are not allowed".into(),
            ));
        }
        let original = fs::read_to_string(&canonical).map_err(map_io)?;
        if original.len() > MAX_LSP_DOCUMENT_BYTES {
            return Err(ApiError::BadRequest(
                "an LSP-edited file exceeds the 8 MiB limit".into(),
            ));
        }
        let updated = apply_text_edits(&original, &edits)?;
        let old_sha256 = digest(original.as_bytes());
        let new_sha256 = digest(updated.as_bytes());
        prepared.push(PreparedFileEdit {
            display_path: canonical
                .strip_prefix(root)
                .unwrap_or(&canonical)
                .to_string_lossy()
                .into_owned(),
            path: canonical,
            original,
            updated,
            edit_count: edits.len(),
            old_sha256,
            new_sha256,
        });
    }

    if !dry_run {
        for item in &prepared {
            let current = fs::read(&item.path).map_err(map_io)?;
            if digest(&current) != item.old_sha256 {
                return Err(ApiError::Conflict(format!(
                    "{} changed after LSP preflight",
                    item.display_path
                )));
            }
        }
        for item in &prepared {
            if item.original != item.updated {
                atomic_replace(&item.path, item.updated.as_bytes())?;
            }
        }
    }

    Ok(LspWorkspaceEditResponse {
        server: server.into(),
        dry_run,
        applied: !dry_run,
        files: prepared
            .into_iter()
            .map(|item| LspEditFile {
                path: item.display_path,
                old_sha256: item.old_sha256,
                new_sha256: item.new_sha256,
                edits: item.edit_count,
                changed: item.original != item.updated,
            })
            .collect(),
    })
}

fn collect_workspace_edits(workspace_edit: &Value) -> Result<BTreeMap<String, Vec<Value>>, ApiError> {
    let mut grouped = BTreeMap::<String, Vec<Value>>::new();
    if workspace_edit.is_null() {
        return Ok(grouped);
    }
    if let Some(changes) = workspace_edit.get("changes").and_then(Value::as_object) {
        for (uri, edits) in changes {
            let items = edits
                .as_array()
                .ok_or_else(|| ApiError::BadRequest("LSP changes entry is not an edit array".into()))?;
            grouped.entry(uri.clone()).or_default().extend(items.clone());
        }
    }
    if let Some(document_changes) = workspace_edit
        .get("documentChanges")
        .and_then(Value::as_array)
    {
        for change in document_changes {
            if change.get("kind").is_some() {
                return Err(ApiError::Unsupported(
                    "LSP resource create/rename/delete operations are not automatically applied".into(),
                ));
            }
            let uri = change
                .pointer("/textDocument/uri")
                .and_then(Value::as_str)
                .ok_or_else(|| ApiError::BadRequest("LSP TextDocumentEdit is missing a URI".into()))?;
            let edits = change
                .get("edits")
                .and_then(Value::as_array)
                .ok_or_else(|| ApiError::BadRequest("LSP TextDocumentEdit is missing edits".into()))?;
            grouped.entry(uri.to_owned()).or_default().extend(edits.clone());
        }
    }
    Ok(grouped)
}

fn apply_text_edits(text: &str, edits: &[Value]) -> Result<String, ApiError> {
    let mut ranged = Vec::<(usize, usize, String)>::new();
    for edit in edits {
        let range = edit
            .get("range")
            .ok_or_else(|| ApiError::Unsupported("LSP InsertReplaceEdit is not supported yet".into()))?;
        let start = lsp_position_to_byte(text, range.get("start").unwrap_or(&Value::Null))?;
        let end = lsp_position_to_byte(text, range.get("end").unwrap_or(&Value::Null))?;
        if start > end {
            return Err(ApiError::BadRequest("LSP edit has an inverted range".into()));
        }
        let replacement = edit
            .get("newText")
            .and_then(Value::as_str)
            .ok_or_else(|| ApiError::BadRequest("LSP text edit is missing newText".into()))?
            .to_owned();
        ranged.push((start, end, replacement));
    }
    ranged.sort_by(|left, right| right.0.cmp(&left.0).then(right.1.cmp(&left.1)));
    for pair in ranged.windows(2) {
        if pair[1].1 > pair[0].0 {
            return Err(ApiError::Conflict("LSP returned overlapping text edits".into()));
        }
    }
    let mut updated = text.to_owned();
    for (start, end, replacement) in ranged {
        if !updated.is_char_boundary(start) || !updated.is_char_boundary(end) {
            return Err(ApiError::BadRequest(
                "LSP edit resolved to a non-UTF-8 character boundary".into(),
            ));
        }
        updated.replace_range(start..end, &replacement);
    }
    Ok(updated)
}

fn agent_position_to_lsp(text: &str, line: u32, column: u32) -> Result<Value, ApiError> {
    if line == 0 || column == 0 {
        return Err(ApiError::BadRequest(
            "line and column are 1-based and must be >= 1".into(),
        ));
    }
    let source_line = text
        .lines()
        .nth((line - 1) as usize)
        .ok_or_else(|| ApiError::BadRequest("line is outside the document".into()))?;
    let scalar_index = (column - 1) as usize;
    let mut utf16 = 0usize;
    let mut seen = 0usize;
    for character in source_line.chars() {
        if seen == scalar_index {
            break;
        }
        utf16 += character.len_utf16();
        seen += 1;
    }
    if seen < scalar_index {
        return Err(ApiError::BadRequest("column is outside the line".into()));
    }
    Ok(json!({"line":line - 1,"character":utf16}))
}

fn lsp_position_to_byte(text: &str, position: &Value) -> Result<usize, ApiError> {
    let line = position
        .get("line")
        .and_then(Value::as_u64)
        .ok_or_else(|| ApiError::BadRequest("LSP position is missing line".into()))?
        as usize;
    let character = position
        .get("character")
        .and_then(Value::as_u64)
        .ok_or_else(|| ApiError::BadRequest("LSP position is missing character".into()))?
        as usize;

    let mut byte_base = 0usize;
    let mut lines = text.split_inclusive('\n');
    let source_line = loop {
        match lines.next() {
            Some(value) if line == 0 && byte_base == 0 => break value,
            Some(value) => {
                let current_line = text[..byte_base].bytes().filter(|byte| *byte == b'\n').count();
                if current_line == line {
                    break value;
                }
                byte_base += value.len();
            }
            None => {
                if line == text.bytes().filter(|byte| *byte == b'\n').count() {
                    break "";
                }
                return Err(ApiError::BadRequest("LSP line is outside the document".into()));
            }
        }
    };

    let source_line = source_line.strip_suffix('\n').unwrap_or(source_line);
    let mut utf16 = 0usize;
    let mut byte_in_line = 0usize;
    for character_value in source_line.chars() {
        if utf16 == character {
            return Ok(byte_base + byte_in_line);
        }
        let units = character_value.len_utf16();
        if utf16 + units > character {
            return Err(ApiError::BadRequest(
                "LSP character points into the middle of a UTF-16 surrogate pair".into(),
            ));
        }
        utf16 += units;
        byte_in_line += character_value.len_utf8();
    }
    if utf16 == character {
        Ok(byte_base + byte_in_line)
    } else {
        Err(ApiError::BadRequest(
            "LSP character is outside the line".into(),
        ))
    }
}

fn safe_existing_file(root: &Path, requested: &str) -> Result<PathBuf, ApiError> {
    if requested.is_empty() {
        return Err(ApiError::BadRequest("path must not be empty".into()));
    }
    let relative = Path::new(requested);
    if relative.is_absolute()
        || relative.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(ApiError::BadRequest(
            "path must be a safe workspace-relative file path".into(),
        ));
    }
    let raw = root.join(relative);
    if fs::symlink_metadata(&raw)
        .map_err(map_io)?
        .file_type()
        .is_symlink()
    {
        return Err(ApiError::Forbidden(
            "LSP document cannot be a symlink".into(),
        ));
    }
    let canonical = fs::canonicalize(raw).map_err(map_io)?;
    if !canonical.starts_with(root) {
        return Err(ApiError::Forbidden(
            "resolved path escapes workspace root".into(),
        ));
    }
    if !canonical.is_file() {
        return Err(ApiError::BadRequest("path is not a regular file".into()));
    }
    Ok(canonical)
}

fn atomic_replace(path: &Path, content: &[u8]) -> Result<(), ApiError> {
    let parent = path
        .parent()
        .ok_or_else(|| ApiError::BadRequest("edited file has no parent directory".into()))?;
    let permissions = fs::metadata(path).map_err(map_io)?.permissions();
    let mut temporary = NamedTempFile::new_in(parent).map_err(map_io)?;
    temporary.write_all(content).map_err(map_io)?;
    temporary.as_file().sync_all().map_err(map_io)?;
    temporary
        .as_file()
        .set_permissions(permissions)
        .map_err(map_io)?;
    temporary
        .persist(path)
        .map_err(|error| map_io(error.error))?;
    Ok(())
}

fn readable_workspace<'a>(
    state: &'a AppState,
    name: &str,
) -> Result<&'a WorkspaceConfig, ApiError> {
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

fn writable_workspace<'a>(
    state: &'a AppState,
    name: &str,
) -> Result<&'a WorkspaceConfig, ApiError> {
    let workspace = readable_workspace(state, name)?;
    if !workspace.capabilities.fs_write {
        return Err(ApiError::Forbidden(format!(
            "workspace {name:?} does not allow filesystem writes"
        )));
    }
    Ok(workspace)
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn default_true() -> bool {
    true
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
    fn converts_utf16_positions_without_splitting_surrogates() {
        let text = "a😀b\n";
        assert_eq!(lsp_position_to_byte(text, &json!({"line":0,"character":0})).unwrap(), 0);
        assert_eq!(lsp_position_to_byte(text, &json!({"line":0,"character":1})).unwrap(), 1);
        assert!(lsp_position_to_byte(text, &json!({"line":0,"character":2})).is_err());
        assert_eq!(lsp_position_to_byte(text, &json!({"line":0,"character":3})).unwrap(), 5);
    }

    #[test]
    fn applies_edits_from_the_end() {
        let text = "hello world";
        let edits = vec![
            json!({"range":{"start":{"line":0,"character":6},"end":{"line":0,"character":11}},"newText":"rust"}),
            json!({"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":5}},"newText":"hi"})
        ];
        assert_eq!(apply_text_edits(text, &edits).unwrap(), "hi rust");
    }
}

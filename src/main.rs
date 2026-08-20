use std::{env, net::SocketAddr, path::PathBuf};

use axum::{
    Json, Router,
    extract::DefaultBodyLimit,
    middleware,
    routing::{get, post},
};
use serde::Serialize;
use tokio::net::TcpListener;

mod auth;
mod bonus;
mod code_tools;
mod command;
mod config;
mod doctor;
mod error;
mod fs;
mod git;
mod git_extra;
mod git_mutation;
mod hardening;
mod lsp;
mod lsp_status;
mod mutation;
mod project;
mod raw_command;
mod security;
mod state;
mod system;
mod user_files;
mod workspace;

use config::Config;
use state::AppState;

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
    service: &'static str,
    version: &'static str,
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        service: "tuxbridge",
        version: env!("CARGO_PKG_VERSION"),
    })
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config_path = env::var_os("TUXBRIDGE_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("tuxbridge.toml"));

    let config = Config::load(&config_path)?;
    let listen: SocketAddr = config.server.listen.parse()?;
    let max_body_bytes = config.limits.max_body_bytes;
    let state = AppState::new(config)?;

    let protected = Router::new()
        .route("/v1/system", get(system::system_info))
        .route("/v1/system/tool-versions", get(bonus::tool_versions))
        .route("/v1/system/processes", get(bonus::processes))
        .route("/v1/system/listeners", get(bonus::listeners))
        .route("/v1/system/disks", get(bonus::disks))
        .route("/v1/security/profile", get(security::get_security_profile))
        .route("/v1/doctor", get(doctor::doctor))
        .route("/v1/workspaces", get(workspace::list_workspaces))
        .route("/v1/workspaces/{name}", get(workspace::get_workspace))
        .route("/v1/workspaces/resolve", post(workspace::resolve_path))
        .route("/v1/workspaces/tree-summary", post(bonus::workspace_tree_summary))
        .route("/v1/fs/list", post(fs::list_directory))
        .route("/v1/fs/stat", post(fs::stat_path))
        .route("/v1/fs/read", post(fs::read_file))
        .route("/v1/fs/read-batch", post(fs::read_files))
        .route("/v1/fs/search", post(fs::search_files))
        .route("/v1/fs/hash", post(mutation::hash_file))
        .route("/v1/fs/write", post(mutation::write_file))
        .route("/v1/fs/patch", post(mutation::patch_file))
        .route("/v1/user-files", get(user_files::list_mounts))
        .route("/v1/user-files/list", post(user_files::list_directory))
        .route("/v1/user-files/stat", post(user_files::stat_path))
        .route("/v1/user-files/read", post(user_files::read_file))
        .route("/v1/user-files/hash", post(user_files::hash_file))
        .route("/v1/user-files/write", post(user_files::write_file))
        .route("/v1/user-files/patch", post(user_files::patch_file))
        .route("/v1/project/inspect", post(project::inspect_project))
        .route("/v1/code/context", post(code_tools::code_context))
        .route("/v1/code/symbols", post(code_tools::code_symbols))
        .route("/v1/code/references", post(code_tools::code_references))
        .route("/v1/code/edit-plan", post(code_tools::code_edit_plan))
        .route("/v1/code/tasks", post(code_tools::discover_code_tasks))
        .route("/v1/lsp/servers", get(lsp_status::language_servers))
        .route("/v1/lsp/definition", post(lsp::definition))
        .route("/v1/lsp/references", post(lsp::references))
        .route("/v1/lsp/hover", post(lsp::hover))
        .route("/v1/lsp/document-symbols", post(lsp::document_symbols))
        .route("/v1/lsp/workspace-symbols", post(lsp::workspace_symbols))
        .route("/v1/lsp/diagnostics", post(lsp::diagnostics))
        .route("/v1/lsp/rename", post(lsp::rename))
        .route("/v1/lsp/format", post(lsp::formatting))
        .route("/v1/commands/run", post(command::run_command))
        .route("/v1/commands/raw", post(raw_command::run_raw_command))
        .route("/v1/commands/start", post(command::start_command))
        .route(
            "/v1/jobs/{id}",
            get(command::get_job).delete(command::cancel_job),
        )
        .route("/v1/git/status", post(git::git_status))
        .route("/v1/git/branches", post(git::git_branches))
        .route("/v1/git/log", post(git::git_log))
        .route("/v1/git/diff", post(git::git_diff))
        .route("/v1/git/head", post(git_extra::git_head))
        .route("/v1/git/remotes", post(git_extra::git_remotes))
        .route("/v1/git/stashes", post(git_extra::git_stashes))
        .route("/v1/git/fetch", post(git_mutation::git_fetch))
        .route("/v1/git/pull", post(git_mutation::git_pull))
        .route("/v1/git/add", post(git_mutation::git_add))
        .route("/v1/git/commit", post(git_mutation::git_commit))
        .route("/v1/git/push", post(git_mutation::git_push))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_api_key,
        ));

    let app = Router::new()
        .route("/health", get(health))
        .merge(protected)
        .layer(DefaultBodyLimit::max(max_body_bytes))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            hardening::protect_host,
        ))
        .with_state(state);

    let listener = TcpListener::bind(listen).await?;
    println!("tuxbridge listening on {listen}");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

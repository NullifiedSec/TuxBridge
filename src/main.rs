use std::{env, net::SocketAddr, path::PathBuf};

use axum::{
    Json, Router, middleware,
    routing::{get, post},
};
use serde::Serialize;
use tokio::net::TcpListener;

mod auth;
mod command;
mod config;
mod error;
mod fs;
mod git;
mod mutation;
mod project;
mod state;
mod system;
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
    let state = AppState::new(config)?;

    let protected = Router::new()
        .route("/v1/system", get(system::system_info))
        .route("/v1/workspaces", get(workspace::list_workspaces))
        .route("/v1/workspaces/{name}", get(workspace::get_workspace))
        .route("/v1/fs/list", post(fs::list_directory))
        .route("/v1/fs/stat", post(fs::stat_path))
        .route("/v1/fs/read", post(fs::read_file))
        .route("/v1/fs/read-batch", post(fs::read_files))
        .route("/v1/fs/search", post(fs::search_files))
        .route("/v1/fs/hash", post(mutation::hash_file))
        .route("/v1/fs/write", post(mutation::write_file))
        .route("/v1/fs/patch", post(mutation::patch_file))
        .route("/v1/project/inspect", post(project::inspect_project))
        .route("/v1/commands/run", post(command::run_command))
        .route("/v1/commands/start", post(command::start_command))
        .route(
            "/v1/jobs/{id}",
            get(command::get_job).delete(command::cancel_job),
        )
        .route("/v1/git/status", post(git::git_status))
        .route("/v1/git/branches", post(git::git_branches))
        .route("/v1/git/log", post(git::git_log))
        .route("/v1/git/diff", post(git::git_diff))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_api_key,
        ));

    let app = Router::new()
        .route("/health", get(health))
        .merge(protected)
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

use axum::{extract::State, Json};
use serde::Serialize;

use crate::{state::AppState, system::command_exists};

#[derive(Debug, Serialize)]
pub struct LanguageServerStatus {
    name: String,
    source: &'static str,
    enabled: bool,
    executable: String,
    available: bool,
    extensions: Vec<String>,
    language_id: Option<String>,
}

pub async fn language_servers(State(state): State<AppState>) -> Json<Vec<LanguageServerStatus>> {
    let mut results = Vec::new();

    for (name, server) in &state.config.lsp.servers {
        let executable = server.argv.first().cloned().unwrap_or_default();
        results.push(LanguageServerStatus {
            name: name.clone(),
            source: "config",
            enabled: server.enabled,
            available: server.enabled && !executable.is_empty() && command_exists(&executable),
            executable,
            extensions: server.extensions.clone(),
            language_id: server.language_id.clone(),
        });
    }

    let builtins = [
        ("rust-analyzer", "rust-analyzer", &["rs"][..], "rust"),
        ("gopls", "gopls", &["go"][..], "go"),
        (
            "typescript-language-server",
            "typescript-language-server",
            &["ts", "tsx", "js", "jsx", "mjs", "cjs"][..],
            "typescript/javascript",
        ),
        ("pyright", "pyright-langserver", &["py"][..], "python"),
        ("intelephense", "intelephense", &["php"][..], "php"),
        ("jdtls", "jdtls", &["java"][..], "java"),
        (
            "kotlin-language-server",
            "kotlin-language-server",
            &["kt", "kts"][..],
            "kotlin",
        ),
    ];

    for (name, executable, extensions, language_id) in builtins {
        if state.config.lsp.servers.contains_key(name) {
            continue;
        }
        results.push(LanguageServerStatus {
            name: name.into(),
            source: "builtin",
            enabled: true,
            executable: executable.into(),
            available: command_exists(executable),
            extensions: extensions.iter().map(|value| (*value).to_owned()).collect(),
            language_id: Some(language_id.into()),
        });
    }

    results.sort_by(|left, right| left.name.cmp(&right.name));
    Json(results)
}

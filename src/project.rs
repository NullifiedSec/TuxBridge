use std::{collections::BTreeMap, fs, path::Path};

use axum::{extract::State, Json};
use serde::{Deserialize, Serialize};

use crate::{error::ApiError, state::AppState};

#[derive(Debug, Deserialize)]
pub struct InspectRequest {
    workspace: String,
}

#[derive(Debug, Serialize)]
pub struct ProjectInspection {
    workspace: String,
    root: String,
    project_types: Vec<String>,
    package_managers: Vec<String>,
    manifests: BTreeMap<String, bool>,
    top_level: Vec<TopLevelEntry>,
}

#[derive(Debug, Serialize)]
pub struct TopLevelEntry {
    name: String,
    kind: &'static str,
}

pub async fn inspect_project(
    State(state): State<AppState>,
    Json(request): Json<InspectRequest>,
) -> Result<Json<ProjectInspection>, ApiError> {
    let workspace = state
        .config
        .workspaces
        .get(&request.workspace)
        .ok_or_else(|| ApiError::NotFound(format!("workspace {:?} is not configured", request.workspace)))?;
    if !workspace.capabilities.fs_read {
        return Err(ApiError::Forbidden(format!(
            "workspace {:?} does not allow filesystem reads",
            request.workspace
        )));
    }

    let root = fs::canonicalize(&workspace.root)
        .map_err(|error| ApiError::Internal(format!("failed to resolve workspace root: {error}")))?;

    let markers = [
        "Cargo.toml",
        "Cargo.lock",
        "go.mod",
        "go.sum",
        "package.json",
        "package-lock.json",
        "pnpm-lock.yaml",
        "yarn.lock",
        "bun.lock",
        "bun.lockb",
        "composer.json",
        "composer.lock",
        "pyproject.toml",
        "requirements.txt",
        "Pipfile",
        "Gemfile",
        "pom.xml",
        "build.gradle",
        "build.gradle.kts",
        "CMakeLists.txt",
        "Makefile",
        "Justfile",
        "Dockerfile",
        "docker-compose.yml",
        "docker-compose.yaml",
        "nuxt.config.ts",
        "nuxt.config.js",
        "next.config.js",
        "next.config.mjs",
        "vite.config.ts",
        "vite.config.js",
        "tsconfig.json",
        "capacitor.config.ts",
        "capacitor.config.json",
    ];

    let manifests = markers
        .into_iter()
        .map(|marker| (marker.to_owned(), root.join(marker).exists()))
        .collect::<BTreeMap<_, _>>();

    let mut project_types = Vec::new();
    push_if(&mut project_types, "rust", exists(&manifests, "Cargo.toml"));
    push_if(&mut project_types, "go", exists(&manifests, "go.mod"));
    push_if(&mut project_types, "node", exists(&manifests, "package.json"));
    push_if(&mut project_types, "php", exists(&manifests, "composer.json"));
    push_if(
        &mut project_types,
        "python",
        exists(&manifests, "pyproject.toml") || exists(&manifests, "requirements.txt"),
    );
    push_if(&mut project_types, "java", exists(&manifests, "pom.xml"));
    push_if(
        &mut project_types,
        "gradle",
        exists(&manifests, "build.gradle") || exists(&manifests, "build.gradle.kts"),
    );
    push_if(
        &mut project_types,
        "nuxt",
        exists(&manifests, "nuxt.config.ts") || exists(&manifests, "nuxt.config.js"),
    );
    push_if(
        &mut project_types,
        "next",
        exists(&manifests, "next.config.js") || exists(&manifests, "next.config.mjs"),
    );
    push_if(
        &mut project_types,
        "vite",
        exists(&manifests, "vite.config.ts") || exists(&manifests, "vite.config.js"),
    );
    push_if(
        &mut project_types,
        "capacitor",
        exists(&manifests, "capacitor.config.ts") || exists(&manifests, "capacitor.config.json"),
    );

    let mut package_managers = Vec::new();
    push_if(&mut package_managers, "cargo", exists(&manifests, "Cargo.toml"));
    push_if(&mut package_managers, "go", exists(&manifests, "go.mod"));
    push_if(&mut package_managers, "npm", exists(&manifests, "package-lock.json"));
    push_if(&mut package_managers, "pnpm", exists(&manifests, "pnpm-lock.yaml"));
    push_if(&mut package_managers, "yarn", exists(&manifests, "yarn.lock"));
    push_if(
        &mut package_managers,
        "bun",
        exists(&manifests, "bun.lock") || exists(&manifests, "bun.lockb"),
    );
    push_if(&mut package_managers, "composer", exists(&manifests, "composer.json"));

    let mut top_level = Vec::new();
    for entry in fs::read_dir(&root).map_err(map_io)? {
        let entry = entry.map_err(map_io)?;
        let metadata = fs::symlink_metadata(entry.path()).map_err(map_io)?;
        let kind = if metadata.file_type().is_symlink() {
            "symlink"
        } else if metadata.is_dir() {
            "directory"
        } else if metadata.is_file() {
            "file"
        } else {
            "other"
        };
        top_level.push(TopLevelEntry {
            name: entry.file_name().to_string_lossy().into_owned(),
            kind,
        });
    }
    top_level.sort_by(|a, b| a.name.cmp(&b.name));

    Ok(Json(ProjectInspection {
        workspace: request.workspace,
        root: root.display().to_string(),
        project_types,
        package_managers,
        manifests,
        top_level,
    }))
}

fn exists(manifests: &BTreeMap<String, bool>, name: &str) -> bool {
    manifests.get(name).copied().unwrap_or(false)
}

fn push_if(values: &mut Vec<String>, value: &str, condition: bool) {
    if condition {
        values.push(value.to_owned());
    }
}

fn map_io(error: std::io::Error) -> ApiError {
    match error.kind() {
        std::io::ErrorKind::NotFound => ApiError::NotFound(error.to_string()),
        std::io::ErrorKind::PermissionDenied => ApiError::Forbidden(error.to_string()),
        _ => ApiError::Internal(error.to_string()),
    }
}

#[allow(dead_code)]
fn is_within(root: &Path, path: &Path) -> bool {
    path.starts_with(root)
}

use std::{collections::{BTreeMap, BTreeSet}, fs, path::Path, process::Command};

use axum::{extract::State, Json};
use serde::{Deserialize, Serialize};

use crate::{config::WorkspaceConfig, error::ApiError, state::AppState};

const MAX_FILES: usize = 20_000;
const MAX_SAMPLE: usize = 250;

#[derive(Debug, Deserialize)]
pub struct RepoMapRequest { workspace: String }

#[derive(Debug, Serialize)]
pub struct RepoMapResponse {
    workspace: String,
    root: String,
    project_kinds: Vec<String>,
    manifests: Vec<String>,
    language_files: BTreeMap<String, usize>,
    source_roots: Vec<String>,
    test_roots: Vec<String>,
    config_files: Vec<String>,
    important_files: Vec<String>,
    changed_files: Vec<String>,
    sampled_source_files: Vec<String>,
    files_scanned: usize,
    truncated: bool,
}

pub async fn repository_map(State(state):State<AppState>,Json(req):Json<RepoMapRequest>)->Result<Json<RepoMapResponse>,ApiError>{
    let ws=readable_workspace(&state,&req.workspace)?;let root=fs::canonicalize(&ws.root).map_err(map_io)?;
    let mut acc=Accumulator::default();scan(&root,&root,&mut acc)?;
    let (project_kinds,manifests)=detect_manifests(&root);
    let changed_files=if ws.capabilities.git_read{git_changed(&root)}else{Vec::new()};
    let response=RepoMapResponse{workspace:req.workspace,root:root.display().to_string(),project_kinds,manifests,language_files:acc.languages,source_roots:acc.source_roots.into_iter().collect(),test_roots:acc.test_roots.into_iter().collect(),config_files:acc.configs.into_iter().take(MAX_SAMPLE).collect(),important_files:acc.important.into_iter().take(MAX_SAMPLE).collect(),changed_files,sampled_source_files:acc.samples.into_iter().take(MAX_SAMPLE).collect(),files_scanned:acc.files,truncated:acc.truncated};
    state.events.emit("code.repo_map",Some(&response.workspace),format!("mapped {} files",response.files_scanned),serde_json::json!({"files_scanned":response.files_scanned,"project_kinds":response.project_kinds.clone()})).await;
    Ok(Json(response))
}

#[derive(Default)]
struct Accumulator{languages:BTreeMap<String,usize>,source_roots:BTreeSet<String>,test_roots:BTreeSet<String>,configs:BTreeSet<String>,important:BTreeSet<String>,samples:BTreeSet<String>,files:usize,truncated:bool}

fn scan(root:&Path,path:&Path,acc:&mut Accumulator)->Result<(),ApiError>{
    if acc.files>=MAX_FILES{acc.truncated=true;return Ok(());}let meta=fs::symlink_metadata(path).map_err(map_io)?;if meta.file_type().is_symlink(){return Ok(());}if meta.is_dir(){for entry in fs::read_dir(path).map_err(map_io)?{let entry=entry.map_err(map_io)?;let name=entry.file_name();let name=name.to_string_lossy();if matches!(name.as_ref(),".git"|"node_modules"|"target"|"dist"|"build"|"vendor"|".next"|".nuxt"|"coverage"){continue;}scan(root,&entry.path(),acc)?;if acc.truncated{break;}}return Ok(());}if !meta.is_file(){return Ok(());}acc.files+=1;let rel=relative(root,path);let lower=rel.to_ascii_lowercase();let ext=path.extension().and_then(|v|v.to_str()).unwrap_or("").to_ascii_lowercase();if let Some(lang)=language(&ext){*acc.languages.entry(lang.into()).or_default()+=1;if acc.samples.len()<MAX_SAMPLE{acc.samples.insert(rel.clone());}}
    if let Some(first)=Path::new(&rel).components().next().map(|c|c.as_os_str().to_string_lossy().into_owned()){if matches!(first.as_str(),"src"|"app"|"lib"|"packages"|"crates"|"apps"|"internal"|"cmd"){acc.source_roots.insert(first.clone());}if matches!(first.as_str(),"test"|"tests"|"spec"|"specs"|"__tests__"){acc.test_roots.insert(first);}}
    if lower.contains("test")||lower.contains("spec"){if let Some(parent)=Path::new(&rel).parent(){acc.test_roots.insert(parent.to_string_lossy().into_owned());}}
    let filename=path.file_name().and_then(|v|v.to_str()).unwrap_or("");if is_config(filename,&ext){acc.configs.insert(rel.clone());}if is_important(filename){acc.important.insert(rel);}Ok(())
}

fn detect_manifests(root:&Path)->(Vec<String>,Vec<String>){let candidates=[("Cargo.toml","rust"),("go.mod","go"),("package.json","javascript/typescript"),("pyproject.toml","python"),("requirements.txt","python"),("composer.json","php"),("pom.xml","java"),("build.gradle","java/kotlin"),("build.gradle.kts","kotlin"),("Gemfile","ruby")];let mut kinds=BTreeSet::new();let mut manifests=Vec::new();for (file,kind) in candidates{if root.join(file).is_file(){kinds.insert(kind.into());manifests.push(file.into());}}(kinds.into_iter().collect(),manifests)}
fn git_changed(root:&Path)->Vec<String>{Command::new("git").args(["-c","core.fsmonitor=false","status","--porcelain=v1"]).current_dir(root).env("GIT_TERMINAL_PROMPT","0").output().ok().filter(|o|o.status.success()).map(|o|String::from_utf8_lossy(&o.stdout).lines().filter_map(|l|l.get(3..)).map(|v|v.trim().to_owned()).take(1000).collect()).unwrap_or_default()}
fn language(ext:&str)->Option<&'static str>{match ext{"rs"=>Some("rust"),"go"=>Some("go"),"ts"|"tsx"=>Some("typescript"),"js"|"jsx"|"mjs"|"cjs"=>Some("javascript"),"py"=>Some("python"),"php"=>Some("php"),"java"=>Some("java"),"kt"|"kts"=>Some("kotlin"),"c"|"h"=>Some("c"),"cc"|"cpp"|"cxx"|"hpp"=>Some("cpp"),"rb"=>Some("ruby"),"cs"=>Some("csharp"),_=>None}}
fn is_config(name:&str,ext:&str)->bool{name.starts_with('.')||matches!(ext,"toml"|"yaml"|"yml"|"json"|"ini"|"conf")||name.ends_with(".config.js")||name.ends_with(".config.ts")}
fn is_important(name:&str)->bool{matches!(name,"README.md"|"AGENTS.md"|"CLAUDE.md"|"CONTRIBUTING.md"|"Dockerfile"|"Makefile"|"Justfile"|"justfile"|"package.json"|"Cargo.toml"|"go.mod"|"pyproject.toml"|"composer.json")}
fn relative(root:&Path,path:&Path)->String{path.strip_prefix(root).unwrap_or(path).to_string_lossy().into_owned()}
fn readable_workspace<'a>(state:&'a AppState,name:&str)->Result<&'a WorkspaceConfig,ApiError>{let ws=state.config.workspaces.get(name).ok_or_else(||ApiError::NotFound(format!("workspace {name:?} is not configured")))?;if !ws.capabilities.fs_read{return Err(ApiError::Forbidden("workspace does not allow filesystem reads".into()));}Ok(ws)}
fn map_io(e:std::io::Error)->ApiError{match e.kind(){std::io::ErrorKind::NotFound=>ApiError::NotFound(e.to_string()),std::io::ErrorKind::PermissionDenied=>ApiError::Forbidden(e.to_string()),_=>ApiError::Internal(e.to_string())}}

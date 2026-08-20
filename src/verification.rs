use std::{fs, path::Path};

use axum::{extract::State, Json};
use serde::{Deserialize, Serialize};

use crate::{error::ApiError, state::AppState};

#[derive(Debug, Deserialize)]
pub struct VerificationRequest { workspace:String, #[serde(default)] changed_paths:Vec<String> }
#[derive(Debug, Serialize)]
pub struct VerificationPlan { workspace:String, changed_paths:Vec<String>, fast:Vec<VerificationTask>, full:Vec<VerificationTask> }
#[derive(Debug, Serialize)]
pub struct VerificationTask { label:String, argv:Vec<String>, rationale:String }

pub async fn verification_plan(State(state):State<AppState>,Json(req):Json<VerificationRequest>)->Result<Json<VerificationPlan>,ApiError>{
    let ws=state.config.workspaces.get(&req.workspace).ok_or_else(||ApiError::NotFound("workspace not configured".into()))?;if !ws.capabilities.fs_read{return Err(ApiError::Forbidden("workspace does not allow filesystem reads".into()));}let root=fs::canonicalize(&ws.root).map_err(|e|ApiError::Internal(e.to_string()))?;
    let mut fast=Vec::new();let mut full=Vec::new();
    if root.join("Cargo.toml").is_file(){fast.push(task("Cargo check",["cargo","check","--all-targets"],"compiler/type feedback without running the test suite"));fast.push(task("Rustfmt check",["cargo","fmt","--all","--","--check"],"cheap formatting validation"));full.push(task("Cargo test",["cargo","test","--all-targets","--all-features"],"full Rust verification"));full.push(task("Clippy",["cargo","clippy","--all-targets","--all-features","--","-D","warnings"],"lint all Rust targets"));}
    if root.join("go.mod").is_file(){let packages=go_packages(&req.changed_paths);for pkg in packages{fast.push(VerificationTask{label:format!("Go test {pkg}"),argv:vec!["go".into(),"test".into(),pkg.clone()],rationale:"test the package containing changed Go files".into()});}full.push(task("Go test all",["go","test","./..."],"full Go module tests"));full.push(task("Go vet",["go","vet","./..."],"full Go static analysis"));}
    if root.join("package.json").is_file(){let manager=if root.join("bun.lock").is_file()||root.join("bun.lockb").is_file(){"bun"}else if root.join("pnpm-lock.yaml").is_file(){"pnpm"}else if root.join("yarn.lock").is_file(){"yarn"}else{"npm"};if let Ok(raw)=fs::read_to_string(root.join("package.json")){if let Ok(value)=serde_json::from_str::<serde_json::Value>(&raw){if let Some(scripts)=value.get("scripts").and_then(|v|v.as_object()){for name in ["typecheck","check","lint"]{if scripts.contains_key(name){fast.push(script_task(manager,name,"fast project-defined static verification"));}}for name in ["test","build"]{if scripts.contains_key(name){full.push(script_task(manager,name,"project-defined full verification"));}}}}}}
    if root.join("pyproject.toml").is_file(){full.push(task("Pytest",["python3","-m","pytest"],"full Python test suite"));}
    state.events.emit("code.verification_plan",Some(&req.workspace),format!("planned {} fast and {} full checks",fast.len(),full.len()),serde_json::json!({"fast":fast.len(),"full":full.len()})).await;
    Ok(Json(VerificationPlan{workspace:req.workspace,changed_paths:req.changed_paths,fast,full}))
}

fn task<const N:usize>(label:&str,argv:[&str;N],rationale:&str)->VerificationTask{VerificationTask{label:label.into(),argv:argv.into_iter().map(str::to_owned).collect(),rationale:rationale.into()}}
fn script_task(manager:&str,name:&str,rationale:&str)->VerificationTask{let argv=match manager{"bun"=>vec!["bun","run",name],"pnpm"=>vec!["pnpm","run",name],"yarn"=>vec!["yarn",name],_=>vec!["npm","run",name]};VerificationTask{label:format!("{name} script"),argv:argv.into_iter().map(str::to_owned).collect(),rationale:rationale.into()}}
fn go_packages(paths:&[String])->Vec<String>{let mut set=std::collections::BTreeSet::new();for path in paths{if Path::new(path).extension().and_then(|v|v.to_str())==Some("go"){let parent=Path::new(path).parent().unwrap_or(Path::new("."));set.insert(if parent==Path::new(""){".".into()}else{format!("./{}",parent.to_string_lossy())});}}if set.is_empty(){set.insert("./...".into());}set.into_iter().collect()}

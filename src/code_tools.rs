use std::{
    collections::BTreeSet,
    fs,
    io::Write,
    path::{Component, Path, PathBuf},
};

use axum::{extract::State, Json};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;

use crate::{config::WorkspaceConfig, error::ApiError, state::AppState};

const MAX_FILE_BYTES: usize = 8 * 1024 * 1024;
const MAX_CONTEXT_BYTES: usize = 512 * 1024;
const MAX_EDIT_FILES: usize = 64;
const MAX_EDITS_PER_FILE: usize = 256;
const MAX_REFERENCES: usize = 2_000;
const MAX_SCAN_FILE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_SYMBOLS: usize = 2_000;

#[derive(Debug, Deserialize)]
pub struct CodeContextRequest { workspace:String, path:String, start_line:Option<usize>, end_line:Option<usize>, context_before:Option<usize>, context_after:Option<usize>, max_bytes:Option<usize> }
#[derive(Debug, Serialize)]
pub struct CodeContextResponse { path:String, sha256:String, total_lines:usize, start_line:usize, end_line:usize, content:String, truncated:bool }
#[derive(Debug, Deserialize)]
pub struct CodeSymbolsRequest { workspace:String, path:String }
#[derive(Debug, Serialize)]
pub struct CodeSymbol { name:String, kind:String, line:usize, signature:String }
#[derive(Debug, Serialize)]
pub struct CodeSymbolsResponse { path:String, sha256:String, language:String, symbols:Vec<CodeSymbol>, truncated:bool }
#[derive(Debug, Deserialize)]
pub struct CodeReferencesRequest { workspace:String, identifier:String, path:Option<String>, max_results:Option<usize> }
#[derive(Debug, Serialize)]
pub struct CodeReference { path:String, line:usize, column:usize, preview:String }
#[derive(Debug, Serialize)]
pub struct CodeReferencesResponse { identifier:String, references:Vec<CodeReference>, files_scanned:usize, truncated:bool }

#[derive(Debug, Deserialize)]
pub struct CodeEditPlanRequest {
    workspace: String,
    files: Vec<FileEditPlan>,
    session_id: Option<String>,
    #[serde(default)] dry_run: bool,
}
#[derive(Debug, Deserialize)]
pub struct FileEditPlan { path:String, expected_sha256:String, edits:Vec<CodeEdit> }
#[derive(Debug, Deserialize)]
#[serde(tag="kind",rename_all="snake_case")]
pub enum CodeEdit { ReplaceExact{old:String,new:String}, ReplaceLines{start_line:usize,end_line:usize,new:String}, InsertBefore{line:usize,new:String}, InsertAfter{line:usize,new:String} }
#[derive(Debug, Serialize)]
pub struct CodeEditPlanResponse { dry_run:bool, session_id:Option<String>, files:Vec<FileEditResult> }
#[derive(Debug, Serialize)]
pub struct FileEditResult { path:String, old_sha256:String, new_sha256:String, changed:bool, additions:usize, deletions:usize, preview:String }
#[derive(Debug, Deserialize)]
pub struct CodeTaskRequest { workspace:String }
#[derive(Debug, Serialize)]
pub struct CodeTask { id:String, label:String, argv:Vec<String>, source:String, execution_risk:String }
#[derive(Debug, Serialize)]
pub struct CodeTasksResponse { workspace:String, tasks:Vec<CodeTask> }

pub async fn code_context(State(state):State<AppState>,Json(request):Json<CodeContextRequest>)->Result<Json<CodeContextResponse>,ApiError>{
    let workspace=workspace(&state,&request.workspace,false)?;let(root,target)=existing_file(workspace,&request.path)?;let bytes=read_bounded(&target)?;let sha256=digest(&bytes);let text=String::from_utf8(bytes).map_err(|_|ApiError::Unsupported("file is not valid UTF-8 text".into()))?;let lines=text.lines().collect::<Vec<_>>();let total_lines=lines.len();let requested_start=request.start_line.unwrap_or(1).max(1);let requested_end=request.end_line.unwrap_or(requested_start).max(requested_start);let start_line=requested_start.saturating_sub(request.context_before.unwrap_or(20)).max(1);let end_line=requested_end.saturating_add(request.context_after.unwrap_or(20)).min(total_lines.max(1));let max_bytes=request.max_bytes.unwrap_or(MAX_CONTEXT_BYTES).clamp(1,MAX_CONTEXT_BYTES);let mut content=String::new();let mut truncated=false;for number in start_line..=end_line{let line=lines.get(number-1).copied().unwrap_or("");let rendered=format!("{number:>6} | {line}\n");if content.len().saturating_add(rendered.len())>max_bytes{truncated=true;break;}content.push_str(&rendered);}Ok(Json(CodeContextResponse{path:relative(&root,&target),sha256,total_lines,start_line,end_line,content,truncated}))
}

pub async fn code_symbols(State(state):State<AppState>,Json(request):Json<CodeSymbolsRequest>)->Result<Json<CodeSymbolsResponse>,ApiError>{let workspace=workspace(&state,&request.workspace,false)?;let(root,target)=existing_file(workspace,&request.path)?;let bytes=read_bounded(&target)?;let sha256=digest(&bytes);let text=String::from_utf8(bytes).map_err(|_|ApiError::Unsupported("file is not valid UTF-8 text".into()))?;let language=language_for(&target).to_owned();let mut symbols=Vec::new();let mut truncated=false;for(index,line)in text.lines().enumerate(){if let Some((kind,name))=detect_symbol(&language,line){symbols.push(CodeSymbol{name,kind,line:index+1,signature:line.trim().chars().take(300).collect()});if symbols.len()==MAX_SYMBOLS{truncated=true;break;}}}Ok(Json(CodeSymbolsResponse{path:relative(&root,&target),sha256,language,symbols,truncated}))}

pub async fn code_references(State(state):State<AppState>,Json(request):Json<CodeReferencesRequest>)->Result<Json<CodeReferencesResponse>,ApiError>{validate_identifier(&request.identifier)?;let workspace=workspace(&state,&request.workspace,false)?;let root=canonical_root(workspace)?;let start=match request.path.as_deref(){Some(path)=>existing_path(&root,path)?,None=>root.clone()};let max_results=request.max_results.unwrap_or(500).clamp(1,MAX_REFERENCES);let mut result=CodeReferencesResponse{identifier:request.identifier.clone(),references:Vec::new(),files_scanned:0,truncated:false};scan_references(&root,&start,&request.identifier,max_results,&mut result)?;Ok(Json(result))}

pub async fn code_edit_plan(State(state):State<AppState>,Json(request):Json<CodeEditPlanRequest>)->Result<Json<CodeEditPlanResponse>,ApiError>{
    if request.files.is_empty()||request.files.len()>MAX_EDIT_FILES{return Err(ApiError::BadRequest(format!("files must contain between 1 and {MAX_EDIT_FILES} entries")));}
    let workspace=workspace(&state,&request.workspace,true)?;let root=canonical_root(workspace)?;let mut seen=BTreeSet::new();let mut prepared=Vec::with_capacity(request.files.len());
    for file in &request.files{if file.edits.is_empty()||file.edits.len()>MAX_EDITS_PER_FILE{return Err(ApiError::BadRequest(format!("each file must contain between 1 and {MAX_EDITS_PER_FILE} edits")));}let target=existing_from_root(&root,&file.path)?;let path=relative(&root,&target);if !seen.insert(path.clone()){return Err(ApiError::BadRequest(format!("duplicate edit target {path:?}")));}reject_symlink(&target)?;let original_bytes=read_bounded(&target)?;let old_sha256=digest(&original_bytes);if !file.expected_sha256.eq_ignore_ascii_case(&old_sha256){return Err(ApiError::Conflict(format!("file changed: {path} expected sha256 {}, current sha256 {old_sha256}",file.expected_sha256)));}let original=String::from_utf8(original_bytes).map_err(|_|ApiError::Unsupported(format!("{path} is not valid UTF-8 text")))?;let updated=apply_edits(original.clone(),&file.edits)?;if updated.len()>MAX_FILE_BYTES{return Err(ApiError::BadRequest(format!("updated file {path} exceeds {MAX_FILE_BYTES} bytes")));}prepared.push(PreparedEdit{target,path,permissions:fs::metadata(&root.join(&file.path)).map_err(map_io)?.permissions(),old_sha256,original,updated});}
    let results=prepared.iter().map(result_for).collect::<Vec<_>>();
    if !request.dry_run{
        if let Some(session_id)=request.session_id.as_deref(){for item in &prepared{state.sessions.capture_change(session_id,&request.workspace,&item.path,item.original.as_bytes(),item.updated.as_bytes()).await?;}}
        for item in &prepared{atomic_write(&item.target,item.updated.as_bytes(),item.permissions.clone())?;}
        state.events.emit("code.edit_plan.applied",Some(&request.workspace),format!("applied edits to {} files",prepared.len()),serde_json::json!({"files":prepared.iter().map(|p|p.path.clone()).collect::<Vec<_>>(),"session_id":request.session_id.clone()})).await;
    }else{state.events.emit("code.edit_plan.previewed",Some(&request.workspace),format!("previewed edits to {} files",prepared.len()),serde_json::json!({"files":prepared.iter().map(|p|p.path.clone()).collect::<Vec<_>>(),"session_id":request.session_id.clone()})).await;}
    Ok(Json(CodeEditPlanResponse{dry_run:request.dry_run,session_id:request.session_id,files:results}))
}

pub async fn discover_code_tasks(State(state):State<AppState>,Json(request):Json<CodeTaskRequest>)->Result<Json<CodeTasksResponse>,ApiError>{let workspace=workspace(&state,&request.workspace,false)?;let root=canonical_root(workspace)?;let mut tasks=Vec::new();if root.join("Cargo.toml").is_file(){tasks.extend([task("rust-check","Rust check",&["cargo","check","--all-targets","--all-features"],"Cargo.toml"),task("rust-test","Rust tests",&["cargo","test","--all-targets","--all-features"],"Cargo.toml"),task("rust-format-check","Rust format check",&["cargo","fmt","--all","--","--check"],"Cargo.toml"),task("rust-clippy","Rust Clippy",&["cargo","clippy","--all-targets","--all-features","--","-D","warnings"],"Cargo.toml")]);}if root.join("go.mod").is_file(){tasks.extend([task("go-test","Go tests",&["go","test","./..."],"go.mod"),task("go-vet","Go vet",&["go","vet","./..."],"go.mod")]);}if root.join("package.json").is_file(){discover_package_scripts(&root,&mut tasks);}if root.join("pyproject.toml").is_file(){tasks.push(task("python-test","Python tests",&["python3","-m","pytest"],"pyproject.toml"));}if root.join("composer.json").is_file(){tasks.push(task("composer-test","Composer test script",&["composer","test"],"composer.json"));}Ok(Json(CodeTasksResponse{workspace:request.workspace,tasks}))}

struct PreparedEdit{target:PathBuf,path:String,permissions:fs::Permissions,old_sha256:String,original:String,updated:String}
fn result_for(item:&PreparedEdit)->FileEditResult{let old_lines=item.original.lines().collect::<Vec<_>>();let new_lines=item.updated.lines().collect::<Vec<_>>();let prefix=old_lines.iter().zip(&new_lines).take_while(|(a,b)|a==b).count();let suffix=old_lines[prefix..].iter().rev().zip(new_lines[prefix..].iter().rev()).take_while(|(a,b)|a==b).count();let old_end=old_lines.len().saturating_sub(suffix);let new_end=new_lines.len().saturating_sub(suffix);FileEditResult{path:item.path.clone(),old_sha256:item.old_sha256.clone(),new_sha256:digest(item.updated.as_bytes()),changed:item.original!=item.updated,additions:new_end.saturating_sub(prefix),deletions:old_end.saturating_sub(prefix),preview:preview(&old_lines,&new_lines,prefix,old_end,new_end)}}
fn preview(old:&[&str],new:&[&str],start:usize,old_end:usize,new_end:usize)->String{let mut out=format!("@@ line {} @@\n",start+1);for line in &old[start.saturating_sub(3)..start]{out.push_str("  ");out.push_str(line);out.push('\n');}for line in &old[start..old_end]{out.push_str("- ");out.push_str(line);out.push('\n');}for line in &new[start..new_end]{out.push_str("+ ");out.push_str(line);out.push('\n');}for line in new.iter().skip(new_end).take(3){out.push_str("  ");out.push_str(line);out.push('\n');}if out.len()>32*1024{out.truncate(32*1024);out.push_str("\n... preview truncated ...\n");}out}
fn apply_edits(mut text:String,edits:&[CodeEdit])->Result<String,ApiError>{for edit in edits{text=match edit{CodeEdit::ReplaceExact{old,new}=>{if old.is_empty(){return Err(ApiError::BadRequest("replace_exact old text must not be empty".into()));}let count=text.match_indices(old).count();if count!=1{return Err(ApiError::Conflict(format!("replace_exact requires exactly one match, found {count}")));}text.replacen(old,new,1)},CodeEdit::ReplaceLines{start_line,end_line,new}=>replace_lines(&text,*start_line,*end_line,new)?,CodeEdit::InsertBefore{line,new}=>insert_line(&text,*line,new,false)?,CodeEdit::InsertAfter{line,new}=>insert_line(&text,*line,new,true)?};}Ok(text)}
fn replace_lines(text:&str,start:usize,end:usize,new:&str)->Result<String,ApiError>{if start==0||end<start{return Err(ApiError::BadRequest("invalid line range".into()));}let lines=split_lines(text);if end>lines.len(){return Err(ApiError::Conflict(format!("line range {start}..={end} exceeds file length {}",lines.len())));}let mut out=lines[..start-1].concat();out.push_str(new);if !new.is_empty()&&!new.ends_with('\n')&&end<lines.len(){out.push('\n');}out.push_str(&lines[end..].concat());Ok(out)}
fn insert_line(text:&str,line:usize,new:&str,after:bool)->Result<String,ApiError>{if line==0{return Err(ApiError::BadRequest("line numbers are 1-based".into()));}let lines=split_lines(text);if line>lines.len().max(1){return Err(ApiError::Conflict(format!("line {line} exceeds file length {}",lines.len())));}let index=if after{line.min(lines.len())}else{line.saturating_sub(1).min(lines.len())};let mut out=lines[..index].concat();out.push_str(new);if !new.is_empty()&&!new.ends_with('\n')&&index<lines.len(){out.push('\n');}out.push_str(&lines[index..].concat());Ok(out)}
fn split_lines(text:&str)->Vec<String>{text.split_inclusive('\n').map(str::to_owned).collect()}
fn scan_references(root:&Path,path:&Path,identifier:&str,max:usize,out:&mut CodeReferencesResponse)->Result<(),ApiError>{if out.references.len()>=max{out.truncated=true;return Ok(());}let metadata=fs::symlink_metadata(path).map_err(map_io)?;if metadata.file_type().is_symlink(){return Ok(());}if metadata.is_file(){if metadata.len()>MAX_SCAN_FILE_BYTES||binary_extension(path){return Ok(());}out.files_scanned+=1;let Ok(text)=fs::read_to_string(path)else{return Ok(());};for(line_index,line)in text.lines().enumerate(){for column in identifier_columns(line,identifier){out.references.push(CodeReference{path:relative(root,path),line:line_index+1,column:column+1,preview:line.trim().chars().take(300).collect()});if out.references.len()>=max{out.truncated=true;return Ok(());}}}}else if metadata.is_dir(){for entry in fs::read_dir(path).map_err(map_io)?{let entry=entry.map_err(map_io)?;let name=entry.file_name();let name=name.to_string_lossy();if matches!(name.as_ref(),".git"|"node_modules"|"target"|".nuxt"|".next"|"dist"|"build"|"vendor"){continue;}scan_references(root,&entry.path(),identifier,max,out)?;if out.truncated{break;}}}Ok(())}
fn identifier_columns(line:&str,identifier:&str)->Vec<usize>{let mut columns=Vec::new();let mut offset=0;while offset<=line.len(){let Some(found)=line[offset..].find(identifier)else{break;};let start=offset+found;let end=start+identifier.len();let left_ok=line[..start].chars().next_back().is_none_or(|ch|!ident_char(ch));let right_ok=line[end..].chars().next().is_none_or(|ch|!ident_char(ch));if left_ok&&right_ok{columns.push(start);}offset=end;if offset==line.len(){break;}}columns}
fn validate_identifier(identifier:&str)->Result<(),ApiError>{if identifier.is_empty()||identifier.len()>256||!identifier.chars().all(ident_char)||identifier.chars().next().is_some_and(|ch|ch.is_ascii_digit()){Err(ApiError::BadRequest("identifier must be a simple programming-language identifier".into()))}else{Ok(())}}
fn ident_char(ch:char)->bool{ch=='_'||ch.is_ascii_alphanumeric()}
fn detect_symbol(language:&str,line:&str)->Option<(String,String)>{let line=line.trim_start();let patterns:&[(&str,&str)]=match language{"rust"=>&[("pub fn ","function"),("fn ","function"),("pub struct ","struct"),("struct ","struct"),("pub enum ","enum"),("enum ","enum"),("trait ","trait"),("impl ","impl"),("mod ","module")],"go"=>&[("func ","function"),("type ","type")],"python"=>&[("async def ","function"),("def ","function"),("class ","class")],"javascript"|"typescript"=>&[("export async function ","function"),("export function ","function"),("async function ","function"),("function ","function"),("export class ","class"),("class ","class"),("export interface ","interface"),("interface ","interface"),("export type ","type"),("type ","type")],"php"=>&[("function ","function"),("class ","class"),("interface ","interface"),("trait ","trait")],"java"|"kotlin"=>&[("class ","class"),("interface ","interface"),("enum ","enum"),("fun ","function")],_=>&[]};for(prefix,kind)in patterns{if let Some(rest)=line.strip_prefix(prefix){let name=rest.split(|ch:char|!ident_char(ch)).next().unwrap_or("");if !name.is_empty(){return Some(((*kind).into(),name.into()));}}}None}
fn language_for(path:&Path)->&'static str{match path.extension().and_then(|x|x.to_str()).unwrap_or(""){"rs"=>"rust","go"=>"go","py"=>"python","js"|"jsx"|"mjs"|"cjs"=>"javascript","ts"|"tsx"|"mts"|"cts"|"vue"=>"typescript","php"=>"php","java"=>"java","kt"|"kts"=>"kotlin","c"|"h"=>"c","cc"|"cpp"|"cxx"|"hpp"=>"cpp","sh"|"bash"=>"shell","toml"=>"toml","json"=>"json","yaml"|"yml"=>"yaml",_=>"text"}}
fn binary_extension(path:&Path)->bool{matches!(path.extension().and_then(|x|x.to_str()).unwrap_or("").to_ascii_lowercase().as_str(),"png"|"jpg"|"jpeg"|"gif"|"webp"|"ico"|"pdf"|"zip"|"gz"|"xz"|"bz2"|"7z"|"tar"|"wasm"|"exe"|"dll"|"so"|"a"|"o"|"class"|"jar"|"lockb")}
fn discover_package_scripts(root:&Path,tasks:&mut Vec<CodeTask>){let manager=if root.join("bun.lock").is_file()||root.join("bun.lockb").is_file(){"bun"}else if root.join("pnpm-lock.yaml").is_file(){"pnpm"}else if root.join("yarn.lock").is_file(){"yarn"}else{"npm"};let Ok(raw)=fs::read_to_string(root.join("package.json"))else{return;};let Ok(value)=serde_json::from_str::<serde_json::Value>(&raw)else{return;};let Some(scripts)=value.get("scripts").and_then(|x|x.as_object())else{return;};for name in ["typecheck","check","lint","test","build","format"]{if !scripts.contains_key(name){continue;}let argv=match manager{"bun"=>vec!["bun".into(),"run".into(),name.into()],"pnpm"=>vec!["pnpm".into(),"run".into(),name.into()],"yarn"=>vec!["yarn".into(),name.into()],_=>vec!["npm".into(),"run".into(),name.into()]};tasks.push(CodeTask{id:format!("js-{name}"),label:format!("package script: {name}"),argv,source:"package.json".into(),execution_risk:"project scripts can execute arbitrary code with the TuxBridge service user's privileges".into()});}}
fn task(id:&str,label:&str,argv:&[&str],source:&str)->CodeTask{CodeTask{id:id.into(),label:label.into(),argv:argv.iter().map(|x|(*x).into()).collect(),source:source.into(),execution_risk:"build, test, lint, and package-manager commands may execute project-controlled code".into()}}
fn workspace<'a>(state:&'a AppState,name:&str,write:bool)->Result<&'a WorkspaceConfig,ApiError>{let workspace=state.config.workspaces.get(name).ok_or_else(||ApiError::NotFound(format!("workspace {name:?} is not configured")))?;let allowed=if write{workspace.capabilities.fs_write}else{workspace.capabilities.fs_read};if allowed{Ok(workspace)}else{Err(ApiError::Forbidden(format!("workspace {name:?} does not allow filesystem {}",if write{"writes"}else{"reads"})))}}
fn canonical_root(workspace:&WorkspaceConfig)->Result<PathBuf,ApiError>{let root=fs::canonicalize(&workspace.root).map_err(map_io)?;if root.is_dir(){Ok(root)}else{Err(ApiError::BadRequest("workspace root is not a directory".into()))}}
fn existing_file(workspace:&WorkspaceConfig,requested:&str)->Result<(PathBuf,PathBuf),ApiError>{let root=canonical_root(workspace)?;let target=existing_from_root(&root,requested)?;if fs::metadata(&target).map_err(map_io)?.is_file(){Ok((root,target))}else{Err(ApiError::BadRequest("path is not a regular file".into()))}}
fn existing_from_root(root:&Path,requested:&str)->Result<PathBuf,ApiError>{let path=validate_relative(requested,false)?;let raw=root.join(path);reject_symlink(&raw)?;let target=fs::canonicalize(raw).map_err(map_io)?;ensure_within(root,&target)?;Ok(target)}
fn existing_path(root:&Path,requested:&str)->Result<PathBuf,ApiError>{let path=validate_relative(requested,true)?;let target=fs::canonicalize(root.join(path)).map_err(map_io)?;ensure_within(root,&target)?;Ok(target)}
fn validate_relative(requested:&str,allow_empty:bool)->Result<&Path,ApiError>{if requested.is_empty(){return if allow_empty{Ok(Path::new("."))}else{Err(ApiError::BadRequest("path must not be empty".into()))};}let path=Path::new(requested);if path.is_absolute(){return Err(ApiError::BadRequest("path must be relative to the workspace".into()));}for component in path.components(){if !matches!(component,Component::Normal(_)|Component::CurDir){return Err(ApiError::BadRequest("path traversal outside the workspace is not allowed".into()));}}Ok(path)}
fn ensure_within(root:&Path,target:&Path)->Result<(),ApiError>{if target.starts_with(root){Ok(())}else{Err(ApiError::Forbidden("resolved path escapes workspace root".into()))}}
fn reject_symlink(path:&Path)->Result<(),ApiError>{let metadata=fs::symlink_metadata(path).map_err(map_io)?;if metadata.file_type().is_symlink(){Err(ApiError::Forbidden("code tools do not mutate through symlinks".into()))}else{Ok(())}}
fn read_bounded(path:&Path)->Result<Vec<u8>,ApiError>{let metadata=fs::metadata(path).map_err(map_io)?;if metadata.len()>MAX_FILE_BYTES as u64{Err(ApiError::BadRequest(format!("file exceeds {MAX_FILE_BYTES} bytes")))}else{fs::read(path).map_err(map_io)}}
fn atomic_write(target:&Path,content:&[u8],permissions:fs::Permissions)->Result<(),ApiError>{let parent=target.parent().ok_or_else(||ApiError::BadRequest("target must have a parent directory".into()))?;let mut temp=NamedTempFile::new_in(parent).map_err(map_io)?;temp.write_all(content).map_err(map_io)?;temp.as_file().sync_all().map_err(map_io)?;temp.as_file().set_permissions(permissions).map_err(map_io)?;temp.persist(target).map_err(|error|map_io(error.error))?;Ok(())}
fn digest(bytes:&[u8])->String{format!("{:x}",Sha256::digest(bytes))}fn relative(root:&Path,path:&Path)->String{path.strip_prefix(root).unwrap_or(path).to_string_lossy().into_owned()}fn map_io(error:std::io::Error)->ApiError{match error.kind(){std::io::ErrorKind::NotFound=>ApiError::NotFound(error.to_string()),std::io::ErrorKind::PermissionDenied=>ApiError::Forbidden(error.to_string()),_=>ApiError::Internal(error.to_string())}}

#[cfg(test)]mod tests{use super::*;#[test]fn references_respect_identifier_boundaries(){assert_eq!(identifier_columns("foo foobar foo","foo"),vec![0,11]);}#[test]fn replace_lines_is_one_based(){assert_eq!(replace_lines("a\nb\nc\n",2,2,"B\n").unwrap(),"a\nB\nc\n");}#[test]fn exact_replace_refuses_ambiguity(){let edits=vec![CodeEdit::ReplaceExact{old:"x".into(),new:"y".into()}];assert!(apply_edits("x x".into(),&edits).is_err());}}

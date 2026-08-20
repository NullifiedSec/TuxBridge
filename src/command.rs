use std::{collections::HashMap, process::Stdio, sync::{Arc, atomic::{AtomicU64, Ordering}}, time::{Duration, SystemTime, UNIX_EPOCH}};

use axum::{Json, extract::{Path as AxumPath, State}};
use serde::{Deserialize, Serialize};
use tokio::{io::{AsyncRead, AsyncReadExt}, process::Command, sync::{Mutex, oneshot}, time};

use crate::{config::WorkspaceConfig, diagnostics::{StructuredDiagnostic, parse_command_diagnostics}, error::ApiError, events::EventHub, state::AppState};

#[derive(Clone)]
pub struct JobStore { jobs:Arc<Mutex<HashMap<String,JobRecord>>>, next_id:Arc<AtomicU64>, max_jobs:usize, retention_seconds:u64 }
impl JobStore { pub fn new(max_jobs:usize,retention_seconds:u64)->Self{Self{jobs:Arc::new(Mutex::new(HashMap::new())),next_id:Arc::new(AtomicU64::new(0)),max_jobs,retention_seconds}} }
struct JobRecord { snapshot:JobSnapshot, cancel:Option<oneshot::Sender<()>> }

#[derive(Debug, Clone, Deserialize)]
pub struct CommandRequest { workspace:String, argv:Vec<String>, timeout_seconds:Option<u64> }

#[derive(Debug, Clone, Serialize)]
pub struct CommandResult {
    argv:Vec<String>, exit_code:Option<i32>, stdout:String, stderr:String,
    stdout_truncated:bool, stderr_truncated:bool, timed_out:bool,
    diagnostics:Vec<StructuredDiagnostic>,
}

#[derive(Debug, Clone, Serialize)]
pub struct JobSnapshot {
    id:String, workspace:String, argv:Vec<String>, status:JobStatus,
    started_at_unix:u64, finished_at_unix:Option<u64>, exit_code:Option<i32>,
    stdout:String, stderr:String, stdout_truncated:bool, stderr_truncated:bool,
    diagnostics:Vec<StructuredDiagnostic>, error:Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all="snake_case")]
pub enum JobStatus { Running, Completed, Failed, TimedOut, Cancelled }

pub async fn run_command(State(state):State<AppState>,Json(request):Json<CommandRequest>)->Result<Json<CommandResult>,ApiError>{
    let workspace=command_workspace(&state,&request.workspace)?;validate_argv(&request.argv)?;let timeout=command_timeout(&state,request.timeout_seconds);let root=canonical_root(workspace)?;
    state.events.emit("command.started",Some(&request.workspace),format!("running {}",request.argv[0]),serde_json::json!({"argv":request.argv.clone()})).await;
    let result=execute(&root,request.argv,timeout,state.config.limits.command_output_bytes,None).await?;
    state.events.emit("command.finished",Some(&request.workspace),format!("command exited {:?}",result.exit_code),serde_json::json!({"exit_code":result.exit_code,"timed_out":result.timed_out,"diagnostics":result.diagnostics.len()})).await;
    Ok(Json(result))
}

pub async fn start_command(State(state):State<AppState>,Json(request):Json<CommandRequest>)->Result<Json<JobSnapshot>,ApiError>{
    let workspace=command_workspace(&state,&request.workspace)?;validate_argv(&request.argv)?;let timeout=command_timeout(&state,request.timeout_seconds);let output_limit=state.config.limits.command_output_bytes;let root=canonical_root(workspace)?;
    let id=state.jobs.next_id();let(cancel_tx,cancel_rx)=oneshot::channel();let snapshot=JobSnapshot{id:id.clone(),workspace:request.workspace.clone(),argv:request.argv.clone(),status:JobStatus::Running,started_at_unix:unix_now(),finished_at_unix:None,exit_code:None,stdout:String::new(),stderr:String::new(),stdout_truncated:false,stderr_truncated:false,diagnostics:Vec::new(),error:None};
    state.jobs.insert(id.clone(),JobRecord{snapshot:snapshot.clone(),cancel:Some(cancel_tx)}).await?;
    state.events.emit("job.started",Some(&request.workspace),format!("started {id}: {}",request.argv[0]),serde_json::json!({"job_id":id.clone(),"argv":request.argv.clone()})).await;
    let jobs=state.jobs.clone();let argv=request.argv;let events=state.events.clone();let workspace_name=request.workspace.clone();let job_id=id.clone();
    tokio::spawn(async move{let stream=StreamMeta{events:events.clone(),job_id:job_id.clone(),workspace:workspace_name.clone()};let completion=execute_inner(&root,argv,timeout,output_limit,Some(cancel_rx),Some(stream)).await;jobs.finish(&job_id,completion).await;if let Some(snapshot)=jobs.get(&job_id).await{events.emit("job.finished",Some(&workspace_name),format!("{} finished as {:?}",job_id,snapshot.status),serde_json::json!({"job_id":job_id,"status":snapshot.status,"exit_code":snapshot.exit_code,"diagnostics":snapshot.diagnostics.len()})).await;}});
    Ok(Json(snapshot))
}

pub async fn list_jobs(State(state):State<AppState>)->Json<Vec<JobSnapshot>>{Json(state.jobs.list().await)}
pub async fn get_job(State(state):State<AppState>,AxumPath(id):AxumPath<String>)->Result<Json<JobSnapshot>,ApiError>{state.jobs.get(&id).await.map(Json).ok_or_else(||ApiError::NotFound(format!("job {id:?} was not found")))}
pub async fn cancel_job(State(state):State<AppState>,AxumPath(id):AxumPath<String>)->Result<Json<JobSnapshot>,ApiError>{let snapshot=state.jobs.cancel(&id).await?;state.events.emit("job.cancel_requested",Some(&snapshot.workspace),format!("cancel requested for {id}"),serde_json::json!({"job_id":id})).await;Ok(Json(snapshot))}

impl JobStore {
    fn next_id(&self)->String{format!("job-{:016x}",self.next_id.fetch_add(1,Ordering::Relaxed)+1)}
    async fn insert(&self,id:String,record:JobRecord)->Result<(),ApiError>{let mut jobs=self.jobs.lock().await;self.prune_locked(&mut jobs);if jobs.len()>=self.max_jobs{return Err(ApiError::Conflict(format!("background job limit of {} has been reached",self.max_jobs)));}jobs.insert(id,record);Ok(())}
    async fn get(&self,id:&str)->Option<JobSnapshot>{let mut jobs=self.jobs.lock().await;self.prune_locked(&mut jobs);jobs.get(id).map(|r|r.snapshot.clone())}
    async fn list(&self)->Vec<JobSnapshot>{let mut jobs=self.jobs.lock().await;self.prune_locked(&mut jobs);let mut out=jobs.values().map(|r|r.snapshot.clone()).collect::<Vec<_>>();out.sort_by_key(|j|j.started_at_unix);out}
    async fn cancel(&self,id:&str)->Result<JobSnapshot,ApiError>{let mut jobs=self.jobs.lock().await;self.prune_locked(&mut jobs);let record=jobs.get_mut(id).ok_or_else(||ApiError::NotFound(format!("job {id:?} was not found")))?;if !matches!(record.snapshot.status,JobStatus::Running){return Err(ApiError::Conflict("job is not running".into()));}let sender=record.cancel.take().ok_or_else(||ApiError::Conflict("job cannot be cancelled".into()))?;let _=sender.send(());Ok(record.snapshot.clone())}
    async fn finish(&self,id:&str,completion:Result<CommandResult,ExecuteError>){let mut jobs=self.jobs.lock().await;let Some(record)=jobs.get_mut(id)else{return;};record.cancel=None;record.snapshot.finished_at_unix=Some(unix_now());match completion{Ok(result)=>{record.snapshot.exit_code=result.exit_code;record.snapshot.stdout=result.stdout;record.snapshot.stderr=result.stderr;record.snapshot.stdout_truncated=result.stdout_truncated;record.snapshot.stderr_truncated=result.stderr_truncated;record.snapshot.diagnostics=result.diagnostics;record.snapshot.status=if result.timed_out{JobStatus::TimedOut}else if result.exit_code==Some(0){JobStatus::Completed}else{JobStatus::Failed};}Err(ExecuteError::Cancelled{stdout,stderr,stdout_truncated,stderr_truncated,argv})=>{record.snapshot.stdout=stdout;record.snapshot.stderr=stderr;record.snapshot.stdout_truncated=stdout_truncated;record.snapshot.stderr_truncated=stderr_truncated;record.snapshot.diagnostics=parse_command_diagnostics(&argv,&record.snapshot.stdout,&record.snapshot.stderr);record.snapshot.status=JobStatus::Cancelled;}Err(ExecuteError::Failed(message))=>{record.snapshot.error=Some(message);record.snapshot.status=JobStatus::Failed;}}}
    fn prune_locked(&self,jobs:&mut HashMap<String,JobRecord>){if self.retention_seconds==0{return;}let cutoff=unix_now().saturating_sub(self.retention_seconds);jobs.retain(|_,r|matches!(r.snapshot.status,JobStatus::Running)||r.snapshot.finished_at_unix.is_none_or(|f|f>=cutoff));}
}

#[derive(Debug)]
enum ExecuteError{Cancelled{stdout:String,stderr:String,stdout_truncated:bool,stderr_truncated:bool,argv:Vec<String>},Failed(String)}
#[derive(Clone)]
struct StreamMeta { events:EventHub, job_id:String, workspace:String }

fn command_timeout(state:&AppState,requested:Option<u64>)->u64{requested.unwrap_or(state.config.limits.command_timeout_seconds).clamp(1,state.config.limits.max_command_timeout_seconds)}
async fn execute(root:&std::path::Path,argv:Vec<String>,timeout_seconds:u64,output_limit:usize,stream:Option<StreamMeta>)->Result<CommandResult,ApiError>{execute_inner(root,argv,timeout_seconds,output_limit,None,stream).await.map_err(|e|match e{ExecuteError::Cancelled{..}=>ApiError::Conflict("command was cancelled".into()),ExecuteError::Failed(m)=>ApiError::Internal(m)})}

async fn execute_inner(root:&std::path::Path,argv:Vec<String>,timeout_seconds:u64,output_limit:usize,cancel:Option<oneshot::Receiver<()>>,stream:Option<StreamMeta>)->Result<CommandResult,ExecuteError>{
    let mut command=Command::new(&argv[0]);command.args(&argv[1..]).current_dir(root).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    let mut child=command.spawn().map_err(|e|ExecuteError::Failed(format!("failed to start command: {e}")))?;let stdout=child.stdout.take().ok_or_else(||ExecuteError::Failed("failed to capture stdout".into()))?;let stderr=child.stderr.take().ok_or_else(||ExecuteError::Failed("failed to capture stderr".into()))?;
    let stdout_task=tokio::spawn(drain_limited(stdout,output_limit,stream.clone(),"stdout"));let stderr_task=tokio::spawn(drain_limited(stderr,output_limit,stream.clone(),"stderr"));
    enum EndState{Exited(std::process::ExitStatus),TimedOut,Cancelled}
    let end_state=if let Some(mut cancel)=cancel{tokio::select!{status=child.wait()=>EndState::Exited(status.map_err(|e|ExecuteError::Failed(format!("failed waiting for command: {e}")))?),_=time::sleep(Duration::from_secs(timeout_seconds))=>EndState::TimedOut,_=&mut cancel=>EndState::Cancelled}}else{tokio::select!{status=child.wait()=>EndState::Exited(status.map_err(|e|ExecuteError::Failed(format!("failed waiting for command: {e}")))?),_=time::sleep(Duration::from_secs(timeout_seconds))=>EndState::TimedOut}};
    if matches!(end_state,EndState::TimedOut|EndState::Cancelled){let _=child.kill().await;let _=child.wait().await;}
    let(stdout,stdout_truncated)=stdout_task.await.map_err(|e|ExecuteError::Failed(format!("stdout reader task failed: {e}")))?.map_err(|e|ExecuteError::Failed(format!("failed reading stdout: {e}")))?;let(stderr,stderr_truncated)=stderr_task.await.map_err(|e|ExecuteError::Failed(format!("stderr reader task failed: {e}")))?.map_err(|e|ExecuteError::Failed(format!("failed reading stderr: {e}")))?;
    let stdout=String::from_utf8_lossy(&stdout).into_owned();let stderr=String::from_utf8_lossy(&stderr).into_owned();let diagnostics=parse_command_diagnostics(&argv,&stdout,&stderr);
    match end_state{EndState::Exited(status)=>Ok(CommandResult{argv,exit_code:status.code(),stdout,stderr,stdout_truncated,stderr_truncated,timed_out:false,diagnostics}),EndState::TimedOut=>Ok(CommandResult{argv,exit_code:None,stdout,stderr,stdout_truncated,stderr_truncated,timed_out:true,diagnostics}),EndState::Cancelled=>Err(ExecuteError::Cancelled{stdout,stderr,stdout_truncated,stderr_truncated,argv})}
}

async fn drain_limited<R>(mut reader:R,limit:usize,stream:Option<StreamMeta>,channel:&'static str)->std::io::Result<(Vec<u8>,bool)> where R:AsyncRead+Unpin{
    let mut output=Vec::new();let mut truncated=false;let mut buffer=[0u8;8192];loop{let read=reader.read(&mut buffer).await?;if read==0{break;}if let Some(meta)=&stream{let chunk=String::from_utf8_lossy(&buffer[..read]).into_owned();meta.events.emit(format!("job.{channel}"),Some(&meta.workspace),format!("{} {channel}",meta.job_id),serde_json::json!({"job_id":meta.job_id.clone(),"chunk":chunk})).await;}if output.len()<limit{let remaining=limit-output.len();output.extend_from_slice(&buffer[..read.min(remaining)]);if read>remaining{truncated=true;}}else{truncated=true;}}Ok((output,truncated))
}

fn command_workspace<'a>(state:&'a AppState,name:&str)->Result<&'a WorkspaceConfig,ApiError>{let ws=state.config.workspaces.get(name).ok_or_else(||ApiError::NotFound(format!("workspace {name:?} is not configured")))?;if !ws.capabilities.commands{return Err(ApiError::Forbidden(format!("workspace {name:?} does not allow command execution")));}Ok(ws)}
fn validate_argv(argv:&[String])->Result<(),ApiError>{if argv.is_empty()||argv[0].trim().is_empty(){return Err(ApiError::BadRequest("argv must contain a program name".into()));}if argv.len()>256{return Err(ApiError::BadRequest("argv contains too many arguments".into()));}if argv.iter().any(|a|a.as_bytes().contains(&0)){return Err(ApiError::BadRequest("argv must not contain NUL bytes".into()));}Ok(())}
fn canonical_root(ws:&WorkspaceConfig)->Result<std::path::PathBuf,ApiError>{std::fs::canonicalize(&ws.root).map_err(|e|ApiError::Internal(format!("failed to resolve workspace root: {e}")))}
fn unix_now()->u64{SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()}

#[cfg(test)]mod tests{use super::*;#[test]fn rejects_empty_argv(){assert!(validate_argv(&[]).is_err());}#[test]fn rejects_blank_program(){assert!(validate_argv(&["   ".into()]).is_err());}}

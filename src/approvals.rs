use std::{collections::HashMap, sync::{Arc, atomic::{AtomicU64, Ordering}}, time::{SystemTime, UNIX_EPOCH}};

use axum::{extract::{Path as AxumPath, State}, Json};
use serde::Serialize;
use tokio::sync::Mutex;

use crate::{error::ApiError, state::AppState};

#[derive(Clone, Default)]
pub struct ApprovalStore {
    inner: Arc<Mutex<HashMap<String, ApprovalRecord>>>,
    next: Arc<AtomicU64>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all="snake_case")]
pub enum ApprovalStatus { Pending, Approved, Denied, Consumed }

#[derive(Debug, Clone, Serialize)]
pub struct ApprovalRecord {
    pub id: String,
    pub path: String,
    pub status: ApprovalStatus,
    pub created_at_unix_ms: u128,
}

impl ApprovalStore {
    pub async fn create(&self, path:&str)->ApprovalRecord{
        let id=format!("approval-{:016x}",self.next.fetch_add(1,Ordering::Relaxed)+1);
        let record=ApprovalRecord{id:id.clone(),path:path.into(),status:ApprovalStatus::Pending,created_at_unix_ms:now_ms()};
        self.inner.lock().await.insert(id,record.clone());record
    }
    pub async fn consume(&self,id:&str,path:&str)->bool{
        let mut items=self.inner.lock().await;let Some(item)=items.get_mut(id)else{return false;};if item.path==path&&item.status==ApprovalStatus::Approved{item.status=ApprovalStatus::Consumed;true}else{false}
    }
    async fn set(&self,id:&str,status:ApprovalStatus)->Result<ApprovalRecord,ApiError>{let mut items=self.inner.lock().await;let item=items.get_mut(id).ok_or_else(||ApiError::NotFound(format!("approval {id:?} not found")))?;if item.status!=ApprovalStatus::Pending{return Err(ApiError::Conflict("approval is no longer pending".into()));}item.status=status;Ok(item.clone())}
    async fn list(&self)->Vec<ApprovalRecord>{let mut values=self.inner.lock().await.values().cloned().collect::<Vec<_>>();values.sort_by_key(|v|v.created_at_unix_ms);values}
}

pub async fn list_approvals(State(state):State<AppState>)->Json<Vec<ApprovalRecord>>{Json(state.approvals.list().await)}
pub async fn approve(State(state):State<AppState>,AxumPath(id):AxumPath<String>)->Result<Json<ApprovalRecord>,ApiError>{let r=state.approvals.set(&id,ApprovalStatus::Approved).await?;state.events.emit("approval.approved",None,format!("approved {}",r.path),serde_json::json!({"approval_id":r.id.clone(),"path":r.path.clone()})).await;Ok(Json(r))}
pub async fn deny(State(state):State<AppState>,AxumPath(id):AxumPath<String>)->Result<Json<ApprovalRecord>,ApiError>{let r=state.approvals.set(&id,ApprovalStatus::Denied).await?;state.events.emit("approval.denied",None,format!("denied {}",r.path),serde_json::json!({"approval_id":r.id.clone(),"path":r.path.clone()})).await;Ok(Json(r))}

fn now_ms()->u128{SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis()}

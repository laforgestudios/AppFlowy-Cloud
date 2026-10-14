//! Atomic creation and batch full-sync routes of the current web client.
//!
//! Registered on the App with full paths, before `workspace_scope`, so that these literal paths
//! never fall into a generic `/{workspace_id}/...` pattern.

use crate::api::util::realtime_user_for_web_request;
use crate::biz::authentication::jwt::UserUuid;
use crate::biz::laforge_atomic::full_sync::collab_full_sync_batch;
use crate::state::AppState;
use actix_web::web::{Bytes, Data, PayloadConfig};
use actix_web::{web, HttpRequest, HttpResponse, Result};
use app_error::AppError;
use collab::core::origin::{CollabClient, CollabOrigin};
use collab_rt_entity::collab_proto::CollabBatchSyncRequest;
use prost::Message;
use uuid::Uuid;

/// Batch payloads can carry whole documents (slow-sync lane).
const FULL_SYNC_BATCH_MAX_BODY: usize = 64 * 1024 * 1024;

pub fn laforge_atomic_routes(cfg: &mut web::ServiceConfig) {
  cfg.service(
    web::resource("/api/workspace/v1/{workspace_id}/collab/full-sync/batch")
      .app_data(PayloadConfig::new(FULL_SYNC_BATCH_MAX_BODY))
      .route(web::post().to(collab_full_sync_batch_handler)),
  );
}

/// Protobuf in, raw protobuf out (HTTP 200, no JSON envelope). Per-item failures are reported in
/// each result's `error`.
async fn collab_full_sync_batch_handler(
  user_uuid: UserUuid,
  path: web::Path<Uuid>,
  body: Bytes,
  state: Data<AppState>,
  req: HttpRequest,
) -> Result<HttpResponse> {
  let workspace_id = path.into_inner();
  let request = CollabBatchSyncRequest::decode(body.as_ref()).map_err(|err| {
    AppError::InvalidRequest(format!("Failed to parse CollabBatchSyncRequest: {}", err))
  })?;
  let uid = state.user_cache.get_user_uid(&user_uuid).await?;
  let user = realtime_user_for_web_request(req.headers(), uid)?;
  let origin = CollabOrigin::Client(CollabClient::new(uid, user.device_id));
  let response = collab_full_sync_batch(&state, uid, origin, workspace_id, request).await?;
  Ok(
    HttpResponse::Ok()
      .content_type("application/octet-stream")
      .body(response.encode_to_vec()),
  )
}

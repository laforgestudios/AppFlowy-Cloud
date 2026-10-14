//! Atomic creation and batch full-sync routes of the current web client.
//!
//! Registered on the App with full paths, before `workspace_scope`, so that these literal paths
//! never fall into a generic `/{workspace_id}/...` pattern.

use crate::api::util::realtime_user_for_web_request;
use crate::biz::authentication::jwt::UserUuid;
use crate::biz::laforge_atomic::database_page::create_default_database_page;
use crate::biz::laforge_atomic::database_view::create_default_database_view;
use crate::biz::laforge_atomic::document::create_default_document_page;
use crate::biz::laforge_atomic::full_sync::collab_full_sync_batch;
use crate::biz::laforge_atomic::row_document::create_row_document;
use crate::state::AppState;
use actix_web::web::{Bytes, Data, Json, PayloadConfig};
use actix_web::{web, HttpRequest, HttpResponse, Result};
use app_error::AppError;
use collab::core::origin::{CollabClient, CollabOrigin};
use collab_rt_entity::collab_proto::CollabBatchSyncRequest;
use prost::Message;
use shared_entity::response::AppResponse;
use uuid::Uuid;

/// Batch payloads can carry whole documents (slow-sync lane).
const FULL_SYNC_BATCH_MAX_BODY: usize = 64 * 1024 * 1024;
/// Creation requests are compact recipes.
const ATOMIC_CREATION_MAX_BODY: usize = 1024 * 1024;

pub fn laforge_atomic_routes(cfg: &mut web::ServiceConfig) {
  cfg
    .service(
      web::resource("/api/workspace/v1/{workspace_id}/collab/full-sync/batch")
        .app_data(PayloadConfig::new(FULL_SYNC_BATCH_MAX_BODY))
        .route(web::post().to(collab_full_sync_batch_handler)),
    )
    .service(
      web::resource("/api/workspace/{workspace_id}/v2/page-view/document/default")
        .app_data(PayloadConfig::new(ATOMIC_CREATION_MAX_BODY))
        .route(web::post().to(create_default_document_handler)),
    )
    .service(
      web::resource("/api/workspace/{workspace_id}/v2/page-view/database/default")
        .app_data(PayloadConfig::new(ATOMIC_CREATION_MAX_BODY))
        .route(web::post().to(create_default_database_handler)),
    )
    .service(
      web::resource("/api/workspace/{workspace_id}/v2/page-view/database-view/default")
        .app_data(PayloadConfig::new(ATOMIC_CREATION_MAX_BODY))
        .route(web::post().to(create_default_database_view_handler)),
    )
    .service(
      web::resource("/api/workspace/{workspace_id}/v2/page-view/row-document")
        .app_data(PayloadConfig::new(ATOMIC_CREATION_MAX_BODY))
        .route(web::post().to(create_row_document_handler)),
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

/// The body is hashed as received: the web retries the byte-identical JSON string.
async fn create_default_document_handler(
  user_uuid: UserUuid,
  path: web::Path<Uuid>,
  body: Bytes,
  state: Data<AppState>,
  req: HttpRequest,
) -> Result<Json<AppResponse<serde_json::Value>>> {
  let uid = state.user_cache.get_user_uid(&user_uuid).await?;
  let user = realtime_user_for_web_request(req.headers(), uid)?;
  let receipt = create_default_document_page(&state, user, path.into_inner(), &body).await?;
  Ok(Json(AppResponse::Ok().with_data(receipt)))
}

async fn create_default_database_handler(
  user_uuid: UserUuid,
  path: web::Path<Uuid>,
  body: Bytes,
  state: Data<AppState>,
  req: HttpRequest,
) -> Result<Json<AppResponse<serde_json::Value>>> {
  let uid = state.user_cache.get_user_uid(&user_uuid).await?;
  let user = realtime_user_for_web_request(req.headers(), uid)?;
  let receipt = create_default_database_page(&state, user, path.into_inner(), &body).await?;
  Ok(Json(AppResponse::Ok().with_data(receipt)))
}

async fn create_default_database_view_handler(
  user_uuid: UserUuid,
  path: web::Path<Uuid>,
  body: Bytes,
  state: Data<AppState>,
  req: HttpRequest,
) -> Result<Json<AppResponse<serde_json::Value>>> {
  let uid = state.user_cache.get_user_uid(&user_uuid).await?;
  let user = realtime_user_for_web_request(req.headers(), uid)?;
  let receipt = create_default_database_view(&state, user, path.into_inner(), &body).await?;
  Ok(Json(AppResponse::Ok().with_data(receipt)))
}

async fn create_row_document_handler(
  user_uuid: UserUuid,
  path: web::Path<Uuid>,
  body: Bytes,
  state: Data<AppState>,
  req: HttpRequest,
) -> Result<Json<AppResponse<serde_json::Value>>> {
  let uid = state.user_cache.get_user_uid(&user_uuid).await?;
  let user = realtime_user_for_web_request(req.headers(), uid)?;
  let receipt = create_row_document(&state, user, path.into_inner(), &body).await?;
  Ok(Json(AppResponse::Ok().with_data(receipt)))
}

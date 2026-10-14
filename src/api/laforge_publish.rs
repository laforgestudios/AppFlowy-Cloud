use actix_web::web::{self, Data, Json};
use actix_web::{Resource, Result};
use anyhow::anyhow;
use app_error::AppError;
use shared_entity::response::{AppResponse, JsonAppResponse};

use crate::biz::publish_snapshot::build_published_snapshot;
use crate::state::AppState;

/// Registered at the app root, ahead of `workspace_scope()`, as a single resource so it
/// shadows nothing else under `/api/workspace`.
pub fn published_snapshot_resource() -> Resource {
  web::resource("/api/workspace/v2/published/{publish_namespace}/{publish_name}/snapshot")
    .route(web::get().to(get_published_snapshot_handler))
}

/// Anonymous: serves only what was published.
async fn get_published_snapshot_handler(
  path_param: web::Path<(String, String)>,
  state: Data<AppState>,
) -> Result<JsonAppResponse<serde_json::Value>> {
  let (publish_namespace, publish_name) = path_param.into_inner();
  // Fails with RecordNotFound for an unknown or unpublished page.
  let metadata = state
    .published_collab_store
    .get_collab_metadata(&publish_namespace, &publish_name)
    .await?;
  let blob = state
    .published_collab_store
    .get_collab_blob_by_publish_namespace(&publish_namespace, &publish_name)
    .await?;
  let snapshot = tokio::task::spawn_blocking(move || {
    build_published_snapshot(&publish_namespace, &publish_name, &metadata, &blob)
  })
  .await
  .map_err(|err| AppError::Internal(anyhow!("build published snapshot: {err}")))??;
  Ok(Json(AppResponse::Ok().with_data(snapshot)))
}

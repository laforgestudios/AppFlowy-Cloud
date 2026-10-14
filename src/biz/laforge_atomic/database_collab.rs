//! Database collab helpers shared by the database page and database view creations.

use crate::biz::collab::ops::get_latest_workspace_database;
use crate::biz::collab::utils::get_latest_collab_database_body;
use crate::biz::workspace::page_view::update_workspace_database_data;
use crate::state::AppState;
use anyhow::anyhow;
use app_error::AppError;
use collab::core::collab::{default_client_id, Collab, CollabOptions, DataSource};
use collab::core::origin::CollabOrigin;
use collab::entity::EncoderVersion;
use collab_database::database::DatabaseBody;
use collab_database::database_trait::NoPersistenceDatabaseCollabService;
use collab_entity::CollabType;
use collab_rt_entity::user::RealtimeUser;
use database::collab::GetCollabOrigin;
use database_entity::dto::QueryCollab;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

const READABLE_ATTEMPTS: usize = 20;
const READABLE_INTERVAL: Duration = Duration::from_millis(50);

/// The latest database collab (snapshot and update stream) with its body.
pub async fn load_database(
  state: &AppState,
  workspace_id: Uuid,
  database_id: Uuid,
) -> Result<(Collab, DatabaseBody), AppError> {
  get_latest_collab_database_body(&state.collab_storage, workspace_id, database_id).await
}

/// Ids of the database views stored in a database doc state, `None` when it holds no database
/// with this id.
pub fn database_view_ids(database_id: &Uuid, doc_state: Vec<u8>) -> Option<Vec<String>> {
  let options = CollabOptions::new(database_id.to_string(), default_client_id())
    .with_data_source(DataSource::DocStateV1(doc_state));
  let collab = Collab::new_with_options(CollabOrigin::Server, options).ok()?;
  let body = DatabaseBody::from_collab(
    &collab,
    Arc::new(NoPersistenceDatabaseCollabService::new(default_client_id())),
    None,
  )?;
  let txn = collab.transact();
  if body.get_database_id(&txn) != database_id.to_string() {
    return None;
  }
  Some(
    body
      .views
      .get_all_views(&txn)
      .into_iter()
      .map(|view| view.id)
      .collect(),
  )
}

/// Read-your-writes: waits until the database read by full-sync holds every view, else answers
/// RetryLater.
pub async fn await_database_views(
  state: &AppState,
  workspace_id: Uuid,
  database_id: Uuid,
  view_ids: &[Uuid],
) -> Result<(), AppError> {
  for attempt in 0..READABLE_ATTEMPTS {
    match state
      .collab_cache
      .get_full_collab(
        &workspace_id,
        QueryCollab::new(database_id, CollabType::Database),
        None,
        EncoderVersion::V1,
      )
      .await
    {
      Ok(collab) => {
        let doc_state = collab.encoded_collab.doc_state.to_vec();
        let stored =
          tokio::task::spawn_blocking(move || database_view_ids(&database_id, doc_state)).await?;
        if stored.is_some_and(|stored| {
          view_ids
            .iter()
            .all(|view_id| stored.contains(&view_id.to_string()))
        }) {
          return Ok(());
        }
      },
      Err(AppError::RecordNotFound(_)) => {},
      Err(err) => return Err(err),
    }
    if attempt + 1 < READABLE_ATTEMPTS {
      tokio::time::sleep(READABLE_INTERVAL).await;
    }
  }
  Err(AppError::RetryLater(anyhow!(
    "the views of database {} are not readable yet",
    database_id
  )))
}

/// Registers the database and links its views in the WorkspaceDatabase (what the legacy lookups
/// by view id read). Already linked views are left as is.
pub async fn register_database_views(
  state: &AppState,
  user: RealtimeUser,
  workspace_id: Uuid,
  database_id: Uuid,
  view_ids: &[Uuid],
) -> Result<(), AppError> {
  let (workspace_database_id, mut workspace_database) = get_latest_workspace_database(
    &state.collab_storage,
    &state.pg_pool,
    GetCollabOrigin::User { uid: user.uid },
    workspace_id,
  )
  .await?;
  let database_id_str = database_id.to_string();
  let view_ids: Vec<String> = view_ids.iter().map(|id| id.to_string()).collect();
  let update = match workspace_database.get_database_meta(&database_id_str) {
    Some(meta) => {
      let missing: Vec<String> = view_ids
        .into_iter()
        .filter(|id| !meta.linked_views.contains(id))
        .collect();
      if missing.is_empty() {
        return Ok(());
      }
      let txn = workspace_database.update_database(&database_id_str, |record| {
        for id in &missing {
          if !record.linked_views.contains(id) {
            record.linked_views.push(id.clone());
          }
        }
      });
      txn.encode_update_v1()
    },
    None => {
      let txn = workspace_database.add_database(&database_id_str, view_ids);
      txn.encode_update_v1()
    },
  };
  update_workspace_database_data(
    &state.metrics.appflowy_web_metrics,
    &state.ws_server,
    user,
    workspace_id,
    workspace_database_id,
    update,
  )
  .await
}

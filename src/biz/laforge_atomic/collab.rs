use crate::state::AppState;
use anyhow::anyhow;
use app_error::AppError;
use collab::core::collab::default_client_id;
use collab::entity::EncoderVersion;
use collab_document::document::Document;
use collab_document::document_data::default_document_data;
use collab_entity::CollabType;
use database_entity::dto::{CollabParams, QueryCollab};
use std::time::Duration;
use uuid::Uuid;

const READABLE_ATTEMPTS: usize = 20;
const READABLE_INTERVAL: Duration = Duration::from_millis(50);

/// Same mapping as `database::collab::partition_key_from_collab_type` (crate private).
fn partition_key(collab_type: CollabType) -> i32 {
  match collab_type {
    CollabType::Document => 0,
    CollabType::Database => 1,
    CollabType::WorkspaceDatabase => 2,
    CollabType::Folder => 3,
    CollabType::DatabaseRow => 4,
    CollabType::UserAwareness => 5,
    CollabType::Unknown => 0,
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CollabPresence {
  Absent,
  /// Already stored for this workspace with this type (left by an interrupted attempt of the
  /// same request, or created by another client): it must not be overwritten.
  Present,
}

/// Whether a client-chosen collab id is free in this workspace. An id stored in another
/// workspace, with another type, or soft-deleted is a conflict (`RecordAlreadyExists`).
/// A collab only present in the workspace update stream (never snapshotted) counts as present.
pub async fn collab_presence(
  state: &AppState,
  workspace_id: Uuid,
  object_id: Uuid,
  collab_type: CollabType,
) -> Result<CollabPresence, AppError> {
  let row: Option<(Uuid, i32, bool)> = sqlx::query_as(
    r#"
      SELECT workspace_id, partition_key, deleted_at IS NOT NULL
      FROM af_collab
      WHERE oid = $1
    "#,
  )
  .bind(object_id)
  .fetch_optional(&state.pg_pool)
  .await?;

  match row {
    Some((owner_workspace, key, deleted))
      if owner_workspace == workspace_id && key == partition_key(collab_type) && !deleted =>
    {
      Ok(CollabPresence::Present)
    },
    Some(_) => Err(AppError::RecordAlreadyExists(format!(
      "object {} already exists",
      object_id
    ))),
    None => {
      // Stream-only collabs: the snapshot table has no row yet.
      match state
        .collab_cache
        .get_workspace_updates(&workspace_id, Some(&object_id), None, None)
        .await
      {
        Ok(updates) if !updates.is_empty() => Ok(CollabPresence::Present),
        Ok(_) => Ok(CollabPresence::Absent),
        Err(err) => Err(err),
      }
    },
  }
}

/// Persists a new collab in its own committed transaction, so that nothing (Folder view,
/// receipt) can reference it before it is durable.
pub async fn insert_new_collab(
  state: &AppState,
  uid: i64,
  workspace_id: Uuid,
  params: CollabParams,
) -> Result<(), AppError> {
  let action = format!("Atomic creation of {}", params.object_id);
  let mut transaction = state.pg_pool.begin().await?;
  state
    .collab_storage
    .upsert_new_collab_with_transaction(workspace_id, &uid, params, &mut transaction, &action)
    .await?;
  transaction.commit().await?;
  Ok(())
}

/// The default document: a `page` root block (id derived from the document id, distinct from it)
/// holding one empty paragraph, with complete `children_map` and `text_map`.
pub async fn default_document_collab_params(document_id: Uuid) -> Result<CollabParams, AppError> {
  tokio::task::spawn_blocking(move || {
    let object_id = document_id.to_string();
    let document = Document::create(
      &object_id,
      default_document_data(&object_id),
      default_client_id(),
    )
    .map_err(|err| AppError::Internal(anyhow!("Failed to create default document: {}", err)))?;
    let encoded_collab_v1 = document
      .encode_collab()
      .map_err(|err| AppError::Internal(anyhow!("Failed to encode default document: {}", err)))?
      .encode_to_bytes()?;
    Ok::<_, AppError>(CollabParams {
      object_id: document_id,
      encoded_collab_v1: encoded_collab_v1.into(),
      collab_type: CollabType::Document,
      updated_at: None,
    })
  })
  .await?
}

/// Creates the default document unless the id is already present (see [collab_presence]).
pub async fn ensure_default_document(
  state: &AppState,
  uid: i64,
  workspace_id: Uuid,
  document_id: Uuid,
) -> Result<CollabPresence, AppError> {
  let presence = collab_presence(state, workspace_id, document_id, CollabType::Document).await?;
  if presence == CollabPresence::Absent {
    let params = default_document_collab_params(document_id).await?;
    insert_new_collab(state, uid, workspace_id, params).await?;
  }
  Ok(presence)
}

/// Waits until the collab reads back through the same path as full-sync and collab reads,
/// else answers RetryLater.
pub async fn await_collab_readable(
  state: &AppState,
  workspace_id: Uuid,
  object_id: Uuid,
  collab_type: CollabType,
) -> Result<(), AppError> {
  for attempt in 0..READABLE_ATTEMPTS {
    match state
      .collab_cache
      .get_full_collab(
        &workspace_id,
        QueryCollab::new(object_id, collab_type),
        None,
        EncoderVersion::V1,
      )
      .await
    {
      Ok(collab) if !collab.encoded_collab.doc_state.is_empty() => return Ok(()),
      Ok(_) | Err(AppError::RecordNotFound(_)) => {},
      Err(err) => return Err(err),
    }
    if attempt + 1 < READABLE_ATTEMPTS {
      tokio::time::sleep(READABLE_INTERVAL).await;
    }
  }
  Err(AppError::RetryLater(anyhow!(
    "created object {} is not readable yet",
    object_id
  )))
}

#[cfg(test)]
mod tests {
  use super::*;
  use collab::core::collab::{Collab, CollabOptions};
  use collab::core::origin::CollabOrigin;
  use collab::entity::EncodedCollab;

  #[tokio::test]
  async fn default_document_has_a_page_root_distinct_from_the_document_id() {
    let document_id = Uuid::new_v4();
    let params = default_document_collab_params(document_id).await.unwrap();
    assert_eq!(params.object_id, document_id);
    assert_eq!(params.collab_type, CollabType::Document);

    let encoded = EncodedCollab::decode_from_bytes(&params.encoded_collab_v1).unwrap();
    let options = CollabOptions::new(document_id.to_string(), default_client_id())
      .with_data_source(encoded.into());
    let collab = Collab::new_with_options(CollabOrigin::Empty, options).unwrap();
    let document = Document::open(collab).unwrap();
    let data = document.get_document_data().unwrap();
    assert_ne!(data.page_id, document_id.to_string());
    let root = &data.blocks[&data.page_id];
    assert_eq!(root.ty, "page");
    assert_eq!(root.parent, "");
    let children = &data.meta.children_map[&root.children];
    assert_eq!(children.len(), 1);
    let paragraph = &data.blocks[&children[0]];
    assert_eq!(paragraph.parent, data.page_id);
    let text_id = paragraph.external_id.clone().unwrap();
    assert!(data.meta.text_map.as_ref().unwrap().contains_key(&text_id));
    assert!(data.meta.children_map.contains_key(&paragraph.children));
  }

  #[test]
  fn partition_keys_match_storage() {
    assert_eq!(partition_key(CollabType::Document), 0);
    assert_eq!(partition_key(CollabType::Database), 1);
    assert_eq!(partition_key(CollabType::DatabaseRow), 4);
  }
}

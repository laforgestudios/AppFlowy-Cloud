//! `POST /api/workspace/{workspace_id}/v2/page-view/row-document`
//!
//! Creates the default (blank) document of an existing database row. The row is not mutated:
//! its document id is derived from the row id, and an absent `is_document_empty` already means
//! empty.

use super::collab::{await_collab_readable, ensure_default_document};
use super::receipt::{AtomicCreation, AtomicCreationKind, AtomicCreationStart};
use crate::state::AppState;
use access_control::act::Action;
use anyhow::anyhow;
use app_error::AppError;
use collab::core::collab::{default_client_id, Collab, CollabOptions};
use collab::core::origin::CollabOrigin;
use collab::entity::EncoderVersion;
use collab_database::rows::{meta_id_from_row_id, row_from_map_ref, RowMetaKey};
use collab_entity::define::DATABASE_ROW_DATA;
use collab_entity::CollabType;
use collab_rt_entity::user::RealtimeUser;
use database_entity::dto::QueryCollab;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;
use yrs::{Map, Out};

/// Web `PublishRowDocumentRequest`.
#[derive(Debug, Deserialize)]
pub struct PublishRowDocumentRequest {
  pub request_id: Uuid,
  pub database_id: Uuid,
  pub database_restore_id: String,
  pub row_id: Uuid,
  pub source_view_id: String,
  pub name: String,
  pub contribution: RowDocumentContribution,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RowDocumentContribution {
  DefaultSchema {
    schema_version: String,
  },
  /// Authored first-body actions: not supported by this server.
  DefaultActions {
    schema_version: String,
    #[serde(default)]
    actions: Value,
  },
}

/// Web `PublishRowDocumentResult`. This server has no collab versions.
#[derive(Debug, Serialize)]
pub struct PublishRowDocumentResult {
  pub document_id: Uuid,
  pub row_id: Uuid,
  pub database_id: Uuid,
  pub database_restore_id: String,
  pub row_version: Option<String>,
  pub document_version: Option<String>,
  pub update_batch_id: Uuid,
}

pub fn parse_request(body: &[u8]) -> Result<PublishRowDocumentRequest, AppError> {
  let request: PublishRowDocumentRequest = serde_json::from_slice(body)
    .map_err(|err| AppError::InvalidRequest(format!("invalid row document request: {}", err)))?;
  match &request.contribution {
    RowDocumentContribution::DefaultSchema { schema_version } if schema_version == "v1" => {
      Ok(request)
    },
    RowDocumentContribution::DefaultSchema { schema_version } => Err(AppError::InvalidRequest(
      format!("unsupported schema_version {}", schema_version),
    )),
    RowDocumentContribution::DefaultActions { .. } => Err(AppError::InvalidRequest(
      "default_actions contributions are not supported; use default_schema".to_string(),
    )),
  }
}

/// The document id the web derives from the row id (`RowMetaKey::DocumentId`).
pub fn row_document_id(row_id: &Uuid) -> Result<Uuid, AppError> {
  Uuid::parse_str(&meta_id_from_row_id(row_id, RowMetaKey::DocumentId))
    .map_err(|err| AppError::Internal(anyhow!("invalid row document id: {}", err)))
}

/// `(row id, database id)` stored in a row collab, if it holds a row.
pub fn row_identity(row_id: &Uuid, doc_state: Vec<u8>) -> Option<(String, String)> {
  let options = CollabOptions::new(row_id.to_string(), default_client_id())
    .with_data_source(collab::core::collab::DataSource::DocStateV1(doc_state));
  let collab = Collab::new_with_options(CollabOrigin::Server, options).ok()?;
  let txn = collab.transact();
  let row = match collab.data.get(&txn, DATABASE_ROW_DATA) {
    Some(Out::YMap(map)) => row_from_map_ref(&map, &txn)?,
    _ => return None,
  };
  Some((row.id.to_string(), row.database_id))
}

/// A row readable through the collab cache (snapshot or update stream) that belongs to the
/// database. A row that is not readable yet answers RetryLater: a newly added row may still be
/// syncing from its client.
async fn ensure_row_readable(
  state: &AppState,
  workspace_id: Uuid,
  database_id: Uuid,
  row_id: Uuid,
) -> Result<(), AppError> {
  let not_ready = || AppError::RetryLater(anyhow!("row {} is not readable yet", row_id));
  let row = match state
    .collab_cache
    .get_full_collab(
      &workspace_id,
      QueryCollab::new(row_id, CollabType::DatabaseRow),
      None,
      EncoderVersion::V1,
    )
    .await
  {
    Ok(row) => row,
    Err(AppError::RecordNotFound(_)) => return Err(not_ready()),
    Err(err) => return Err(err),
  };
  let doc_state = row.encoded_collab.doc_state.to_vec();
  let identity = tokio::task::spawn_blocking(move || row_identity(&row_id, doc_state)).await?;
  match identity {
    None => Err(not_ready()),
    Some((id, row_database_id)) if id == row_id.to_string() => {
      if row_database_id == database_id.to_string() {
        Ok(())
      } else {
        Err(AppError::InvalidRequest(format!(
          "row {} does not belong to database {}",
          row_id, database_id
        )))
      }
    },
    Some(_) => Err(not_ready()),
  }
}

/// Creates the default document of a row and returns the receipt JSON.
pub async fn create_row_document(
  state: &AppState,
  user: RealtimeUser,
  workspace_id: Uuid,
  body: &[u8],
) -> Result<Value, AppError> {
  let request = parse_request(body)?;
  let uid = user.uid;
  state
    .workspace_access_control
    .enforce_action(&uid, &workspace_id, Action::Write)
    .await?;
  state
    .collab_access_control
    .enforce_action(&workspace_id, &uid, &request.database_id, Action::Write)
    .await?;

  let creation = match AtomicCreation::begin(
    &state.pg_pool,
    workspace_id,
    AtomicCreationKind::RowDocument,
    request.request_id,
    body,
  )
  .await?
  {
    AtomicCreationStart::Replay(receipt) => return Ok(receipt),
    AtomicCreationStart::Apply(creation) => creation,
  };

  ensure_row_readable(state, workspace_id, request.database_id, request.row_id).await?;
  let document_id = row_document_id(&request.row_id)?;
  // An existing document (another client, an interrupted attempt) is kept as is.
  ensure_default_document(state, uid, workspace_id, document_id).await?;
  await_collab_readable(state, workspace_id, document_id, CollabType::Document).await?;

  creation
    .commit(PublishRowDocumentResult {
      document_id,
      row_id: request.row_id,
      database_id: request.database_id,
      database_restore_id: request.database_restore_id,
      row_version: None,
      document_version: None,
      update_batch_id: Uuid::new_v4(),
    })
    .await
}

#[cfg(test)]
mod tests {
  use super::*;
  use collab_database::rows::{DatabaseRowBody, Row, RowId};
  use yrs::{ReadTxn, StateVector};

  const BODY: &str = r#"{"request_id":"5b1f7a4e-1a2b-4c3d-9e8f-001122334455","database_id":"6c2a8b5f-2b3c-4d4e-8f90-112233445566","database_restore_id":"00000000-0000-0000-0000-000000000000","row_id":"7d3b9c60-3c4d-4e5f-9a01-223344556677","source_view_id":"8e4cad71-4d5e-4f60-ab12-334455667788","name":"Row","contribution":{"kind":"default_schema","schema_version":"v1"}}"#;

  #[test]
  fn parses_default_schema() {
    let request = parse_request(BODY.as_bytes()).unwrap();
    assert_eq!(
      request.database_restore_id,
      "00000000-0000-0000-0000-000000000000"
    );
    assert_eq!(request.name, "Row");
  }

  #[test]
  fn rejects_default_actions_and_unknown_schema() {
    let actions = BODY.replace(
      r#"{"kind":"default_schema","schema_version":"v1"}"#,
      r#"{"kind":"default_actions","schema_version":"v1","actions":[]}"#,
    );
    assert!(matches!(
      parse_request(actions.as_bytes()),
      Err(AppError::InvalidRequest(_))
    ));
    let v2 = BODY.replace(r#""schema_version":"v1""#, r#""schema_version":"v2""#);
    assert!(matches!(
      parse_request(v2.as_bytes()),
      Err(AppError::InvalidRequest(_))
    ));
  }

  #[test]
  fn document_id_is_derived_from_the_row_id() {
    let row_id = Uuid::parse_str("7d3b9c60-3c4d-4e5f-9a01-223344556677").unwrap();
    let document_id = row_document_id(&row_id).unwrap();
    assert_eq!(
      document_id,
      Uuid::new_v5(&row_id, RowMetaKey::DocumentId.as_str().as_bytes())
    );
    assert_ne!(document_id, row_id);
    assert_eq!(document_id, row_document_id(&row_id).unwrap());
  }

  #[test]
  fn reads_the_row_identity() {
    let row_id = Uuid::new_v4();
    let database_id = Uuid::new_v4();
    let options = CollabOptions::new(row_id.to_string(), default_client_id());
    let mut collab = Collab::new_with_options(CollabOrigin::Empty, options).unwrap();
    DatabaseRowBody::create(
      RowId::from(row_id.to_string()),
      &mut collab,
      Row::empty(RowId::from(row_id.to_string()), &database_id.to_string()),
    );
    let doc_state = collab
      .transact()
      .encode_state_as_update_v1(&StateVector::default());
    assert_eq!(
      row_identity(&row_id, doc_state),
      Some((row_id.to_string(), database_id.to_string()))
    );

    let empty = Collab::new_with_options(
      CollabOrigin::Empty,
      CollabOptions::new(row_id.to_string(), default_client_id()),
    )
    .unwrap();
    let doc_state = empty
      .transact()
      .encode_state_as_update_v1(&StateVector::default());
    assert_eq!(row_identity(&row_id, doc_state), None);
  }
}

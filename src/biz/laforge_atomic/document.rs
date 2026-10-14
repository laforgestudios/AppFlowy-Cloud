//! `POST /api/workspace/{workspace_id}/v2/page-view/document/default`

use super::collab::{await_collab_readable, ensure_default_document};
use super::folder::{
  await_folder_views, encode_folder_insert, folder_child_ids, new_folder_view,
  publish_folder_update, requested_view, FolderViewInsert, RequestedView,
};
use super::position::{resolve_insert_index, CreationViewPosition};
use super::receipt::{AtomicCreation, AtomicCreationKind, AtomicCreationStart};
use crate::biz::collab::folder_view::to_folder_view_icon;
use crate::state::AppState;
use access_control::act::Action;
use app_error::AppError;
use appflowy_collaborate::ws2::WorkspaceCollabInstanceCache;
use collab_entity::CollabType;
use collab_rt_entity::user::RealtimeUser;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use shared_entity::dto::workspace_dto::ViewIcon;
use uuid::Uuid;

/// Web `CreateDefaultDocumentPageRequest`.
#[derive(Debug, Deserialize)]
pub struct CreateDefaultDocumentPageRequest {
  pub schema_version: String,
  pub request_id: Uuid,
  pub parent_view_id: Uuid,
  pub view_id: Uuid,
  pub name: String,
  #[serde(default)]
  pub position: Option<CreationViewPosition>,
  #[serde(default)]
  pub icon: Option<ViewIcon>,
  #[serde(default)]
  pub extra: Option<serde_json::Map<String, Value>>,
  #[serde(default)]
  pub is_favorite: Option<bool>,
}

/// Web `AtomicPageResult`.
#[derive(Debug, Serialize)]
pub struct AtomicPageResult {
  pub view_id: Uuid,
  pub database_id: Option<Uuid>,
}

pub fn parse_request(body: &[u8]) -> Result<CreateDefaultDocumentPageRequest, AppError> {
  let request: CreateDefaultDocumentPageRequest = serde_json::from_slice(body).map_err(|err| {
    AppError::InvalidRequest(format!("invalid document creation request: {}", err))
  })?;
  if request.schema_version != "v1" {
    return Err(AppError::InvalidRequest(format!(
      "unsupported schema_version {}",
      request.schema_version
    )));
  }
  if request.view_id == request.parent_view_id {
    return Err(AppError::InvalidRequest(
      "view_id must differ from parent_view_id".to_string(),
    ));
  }
  Ok(request)
}

/// Creates the default document collab and its Folder view, and returns the receipt JSON.
pub async fn create_default_document_page(
  state: &AppState,
  user: RealtimeUser,
  workspace_id: Uuid,
  body: &[u8],
) -> Result<Value, AppError> {
  let request = parse_request(body)?;
  if request.view_id == workspace_id {
    return Err(AppError::InvalidRequest(
      "view_id must differ from the workspace id".to_string(),
    ));
  }
  let uid = user.uid;
  state
    .workspace_access_control
    .enforce_action(&uid, &workspace_id, Action::Write)
    .await?;

  let creation = match AtomicCreation::begin(
    &state.pg_pool,
    workspace_id,
    AtomicCreationKind::Document,
    request.request_id,
    body,
  )
  .await?
  {
    AtomicCreationStart::Replay(receipt) => return Ok(receipt),
    AtomicCreationStart::Apply(creation) => creation,
  };

  let view_id = request.view_id.to_string();
  let parent_view_id = request.parent_view_id.to_string();
  let mut folder = state.ws_server.get_folder(workspace_id).await?;
  let siblings = folder_child_ids(&folder, &parent_view_id, uid)
    .ok_or_else(|| AppError::RecordNotFound(format!("parent view {} not found", parent_view_id)))?;
  let insert_index = match requested_view(&folder, &view_id, &parent_view_id, uid) {
    RequestedView::Absent => Some(resolve_insert_index(
      &siblings,
      &request.position.clone().unwrap_or_default(),
    )?),
    RequestedView::UnderParent => None,
    RequestedView::Elsewhere => {
      return Err(AppError::RecordAlreadyExists(format!(
        "view {} already exists under another parent",
        view_id
      )))
    },
  };

  // Content before navigation: the view never references a missing document.
  ensure_default_document(state, uid, workspace_id, request.view_id).await?;

  if let Some(index) = insert_index {
    let extra = request
      .extra
      .as_ref()
      .map(|extra| Value::Object(extra.clone()).to_string());
    let view = new_folder_view(
      &request.view_id,
      &request.parent_view_id,
      &request.name,
      collab_folder::ViewLayout::Document,
      uid,
      request.icon.clone().map(to_folder_view_icon),
      extra,
    );
    let update = encode_folder_insert(
      &mut folder,
      uid,
      vec![FolderViewInsert {
        view,
        index,
        is_favorite: request.is_favorite.unwrap_or(false),
      }],
    );
    publish_folder_update(state, user, workspace_id, update).await?;
  }

  await_folder_views(state, workspace_id, &[(view_id, parent_view_id)], uid).await?;
  await_collab_readable(state, workspace_id, request.view_id, CollabType::Document).await?;

  creation
    .commit(AtomicPageResult {
      view_id: request.view_id,
      database_id: None,
    })
    .await
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn parses_the_web_request() {
    let body = br#"{"schema_version":"v1","request_id":"5b1f7a4e-1a2b-4c3d-9e8f-001122334455","parent_view_id":"6c2a8b5f-2b3c-4d4e-8f90-112233445566","name":"","position":{"kind":"after","view_id":"7d3b9c60-3c4d-4e5f-9a01-223344556677"},"view_id":"8e4cad71-4d5e-4f60-ab12-334455667788"}"#;
    let request = parse_request(body).unwrap();
    assert_eq!(request.name, "");
    assert_eq!(
      request.position,
      Some(CreationViewPosition::After {
        view_id: "7d3b9c60-3c4d-4e5f-9a01-223344556677".to_string()
      })
    );
    assert!(request.icon.is_none() && request.extra.is_none() && request.is_favorite.is_none());
  }

  #[test]
  fn parses_optional_presentation() {
    let body = br#"{"schema_version":"v1","request_id":"5b1f7a4e-1a2b-4c3d-9e8f-001122334455","parent_view_id":"6c2a8b5f-2b3c-4d4e-8f90-112233445566","view_id":"8e4cad71-4d5e-4f60-ab12-334455667788","name":"Notes","icon":{"ty":0,"value":"x"},"extra":{"cover":{"type":"0","value":"red"}},"is_favorite":true}"#;
    let request = parse_request(body).unwrap();
    assert_eq!(request.position, None);
    assert_eq!(request.icon.unwrap().value, "x");
    assert!(request.extra.unwrap().contains_key("cover"));
    assert_eq!(request.is_favorite, Some(true));

    let null_presentation = br#"{"schema_version":"v1","request_id":"5b1f7a4e-1a2b-4c3d-9e8f-001122334455","parent_view_id":"6c2a8b5f-2b3c-4d4e-8f90-112233445566","view_id":"8e4cad71-4d5e-4f60-ab12-334455667788","name":"Notes","icon":null,"extra":null}"#;
    let request = parse_request(null_presentation).unwrap();
    assert!(request.icon.is_none() && request.extra.is_none());
  }

  #[test]
  fn rejects_invalid_requests() {
    let wrong_schema = br#"{"schema_version":"v2","request_id":"5b1f7a4e-1a2b-4c3d-9e8f-001122334455","parent_view_id":"6c2a8b5f-2b3c-4d4e-8f90-112233445566","view_id":"8e4cad71-4d5e-4f60-ab12-334455667788","name":""}"#;
    assert!(matches!(
      parse_request(wrong_schema),
      Err(AppError::InvalidRequest(_))
    ));
    let self_parent = br#"{"schema_version":"v1","request_id":"5b1f7a4e-1a2b-4c3d-9e8f-001122334455","parent_view_id":"8e4cad71-4d5e-4f60-ab12-334455667788","view_id":"8e4cad71-4d5e-4f60-ab12-334455667788","name":""}"#;
    assert!(matches!(
      parse_request(self_parent),
      Err(AppError::InvalidRequest(_))
    ));
    assert!(matches!(
      parse_request(b"not json"),
      Err(AppError::InvalidRequest(_))
    ));
  }
}

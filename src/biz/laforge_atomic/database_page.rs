//! `POST /api/workspace/{workspace_id}/v2/page-view/database/default`
//!
//! Creates a database page: the database collab and its rows (client ids), a Folder container
//! view (`extra.is_database_container`) under the parent and, under the container, the primary
//! database view and, for a Form, its responses Grid.

use super::collab::{await_collab_readable, collab_presence, CollabPresence};
use super::database_collab::{load_database, register_database_views};
use super::database_schema::{
  encode_default_database, folder_view_layout, parse_database_layout, DefaultDatabaseRecipe,
  RESPONSES_VIEW_NAME,
};
use super::document::AtomicPageResult;
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
use collab_database::views::DatabaseLayout;
use collab_entity::CollabType;
use collab_rt_entity::user::RealtimeUser;
use database_entity::dto::CollabParams;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use shared_entity::dto::workspace_dto::ViewIcon;
use std::collections::HashSet;
use uuid::Uuid;

/// More rows belong in an import, not a default page.
const MAX_INITIAL_ROWS: usize = 100;

/// Web `DefaultDatabaseOptions`.
#[derive(Debug, Deserialize)]
pub struct DefaultDatabaseOptions {
  pub database_id: Uuid,
  pub view_id: Uuid,
  pub name: String,
  pub layout: u8,
  pub embedded: bool,
  #[serde(default)]
  pub responses_view_id: Option<Uuid>,
  #[serde(default)]
  pub initial_row_ids: Vec<Uuid>,
  #[serde(default)]
  pub timezone_id: Option<String>,
}

/// Web `CreateDefaultDatabasePageRequest`.
#[derive(Debug, Deserialize)]
pub struct CreateDefaultDatabasePageRequest {
  pub schema_version: String,
  pub request_id: Uuid,
  pub parent_view_id: Uuid,
  pub container_id: Uuid,
  pub name: String,
  #[serde(default)]
  pub position: Option<CreationViewPosition>,
  #[serde(default)]
  pub icon: Option<ViewIcon>,
  #[serde(default)]
  pub extra: Option<Map<String, Value>>,
  #[serde(default)]
  pub is_favorite: Option<bool>,
  pub database: DefaultDatabaseOptions,
}

/// A parsed request with its database layout.
#[derive(Debug)]
pub struct DatabasePageRequest {
  pub request: CreateDefaultDatabasePageRequest,
  pub layout: DatabaseLayout,
}

impl DatabasePageRequest {
  pub fn recipe(&self) -> DefaultDatabaseRecipe {
    let database = &self.request.database;
    DefaultDatabaseRecipe {
      database_id: database.database_id,
      view_id: database.view_id,
      name: database.name.clone(),
      layout: self.layout,
      responses_view_id: database.responses_view_id,
      row_ids: database.initial_row_ids.clone(),
      timezone_id: database.timezone_id.clone(),
    }
  }

  /// Database views under the container: the primary view, then the Form responses view.
  pub fn database_view_ids(&self) -> Vec<Uuid> {
    let database = &self.request.database;
    std::iter::once(database.view_id)
      .chain(database.responses_view_id)
      .collect()
  }

  /// `extra` of the container: the client's presentation keys, then the database identity.
  pub fn container_extra(&self) -> String {
    let mut extra = self.request.extra.clone().unwrap_or_default();
    extra.insert(
      "database_id".to_string(),
      json!(self.request.database.database_id),
    );
    extra.insert("is_database_container".to_string(), json!(true));
    extra.insert(
      "embedded".to_string(),
      json!(self.request.database.embedded),
    );
    Value::Object(extra).to_string()
  }

  pub fn child_extra(&self) -> String {
    database_child_extra(
      &self.request.database.database_id,
      self.request.database.embedded,
    )
  }
}

/// `extra` of a database view in the Folder.
pub fn database_child_extra(database_id: &Uuid, embedded: bool) -> String {
  let mut extra = Map::new();
  extra.insert("database_id".to_string(), json!(database_id));
  if embedded {
    extra.insert("embedded".to_string(), json!(true));
  }
  Value::Object(extra).to_string()
}

pub fn parse_request(body: &[u8]) -> Result<DatabasePageRequest, AppError> {
  let request: CreateDefaultDatabasePageRequest = serde_json::from_slice(body).map_err(|err| {
    AppError::InvalidRequest(format!("invalid database creation request: {}", err))
  })?;
  if request.schema_version != "v1" {
    return Err(AppError::InvalidRequest(format!(
      "unsupported schema_version {}",
      request.schema_version
    )));
  }
  let layout = parse_database_layout(request.database.layout)?;
  let database = &request.database;
  match (layout, database.responses_view_id) {
    (DatabaseLayout::Form, None) => {
      return Err(AppError::InvalidRequest(
        "a Form database requires responses_view_id".to_string(),
      ))
    },
    (DatabaseLayout::Form, Some(_)) | (_, None) => {},
    (_, Some(_)) => {
      return Err(AppError::InvalidRequest(
        "responses_view_id is only valid for a Form database".to_string(),
      ))
    },
  }
  if database.initial_row_ids.len() > MAX_INITIAL_ROWS {
    return Err(AppError::InvalidRequest(format!(
      "at most {} initial rows",
      MAX_INITIAL_ROWS
    )));
  }
  let ids = [
    request.parent_view_id,
    request.container_id,
    database.database_id,
    database.view_id,
  ]
  .into_iter()
  .chain(database.responses_view_id)
  .chain(database.initial_row_ids.iter().copied());
  let mut seen = HashSet::new();
  for id in ids {
    if !seen.insert(id) {
      return Err(AppError::InvalidRequest(format!(
        "id {} is used twice in the creation request",
        id
      )));
    }
  }
  Ok(DatabasePageRequest { request, layout })
}

/// A database id already present in the workspace is adopted only when it holds the requested
/// views (an interrupted attempt of the same request).
async fn ensure_database(
  state: &AppState,
  uid: i64,
  workspace_id: Uuid,
  parsed: &DatabasePageRequest,
) -> Result<(), AppError> {
  let database = &parsed.request.database;
  match collab_presence(
    state,
    workspace_id,
    database.database_id,
    CollabType::Database,
  )
  .await?
  {
    CollabPresence::Present => {
      let (collab, body) = load_database(state, workspace_id, database.database_id).await?;
      let txn = collab.transact();
      let holds_views = parsed
        .database_view_ids()
        .iter()
        .all(|view_id| body.views.get_view(&txn, &view_id.to_string()).is_some());
      if holds_views {
        Ok(())
      } else {
        Err(AppError::RecordAlreadyExists(format!(
          "database {} already exists",
          database.database_id
        )))
      }
    },
    CollabPresence::Absent => {
      for row_id in &database.initial_row_ids {
        if collab_presence(state, workspace_id, *row_id, CollabType::DatabaseRow).await?
          == CollabPresence::Present
        {
          return Err(AppError::RecordAlreadyExists(format!(
            "row {} already exists",
            row_id
          )));
        }
      }
      let encoded = encode_default_database(&parsed.recipe()).await?;
      let mut collabs = vec![CollabParams {
        object_id: encoded.encoded_database_collab.object_id,
        encoded_collab_v1: encoded
          .encoded_database_collab
          .encoded_collab
          .encode_to_bytes()?
          .into(),
        collab_type: CollabType::Database,
        updated_at: None,
      }];
      for row in encoded.encoded_row_collabs {
        collabs.push(CollabParams {
          object_id: row.object_id,
          encoded_collab_v1: row.encoded_collab.encode_to_bytes()?.into(),
          collab_type: CollabType::DatabaseRow,
          updated_at: None,
        });
      }
      // The database and its rows are committed together: a retry either finds all of them or
      // none.
      let action = format!("Atomic creation of database {}", database.database_id);
      let mut transaction = state.pg_pool.begin().await?;
      for params in collabs {
        state
          .collab_storage
          .upsert_new_collab_with_transaction(workspace_id, &uid, params, &mut transaction, &action)
          .await?;
      }
      transaction.commit().await?;
      Ok(())
    },
  }
}

/// Creates the default database page and returns the receipt JSON.
pub async fn create_default_database_page(
  state: &AppState,
  user: RealtimeUser,
  workspace_id: Uuid,
  body: &[u8],
) -> Result<Value, AppError> {
  let parsed = parse_request(body)?;
  let request = &parsed.request;
  if parsed
    .database_view_ids()
    .iter()
    .chain([&request.container_id, &request.database.database_id])
    .any(|id| *id == workspace_id)
  {
    return Err(AppError::InvalidRequest(
      "created ids must differ from the workspace id".to_string(),
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
    AtomicCreationKind::Database,
    request.request_id,
    body,
  )
  .await?
  {
    AtomicCreationStart::Replay(receipt) => return Ok(receipt),
    AtomicCreationStart::Apply(creation) => creation,
  };

  let container_id = request.container_id.to_string();
  let parent_view_id = request.parent_view_id.to_string();
  let mut folder = state.ws_server.get_folder(workspace_id).await?;
  let siblings = folder_child_ids(&folder, &parent_view_id, uid)
    .ok_or_else(|| AppError::RecordNotFound(format!("parent view {} not found", parent_view_id)))?;
  let container_index = match requested_view(&folder, &container_id, &parent_view_id, uid) {
    RequestedView::Absent => Some(resolve_insert_index(
      &siblings,
      &request.position.clone().unwrap_or_default(),
    )?),
    RequestedView::UnderParent => None,
    RequestedView::Elsewhere => {
      return Err(AppError::RecordAlreadyExists(format!(
        "view {} already exists under another parent",
        container_id
      )))
    },
  };
  let mut absent_children = vec![];
  for view_id in parsed.database_view_ids() {
    match requested_view(&folder, &view_id.to_string(), &container_id, uid) {
      RequestedView::Absent => absent_children.push(view_id),
      RequestedView::UnderParent => {},
      RequestedView::Elsewhere => {
        return Err(AppError::RecordAlreadyExists(format!(
          "view {} already exists under another parent",
          view_id
        )))
      },
    }
  }

  // Content before navigation: no Folder view ever references a missing database.
  ensure_database(state, uid, workspace_id, &parsed).await?;
  register_database_views(
    state,
    user.clone(),
    workspace_id,
    request.database.database_id,
    &parsed.database_view_ids(),
  )
  .await?;

  let mut inserts = vec![];
  if let Some(index) = container_index {
    inserts.push(FolderViewInsert {
      view: new_folder_view(
        &request.container_id,
        &request.parent_view_id,
        &request.name,
        folder_view_layout(parsed.layout),
        uid,
        request.icon.clone().map(to_folder_view_icon),
        Some(parsed.container_extra()),
      ),
      index,
      is_favorite: request.is_favorite.unwrap_or(false),
    });
  }
  for view_id in &absent_children {
    let (name, layout) = if Some(*view_id) == request.database.responses_view_id {
      (RESPONSES_VIEW_NAME, DatabaseLayout::Grid)
    } else {
      (request.database.name.as_str(), parsed.layout)
    };
    inserts.push(FolderViewInsert {
      view: new_folder_view(
        view_id,
        &request.container_id,
        name,
        folder_view_layout(layout),
        uid,
        None,
        Some(parsed.child_extra()),
      ),
      index: None,
      is_favorite: false,
    });
  }
  if !inserts.is_empty() {
    let update = encode_folder_insert(&mut folder, uid, inserts);
    publish_folder_update(state, user, workspace_id, update).await?;
  }

  let mut expected = vec![(container_id.clone(), parent_view_id)];
  expected.extend(
    parsed
      .database_view_ids()
      .iter()
      .map(|view_id| (view_id.to_string(), container_id.clone())),
  );
  await_folder_views(state, workspace_id, &expected, uid).await?;
  await_collab_readable(
    state,
    workspace_id,
    request.database.database_id,
    CollabType::Database,
  )
  .await?;
  for row_id in &request.database.initial_row_ids {
    await_collab_readable(state, workspace_id, *row_id, CollabType::DatabaseRow).await?;
  }

  creation
    .commit(AtomicPageResult {
      view_id: request.container_id,
      database_id: Some(request.database.database_id),
    })
    .await
}

#[cfg(test)]
mod tests;

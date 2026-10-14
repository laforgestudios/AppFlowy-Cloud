//! `POST /api/workspace/{workspace_id}/v2/page-view/database-view/default`
//!
//! Adds a view (and, for a Form, its responses Grid) to an existing database, under a Folder
//! parent, with settings duplicated from `source_view_id` when given.

use super::collab::{collab_presence, CollabPresence};
use super::database_collab::{await_database_views, load_database, register_database_views};
use super::database_page::database_child_extra;
use super::database_schema::{
  default_view_settings, folder_view_layout, linked_view_settings, parse_database_layout,
  ViewSettings, RESPONSES_VIEW_NAME,
};
use super::folder::{
  await_folder_views, encode_folder_insert, folder_child_ids, new_folder_view,
  publish_folder_update, requested_view, FolderViewInsert, RequestedView,
};
use super::position::{resolve_insert_index, CreationViewPosition};
use super::receipt::{AtomicCreation, AtomicCreationKind, AtomicCreationStart};
use crate::biz::collab::folder_view::to_folder_view_icon;
use crate::biz::workspace::page_view::update_database_data;
use crate::state::AppState;
use access_control::act::Action;
use app_error::AppError;
use appflowy_collaborate::ws2::WorkspaceCollabInstanceCache;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use collab::core::collab::Collab;
use collab_database::database::DatabaseBody;
use collab_database::entity::CreateViewParams;
use collab_database::views::{DatabaseLayout, FieldOrder, RowOrder};
use collab_entity::CollabType;
use collab_rt_entity::user::RealtimeUser;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use shared_entity::dto::workspace_dto::ViewIcon;
use uuid::Uuid;
use yrs::{ReadTxn, StateVector};

/// Web `DefaultDatabaseViewOptions`.
#[derive(Debug, Deserialize)]
pub struct DefaultDatabaseViewOptions {
  pub database_id: Uuid,
  pub view_id: Uuid,
  pub parent_view_id: Uuid,
  #[serde(default)]
  pub source_view_id: Option<String>,
  pub layout: u8,
  pub name: String,
  pub embedded: bool,
  #[serde(default)]
  pub responses_view_id: Option<Uuid>,
  #[serde(default)]
  pub timezone_id: Option<String>,
}

/// Web `CreateDefaultDatabaseViewRequest`.
#[derive(Debug, Deserialize)]
pub struct CreateDefaultDatabaseViewRequest {
  pub schema_version: String,
  pub request_id: Uuid,
  pub database_restore_id: String,
  pub view: DefaultDatabaseViewOptions,
  #[serde(default)]
  pub position: Option<CreationViewPosition>,
  #[serde(default)]
  pub icon: Option<ViewIcon>,
  #[serde(default)]
  pub extra: Option<Map<String, Value>>,
}

/// Web `CreateDatabaseViewResult`. This server has no database versions.
#[derive(Debug, Serialize)]
pub struct CreateDatabaseViewResult {
  pub view_id: Uuid,
  pub view_ids: Vec<Uuid>,
  pub database_id: Uuid,
  pub database_restore_id: String,
  pub database_version: Option<String>,
  /// Base64 Yrs update (v1) adding the views to the database. A retry that finds the views
  /// already present carries the whole database state instead.
  pub database_update_v1: String,
  pub update_batch_id: Uuid,
}

#[derive(Debug)]
pub struct DatabaseViewRequest {
  pub request: CreateDefaultDatabaseViewRequest,
  pub layout: DatabaseLayout,
}

impl DatabaseViewRequest {
  /// The created views: the requested one, then the Form responses view.
  pub fn view_ids(&self) -> Vec<Uuid> {
    std::iter::once(self.request.view.view_id)
      .chain(self.request.view.responses_view_id)
      .collect()
  }

  fn layout_of(&self, view_id: &Uuid) -> DatabaseLayout {
    if Some(*view_id) == self.request.view.responses_view_id {
      DatabaseLayout::Grid
    } else {
      self.layout
    }
  }

  fn name_of(&self, view_id: &Uuid) -> &str {
    if Some(*view_id) == self.request.view.responses_view_id {
      RESPONSES_VIEW_NAME
    } else {
      &self.request.view.name
    }
  }

  fn folder_extra(&self, view_id: &Uuid) -> String {
    let view = &self.request.view;
    let identity = database_child_extra(&view.database_id, view.embedded);
    match (&self.request.extra, *view_id == view.view_id) {
      (Some(extra), true) => {
        let mut merged = extra.clone();
        if let Ok(Value::Object(identity)) = serde_json::from_str::<Value>(&identity) {
          merged.extend(identity);
        }
        Value::Object(merged).to_string()
      },
      _ => identity,
    }
  }
}

pub fn parse_request(body: &[u8]) -> Result<DatabaseViewRequest, AppError> {
  let request: CreateDefaultDatabaseViewRequest = serde_json::from_slice(body).map_err(|err| {
    AppError::InvalidRequest(format!("invalid database view creation request: {}", err))
  })?;
  if request.schema_version != "v1" {
    return Err(AppError::InvalidRequest(format!(
      "unsupported schema_version {}",
      request.schema_version
    )));
  }
  let view = &request.view;
  let layout = parse_database_layout(view.layout)?;
  match (layout, view.responses_view_id) {
    (DatabaseLayout::Form, None) => {
      return Err(AppError::InvalidRequest(
        "a Form view requires responses_view_id".to_string(),
      ))
    },
    (DatabaseLayout::Form, Some(_)) | (_, None) => {},
    (_, Some(_)) => {
      return Err(AppError::InvalidRequest(
        "responses_view_id is only valid for a Form view".to_string(),
      ))
    },
  }
  let mut ids = vec![view.database_id, view.view_id, view.parent_view_id];
  ids.extend(view.responses_view_id);
  for (index, id) in ids.iter().enumerate() {
    if ids[..index].contains(id) {
      return Err(AppError::InvalidRequest(format!(
        "id {} is used twice in the creation request",
        id
      )));
    }
  }
  Ok(DatabaseViewRequest { request, layout })
}

/// The `CreateViewParams` of each view the database lacks, with the orders it starts from.
pub fn plan_database_views(
  collab: &Collab,
  body: &DatabaseBody,
  parsed: &DatabaseViewRequest,
) -> Result<Vec<(CreateViewParams, Vec<FieldOrder>, Vec<RowOrder>)>, AppError> {
  let request = &parsed.request.view;
  let txn = collab.transact();
  let database_id = body.get_database_id(&txn);
  if database_id != request.database_id.to_string() {
    return Err(AppError::RecordNotFound(format!(
      "database {} not found",
      request.database_id
    )));
  }
  let source = match &request.source_view_id {
    Some(source_view_id) => Some(body.views.get_view(&txn, source_view_id).ok_or_else(|| {
      AppError::InvalidRequest(format!(
        "source view {} is not a view of database {}",
        source_view_id, request.database_id
      ))
    })?),
    None => None,
  };
  let (field_orders, row_orders) = match &source {
    Some(source) => (source.field_orders.clone(), source.row_orders.clone()),
    None => {
      let inline_view_id = body.get_inline_view_id(&txn);
      (
        body.views.get_field_orders(&txn, &inline_view_id),
        body.views.get_row_orders(&txn, &inline_view_id),
      )
    },
  };
  let field_order: Vec<String> = field_orders.iter().map(|order| order.id.clone()).collect();
  let fields = body.fields.get_all_fields(&txn);
  let timezone_id = request.timezone_id.as_deref();

  let mut plans = vec![];
  for view_id in parsed.view_ids() {
    let layout = parsed.layout_of(&view_id);
    if let Some(existing) = body.views.get_view(&txn, &view_id.to_string()) {
      // An interrupted attempt of the same request left it: adopt it.
      if existing.layout == layout {
        continue;
      }
      return Err(AppError::RecordAlreadyExists(format!(
        "database view {} already exists with another layout",
        view_id
      )));
    }
    let settings: ViewSettings = match (&source, view_id == request.view_id) {
      (Some(source), true) => linked_view_settings(layout, source, &fields, timezone_id),
      _ => default_view_settings(layout, &fields, &field_order, timezone_id),
    };
    plans.push((
      settings.into_params(&database_id, &view_id, parsed.name_of(&view_id), layout),
      field_orders.clone(),
      row_orders.clone(),
    ));
  }
  Ok(plans)
}

/// Applies the planned views to the database and returns the update (v1), `None` when the
/// database already holds every view.
pub fn apply_database_views(
  collab: &mut Collab,
  body: &DatabaseBody,
  plans: Vec<(CreateViewParams, Vec<FieldOrder>, Vec<RowOrder>)>,
) -> Result<Option<Vec<u8>>, AppError> {
  if plans.is_empty() {
    return Ok(None);
  }
  let mut txn = collab.transact_mut();
  for (params, field_orders, row_orders) in plans {
    let view_id = params.view_id.clone();
    body
      .create_linked_view(&mut txn, params, field_orders, row_orders)
      .map_err(|err| {
        AppError::Internal(anyhow::anyhow!(
          "Unable to create database view {}: {}",
          view_id,
          err
        ))
      })?;
  }
  Ok(Some(txn.encode_update_v1()))
}

/// Creates the database view(s) and their Folder views and returns the receipt JSON.
pub async fn create_default_database_view(
  state: &AppState,
  user: RealtimeUser,
  workspace_id: Uuid,
  body: &[u8],
) -> Result<Value, AppError> {
  let parsed = parse_request(body)?;
  let request = &parsed.request;
  let view = &request.view;
  if parsed.view_ids().contains(&workspace_id) {
    return Err(AppError::InvalidRequest(
      "created ids must differ from the workspace id".to_string(),
    ));
  }
  let uid = user.uid;
  state
    .workspace_access_control
    .enforce_action(&uid, &workspace_id, Action::Write)
    .await?;
  state
    .collab_access_control
    .enforce_action(&workspace_id, &uid, &view.database_id, Action::Write)
    .await?;
  // Only a database of this workspace gets views in its Folder.
  let not_found = || AppError::RecordNotFound(format!("database {} not found", view.database_id));
  match collab_presence(state, workspace_id, view.database_id, CollabType::Database).await {
    Ok(CollabPresence::Present) => {},
    Ok(CollabPresence::Absent) | Err(AppError::RecordAlreadyExists(_)) => return Err(not_found()),
    Err(err) => return Err(err),
  }

  let creation = match AtomicCreation::begin(
    &state.pg_pool,
    workspace_id,
    AtomicCreationKind::DatabaseView,
    request.request_id,
    body,
  )
  .await?
  {
    AtomicCreationStart::Replay(receipt) => return Ok(receipt),
    AtomicCreationStart::Apply(creation) => creation,
  };

  let parent_view_id = view.parent_view_id.to_string();
  let mut folder = state.ws_server.get_folder(workspace_id).await?;
  let siblings = folder_child_ids(&folder, &parent_view_id, uid)
    .ok_or_else(|| AppError::RecordNotFound(format!("parent view {} not found", parent_view_id)))?;
  let mut absent = vec![];
  for view_id in parsed.view_ids() {
    match requested_view(&folder, &view_id.to_string(), &parent_view_id, uid) {
      RequestedView::Absent => absent.push(view_id),
      RequestedView::UnderParent => {},
      RequestedView::Elsewhere => {
        return Err(AppError::RecordAlreadyExists(format!(
          "view {} already exists under another parent",
          view_id
        )))
      },
    }
  }
  // The requested view goes at `position`, the responses view right after it.
  let primary_index = if absent.contains(&view.view_id) {
    resolve_insert_index(&siblings, &request.position.clone().unwrap_or_default())?
  } else {
    resolve_insert_index(
      &siblings,
      &CreationViewPosition::After {
        view_id: view.view_id.to_string(),
      },
    )?
    .map(|index| index - 1)
  };

  // Content before navigation.
  let (mut database_collab, database_body) =
    load_database(state, workspace_id, view.database_id).await?;
  let plans = plan_database_views(&database_collab, &database_body, &parsed)?;
  let database_update = match apply_database_views(&mut database_collab, &database_body, plans)? {
    Some(update) => {
      update_database_data(
        &state.metrics.appflowy_web_metrics,
        &state.ws_server,
        user.clone(),
        workspace_id,
        view.database_id,
        update.clone(),
      )
      .await?;
      update
    },
    None => database_collab
      .transact()
      .encode_state_as_update_v1(&StateVector::default()),
  };
  drop(database_body);
  drop(database_collab);
  register_database_views(
    state,
    user.clone(),
    workspace_id,
    view.database_id,
    &parsed.view_ids(),
  )
  .await?;

  if !absent.is_empty() {
    let inserts = absent
      .iter()
      .map(|view_id| {
        let index = if *view_id == view.view_id {
          primary_index
        } else {
          primary_index.map(|index| index + 1)
        };
        FolderViewInsert {
          view: new_folder_view(
            view_id,
            &view.parent_view_id,
            parsed.name_of(view_id),
            folder_view_layout(parsed.layout_of(view_id)),
            uid,
            if *view_id == view.view_id {
              request.icon.clone().map(to_folder_view_icon)
            } else {
              None
            },
            Some(parsed.folder_extra(view_id)),
          ),
          index,
          is_favorite: false,
        }
      })
      .collect();
    let update = encode_folder_insert(&mut folder, uid, inserts);
    publish_folder_update(state, user, workspace_id, update).await?;
  }

  let expected: Vec<(String, String)> = parsed
    .view_ids()
    .iter()
    .map(|view_id| (view_id.to_string(), parent_view_id.clone()))
    .collect();
  await_folder_views(state, workspace_id, &expected, uid).await?;
  await_database_views(state, workspace_id, view.database_id, &parsed.view_ids()).await?;

  creation
    .commit(CreateDatabaseViewResult {
      view_id: view.view_id,
      view_ids: parsed.view_ids(),
      database_id: view.database_id,
      database_restore_id: request.database_restore_id.clone(),
      database_version: None,
      database_update_v1: STANDARD.encode(database_update),
      update_batch_id: Uuid::new_v4(),
    })
    .await
}

#[cfg(test)]
mod tests;

//! Read-side routes the current AppFlowy-Web client expects (see `doc/LAFORGE_PHASE1_SPEC.md`):
//! server info, collab permission probe, view tree / navigation, database list page and
//! a few stubs. Kept in one module so it merges cleanly with the other Laforge route sets.

use std::collections::{HashMap, HashSet};

use access_control::act::Action;
use actix_web::web::{self, Data, Json};
use actix_web::{HttpRequest, Result, Scope};
use app_error::AppError;
use appflowy_collaborate::ws2::WorkspaceCollabInstanceCache;
use collab_database::workspace_database::WorkspaceDatabaseBody;
use collab_entity::CollabType;
use collab::core::collab::default_client_id;
use database::collab::select_workspace_database_oid;
use database::collab::GetCollabOrigin;
use database_entity::dto::AFRole;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use shared_entity::dto::workspace_dto::{FolderView, ViewIcon, ViewLayout};
use shared_entity::response::AppResponse;
use uuid::Uuid;

use crate::api::util::realtime_user_for_web_request;
use crate::biz::authentication::jwt::UserUuid;
use crate::biz::collab::folder_view::{
  clamp_view_depth, collab_folder_to_folder_view, navigation_path_ids, prune_navigation_tree,
  to_dto_view_icon, to_dto_view_layout,
};
use crate::biz::collab::ops::load_folder_and_published_ids;
use crate::biz::collab::utils::get_latest_collab;
use crate::state::AppState;

/// Upper bound on the folder depth built for a navigation request (path length + target depth).
const MAX_NAVIGATION_BUILD_DEPTH: u32 = 32;
const DEFAULT_DATABASE_PAGE_SIZE: usize = 200;
const MAX_DATABASE_PAGE_SIZE: usize = 500;
const MAX_BATCH_VIEW_IDS: usize = 200;

/// Literal-before-generic: this must be applied to the scope before the generic
/// `/{workspace_id}/...` resources are registered.
pub fn read_routes(scope: Scope) -> Scope {
  scope
    .service(
      web::resource("/{workspace_id}/collab/{object_id}/permission")
        .route(web::get().to(get_collab_permission_handler)),
    )
    .service(
      web::resource("/{workspace_id}/collab/{object_id}/collab-exists")
        .route(web::get().to(crate::api::workspace::get_row_document_collab_exists_handler)),
    )
    .service(
      web::resource("/{workspace_id}/view/{view_id}/navigation")
        .route(web::get().to(get_view_navigation_handler)),
    )
    .service(
      web::resource("/{workspace_id}/view/{view_id}").route(web::get().to(get_view_handler)),
    )
    .service(
      web::resource("/{workspace_id}/views")
        .route(web::get().to(get_views_handler))
        .route(web::post().to(post_views_handler)),
    )
    .service(
      web::resource("/{workspace_id}/database/{database_id}/restore-state")
        .route(web::get().to(get_database_restore_state_handler)),
    )
}

// ---------------------------------------------------------------------------------------------
// T1: server info
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Serialize, PartialEq)]
pub struct LaforgeServerInfo {
  pub version: &'static str,
  pub min_web_client_version: &'static str,
  pub enable_page_history: bool,
  pub enable_atomic_creation: bool,
  pub enable_atomic_creation_defaults: bool,
  pub enable_database_history: bool,
  pub ai_enabled: bool,
  pub self_hosted: bool,
}

/// `version` must stay >= 0.18.10 with `+build` metadata only: a `-suffix` is a prerelease
/// and the web client treats it as "server too old".
pub const LAFORGE_SERVER_INFO: LaforgeServerInfo = LaforgeServerInfo {
  version: "0.18.10+laforge",
  min_web_client_version: "0.0.0",
  enable_page_history: false,
  enable_atomic_creation: true,
  enable_atomic_creation_defaults: true,
  enable_database_history: false,
  ai_enabled: false,
  self_hosted: true,
};

pub fn server_info_scope() -> Scope {
  web::scope("/api/server-info")
    .service(web::resource("").route(web::get().to(server_info_handler)))
    .service(web::resource("/auth-providers").route(web::get().to(auth_providers_handler)))
}

async fn server_info_handler() -> Json<AppResponse<&'static LaforgeServerInfo>> {
  Json(AppResponse::Ok().with_data(&LAFORGE_SERVER_INFO))
}

#[derive(Debug, Serialize, PartialEq)]
pub struct AuthProvidersPayload {
  pub count: usize,
  pub providers: Vec<String>,
  pub signup_disabled: bool,
  pub mailer_autoconfirm: bool,
}

impl AuthProvidersPayload {
  fn password_only() -> Self {
    Self {
      count: 1,
      providers: vec!["password".to_string()],
      signup_disabled: false,
      mailer_autoconfirm: false,
    }
  }
}

/// Magic link needs working SMTP, so it is opt-in: `APPFLOWY_ENABLE_MAGIC_LINK=true|1`.
fn magic_link_enabled_from_env() -> bool {
  magic_link_flag(std::env::var("APPFLOWY_ENABLE_MAGIC_LINK").ok().as_deref())
}

fn magic_link_flag(value: Option<&str>) -> bool {
  matches!(
    value.map(|v| v.trim().to_ascii_lowercase()).as_deref(),
    Some("true") | Some("1")
  )
}

fn auth_providers_from_settings(
  oauth_providers: Vec<String>,
  signup_disabled: bool,
  mailer_autoconfirm: bool,
  magic_link: bool,
) -> AuthProvidersPayload {
  let mut providers = vec!["password".to_string()];
  if magic_link {
    providers.push("magic_link".to_string());
  }
  providers.extend(oauth_providers);
  AuthProvidersPayload {
    count: providers.len(),
    providers,
    signup_disabled,
    mailer_autoconfirm,
  }
}

async fn auth_providers_handler(state: Data<AppState>) -> Json<AppResponse<AuthProvidersPayload>> {
  let payload = match state.gotrue_client.settings().await {
    Ok(settings) => auth_providers_from_settings(
      settings
        .external
        .oauth_providers()
        .into_iter()
        .map(|p| p.to_string())
        .collect(),
      settings.disable_signup,
      settings.mailer_autoconfirm,
      magic_link_enabled_from_env(),
    ),
    Err(err) => {
      tracing::warn!("auth-providers: GoTrue settings unavailable, falling back: {}", err);
      AuthProvidersPayload::password_only()
    },
  };
  Json(AppResponse::Ok().with_data(payload))
}

// ---------------------------------------------------------------------------------------------
// T2: collab object permission
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct CollabPermissionQuery {
  collab_type: Option<i32>,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct CollabObjectPermission {
  pub object_id: Uuid,
  pub collab_type: i32,
  pub governing_view_id: Uuid,
  pub access_level: Option<u8>,
  pub can_read: bool,
  pub can_write: bool,
  pub can_comment: bool,
  pub can_share: bool,
}

impl CollabObjectPermission {
  pub fn new(
    object_id: Uuid,
    collab_type: i32,
    governing_view_id: Uuid,
    can_read: bool,
    can_write: bool,
  ) -> Self {
    // AccessLevel in the web client: ReadOnly = 10, FullAccess = 50.
    let access_level = if can_write {
      Some(50)
    } else if can_read {
      Some(10)
    } else {
      None
    };
    Self {
      object_id,
      collab_type,
      governing_view_id,
      access_level,
      can_read,
      can_write,
      can_comment: can_write,
      can_share: can_write,
    }
  }
}

fn permission_denied(err: AppError) -> Result<bool, AppError> {
  match err {
    AppError::NotEnoughPermissions => Ok(false),
    other => Err(other),
  }
}

async fn get_collab_permission_handler(
  user_uuid: UserUuid,
  path: web::Path<(Uuid, Uuid)>,
  query: web::Query<CollabPermissionQuery>,
  state: Data<AppState>,
) -> Result<Json<AppResponse<CollabObjectPermission>>> {
  let (workspace_id, object_id) = path.into_inner();
  let collab_type = query.collab_type.unwrap_or(0);
  let uid = state.user_cache.get_user_uid(&user_uuid).await?;

  let can_read = match state
    .collab_access_control
    .enforce_action(&workspace_id, &uid, &object_id, Action::Read)
    .await
  {
    Ok(()) => true,
    Err(err) => permission_denied(err)?,
  };
  let can_write = can_read
    && match state
      .collab_access_control
      .enforce_action(&workspace_id, &uid, &object_id, Action::Write)
      .await
    {
      Ok(()) => true,
      Err(err) => permission_denied(err)?,
    };

  // Documents are governed by their own view id; a database by its first linked view.
  let governing_view_id = if collab_type == CollabType::Database as i32 {
    first_linked_view_of_database(&state, workspace_id, &object_id)
      .await
      .unwrap_or(object_id)
  } else {
    object_id
  };

  Ok(Json(AppResponse::Ok().with_data(CollabObjectPermission::new(
    object_id,
    collab_type,
    governing_view_id,
    can_read,
    can_write,
  ))))
}

async fn first_linked_view_of_database(
  state: &AppState,
  workspace_id: Uuid,
  database_id: &Uuid,
) -> Option<Uuid> {
  let metas = load_database_metas(state, workspace_id).await.ok()?;
  let database_id = database_id.to_string();
  metas
    .into_iter()
    .find(|(id, _)| *id == database_id)
    .and_then(|(_, views)| views.first().and_then(|v| Uuid::parse_str(v).ok()))
}

// ---------------------------------------------------------------------------------------------
// T3: view tree
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct DepthQuery {
  depth: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct ViewsQuery {
  depth: Option<u32>,
  view_ids: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ViewsBody {
  depth: Option<u32>,
  view_ids: Vec<String>,
}

#[derive(Debug, Serialize)]
struct ViewsResponse {
  views: Vec<FolderView>,
}

pub(crate) async fn require_member(state: &AppState, uid: i64, workspace_id: Uuid) -> Result<(), AppError> {
  // AppFlowy Web does not support guest editors, so require at least a member.
  state
    .workspace_access_control
    .enforce_role_weak(&uid, &workspace_id, AFRole::Member)
    .await
}

fn view_not_found(view_id: &Uuid, err: AppError) -> AppError {
  match err {
    AppError::InvalidFolderView(_) => {
      AppError::RecordNotFound(format!("view {} not found or not accessible", view_id))
    },
    other => other,
  }
}

async fn get_view_handler(
  user_uuid: UserUuid,
  path: web::Path<(Uuid, Uuid)>,
  query: web::Query<DepthQuery>,
  state: Data<AppState>,
  req: HttpRequest,
) -> Result<Json<AppResponse<FolderView>>> {
  let (workspace_id, view_id) = path.into_inner();
  let depth = clamp_view_depth(query.depth, 1);
  let uid = state.user_cache.get_user_uid(&user_uuid).await?;
  require_member(&state, uid, workspace_id).await?;
  let user = realtime_user_for_web_request(req.headers(), uid)?;
  // Read straight from the folder collab (no response cache) for read-your-writes.
  let (folder, published) = load_folder_and_published_ids(&state, user, workspace_id).await?;
  let view = collab_folder_to_folder_view(workspace_id, &view_id, &folder, depth, &published, uid)
    .map_err(|e| view_not_found(&view_id, e))?;
  Ok(Json(AppResponse::Ok().with_data(view)))
}

async fn batch_views(
  state: &AppState,
  req: &HttpRequest,
  user_uuid: &UserUuid,
  workspace_id: Uuid,
  depth: Option<u32>,
  raw_ids: Vec<String>,
) -> Result<Json<AppResponse<ViewsResponse>>> {
  let depth = clamp_view_depth(depth, 2);
  let uid = state.user_cache.get_user_uid(user_uuid).await?;
  require_member(state, uid, workspace_id).await?;
  let user = realtime_user_for_web_request(req.headers(), uid)?;
  let (folder, published) = load_folder_and_published_ids(state, user, workspace_id).await?;
  let views = parse_view_ids(&raw_ids)
    .iter()
    // Unknown / inaccessible ids are skipped rather than failing the whole batch.
    .filter_map(|id| {
      collab_folder_to_folder_view(workspace_id, id, &folder, depth, &published, uid).ok()
    })
    .collect();
  Ok(Json(AppResponse::Ok().with_data(ViewsResponse { views })))
}

fn parse_view_ids(raw: &[String]) -> Vec<Uuid> {
  let mut seen = HashSet::new();
  raw
    .iter()
    .flat_map(|s| s.split(','))
    .filter_map(|s| Uuid::parse_str(s.trim()).ok())
    .filter(|id| seen.insert(*id))
    .take(MAX_BATCH_VIEW_IDS)
    .collect()
}

async fn get_views_handler(
  user_uuid: UserUuid,
  workspace_id: web::Path<Uuid>,
  query: web::Query<ViewsQuery>,
  state: Data<AppState>,
  req: HttpRequest,
) -> Result<Json<AppResponse<ViewsResponse>>> {
  let query = query.into_inner();
  batch_views(
    &state,
    &req,
    &user_uuid,
    workspace_id.into_inner(),
    query.depth,
    query.view_ids.into_iter().collect(),
  )
  .await
}

async fn post_views_handler(
  user_uuid: UserUuid,
  workspace_id: web::Path<Uuid>,
  body: Json<ViewsBody>,
  state: Data<AppState>,
  req: HttpRequest,
) -> Result<Json<AppResponse<ViewsResponse>>> {
  let body = body.into_inner();
  batch_views(
    &state,
    &req,
    &user_uuid,
    workspace_id.into_inner(),
    body.depth,
    body.view_ids,
  )
  .await
}

fn tree_contains(tree: &FolderView, id: &Uuid) -> bool {
  tree.view_id == *id || tree.children.iter().any(|c| tree_contains(c, id))
}

async fn get_view_navigation_handler(
  user_uuid: UserUuid,
  path: web::Path<(Uuid, Uuid)>,
  query: web::Query<DepthQuery>,
  state: Data<AppState>,
  req: HttpRequest,
) -> Result<Json<AppResponse<FolderView>>> {
  let (workspace_id, view_id) = path.into_inner();
  let depth = clamp_view_depth(query.depth, 0);
  let uid = state.user_cache.get_user_uid(&user_uuid).await?;
  require_member(&state, uid, workspace_id).await?;
  let user = realtime_user_for_web_request(req.headers(), uid)?;
  let (folder, published) = load_folder_and_published_ids(&state, user, workspace_id).await?;

  let tree = match navigation_path_ids(&folder, workspace_id, &view_id, uid) {
    Some(nav_path) => {
      let build_depth = ((nav_path.len() as u32 - 1) + depth).min(MAX_NAVIGATION_BUILD_DEPTH);
      let mut tree = collab_folder_to_folder_view(
        workspace_id,
        &nav_path[0],
        &folder,
        build_depth,
        &published,
        uid,
      )
      .map_err(|e| view_not_found(&view_id, e))?;
      if !tree_contains(&tree, &view_id) {
        return Err(
          AppError::RecordNotFound(format!("view {} not found or not accessible", view_id)).into(),
        );
      }
      prune_navigation_tree(&mut tree, &nav_path, depth);
      tree
    },
    // The workspace root itself: its subtree is already the navigation tree.
    None if view_id == workspace_id => {
      collab_folder_to_folder_view(workspace_id, &workspace_id, &folder, depth, &published, uid)
        .map_err(|e| view_not_found(&view_id, e))?
    },
    None => {
      return Err(
        AppError::RecordNotFound(format!("view {} not found or not accessible", view_id)).into(),
      )
    },
  };
  Ok(Json(AppResponse::Ok().with_data(tree)))
}

// ---------------------------------------------------------------------------------------------
// T4: database list page
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct DatabaseListQuery {
  pub offset: Option<usize>,
  pub limit: Option<usize>,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct DatabaseViewItem {
  pub view_id: String,
  pub layout: ViewLayout,
  pub is_container: bool,
  pub embedded: bool,
  pub name: String,
  pub icon: Option<ViewIcon>,
  pub parent_view_id: Option<String>,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct DatabaseWithViews {
  pub database_id: String,
  pub views: Vec<DatabaseViewItem>,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct DatabaseListPage {
  pub databases: Vec<DatabaseWithViews>,
  pub has_more: bool,
}

/// The folder facts the database list needs, decoupled from `collab_folder::View`.
#[derive(Debug, Clone)]
pub struct ViewFacts {
  pub id: String,
  pub name: String,
  pub icon: Option<ViewIcon>,
  pub layout: ViewLayout,
  pub parent_view_id: Option<String>,
  pub extra: Option<Value>,
}

impl ViewFacts {
  fn extra_bool(&self, key: &str) -> bool {
    self
      .extra
      .as_ref()
      .and_then(|e| e.get(key))
      .and_then(|v| v.as_bool())
      .unwrap_or(false)
  }

  fn extra_database_id(&self) -> Option<&str> {
    self
      .extra
      .as_ref()
      .and_then(|e| e.get("database_id"))
      .and_then(|v| v.as_str())
  }

  fn to_item(&self) -> DatabaseViewItem {
    DatabaseViewItem {
      view_id: self.id.clone(),
      layout: self.layout.clone(),
      is_container: self.extra_bool("is_database_container"),
      embedded: self.extra_bool("embedded"),
      name: self.name.clone(),
      icon: self.icon.clone(),
      parent_view_id: self.parent_view_id.clone().filter(|p| !p.is_empty()),
    }
  }
}

/// `metas`: (database_id, linked view ids) in workspace-database order.
/// Linked views come first (in link order), then folder container views whose
/// `extra.database_id` points at the database. Trashed/unknown views are dropped and
/// databases left without any view are omitted.
pub fn build_database_list_page(
  metas: &[(String, Vec<String>)],
  views: &HashMap<String, ViewFacts>,
  trash: &HashSet<String>,
  offset: usize,
  limit: usize,
) -> DatabaseListPage {
  let mut containers: HashMap<&str, Vec<&ViewFacts>> = HashMap::new();
  for view in views.values() {
    if view.extra_bool("is_database_container") && !trash.contains(&view.id) {
      if let Some(database_id) = view.extra_database_id() {
        containers.entry(database_id).or_default().push(view);
      }
    }
  }

  let mut all: Vec<DatabaseWithViews> = Vec::new();
  for (database_id, linked) in metas {
    let mut seen = HashSet::new();
    let mut items = Vec::new();
    for view_id in linked {
      if trash.contains(view_id) || !seen.insert(view_id.clone()) {
        continue;
      }
      if let Some(view) = views.get(view_id) {
        items.push(view.to_item());
      }
    }
    if let Some(found) = containers.get(database_id.as_str()) {
      let mut found = found.clone();
      found.sort_by(|a, b| a.id.cmp(&b.id));
      for view in found {
        if seen.insert(view.id.clone()) {
          items.push(view.to_item());
        }
      }
    }
    if !items.is_empty() {
      all.push(DatabaseWithViews {
        database_id: database_id.clone(),
        views: items,
      });
    }
  }

  let total = all.len();
  let limit = limit.clamp(1, MAX_DATABASE_PAGE_SIZE);
  let databases: Vec<_> = all.into_iter().skip(offset).take(limit).collect();
  DatabaseListPage {
    has_more: offset.saturating_add(databases.len()) < total,
    databases,
  }
}

/// (database_id, linked view ids) for every database of the workspace.
async fn load_database_metas(
  state: &AppState,
  workspace_id: Uuid,
) -> Result<Vec<(String, Vec<String>)>, AppError> {
  let ws_db_oid = select_workspace_database_oid(&state.pg_pool, &workspace_id).await?;
  let mut ws_body_collab = get_latest_collab(
    &state.collab_storage,
    GetCollabOrigin::Server,
    workspace_id,
    ws_db_oid,
    CollabType::WorkspaceDatabase,
    default_client_id(),
  )
  .await?;
  let ws_body = WorkspaceDatabaseBody::open(&mut ws_body_collab).map_err(|e| {
    AppError::Internal(anyhow::anyhow!(
      "Failed to open workspace database body: {:?}",
      e
    ))
  })?;
  let metas = ws_body.get_all_meta(&ws_body_collab.transact());
  Ok(
    metas
      .into_iter()
      .map(|m| (m.database_id, m.linked_views))
      .collect(),
  )
}

/// `GET /{ws}/database`: the legacy `Vec<AFDatabase>` unless `offset` is present, in which
/// case the paged shape the new web client expects is returned.
pub async fn list_database_dispatch_handler(
  user_uuid: UserUuid,
  workspace_id: web::Path<Uuid>,
  query: web::Query<DatabaseListQuery>,
  state: Data<AppState>,
) -> Result<actix_web::HttpResponse> {
  let uid = state.user_cache.get_user_uid(&user_uuid).await?;
  let workspace_id = workspace_id.into_inner();
  let query = query.into_inner();

  let Some(offset) = query.offset else {
    let dbs = crate::biz::collab::ops::list_database(
      &state.pg_pool,
      &state.ws_server,
      &state.collab_storage,
      uid,
      workspace_id,
    )
    .await?;
    return Ok(actix_web::HttpResponse::Ok().json(AppResponse::Ok().with_data(dbs)));
  };

  let metas = load_database_metas(&state, workspace_id).await?;
  let folder = state.ws_server.get_folder(workspace_id).await?;
  let trash: HashSet<String> = folder
    .get_all_trash_sections(uid)
    .into_iter()
    .map(|s| s.id)
    .collect();
  let views: HashMap<String, ViewFacts> = folder
    .get_all_views(uid)
    .into_iter()
    .map(|v| {
      let facts = ViewFacts {
        id: v.id.clone(),
        name: v.name.clone(),
        icon: v.icon.clone().map(to_dto_view_icon),
        layout: to_dto_view_layout(&v.layout),
        parent_view_id: Some(v.parent_view_id.clone()),
        extra: v
          .extra
          .as_deref()
          .and_then(|e| serde_json::from_str::<Value>(e).ok()),
      };
      (v.id.clone(), facts)
    })
    .collect();
  let limit = query.limit.unwrap_or(DEFAULT_DATABASE_PAGE_SIZE);
  let page = build_database_list_page(&metas, &views, &trash, offset, limit);
  Ok(actix_web::HttpResponse::Ok().json(AppResponse::Ok().with_data(page)))
}

// ---------------------------------------------------------------------------------------------
// T6: stubs
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct DatabaseRestoreState {
  database_restore_id: Option<String>,
  version: Option<String>,
}

/// Database history is not supported: there is never a restore in flight and no versions.
async fn get_database_restore_state_handler(
  user_uuid: UserUuid,
  path: web::Path<(Uuid, Uuid)>,
  state: Data<AppState>,
) -> Result<Json<AppResponse<DatabaseRestoreState>>> {
  let (workspace_id, _database_id) = path.into_inner();
  let uid = state.user_cache.get_user_uid(&user_uuid).await?;
  require_member(&state, uid, workspace_id).await?;
  Ok(Json(AppResponse::Ok().with_data(DatabaseRestoreState {
    database_restore_id: None,
    version: None,
  })))
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;

  fn facts(id: &str, layout: ViewLayout, extra: Option<Value>) -> ViewFacts {
    ViewFacts {
      id: id.to_string(),
      name: format!("name-{id}"),
      icon: None,
      layout,
      parent_view_id: Some("parent".to_string()),
      extra,
    }
  }

  fn views(list: Vec<ViewFacts>) -> HashMap<String, ViewFacts> {
    list.into_iter().map(|v| (v.id.clone(), v)).collect()
  }

  #[test]
  fn server_info_json_shape() {
    let v = serde_json::to_value(&LAFORGE_SERVER_INFO).unwrap();
    assert_eq!(v["version"], "0.18.10+laforge");
    assert_eq!(v["enable_atomic_creation"], true);
    assert_eq!(v["enable_atomic_creation_defaults"], true);
    assert_eq!(v["enable_page_history"], false);
    assert_eq!(v["enable_database_history"], false);
    assert_eq!(v["self_hosted"], true);
    assert!(v.get("max_update_bytes").is_none());
    assert!(v.get("max_slow_sync_update_bytes").is_none());
    let parsed = semver::Version::parse(LAFORGE_SERVER_INFO.version).unwrap();
    assert!(parsed.pre.is_empty() && parsed >= semver::Version::new(0, 18, 10));
  }

  #[test]
  fn auth_providers_shape() {
    let p = auth_providers_from_settings(vec!["google".into()], true, false, true);
    let v = serde_json::to_value(&p).unwrap();
    assert_eq!(v["count"], 3);
    assert_eq!(v["providers"], json!(["password", "magic_link", "google"]));
    assert_eq!(v["signup_disabled"], true);
    assert_eq!(v["mailer_autoconfirm"], false);
  }

  #[test]
  fn magic_link_is_opt_in() {
    let off = auth_providers_from_settings(vec!["google".into()], false, false, false);
    assert_eq!(off.providers, vec!["password", "google"]);
    assert_eq!(off.count, 2);
    assert!(!magic_link_flag(None));
    assert!(!magic_link_flag(Some("false")));
    assert!(!magic_link_flag(Some("")));
    assert!(magic_link_flag(Some("true")));
    assert!(magic_link_flag(Some(" TRUE ")));
    assert!(magic_link_flag(Some("1")));
  }

  #[test]
  fn permission_shape_and_levels() {
    let id = Uuid::new_v4();
    let rw = serde_json::to_value(CollabObjectPermission::new(id, 0, id, true, true)).unwrap();
    assert_eq!(rw["access_level"], 50);
    assert_eq!(rw["can_write"], true);
    assert_eq!(rw["governing_view_id"], id.to_string());
    assert_eq!(rw["collab_type"], 0);
    let ro = serde_json::to_value(CollabObjectPermission::new(id, 1, id, true, false)).unwrap();
    assert_eq!(ro["access_level"], 10);
    assert_eq!(ro["can_write"], false);
    assert_eq!(ro["can_comment"], false);
    let none = serde_json::to_value(CollabObjectPermission::new(id, 0, id, false, false)).unwrap();
    assert!(none["access_level"].is_null());
    assert_eq!(none["can_read"], false);
  }

  #[test]
  fn view_ids_parsing_dedupes_and_skips_garbage() {
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let raw = vec![format!("{a},{b},not-a-uuid, {a}")];
    assert_eq!(parse_view_ids(&raw), vec![a, b]);
    assert!(parse_view_ids(&[]).is_empty());
  }

  #[test]
  fn database_page_includes_containers_and_flags() {
    let metas = vec![(
      "db1".to_string(),
      vec!["v1".to_string(), "v2".to_string(), "gone".to_string()],
    )];
    let vs = views(vec![
      facts("v1", ViewLayout::Grid, Some(json!({"database_id": "db1"}))),
      facts(
        "v2",
        ViewLayout::Board,
        Some(json!({"database_id": "db1", "embedded": true})),
      ),
      facts(
        "c1",
        ViewLayout::Grid,
        Some(json!({"database_id": "db1", "is_database_container": true})),
      ),
    ]);
    let page = build_database_list_page(&metas, &vs, &HashSet::new(), 0, 200);
    assert!(!page.has_more);
    assert_eq!(page.databases.len(), 1);
    let items = &page.databases[0].views;
    let ids: Vec<_> = items.iter().map(|i| i.view_id.as_str()).collect();
    assert_eq!(ids, vec!["v1", "v2", "c1"]);
    assert!(!items[0].is_container && !items[0].embedded);
    assert!(items[1].embedded);
    assert!(items[2].is_container);
    let json = serde_json::to_value(&page).unwrap();
    assert_eq!(json["databases"][0]["database_id"], "db1");
    assert_eq!(json["databases"][0]["views"][1]["layout"], 2);
    assert_eq!(json["databases"][0]["views"][0]["parent_view_id"], "parent");
    assert!(json["databases"][0]["views"][0]["icon"].is_null());
  }

  #[test]
  fn database_page_trash_and_pagination() {
    let metas = vec![
      ("a".to_string(), vec!["va".to_string()]),
      ("b".to_string(), vec!["vb".to_string()]),
      ("c".to_string(), vec!["vc".to_string()]),
    ];
    let vs = views(vec![
      facts("va", ViewLayout::Grid, None),
      facts("vb", ViewLayout::Grid, None),
      facts("vc", ViewLayout::Grid, None),
    ]);
    let trash: HashSet<String> = ["vb".to_string()].into();
    let p1 = build_database_list_page(&metas, &vs, &trash, 0, 1);
    assert_eq!(p1.databases.len(), 1);
    assert!(p1.has_more);
    let p2 = build_database_list_page(&metas, &vs, &trash, 1, 1);
    assert_eq!(p2.databases[0].database_id, "c");
    assert!(!p2.has_more);
    let empty = build_database_list_page(&metas, &vs, &trash, 5, 10);
    assert!(empty.databases.is_empty() && !empty.has_more);
  }
}

//! Compatibility stubs for routes the AppFlowy-Web client calls but this server has no backing
//! feature for (notifications, shared-with-me, integrations, error catalog, health).
//!
//! Registered with full paths before `workspace_scope` / `sharing_scope`, like `laforge_atomic`.
//! Every response is the `AppResponse` envelope unless the web client reads something else.

use crate::api::laforge_compat::require_member;
use crate::biz::authentication::jwt::UserUuid;
use crate::state::AppState;
use actix_web::web::{Data, Json};
use actix_web::{web, HttpResponse, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use shared_entity::dto::workspace_dto::FolderView;
use shared_entity::response::AppResponse;
use uuid::Uuid;

pub fn laforge_stub_routes(cfg: &mut web::ServiceConfig) {
  cfg
    .service(
      web::resource("/api/workspace/{workspace_id}/notifications")
        .route(web::get().to(list_notifications_handler)),
    )
    .service(
      web::resource("/api/workspace/{workspace_id}/notifications/unread-count")
        .route(web::get().to(unread_count_handler)),
    )
    .service(
      web::resource("/api/workspace/{workspace_id}/notifications/read")
        .route(web::post().to(notification_noop_handler)),
    )
    .service(
      web::resource("/api/workspace/{workspace_id}/notifications/read-all")
        .route(web::post().to(notification_noop_handler)),
    )
    .service(
      web::resource("/api/workspace/{workspace_id}/notifications/archive")
        .route(web::post().to(notification_noop_handler)),
    )
    .service(
      web::resource("/api/workspace/{workspace_id}/notifications/archive-all")
        .route(web::post().to(notification_noop_handler)),
    )
    .service(
      web::resource("/api/sharing/workspace/{workspace_id}/view/{view_id}")
        .route(web::get().to(shared_with_me_handler)),
    )
    .service(
      web::resource("/api/integrations/connections")
        .route(web::get().to(list_connections_handler)),
    )
    .service(web::resource("/api/error-messages").route(web::get().to(error_messages_handler)))
    .service(web::resource("/api/health").route(web::get().to(health_handler)));
}

// --- notifications (web: components/notifications/types.ts) ---

/// `AFNotificationListResponse`.
#[derive(Debug, Serialize, PartialEq)]
pub struct NotificationList {
  pub notifications: Vec<serde_json::Value>,
  pub has_more: bool,
}

/// `AFUnreadCountResponse`.
#[derive(Debug, Serialize, PartialEq)]
pub struct NotificationUnreadCount {
  pub unread_count: u32,
}

async fn member_check(
  state: &AppState,
  user_uuid: &UserUuid,
  workspace_id: Uuid,
) -> Result<()> {
  let uid = state.user_cache.get_user_uid(user_uuid).await?;
  require_member(state, uid, workspace_id).await?;
  Ok(())
}

async fn list_notifications_handler(
  user_uuid: UserUuid,
  path: web::Path<Uuid>,
  state: Data<AppState>,
) -> Result<Json<AppResponse<NotificationList>>> {
  member_check(&state, &user_uuid, path.into_inner()).await?;
  Ok(Json(AppResponse::Ok().with_data(NotificationList {
    notifications: vec![],
    has_more: false,
  })))
}

async fn unread_count_handler(
  user_uuid: UserUuid,
  path: web::Path<Uuid>,
  state: Data<AppState>,
) -> Result<Json<AppResponse<NotificationUnreadCount>>> {
  member_check(&state, &user_uuid, path.into_inner()).await?;
  Ok(Json(
    AppResponse::Ok().with_data(NotificationUnreadCount { unread_count: 0 }),
  ))
}

/// read / read-all / archive / archive-all: nothing is stored, so there is nothing to change.
async fn notification_noop_handler(
  user_uuid: UserUuid,
  path: web::Path<Uuid>,
  state: Data<AppState>,
) -> Result<Json<AppResponse<()>>> {
  member_check(&state, &user_uuid, path.into_inner()).await?;
  Ok(Json(AppResponse::Ok()))
}

// --- shared with me (web: AccessService.getShareWithMe -> View) ---

/// Empty "Shared with me" root: no guest editors, so nothing is ever shared.
fn empty_shared_root(workspace_id: Uuid) -> FolderView {
  let now = Utc::now();
  FolderView {
    view_id: workspace_id,
    name: String::new(),
    created_at: now,
    last_edited_time: now,
    ..Default::default()
  }
}

async fn shared_with_me_handler(
  user_uuid: UserUuid,
  path: web::Path<(Uuid, Uuid)>,
  state: Data<AppState>,
) -> Result<Json<AppResponse<FolderView>>> {
  let (workspace_id, _view_id) = path.into_inner();
  member_check(&state, &user_uuid, workspace_id).await?;
  Ok(Json(AppResponse::Ok().with_data(empty_shared_root(workspace_id))))
}

// --- integrations (web: integration-api.ts listConnections -> { connections }) ---

#[derive(Debug, Deserialize)]
struct ConnectionsQuery {
  workspace_id: Uuid,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct ConnectionsPayload {
  pub connections: Vec<serde_json::Value>,
}

async fn list_connections_handler(
  user_uuid: UserUuid,
  query: web::Query<ConnectionsQuery>,
  state: Data<AppState>,
) -> Result<Json<AppResponse<ConnectionsPayload>>> {
  member_check(&state, &user_uuid, query.workspace_id).await?;
  Ok(Json(
    AppResponse::Ok().with_data(ConnectionsPayload { connections: vec![] }),
  ))
}

// --- error catalog (web: application/errors/server-error-catalog.ts) ---

/// The web rejects a catalog with zero `reasons`, so an "empty catalog" cannot be expressed. A
/// 204 is the documented way to say "no server copy": the client keeps its bundled wording.
/// Public and unauthenticated (the client fetches it with `credentials: omit`).
async fn error_messages_handler() -> HttpResponse {
  HttpResponse::NoContent().finish()
}

// --- health ---

async fn health_handler() -> HttpResponse {
  HttpResponse::Ok().content_type("text/plain; charset=utf-8").body("OK")
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;

  #[test]
  fn notification_list_shape() {
    let v = serde_json::to_value(NotificationList {
      notifications: vec![],
      has_more: false,
    })
    .unwrap();
    assert_eq!(v, json!({"notifications": [], "has_more": false}));
    let env = serde_json::to_value(AppResponse::Ok().with_data(v)).unwrap();
    assert_eq!(env["code"], 0);
    assert_eq!(env["data"]["notifications"], json!([]));
  }

  #[test]
  fn unread_count_shape() {
    let v = serde_json::to_value(NotificationUnreadCount { unread_count: 0 }).unwrap();
    assert_eq!(v, json!({"unread_count": 0}));
  }

  #[test]
  fn shared_root_is_empty_view() {
    let id = Uuid::new_v4();
    let v = serde_json::to_value(empty_shared_root(id)).unwrap();
    assert_eq!(v["view_id"], id.to_string());
    assert_eq!(v["children"], json!([]));
    assert_eq!(v["has_children"], false);
    assert!(v["extra"].is_null());
  }

  #[test]
  fn connections_shape() {
    let v = serde_json::to_value(ConnectionsPayload { connections: vec![] }).unwrap();
    assert_eq!(v, json!({"connections": []}));
  }

  #[actix_web::test]
  async fn error_messages_is_204_and_health_is_ok() {
    let resp = error_messages_handler().await;
    assert_eq!(resp.status(), 204);
    let resp = health_handler().await;
    assert_eq!(resp.status(), 200);
  }
}

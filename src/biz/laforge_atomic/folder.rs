use crate::biz::workspace::page_view::update_workspace_folder_data;
use crate::state::AppState;
use anyhow::anyhow;
use app_error::AppError;
use appflowy_collaborate::ws2::WorkspaceCollabInstanceCache;
use collab_folder::{Folder, View, ViewIcon, ViewLayout};
use collab_rt_entity::user::RealtimeUser;
use std::time::Duration;
use uuid::Uuid;

const READ_YOUR_WRITES_ATTEMPTS: usize = 20;
const READ_YOUR_WRITES_INTERVAL: Duration = Duration::from_millis(50);

/// Children ids of a Folder parent, in Folder order (trashed and private views included).
/// `None` when the parent does not exist.
pub fn folder_child_ids(folder: &Folder, parent_view_id: &str, uid: i64) -> Option<Vec<String>> {
  if let Some(parent) = folder.get_view(parent_view_id, uid) {
    return Some(
      parent
        .children
        .iter()
        .map(|child| child.id.clone())
        .collect(),
    );
  }
  // The workspace root may have no view entry of its own.
  if folder.get_workspace_id().as_deref() == Some(parent_view_id) {
    return Some(
      folder
        .get_views_belong_to(parent_view_id, uid)
        .iter()
        .map(|view| view.id.clone())
        .collect(),
    );
  }
  None
}

/// Where a client-chosen view id already is in the Folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestedView {
  Absent,
  /// Already a child of the requested parent: an interrupted attempt of the same request,
  /// adopted as is.
  UnderParent,
  /// Used elsewhere in the Folder.
  Elsewhere,
}

pub fn requested_view(
  folder: &Folder,
  view_id: &str,
  parent_view_id: &str,
  uid: i64,
) -> RequestedView {
  match folder.get_view(view_id, uid) {
    None => RequestedView::Absent,
    Some(view) if view.parent_view_id == parent_view_id => RequestedView::UnderParent,
    Some(_) => RequestedView::Elsewhere,
  }
}

/// A Folder view to create. `index` is the result of `position::resolve_insert_index`
/// (`None` appends).
pub struct FolderViewInsert {
  pub view: View,
  pub index: Option<u32>,
  pub is_favorite: bool,
}

pub fn new_folder_view(
  view_id: &Uuid,
  parent_view_id: &Uuid,
  name: &str,
  layout: ViewLayout,
  uid: i64,
  icon: Option<ViewIcon>,
  extra: Option<String>,
) -> View {
  let mut view = View::new(
    view_id.to_string(),
    parent_view_id.to_string(),
    name.to_string(),
    layout,
    Some(uid),
  );
  view.icon = icon;
  view.extra = extra;
  view.last_edited_by = Some(uid);
  view
}

/// Inserts the views in order (a parent before its children) and returns the Folder update (v1).
pub fn encode_folder_insert(
  folder: &mut Folder,
  uid: i64,
  inserts: Vec<FolderViewInsert>,
) -> Vec<u8> {
  let mut txn = folder.collab.transact_mut();
  for insert in inserts {
    let view_id = insert.view.id.clone();
    folder
      .body
      .views
      .insert(&mut txn, insert.view, insert.index, uid);
    if insert.is_favorite {
      folder.body.views.update_view(
        &mut txn,
        &view_id,
        |update| update.set_favorite(true).done(),
        uid,
      );
    }
  }
  txn.encode_update_v1()
}

/// Publishes a Folder update to the workspace update stream (and its WebSocket subscribers).
pub async fn publish_folder_update(
  state: &AppState,
  user: RealtimeUser,
  workspace_id: Uuid,
  update: Vec<u8>,
) -> Result<(), AppError> {
  update_workspace_folder_data(
    &state.metrics.appflowy_web_metrics,
    &state.ws_server,
    user,
    workspace_id,
    update,
  )
  .await
}

/// Read-your-writes: waits until a Folder read (the one used by the view routes) shows every
/// `(view_id, parent_view_id)` linked both ways, else answers RetryLater. Folder reads replay the
/// update stream past the last snapshot, so this normally succeeds on the first read.
pub async fn await_folder_views(
  state: &AppState,
  workspace_id: Uuid,
  expected: &[(String, String)],
  uid: i64,
) -> Result<(), AppError> {
  for attempt in 0..READ_YOUR_WRITES_ATTEMPTS {
    let folder = state.ws_server.get_folder(workspace_id).await?;
    let visible = expected.iter().all(|(view_id, parent_view_id)| {
      requested_view(&folder, view_id, parent_view_id, uid) == RequestedView::UnderParent
        && folder_child_ids(&folder, parent_view_id, uid)
          .is_some_and(|children| children.contains(view_id))
    });
    if visible {
      return Ok(());
    }
    if attempt + 1 < READ_YOUR_WRITES_ATTEMPTS {
      tokio::time::sleep(READ_YOUR_WRITES_INTERVAL).await;
    }
  }
  Err(AppError::RetryLater(anyhow!(
    "the created views are not visible in the workspace folder yet"
  )))
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::biz::laforge_atomic::position::{resolve_insert_index, CreationViewPosition};
  use collab::core::collab::{default_client_id, Collab, CollabOptions, DataSource};
  use collab::core::origin::CollabOrigin;
  use collab_folder::{FolderData, Workspace};
  use yrs::updates::decoder::Decode;
  use yrs::{ReadTxn, StateVector};

  const UID: i64 = 1;

  fn folder_with_children(workspace_id: &Uuid, children: &[Uuid]) -> Folder {
    let options = CollabOptions::new(workspace_id.to_string(), default_client_id());
    let collab = Collab::new_with_options(CollabOrigin::Empty, options).unwrap();
    let workspace = Workspace::new(workspace_id.to_string(), "ws".to_string(), UID);
    let mut data = FolderData::new(UID, workspace);
    for child in children {
      data.views.push(View::new(
        child.to_string(),
        workspace_id.to_string(),
        "child".to_string(),
        ViewLayout::Document,
        Some(UID),
      ));
    }
    Folder::create(collab, None, data)
  }

  #[test]
  fn inserts_at_the_resolved_index_and_replays_on_another_folder() {
    let workspace_id = Uuid::new_v4();
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let mut folder = folder_with_children(&workspace_id, &[a, b]);
    let ws = workspace_id.to_string();

    let siblings = folder_child_ids(&folder, &ws, UID).unwrap();
    assert_eq!(siblings, vec![a.to_string(), b.to_string()]);
    let base = folder
      .collab
      .transact()
      .encode_state_as_update_v1(&StateVector::default());

    let new_id = Uuid::new_v4();
    assert_eq!(
      requested_view(&folder, &new_id.to_string(), &ws, UID),
      RequestedView::Absent
    );
    let index = resolve_insert_index(
      &siblings,
      &CreationViewPosition::After {
        view_id: a.to_string(),
      },
    )
    .unwrap();
    let view = new_folder_view(
      &new_id,
      &workspace_id,
      "Doc",
      ViewLayout::Document,
      UID,
      None,
      Some(r#"{"cover":null}"#.to_string()),
    );
    let update = encode_folder_insert(
      &mut folder,
      UID,
      vec![FolderViewInsert {
        view,
        index,
        is_favorite: true,
      }],
    );

    let children = folder_child_ids(&folder, &ws, UID).unwrap();
    assert_eq!(
      children,
      vec![a.to_string(), new_id.to_string(), b.to_string()]
    );
    assert_eq!(
      requested_view(&folder, &new_id.to_string(), &ws, UID),
      RequestedView::UnderParent
    );
    assert_eq!(
      requested_view(&folder, &new_id.to_string(), &a.to_string(), UID),
      RequestedView::Elsewhere
    );
    let created = folder.get_view(&new_id.to_string(), UID).unwrap();
    assert!(created.is_favorite);
    assert_eq!(created.extra.as_deref(), Some(r#"{"cover":null}"#));

    // The published update alone carries the insertion to a replica of the previous state.
    let mut replica = Folder::from_collab_doc_state(
      CollabOrigin::Server,
      DataSource::DocStateV1(base),
      &ws,
      default_client_id(),
    )
    .unwrap();
    assert!(replica.get_view(&new_id.to_string(), UID).is_none());
    {
      let mut txn = replica.collab.transact_mut();
      txn
        .apply_update(yrs::Update::decode_v1(&update).unwrap())
        .unwrap();
    }
    assert_eq!(
      folder_child_ids(&replica, &ws, UID).unwrap(),
      vec![a.to_string(), new_id.to_string(), b.to_string()]
    );
  }

  #[test]
  fn missing_parent_has_no_children() {
    let workspace_id = Uuid::new_v4();
    let folder = folder_with_children(&workspace_id, &[]);
    assert!(folder_child_ids(&folder, &Uuid::new_v4().to_string(), UID).is_none());
    assert_eq!(
      folder_child_ids(&folder, &workspace_id.to_string(), UID).unwrap(),
      Vec::<String>::new()
    );
  }
}

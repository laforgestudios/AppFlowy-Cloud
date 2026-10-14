//! JSON snapshot of a published page, as read by the web client's publish renderer
//! (`appflowy-web/src/application/publish-snapshot`).
//!
//! Built only from what was published: the stored view metadata and the published
//! blob. Nothing is read from the live workspace.

pub mod slate;

use app_error::AppError;
use collab::core::collab::{default_client_id, CollabOptions, DataSource};
use collab::core::origin::CollabOrigin;
use collab::preclude::Collab;
use collab_document::document::Document;
use serde_json::{json, Map, Value};
use shared_entity::dto::publish_dto::{
  PublishDatabaseData, PublishDatabaseDataWithNonUuidRelations,
};
use tracing::warn;

pub const SNAPSHOT_SCHEMA_VERSION: u8 = 1;

/// `ViewLayout::Document`; every other publishable layout is a database.
const DOCUMENT_LAYOUT: i64 = 0;

/// Builds the snapshot from the published metadata (`PublishViewMetaData` JSON) and the
/// published blob (document doc state, or `PublishDatabaseData` JSON for a database).
pub fn build_published_snapshot(
  namespace: &str,
  publish_name: &str,
  metadata: &Value,
  blob: &[u8],
) -> Result<Value, AppError> {
  let view_meta = metadata
    .get("view")
    .and_then(Value::as_object)
    .ok_or_else(|| AppError::NoRequiredData("published view metadata is missing".to_string()))?;
  let view_id = view_meta
    .get("view_id")
    .and_then(Value::as_str)
    .ok_or_else(|| AppError::NoRequiredData("published view id is missing".to_string()))?
    .to_string();
  let layout = view_meta
    .get("layout")
    .and_then(Value::as_i64)
    .unwrap_or(DOCUMENT_LAYOUT);

  let mut view = Map::new();
  view.insert("viewId".to_string(), Value::String(view_id.clone()));
  view.insert(
    "name".to_string(),
    view_meta.get("name").cloned().unwrap_or_else(|| json!("")),
  );
  view.insert("icon".to_string(), object_or_null(view_meta.get("icon")));
  view.insert("extra".to_string(), extra_string(view_meta.get("extra")));
  view.insert("layout".to_string(), json!(layout));
  for (meta_key, key) in [
    ("child_views", "childViews"),
    ("ancestor_views", "ancestorViews"),
  ] {
    if let Some(Value::Array(views)) = metadata.get(meta_key) {
      view.insert(key.to_string(), Value::Array(views.clone()));
    }
  }

  let mut snapshot = Map::new();
  snapshot.insert("schemaVersion".to_string(), json!(SNAPSHOT_SCHEMA_VERSION));
  snapshot.insert("namespace".to_string(), json!(namespace));
  snapshot.insert("publishName".to_string(), json!(publish_name));

  if layout == DOCUMENT_LAYOUT {
    let children = document_slate_children(&view_id, blob)?;
    snapshot.insert("kind".to_string(), json!("document"));
    let document = Map::from_iter([("children".to_string(), Value::Array(children))]);
    snapshot.insert("document".to_string(), Value::Object(document));
  } else {
    let data: PublishDatabaseData =
      serde_json::from_slice::<PublishDatabaseDataWithNonUuidRelations>(blob)
        .map_err(|err| AppError::Internal(anyhow::anyhow!("invalid published database: {err}")))?
        .into();
    let (database, relations) = database_snapshot(&view_id, data)?;
    if let Some(ids) = database.get("visibleViewIds") {
      view.insert("visibleViewIds".to_string(), ids.clone());
    }
    view.insert("databaseRelations".to_string(), Value::Object(relations));
    snapshot.insert("kind".to_string(), json!("database"));
    snapshot.insert("database".to_string(), Value::Object(database));
  }

  snapshot.insert("view".to_string(), Value::Object(view));
  Ok(Value::Object(snapshot))
}

fn object_or_null(value: Option<&Value>) -> Value {
  match value {
    Some(value @ Value::Object(_)) => value.clone(),
    _ => Value::Null,
  }
}

/// The web expects `extra` as a JSON string.
fn extra_string(value: Option<&Value>) -> Value {
  match value {
    Some(Value::String(extra)) => Value::String(extra.clone()),
    Some(value @ (Value::Object(_) | Value::Array(_))) => Value::String(value.to_string()),
    _ => Value::Null,
  }
}

fn document_slate_children(view_id: &str, doc_state: &[u8]) -> Result<Vec<Value>, AppError> {
  if doc_state.is_empty() {
    return Ok(vec![]);
  }
  let document = open_document(view_id, doc_state)?;
  let data = document
    .get_document_data()
    .map_err(|err| AppError::Internal(anyhow::anyhow!("invalid published document: {err}")))?;
  Ok(slate::document_to_slate_children(&data))
}

fn open_document(object_id: &str, doc_state: &[u8]) -> Result<Document, AppError> {
  Document::open_with_options(
    CollabOrigin::Empty,
    DataSource::DocStateV1(doc_state.to_vec()),
    object_id,
    default_client_id(),
  )
  .map_err(|err| AppError::Internal(anyhow::anyhow!("invalid published document: {err}")))
}

fn collab_json(object_id: &str, doc_state: &[u8]) -> Result<Value, AppError> {
  let options = CollabOptions::new(object_id.to_string(), default_client_id())
    .with_data_source(DataSource::DocStateV1(doc_state.to_vec()));
  let collab = Collab::new_with_options(CollabOrigin::Empty, options)
    .map_err(|err| AppError::Internal(anyhow::anyhow!("invalid published collab: {err}")))?;
  Ok(collab.to_json_value())
}

/// The `database` part of the snapshot (the raw collab JSON the web's
/// `database-yjs-render-bridge` rebuilds Yjs documents from), and the database
/// relations of the view.
fn database_snapshot(
  view_id: &str,
  data: PublishDatabaseData,
) -> Result<(Map<String, Value>, Map<String, Value>), AppError> {
  let database = match collab_json(view_id, &data.database_collab)? {
    Value::Object(mut root) => match root.remove("database") {
      Some(database @ Value::Object(_)) => database,
      _ => json!({}),
    },
    _ => json!({}),
  };
  let database_id = database
    .get("id")
    .and_then(Value::as_str)
    .map(str::to_string)
    .or_else(|| {
      data
        .database_relations
        .keys()
        .next()
        .map(|id| id.to_string())
    })
    .unwrap_or_default();

  let mut rows = Map::new();
  for (row_id, doc_state) in &data.database_row_collabs {
    let row_id = row_id.to_string();
    match collab_json(&row_id, doc_state) {
      Ok(row) => {
        rows.insert(row_id, row);
      },
      Err(err) => warn!("skip published database row {}: {}", row_id, err),
    }
  }

  let mut row_documents = Map::new();
  for (document_id, doc_state) in &data.database_row_document_collabs {
    let document_id = document_id.to_string();
    let document_data = open_document(&document_id, doc_state).and_then(|document| {
      document
        .get_document_data()
        .map_err(|err| AppError::Internal(anyhow::anyhow!("{err}")))
    });
    match document_data.and_then(|data| serde_json::to_value(data).map_err(AppError::from)) {
      Ok(document_data) => {
        row_documents.insert(document_id, json!({ "data": document_data }));
      },
      Err(err) => warn!("skip published row document {}: {}", document_id, err),
    }
  }

  let visible_view_ids: Vec<String> = data
    .visible_database_view_ids
    .iter()
    .map(|id| id.to_string())
    .collect();
  let is_database_view = database
    .get("views")
    .and_then(Value::as_object)
    .is_some_and(|views| views.contains_key(view_id));
  let active_view_id = if is_database_view || visible_view_ids.is_empty() {
    view_id.to_string()
  } else {
    visible_view_ids[0].clone()
  };
  let database_relations: Map<String, Value> = data
    .database_relations
    .iter()
    .map(|(database_id, view_id)| (database_id.to_string(), json!(view_id.to_string())))
    .collect();

  let mut raw = Map::new();
  raw.insert("database".to_string(), database);
  raw.insert("rows".to_string(), Value::Object(rows));
  raw.insert("row_documents".to_string(), Value::Object(row_documents));

  let mut snapshot = Map::new();
  snapshot.insert("databaseId".to_string(), json!(database_id));
  snapshot.insert("activeViewId".to_string(), json!(active_view_id));
  snapshot.insert("visibleViewIds".to_string(), json!(visible_view_ids));
  snapshot.insert("raw".to_string(), Value::Object(raw));
  Ok((snapshot, database_relations))
}

#[cfg(test)]
mod tests;

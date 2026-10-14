//! Port of the web client's Yjs -> Slate document converter
//! (`appflowy-web/src/application/slate-yjs/utils/convert.ts`, `traverseBlock` and
//! `deltaInsertToSlateNode`), so a published document can be sent as the Slate
//! `Descendant[]` the web renderer consumes.
//!
//! Pure: works on [DocumentData] and has no server dependency.

use std::cmp::Ordering;
use std::collections::HashSet;

use collab_document::blocks::DocumentData;
use serde_json::{json, Map, Value};

const PAGE: &str = "page";
const TABLE: &str = "table";
const TABLE_CELL: &str = "table/cell";
const TEXT: &str = "text";
const DELTA: &str = "delta";

/// Containers whose text is never rendered, even if they carry an `external_id`.
const TEXTLESS_CONTAINERS: [&str; 8] = [
  "simple_table",
  "simple_table_row",
  "simple_table_cell",
  "ai_meeting",
  "ai_meeting_summary",
  "ai_meeting_notes",
  "ai_meeting_transcription",
  "ai_meeting_speaker",
];

/// Children trees deeper than this are cut. The web converter has no limit and would
/// overflow its stack; the server must not.
const MAX_DEPTH: usize = 128;

/// Returns the Slate nodes of the page block's children: the `document.children` of a
/// published document snapshot. Empty when the page block is missing.
pub fn document_to_slate_children(data: &DocumentData) -> Vec<Value> {
  if !data.blocks.contains_key(&data.page_id) {
    return vec![];
  }
  let mut path = HashSet::new();
  match traverse_block(&data.page_id, data, &mut path) {
    Some(Value::Object(mut page)) => match page.remove("children") {
      Some(Value::Array(children)) => children,
      _ => vec![],
    },
    _ => vec![],
  }
}

fn traverse_block<'a>(
  id: &'a str,
  data: &'a DocumentData,
  path: &mut HashSet<&'a str>,
) -> Option<Value> {
  let Some(block) = data.blocks.get(id) else {
    return Some(json!({
      "blockId": id,
      "type": "block_not_found",
      "data": {},
      "relationId": id,
      "children": [{ "type": TEXT, "textId": id, "children": [{ "text": "" }] }],
    }));
  };

  // A block reachable from itself would recurse forever.
  if path.len() >= MAX_DEPTH || !path.insert(id) {
    return None;
  }
  let mut children: Vec<Value> = data
    .meta
    .children_map
    .get(&block.children)
    .map(|ids| {
      ids
        .iter()
        .filter_map(|child_id| traverse_block(child_id, data, path))
        .collect()
    })
    .unwrap_or_default();
  path.remove(id);

  let block_data = Value::Object(block.data.clone().into_iter().collect());
  match block.ty.as_str() {
    TABLE => children.sort_by(compare_table_cells),
    TABLE_CELL => children.truncate(1),
    _ => {},
  }

  let mut text_id = block.external_id.clone().filter(|id| !id.is_empty());
  if TEXTLESS_CONTAINERS.contains(&block.ty.as_str()) {
    text_id = None;
  }

  // Built by hand rather than with `json!`, which would deep-copy `children`.
  let make_node = |children: Vec<Value>| {
    let mut node = Map::new();
    node.insert("blockId".to_string(), Value::String(block.id.clone()));
    node.insert(
      "relationId".to_string(),
      Value::String(block.children.clone()),
    );
    node.insert("data".to_string(), block_data);
    node.insert("type".to_string(), Value::String(block.ty.clone()));
    node.insert("children".to_string(), Value::Array(children));
    Value::Object(node)
  };

  if block.ty == PAGE {
    return Some(make_node(children));
  }

  let serialized_delta = text_id.as_ref().and_then(|text_id| {
    data
      .meta
      .text_map
      .as_ref()
      .and_then(|text_map| text_map.get(text_id))
  });

  let delta = match serialized_delta {
    Some(serialized) => serde_json::from_str::<Value>(serialized).unwrap_or(Value::Array(vec![])),
    None => {
      if children.is_empty() {
        children.push(json!({ "text": "" }));
      }
      // Older documents keep their text in `data.delta`.
      match block.data.get(DELTA) {
        Some(delta) => {
          text_id = Some(block.id.clone());
          delta.clone()
        },
        None => return Some(make_node(children)),
      }
    },
  };

  // The web converter throws on a delta that is not an array and drops the block.
  let Value::Array(ops) = delta else {
    return None;
  };
  let mut leaves: Vec<Value> = ops.iter().filter_map(delta_insert_to_slate_node).collect();
  if leaves.is_empty() {
    leaves.push(json!({ "text": "" }));
  }

  children.insert(
    0,
    json!({
      "textId": text_id,
      "type": TEXT,
      "children": leaves,
    }),
  );
  Some(make_node(children))
}

/// `{insert, attributes}` -> `{...attributes, text: insert}`, dropping falsy attributes
/// like the web's `dealWithEmptyAttribute`.
pub fn delta_insert_to_slate_node(op: &Value) -> Option<Value> {
  let op = op.as_object()?;
  let mut leaf = Map::new();
  if let Some(Value::Object(attributes)) = op.get("attributes") {
    for (key, value) in attributes {
      if is_truthy(value) {
        leaf.insert(key.clone(), value.clone());
      }
    }
  }
  if let Some(insert) = op.get("insert") {
    leaf.insert(TEXT.to_string(), insert.clone());
  }
  Some(Value::Object(leaf))
}

/// JavaScript truthiness of a JSON value.
fn is_truthy(value: &Value) -> bool {
  match value {
    Value::Null => false,
    Value::Bool(b) => *b,
    Value::Number(n) => n.as_f64().is_some_and(|n| n != 0.0),
    Value::String(s) => !s.is_empty(),
    Value::Array(_) | Value::Object(_) => true,
  }
}

/// Port of `sortTableCells`: by `colPosition`, then `rowPosition`. Cells missing a
/// position keep their relative order.
fn compare_table_cells(a: &Value, b: &Value) -> Ordering {
  let position = |cell: &Value, key: &str| cell.get("data").and_then(|data| data.get(key)).cloned();
  let (Some(a_col), Some(a_row), Some(b_col), Some(b_row)) = (
    position(a, "colPosition"),
    position(a, "rowPosition"),
    position(b, "colPosition"),
    position(b, "rowPosition"),
  ) else {
    return Ordering::Equal;
  };

  if js_strict_equal(&a_col, &b_col) {
    js_subtract_ordering(&a_row, &b_row)
  } else {
    js_subtract_ordering(&a_col, &b_col)
  }
}

fn js_strict_equal(a: &Value, b: &Value) -> bool {
  match (a, b) {
    (Value::Number(a), Value::Number(b)) => a.as_f64() == b.as_f64(),
    _ => a == b,
  }
}

/// Sign of JavaScript's `a - b`; NaN compares as equal, as `Array.prototype.sort` does.
fn js_subtract_ordering(a: &Value, b: &Value) -> Ordering {
  js_to_number(a)
    .partial_cmp(&js_to_number(b))
    .unwrap_or(Ordering::Equal)
}

fn js_to_number(value: &Value) -> f64 {
  match value {
    Value::Null => 0.0,
    Value::Bool(b) => f64::from(u8::from(*b)),
    Value::Number(n) => n.as_f64().unwrap_or(f64::NAN),
    Value::String(s) => {
      let s = s.trim();
      if s.is_empty() {
        0.0
      } else {
        s.parse().unwrap_or(f64::NAN)
      }
    },
    Value::Array(_) | Value::Object(_) => f64::NAN,
  }
}

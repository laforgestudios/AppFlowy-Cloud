//! Expected outputs follow the web converter (`slate-yjs/utils/convert.ts`) and the
//! snapshot fixtures in `publish-snapshot/__fixtures__/published-page-snapshots.ts`.

use std::collections::HashMap;

use collab::core::collab::{default_client_id, CollabOptions};
use collab::core::origin::CollabOrigin;
use collab::preclude::{Collab, MapExt};
use collab_document::blocks::{Block, DocumentData, DocumentMeta};
use collab_document::document::Document;
use serde_json::{json, Value};
use shared_entity::dto::publish_dto::PublishDatabaseData;
use uuid::Uuid;

use super::build_published_snapshot;
use super::slate::document_to_slate_children;

const PAGE_ID: &str = "page";

/// A block of the test document: `text` is its delta (None: no text element).
struct B {
  id: String,
  ty: &'static str,
  data: Value,
  text: Option<Value>,
  children: Vec<B>,
}

fn b(id: &str, ty: &'static str, data: Value, text: Option<Value>, children: Vec<B>) -> B {
  B {
    id: id.to_string(),
    ty,
    data,
    text,
    children,
  }
}

fn text_block(id: &str, ty: &'static str, data: Value, delta: Value, children: Vec<B>) -> B {
  b(id, ty, data, Some(delta), children)
}

fn plain(value: &str) -> Value {
  json!([{ "insert": value }])
}

fn document_data(children: Vec<B>) -> DocumentData {
  let mut data = DocumentData {
    page_id: PAGE_ID.to_string(),
    blocks: HashMap::new(),
    meta: DocumentMeta {
      children_map: HashMap::new(),
      text_map: Some(HashMap::new()),
    },
  };
  let page = b(PAGE_ID, "page", json!({}), None, children);
  add_block(&mut data, &page, "");
  data
}

fn add_block(data: &mut DocumentData, block: &B, parent: &str) {
  let external_id = block.text.as_ref().map(|_| format!("{}-text", block.id));
  if let (Some(text_id), Some(delta)) = (&external_id, &block.text) {
    data
      .meta
      .text_map
      .as_mut()
      .unwrap()
      .insert(text_id.clone(), delta.to_string());
  }
  data.blocks.insert(
    block.id.clone(),
    Block {
      id: block.id.clone(),
      ty: block.ty.to_string(),
      parent: parent.to_string(),
      children: block.id.clone(),
      external_id: external_id.clone(),
      external_type: external_id.map(|_| "text".to_string()),
      data: serde_json::from_value(block.data.clone()).unwrap(),
    },
  );
  data.meta.children_map.insert(
    block.id.clone(),
    block
      .children
      .iter()
      .map(|child| child.id.clone())
      .collect(),
  );
  for child in &block.children {
    add_block(data, child, &block.id);
  }
}

/// Encodes the document like a published blob (the doc state) and decodes it back
/// through the snapshot builder.
fn publish_document(children: Vec<B>) -> Value {
  let document =
    Document::create("doc-view-id", document_data(children), default_client_id()).unwrap();
  let doc_state = document.encode_collab().unwrap().doc_state.to_vec();
  let metadata = json!({
    "view": {
      "view_id": "doc-view-id",
      "name": "Rich document",
      "icon": null,
      "layout": 0,
      "extra": null,
      "created_by": null,
      "last_edited_by": null,
      "last_edited_time": 0,
      "created_at": 0,
      "child_views": null
    },
    "child_views": [],
    "ancestor_views": []
  });
  build_published_snapshot("ns", "rich-document", &metadata, &doc_state).unwrap()
}

/// Expected Slate element. `leaves`: the text element's leaves, None for blocks
/// without text (which get an empty leaf when they have no child either).
fn node(id: &str, ty: &str, data: Value, leaves: Option<Value>, children: Vec<Value>) -> Value {
  let mut all = vec![];
  match leaves {
    Some(leaves) => {
      all.push(json!({ "textId": format!("{id}-text"), "type": "text", "children": leaves }))
    },
    None if children.is_empty() => all.push(json!({ "text": "" })),
    None => {},
  }
  all.extend(children);
  json!({ "blockId": id, "relationId": id, "data": data, "type": ty, "children": all })
}

fn leaf(text: &str) -> Value {
  json!([{ "text": text }])
}

#[test]
fn document_snapshot_envelope() {
  let snapshot = publish_document(vec![text_block(
    "p1",
    "paragraph",
    json!({}),
    plain("Published document body"),
    vec![],
  )]);
  assert_eq!(
    snapshot,
    json!({
      "schemaVersion": 1,
      "kind": "document",
      "namespace": "ns",
      "publishName": "rich-document",
      "view": {
        "viewId": "doc-view-id",
        "name": "Rich document",
        "icon": null,
        "extra": null,
        "layout": 0,
        "childViews": [],
        "ancestorViews": []
      },
      "document": {
        "children": [{
          "blockId": "p1",
          "relationId": "p1",
          "data": {},
          "type": "paragraph",
          "children": [{
            "textId": "p1-text",
            "type": "text",
            "children": [{ "text": "Published document body" }]
          }]
        }]
      }
    })
  );
}

#[test]
fn headings_and_marks() {
  let snapshot = publish_document(vec![
    text_block(
      "h1",
      "heading",
      json!({ "level": 1 }),
      plain("Introduction"),
      vec![],
    ),
    text_block(
      "p1",
      "paragraph",
      json!({}),
      json!([
        { "insert": "Plain, " },
        { "insert": "bold", "attributes": { "bold": true } },
        { "insert": ", " },
        { "insert": "bold italic", "attributes": { "bold": true, "italic": true } },
        { "insert": " code", "attributes": { "code": true } },
        { "insert": " struck", "attributes": { "strikethrough": true } },
        { "insert": " underlined", "attributes": { "underline": true } },
        { "insert": " colored", "attributes": { "font_color": "0xffe8e0ff", "bg_color": "" } },
        { "insert": " a link", "attributes": { "href": "https://appflowy.com" } },
        { "insert": "." }
      ]),
      vec![],
    ),
  ]);
  assert_eq!(
    snapshot["document"]["children"],
    json!([
      node(
        "h1",
        "heading",
        json!({ "level": 1 }),
        Some(leaf("Introduction")),
        vec![]
      ),
      node(
        "p1",
        "paragraph",
        json!({}),
        Some(json!([
          { "text": "Plain, " },
          { "text": "bold", "bold": true },
          { "text": ", " },
          { "text": "bold italic", "bold": true, "italic": true },
          { "text": " code", "code": true },
          { "text": " struck", "strikethrough": true },
          { "text": " underlined", "underline": true },
          // Falsy attributes are dropped, as `dealWithEmptyAttribute` does.
          { "text": " colored", "font_color": "0xffe8e0ff" },
          { "text": " a link", "href": "https://appflowy.com" },
          { "text": "." }
        ])),
        vec![],
      ),
    ])
  );
}

#[test]
fn lists_todo_toggle_quote_callout_code() {
  let snapshot = publish_document(vec![
    text_block(
      "bl1",
      "bulleted_list",
      json!({}),
      plain("First bullet"),
      vec![text_block(
        "bl2",
        "bulleted_list",
        json!({}),
        plain("Nested bullet"),
        vec![text_block(
          "bl3",
          "bulleted_list",
          json!({}),
          plain("Deeper"),
          vec![],
        )],
      )],
    ),
    text_block(
      "nl1",
      "numbered_list",
      json!({ "number": 3 }),
      plain("Step one"),
      vec![],
    ),
    text_block(
      "td1",
      "todo_list",
      json!({ "checked": true }),
      plain("Done task"),
      vec![],
    ),
    text_block(
      "tg1",
      "toggle_list",
      json!({ "collapsed": true }),
      plain("Toggle title"),
      vec![text_block(
        "tg1p",
        "paragraph",
        json!({}),
        plain("Hidden detail"),
        vec![],
      )],
    ),
    text_block("q1", "quote", json!({}), plain("A quotation"), vec![]),
    text_block(
      "c1",
      "callout",
      json!({ "icon": "💡" }),
      plain("Callout body"),
      vec![],
    ),
    text_block(
      "code1",
      "code",
      json!({ "language": "typescript" }),
      plain("const a = 1 < 2;"),
      vec![],
    ),
    // A block with an empty text renders one empty leaf.
    text_block("empty", "paragraph", json!({}), json!([]), vec![]),
  ]);
  assert_eq!(
    snapshot["document"]["children"],
    json!([
      node(
        "bl1",
        "bulleted_list",
        json!({}),
        Some(leaf("First bullet")),
        vec![node(
          "bl2",
          "bulleted_list",
          json!({}),
          Some(leaf("Nested bullet")),
          vec![node(
            "bl3",
            "bulleted_list",
            json!({}),
            Some(leaf("Deeper")),
            vec![]
          )],
        )]
      ),
      node(
        "nl1",
        "numbered_list",
        json!({ "number": 3 }),
        Some(leaf("Step one")),
        vec![]
      ),
      node(
        "td1",
        "todo_list",
        json!({ "checked": true }),
        Some(leaf("Done task")),
        vec![]
      ),
      node(
        "tg1",
        "toggle_list",
        json!({ "collapsed": true }),
        Some(leaf("Toggle title")),
        vec![node(
          "tg1p",
          "paragraph",
          json!({}),
          Some(leaf("Hidden detail")),
          vec![]
        ),]
      ),
      node("q1", "quote", json!({}), Some(leaf("A quotation")), vec![]),
      node(
        "c1",
        "callout",
        json!({ "icon": "💡" }),
        Some(leaf("Callout body")),
        vec![]
      ),
      node(
        "code1",
        "code",
        json!({ "language": "typescript" }),
        Some(leaf("const a = 1 < 2;")),
        vec![]
      ),
      node("empty", "paragraph", json!({}), Some(leaf("")), vec![]),
    ])
  );
}

#[test]
fn void_blocks_get_an_empty_leaf() {
  let snapshot = publish_document(vec![
    b(
      "eq",
      "math_equation",
      json!({ "formula": "E = mc^2" }),
      None,
      vec![],
    ),
    b("div", "divider", json!({}), None, vec![]),
    b(
      "img",
      "image",
      json!({ "url": "https://example.com/image.png" }),
      None,
      vec![],
    ),
    b(
      "sub",
      "sub_page",
      json!({ "view_id": "child-view" }),
      None,
      vec![],
    ),
  ]);
  assert_eq!(
    snapshot["document"]["children"][1],
    json!({
      "blockId": "div",
      "relationId": "div",
      "data": {},
      "type": "divider",
      "children": [{ "text": "" }]
    })
  );
  assert_eq!(
    snapshot["document"]["children"],
    json!([
      node(
        "eq",
        "math_equation",
        json!({ "formula": "E = mc^2" }),
        None,
        vec![]
      ),
      node("div", "divider", json!({}), None, vec![]),
      node(
        "img",
        "image",
        json!({ "url": "https://example.com/image.png" }),
        None,
        vec![]
      ),
      node(
        "sub",
        "sub_page",
        json!({ "view_id": "child-view" }),
        None,
        vec![]
      ),
    ])
  );
}

#[test]
fn simple_tables_and_columns() {
  let cell = |id: &str, text: &str| {
    b(
      id,
      "simple_table_cell",
      json!({}),
      Some(plain("ignored")),
      vec![text_block(
        &format!("{id}p"),
        "paragraph",
        json!({}),
        plain(text),
        vec![],
      )],
    )
  };
  let snapshot = publish_document(vec![
    b(
      "st",
      "simple_table",
      json!({}),
      None,
      vec![b(
        "row",
        "simple_table_row",
        json!({}),
        None,
        vec![cell("a1", "Cell A1"), cell("b1", "Cell B1")],
      )],
    ),
    b(
      "cols",
      "simple_columns",
      json!({}),
      None,
      vec![
        b(
          "left",
          "simple_column",
          json!({}),
          None,
          vec![text_block(
            "lp",
            "paragraph",
            json!({}),
            plain("Left column"),
            vec![],
          )],
        ),
        b(
          "right",
          "simple_column",
          json!({}),
          None,
          vec![text_block(
            "rp",
            "paragraph",
            json!({}),
            plain("Right column"),
            vec![],
          )],
        ),
      ],
    ),
  ]);
  // Simple table containers never carry text, even with an external id.
  let expected_cell = |id: &str, text: &str| {
    node(
      id,
      "simple_table_cell",
      json!({}),
      None,
      vec![node(
        &format!("{id}p"),
        "paragraph",
        json!({}),
        Some(leaf(text)),
        vec![],
      )],
    )
  };
  assert_eq!(
    snapshot["document"]["children"],
    json!([
      node(
        "st",
        "simple_table",
        json!({}),
        None,
        vec![node(
          "row",
          "simple_table_row",
          json!({}),
          None,
          vec![
            expected_cell("a1", "Cell A1"),
            expected_cell("b1", "Cell B1")
          ],
        )]
      ),
      node(
        "cols",
        "simple_columns",
        json!({}),
        None,
        vec![
          node(
            "left",
            "simple_column",
            json!({}),
            None,
            vec![node(
              "lp",
              "paragraph",
              json!({}),
              Some(leaf("Left column")),
              vec![],
            )]
          ),
          node(
            "right",
            "simple_column",
            json!({}),
            None,
            vec![node(
              "rp",
              "paragraph",
              json!({}),
              Some(leaf("Right column")),
              vec![],
            )]
          ),
        ]
      ),
    ])
  );
}

#[test]
fn legacy_table_cells_are_sorted_and_keep_one_child() {
  let cell = |id: &str, col: i64, row: i64| {
    b(
      id,
      "table/cell",
      json!({ "colPosition": col, "rowPosition": row }),
      None,
      vec![
        text_block(&format!("{id}p"), "paragraph", json!({}), plain(id), vec![]),
        text_block(
          &format!("{id}x"),
          "paragraph",
          json!({}),
          plain("dropped"),
          vec![],
        ),
      ],
    )
  };
  let snapshot = publish_document(vec![b(
    "t",
    "table",
    json!({ "colsLen": 2, "rowsLen": 2 }),
    None,
    vec![
      cell("c11", 1, 1),
      cell("c00", 0, 0),
      cell("c10", 1, 0),
      cell("c01", 0, 1),
    ],
  )]);
  let table = &snapshot["document"]["children"][0];
  let order: Vec<&str> = table["children"]
    .as_array()
    .unwrap()
    .iter()
    .map(|cell| cell["blockId"].as_str().unwrap())
    .collect();
  assert_eq!(order, ["c00", "c01", "c10", "c11"]);
  assert_eq!(
    table["children"][0]["children"],
    json!([node(
      "c00p",
      "paragraph",
      json!({}),
      Some(leaf("c00")),
      vec![]
    )])
  );
}

#[test]
fn mentions_and_formulas() {
  let snapshot = publish_document(vec![text_block(
    "p",
    "paragraph",
    json!({}),
    json!([
      { "insert": "See " },
      { "insert": "$", "attributes": { "mention": { "type": "page", "page_id": "child-view" } } },
      { "insert": " on " },
      { "insert": "$", "attributes": { "mention": { "type": "date", "date": "2026-09-30T10:00:00.000Z" } } },
      { "insert": " with " },
      { "insert": "$", "attributes": { "mention": { "type": "person", "person_id": "p1", "person_name": "Ada" } } },
      { "insert": " and " },
      { "insert": "$", "attributes": { "formula": "x^2" } }
    ]),
    vec![],
  )]);
  assert_eq!(
    snapshot["document"]["children"][0]["children"][0]["children"],
    json!([
      { "text": "See " },
      { "text": "$", "mention": { "type": "page", "page_id": "child-view" } },
      { "text": " on " },
      { "text": "$", "mention": { "type": "date", "date": "2026-09-30T10:00:00.000Z" } },
      { "text": " with " },
      { "text": "$", "mention": { "type": "person", "person_id": "p1", "person_name": "Ada" } },
      { "text": " and " },
      { "text": "$", "formula": "x^2" }
    ])
  );
}

#[test]
fn legacy_delta_in_data_and_missing_blocks() {
  let mut data = document_data(vec![b(
    "old",
    "paragraph",
    json!({ "delta": [{ "insert": "Old text", "attributes": { "italic": true } }] }),
    None,
    vec![],
  )]);
  data
    .meta
    .children_map
    .get_mut(PAGE_ID)
    .unwrap()
    .push("ghost".to_string());

  assert_eq!(
    Value::Array(document_to_slate_children(&data)),
    json!([
      {
        "blockId": "old",
        "relationId": "old",
        "data": { "delta": [{ "insert": "Old text", "attributes": { "italic": true } }] },
        "type": "paragraph",
        // The web converter pushes the empty leaf before finding the legacy delta.
        "children": [
          { "textId": "old", "type": "text", "children": [{ "text": "Old text", "italic": true }] },
          { "text": "" }
        ]
      },
      {
        "blockId": "ghost",
        "type": "block_not_found",
        "data": {},
        "relationId": "ghost",
        "children": [{ "type": "text", "textId": "ghost", "children": [{ "text": "" }] }]
      }
    ])
  );
}

#[test]
fn cycles_do_not_recurse_forever() {
  let mut data = document_data(vec![text_block(
    "a",
    "paragraph",
    json!({}),
    plain("A"),
    vec![],
  )]);
  data
    .meta
    .children_map
    .insert("a".to_string(), vec!["a".to_string()]);
  assert_eq!(
    Value::Array(document_to_slate_children(&data)),
    json!([node("a", "paragraph", json!({}), Some(leaf("A")), vec![])])
  );
}

#[test]
fn empty_or_pageless_documents() {
  let mut data = document_data(vec![]);
  assert!(document_to_slate_children(&data).is_empty());
  data.page_id = "missing".to_string();
  assert!(document_to_slate_children(&data).is_empty());
}

fn encoded_collab(object_id: &str, key: &str, value: Value) -> Vec<u8> {
  let mut collab = Collab::new_with_options(
    CollabOrigin::Empty,
    CollabOptions::new(object_id.to_string(), default_client_id()),
  )
  .unwrap();
  {
    let mut txn = collab.context.transact_mut();
    collab
      .data
      .insert_json_with_path(&mut txn, vec![key], value)
      .unwrap();
  }
  collab
    .encode_collab_v1(|_| Ok::<(), ()>(()))
    .unwrap()
    .doc_state
    .to_vec()
}

#[test]
fn database_snapshot() {
  let database_id = Uuid::new_v4();
  let view_id = Uuid::new_v4();
  let row_id = Uuid::new_v4();
  let row_document_id = Uuid::new_v4();
  let database = json!({
    "id": database_id.to_string(),
    "fields": {
      "name-field": { "id": "name-field", "name": "Name", "type": 0, "is_primary": true, "type_option": {} }
    },
    "views": {
      view_id.to_string(): {
        "database_id": database_id.to_string(),
        "name": "Grid",
        "layout": 0,
        "field_orders": [{ "id": "name-field" }],
        "row_orders": [{ "id": row_id.to_string(), "height": 36 }],
        "field_settings": { "name-field": { "width": "180" } },
        "filters": [],
        "sorts": []
      }
    },
    "metas": { "iid": view_id.to_string() }
  });
  let row = json!({
    "id": row_id.to_string(),
    "database_id": database_id.to_string(),
    "visibility": true,
    "height": 36,
    "cells": { "name-field": { "field_type": 0, "data": "First published row" } }
  });
  let row_document = Document::create(
    &row_document_id.to_string(),
    document_data(vec![text_block(
      "rp",
      "paragraph",
      json!({}),
      plain("Row body"),
      vec![],
    )]),
    default_client_id(),
  )
  .unwrap();

  let data = PublishDatabaseData {
    database_collab: encoded_collab(&database_id.to_string(), "database", database.clone()),
    database_row_collabs: HashMap::from([(
      row_id,
      encoded_collab(&row_id.to_string(), "data", row.clone()),
    )]),
    database_row_document_collabs: HashMap::from([(
      row_document_id,
      row_document.encode_collab().unwrap().doc_state.to_vec(),
    )]),
    visible_database_view_ids: vec![view_id],
    database_relations: HashMap::from([(database_id, view_id)]),
  };
  let metadata = json!({
    "view": {
      "view_id": view_id.to_string(),
      "name": "Published database",
      "icon": { "ty": 0, "value": "📊" },
      "layout": 1,
      "extra": "{\"cover\":{\"type\":\"color\",\"value\":\"#DDEBFF\"}}",
      "created_by": 1,
      "last_edited_by": 1,
      "last_edited_time": 0,
      "created_at": 0,
      "child_views": null
    },
    "child_views": [],
    "ancestor_views": []
  });
  let blob = serde_json::to_vec(&data).unwrap();
  let snapshot = build_published_snapshot("ns", "published-database", &metadata, &blob).unwrap();

  assert_eq!(snapshot["kind"], json!("database"));
  assert_eq!(
    snapshot["view"],
    json!({
      "viewId": view_id.to_string(),
      "name": "Published database",
      "icon": { "ty": 0, "value": "📊" },
      "extra": "{\"cover\":{\"type\":\"color\",\"value\":\"#DDEBFF\"}}",
      "layout": 1,
      "childViews": [],
      "ancestorViews": [],
      "visibleViewIds": [view_id.to_string()],
      "databaseRelations": { database_id.to_string(): view_id.to_string() }
    })
  );
  let db = &snapshot["database"];
  assert_eq!(db["databaseId"], json!(database_id.to_string()));
  assert_eq!(db["activeViewId"], json!(view_id.to_string()));
  assert_eq!(db["visibleViewIds"], json!([view_id.to_string()]));
  assert_eq!(db["raw"]["database"], database);
  assert_eq!(
    db["raw"]["rows"],
    json!({ row_id.to_string(): { "data": row } })
  );

  let raw_document = &db["raw"]["row_documents"][row_document_id.to_string()]["data"];
  assert_eq!(raw_document["page_id"], json!(PAGE_ID));
  assert_eq!(raw_document["meta"]["children_map"][PAGE_ID], json!(["rp"]));
  assert_eq!(
    raw_document["blocks"]["rp"]["external_id"],
    json!("rp-text")
  );
  let delta: Value = serde_json::from_str(
    raw_document["meta"]["text_map"]["rp-text"]
      .as_str()
      .unwrap(),
  )
  .unwrap();
  assert_eq!(delta, plain("Row body"));
}

#[test]
fn rejects_metadata_without_view() {
  assert!(build_published_snapshot("ns", "name", &json!({}), &[]).is_err());
}

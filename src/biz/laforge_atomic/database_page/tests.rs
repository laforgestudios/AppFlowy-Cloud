use super::*;
use crate::biz::laforge_atomic::database_schema::encode_default_database;
use collab::core::collab::{default_client_id, Collab, CollabOptions};
use collab::core::origin::CollabOrigin;
use collab::entity::EncodedCollab;
use serde_json::Value;

fn ids() -> impl FnMut() -> String {
  let mut next = 0u32;
  move || {
    next += 1;
    format!("00000000-0000-4000-8000-{:012x}", next)
  }
}

/// The body `prepareDefaultPage` sends for a Folder layout, with its ids.
fn web_body(layout: u8) -> (String, Value) {
  let mut id = ids();
  let form = layout == 7;
  let dated = layout == 2 || layout == 8;
  let rows: Vec<String> = if form || dated {
    vec![]
  } else {
    (0..3).map(|_| id()).collect()
  };
  let names = [
    "Grid", "Board", "Calendar", "Chart", "List", "Gallery", "Feed", "Form", "Timeline",
  ];
  let body = serde_json::json!({
    "schema_version": "v1",
    "request_id": id(),
    "parent_view_id": id(),
    "name": "",
    "position": {"kind": "start"},
    "container_id": id(),
    "database": {
      "database_id": id(),
      "view_id": id(),
      "name": names[layout as usize],
      "layout": layout,
      "embedded": false,
      "responses_view_id": if form { Value::String(id()) } else { Value::Null },
      "initial_row_ids": rows,
      "timezone_id": if dated { Value::String("Asia/Shanghai".to_string()) } else { Value::Null },
    }
  });
  (body.to_string(), body)
}

fn decode(object_id: &Uuid, encoded: &EncodedCollab) -> Value {
  let options = CollabOptions::new(object_id.to_string(), default_client_id())
    .with_data_source(encoded.clone().into());
  Collab::new_with_options(CollabOrigin::Empty, options)
    .unwrap()
    .to_json_value()
}

fn int(value: &Value) -> i64 {
  value
    .as_i64()
    .unwrap_or_else(|| panic!("not an integer: {}", value))
}

#[test]
fn parses_every_web_layout() {
  for layout in 0..=8u8 {
    let (body, json) = web_body(layout);
    let parsed = parse_request(body.as_bytes()).unwrap();
    assert_eq!(parsed.layout as u8, layout);
    assert_eq!(
      parsed.request.container_id.to_string(),
      json["container_id"].as_str().unwrap()
    );
    let expected_views = if layout == 7 { 2 } else { 1 };
    assert_eq!(parsed.database_view_ids().len(), expected_views);
  }
}

#[test]
fn rejects_invalid_requests() {
  let (_, json) = web_body(7);
  let mut no_responses = json.clone();
  no_responses["database"]["responses_view_id"] = Value::Null;
  assert!(matches!(
    parse_request(no_responses.to_string().as_bytes()),
    Err(AppError::InvalidRequest(_))
  ));

  let (_, json) = web_body(0);
  let mut stray_responses = json.clone();
  stray_responses["database"]["responses_view_id"] =
    Value::String("00000000-0000-4000-8000-0000000000ff".to_string());
  assert!(matches!(
    parse_request(stray_responses.to_string().as_bytes()),
    Err(AppError::InvalidRequest(_))
  ));

  let mut unknown_layout = json.clone();
  unknown_layout["database"]["layout"] = 9.into();
  assert!(matches!(
    parse_request(unknown_layout.to_string().as_bytes()),
    Err(AppError::InvalidRequest(_))
  ));

  let mut reused_id = json.clone();
  reused_id["database"]["view_id"] = json["container_id"].clone();
  assert!(matches!(
    parse_request(reused_id.to_string().as_bytes()),
    Err(AppError::InvalidRequest(_))
  ));

  let mut row_is_database = json.clone();
  row_is_database["database"]["initial_row_ids"] =
    serde_json::json!([json["database"]["database_id"]]);
  assert!(matches!(
    parse_request(row_is_database.to_string().as_bytes()),
    Err(AppError::InvalidRequest(_))
  ));

  let mut too_many_rows = json.clone();
  too_many_rows["database"]["initial_row_ids"] = (0..=MAX_INITIAL_ROWS)
    .map(|index| Value::String(format!("10000000-0000-4000-8000-{:012x}", index)))
    .collect();
  assert!(matches!(
    parse_request(too_many_rows.to_string().as_bytes()),
    Err(AppError::InvalidRequest(_))
  ));

  let mut v2 = json.clone();
  v2["schema_version"] = "v2".into();
  assert!(matches!(
    parse_request(v2.to_string().as_bytes()),
    Err(AppError::InvalidRequest(_))
  ));
}

#[test]
fn folder_extras_identify_the_database() {
  let (_, mut json) = web_body(0);
  json["extra"] =
    serde_json::json!({"cover": {"type": "0", "value": "red"}, "database_id": "spoofed"});
  json["database"]["embedded"] = true.into();
  let parsed = parse_request(json.to_string().as_bytes()).unwrap();
  let database_id = json["database"]["database_id"].clone();

  let container: Value = serde_json::from_str(&parsed.container_extra()).unwrap();
  assert_eq!(container["database_id"], database_id);
  assert_eq!(container["is_database_container"], true);
  assert_eq!(container["embedded"], true);
  assert_eq!(container["cover"]["value"], "red");

  let child: Value = serde_json::from_str(&parsed.child_extra()).unwrap();
  assert_eq!(
    child,
    serde_json::json!({"database_id": database_id, "embedded": true})
  );
  let not_embedded: Value =
    serde_json::from_str(&database_child_extra(&Uuid::nil(), false)).unwrap();
  assert_eq!(
    not_embedded,
    serde_json::json!({"database_id": Uuid::nil().to_string()})
  );
}

/// What `created-page-hydration.ts` (`validateDatabase`, `validateRow`) checks, plus the layout
/// settings each layout parser reads.
#[tokio::test]
async fn default_database_satisfies_the_web_hydration_for_every_layout() {
  for layout in 0..=8u8 {
    let (body, _) = web_body(layout);
    let parsed = parse_request(body.as_bytes()).unwrap();
    let recipe = parsed.recipe();
    let encoded = encode_default_database(&recipe).await.unwrap();
    assert_eq!(
      encoded.encoded_database_collab.object_id,
      recipe.database_id
    );

    let root = decode(
      &recipe.database_id,
      &encoded.encoded_database_collab.encoded_collab,
    );
    let database = &root["database"];
    let database_id = recipe.database_id.to_string();
    assert_eq!(database["id"], database_id.as_str(), "layout {}", layout);

    let fields = database["fields"].as_object().unwrap();
    assert_eq!(
      fields
        .values()
        .filter(|field| field["is_primary"] == true)
        .count(),
      1,
      "layout {} primary field",
      layout
    );

    let views = database["views"].as_object().unwrap();
    let primary = &views[&recipe.view_id.to_string()];
    assert_eq!(primary["id"], recipe.view_id.to_string());
    assert_eq!(primary["database_id"], database_id.as_str());
    assert_eq!(int(&primary["layout"]), layout as i64);
    assert_eq!(primary["name"], recipe.name.as_str());
    assert!(primary["filters"].is_array() && primary["sorts"].is_array());
    assert_eq!(
      primary["field_orders"].as_array().unwrap().len(),
      fields.len()
    );

    if let Some(responses_view_id) = recipe.responses_view_id {
      let responses = &views[&responses_view_id.to_string()];
      assert_eq!(int(&responses["layout"]), 0);
      assert_eq!(responses["database_id"], database_id.as_str());
      assert_eq!(responses["row_orders"], primary["row_orders"]);
    }

    let inline_id = database["metas"]["iid"].as_str().unwrap();
    let inline = &views[inline_id];
    assert_eq!(inline["id"], inline_id);
    assert_eq!(inline["database_id"], database_id.as_str());
    let row_order_ids: Vec<&str> = inline["row_orders"]
      .as_array()
      .unwrap()
      .iter()
      .map(|order| order["id"].as_str().unwrap())
      .collect();
    let row_ids: Vec<String> = recipe.row_ids.iter().map(|id| id.to_string()).collect();
    assert_eq!(row_order_ids, row_ids, "layout {} row orders", layout);
    assert_eq!(primary["row_orders"], inline["row_orders"]);

    assert_eq!(encoded.encoded_row_collabs.len(), recipe.row_ids.len());
    for row in &encoded.encoded_row_collabs {
      assert!(recipe.row_ids.contains(&row.object_id));
      let row_root = decode(&row.object_id, &row.encoded_collab);
      assert_eq!(row_root["data"]["id"], row.object_id.to_string());
      assert_eq!(row_root["data"]["database_id"], database_id.as_str());
      assert!(row_root["data"]["cells"].is_object());
      assert!(row_root["meta"].is_object());
    }

    let settings = &primary["layout_settings"];
    let field_ty = |field_id: &Value| int(&fields[field_id.as_str().unwrap()]["ty"]);
    match layout {
      1 => {
        let groups = primary["groups"].as_array().unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(field_ty(&groups[0]["field_id"]), 3);
        // No-status group plus To Do / Doing / Done.
        assert_eq!(groups[0]["groups"].as_array().unwrap().len(), 4);
        assert_eq!(settings["1"]["hide_ungrouped_column"], true);
        for row in &encoded.encoded_row_collabs {
          let cells = decode(&row.object_id, &row.encoded_collab)["data"]["cells"].clone();
          assert_eq!(cells.as_object().unwrap().len(), 2);
        }
      },
      2 | 8 => {
        let setting = &settings[layout.to_string()];
        let date_field = &fields[setting["field_id"].as_str().unwrap()];
        assert_eq!(int(&date_field["ty"]), 2);
        assert_eq!(
          date_field["type_option"]["2"]["timezone_id"],
          "Asia/Shanghai"
        );
        assert_eq!(int(&setting["layout_ty"]), if layout == 8 { 4 } else { 0 });
        if layout == 8 {
          assert_eq!(setting["show_table"], true);
        } else {
          assert_eq!(int(&setting["first_day_of_week"]), 0);
          assert_eq!(setting["show_weekends"], true);
          assert_eq!(setting["show_week_numbers"], true);
        }
      },
      3 => {
        let chart = &settings["3"];
        assert_eq!(int(&chart["chartType"]), 0);
        assert_eq!(int(&chart["aggregationType"]), 0);
        assert_eq!(chart["showEmptyValues"], true);
        assert_eq!(chart["cumulative"], false);
        assert_eq!(int(&chart["dateCondition"]), 3);
        assert_eq!(field_ty(&chart["xFieldId"]), 3);
      },
      4 => {
        let list = &settings["4"];
        assert_eq!(int(&list["display_mode"]), 1);
        assert_eq!(list["show_cover"], true);
        assert_eq!(list["show_icon"], true);
        assert_eq!(int(&list["card_width"]), 0);
        assert_eq!(list["show_field_names"], true);
        // Three fields: all within the visible positions.
        for setting in primary["field_settings"].as_object().unwrap().values() {
          assert_eq!(int(&setting["visibility"]), 0);
          assert_eq!(setting["wrap"], false);
        }
      },
      5 | 6 => {
        if layout == 5 {
          let gallery = &settings["5"];
          assert_eq!(gallery["show_cover"], true);
          assert_eq!(gallery["fit_image"], false);
          assert_eq!(int(&gallery["card_size"]), 1);
          assert_eq!(int(&gallery["card_width"]), 0);
          assert_eq!(int(&gallery["card_preview"]), 0);
        }
        for (field_id, setting) in primary["field_settings"].as_object().unwrap() {
          let shown = fields[field_id]["is_primary"] == true;
          assert_eq!(int(&setting["visibility"]), if shown { 0 } else { 2 });
        }
      },
      _ => {
        assert!(settings
          .as_object()
          .is_none_or(|settings| settings.is_empty()));
        assert!(primary["form_field_settings"].is_null());
      },
    }
  }
}

#[test]
fn folder_layouts_match_the_web() {
  // Web `FOLDER_LAYOUTS` indexed by `DatabaseViewLayout`.
  let folder_layouts = [1, 2, 3, 5, 6, 7, 8, 9, 10];
  for (layout, folder_layout) in folder_layouts.iter().enumerate() {
    let layout = parse_database_layout(layout as u8).unwrap();
    assert_eq!(folder_view_layout(layout) as i64, *folder_layout);
  }
}

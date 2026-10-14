use super::*;
use crate::biz::laforge_atomic::database_schema::{default_database_params, DefaultDatabaseRecipe};
use collab::core::collab::{default_client_id, CollabOptions, DataSource};
use collab::core::origin::CollabOrigin;
use collab_database::database::{Database, DatabaseContext};
use collab_database::database_trait::NoPersistenceDatabaseCollabService;
use collab_database::entity::FieldType;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use yrs::updates::decoder::Decode;
use yrs::Any;

const ORIGINAL_RESTORE: &str = "00000000-0000-0000-0000-000000000000";

struct Fixture {
  database: Database,
  database_id: Uuid,
  source_view_id: Uuid,
  row_ids: Vec<Uuid>,
}

/// A Grid database whose primary view carries a filter and a hidden field, like the web's
/// interop contract.
async fn grid_with_filter() -> Fixture {
  let database_id = Uuid::new_v4();
  let source_view_id = Uuid::new_v4();
  let row_ids = vec![Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4()];
  let mut params = default_database_params(&DefaultDatabaseRecipe {
    database_id,
    view_id: source_view_id,
    name: "Grid".to_string(),
    layout: DatabaseLayout::Grid,
    responses_view_id: None,
    row_ids: row_ids.clone(),
    timezone_id: None,
  });
  let primary_field_id = params.fields[0].id.clone();
  let view = &mut params.views[0];
  view.filters.push(HashMap::from([
    ("id".to_string(), Any::from("contract-filter-id")),
    ("field_id".to_string(), Any::from(primary_field_id.as_str())),
    ("field_type".to_string(), Any::BigInt(0)),
    ("condition".to_string(), Any::BigInt(0)),
    ("content".to_string(), Any::from("contract-filter")),
  ]));
  let hidden_field_id = params.fields[2].id.clone();
  let mut field_settings = view.field_settings.clone().into_inner();
  field_settings
    .get_mut(&hidden_field_id)
    .unwrap()
    .insert("visibility".to_string(), Any::BigInt(2));
  view.field_settings = field_settings.into();

  let service = Arc::new(NoPersistenceDatabaseCollabService::new(default_client_id()));
  let database = Database::create_with_view(params, DatabaseContext::new(service.clone(), service))
    .await
    .unwrap();
  Fixture {
    database,
    database_id,
    source_view_id,
    row_ids,
  }
}

fn web_body(fixture: &Fixture, layout: u8, source: bool) -> Value {
  json!({
    "schema_version": "v1",
    "request_id": Uuid::new_v4(),
    "database_restore_id": ORIGINAL_RESTORE,
    "view": {
      "database_id": fixture.database_id,
      "view_id": Uuid::new_v4(),
      "parent_view_id": Uuid::new_v4(),
      "source_view_id": if source { json!(fixture.source_view_id) } else { Value::Null },
      "layout": layout,
      "name": format!("Contract {}", layout),
      "embedded": false,
      "responses_view_id": if layout == 7 { json!(Uuid::new_v4()) } else { Value::Null },
      "timezone_id": if layout == 2 || layout == 8 { json!("Asia/Shanghai") } else { Value::Null },
    },
    "position": {"kind": "end"},
    "icon": null,
    "extra": null,
  })
}

fn parse(body: &Value) -> DatabaseViewRequest {
  parse_request(body.to_string().as_bytes()).unwrap()
}

fn database_json(collab: &Collab) -> Value {
  collab.to_json_value()["database"].clone()
}

fn int(value: &Value) -> i64 {
  value
    .as_i64()
    .unwrap_or_else(|| panic!("not an integer: {}", value))
}

#[tokio::test]
async fn duplicates_the_source_view_for_every_layout() {
  for layout in 0..=8u8 {
    let mut fixture = grid_with_filter().await;
    let base = fixture
      .database
      .collab
      .transact()
      .encode_state_as_update_v1(&StateVector::default());
    let parsed = parse(&web_body(&fixture, layout, true));
    let plans =
      plan_database_views(&fixture.database.collab, &fixture.database.body, &parsed).unwrap();
    assert_eq!(plans.len(), parsed.view_ids().len());
    let update = apply_database_views(&mut fixture.database.collab, &fixture.database.body, plans)
      .unwrap()
      .expect("views were created");

    // The update alone brings a replica of the previous state to the new views.
    let options = CollabOptions::new(fixture.database_id.to_string(), default_client_id())
      .with_data_source(DataSource::DocStateV1(base));
    let mut replica = Collab::new_with_options(CollabOrigin::Empty, options).unwrap();
    replica
      .transact_mut()
      .apply_update(yrs::Update::decode_v1(&update).unwrap())
      .unwrap();
    let database = database_json(&replica);
    assert_eq!(database, database_json(&fixture.database.collab));

    let views = &database["views"];
    let source = &views[fixture.source_view_id.to_string()];
    let view = &views[parsed.request.view.view_id.to_string()];
    assert_eq!(int(&view["layout"]), layout as i64, "layout {}", layout);
    assert_eq!(view["database_id"], fixture.database_id.to_string());
    assert_eq!(view["name"], format!("Contract {}", layout));
    assert_eq!(
      view["filters"], source["filters"],
      "layout {} filters",
      layout
    );
    assert_eq!(view["filters"].as_array().unwrap().len(), 1);
    assert_eq!(view["row_orders"], source["row_orders"]);
    let row_ids: Vec<String> = view["row_orders"]
      .as_array()
      .unwrap()
      .iter()
      .map(|order| order["id"].as_str().unwrap().to_string())
      .collect();
    assert_eq!(
      row_ids,
      fixture
        .row_ids
        .iter()
        .map(|id| id.to_string())
        .collect::<Vec<_>>()
    );

    let fields = database["fields"].as_object().unwrap();
    let settings = &view["layout_settings"];
    match layout {
      0 => assert_eq!(view["field_settings"], source["field_settings"]),
      1 => {
        let groups = view["groups"].as_array().unwrap();
        assert!(!groups.is_empty());
        assert!(fields.contains_key(groups[0]["field_id"].as_str().unwrap()));
        assert!(!groups[0]["groups"].as_array().unwrap().is_empty());
        assert!(settings["1"].is_object());
      },
      2 | 8 => {
        let setting = &settings[layout.to_string()];
        let date_field = &fields[setting["field_id"].as_str().unwrap()];
        assert_eq!(int(&date_field["ty"]), FieldType::DateTime as i64);
        assert_eq!(
          date_field["type_option"]["2"]["timezone_id"],
          "Asia/Shanghai"
        );
        // The created date field reaches every view, the source included.
        let date_field_id = setting["field_id"].as_str().unwrap();
        assert!(source["field_orders"]
          .as_array()
          .unwrap()
          .iter()
          .any(|order| order["id"] == date_field_id));
      },
      3 => assert!(settings["3"]["xFieldId"].is_string()),
      4 => assert_eq!(int(&settings["4"]["display_mode"]), 1),
      5 => assert_eq!(int(&settings["5"]["card_size"]), 1),
      7 => {
        let responses = &views[parsed.request.view.responses_view_id.unwrap().to_string()];
        assert_eq!(int(&responses["layout"]), 0);
        assert_eq!(responses["row_orders"], source["row_orders"]);
        assert_eq!(responses["name"], RESPONSES_VIEW_NAME);
      },
      _ => {},
    }

    // A retry finds the views and changes nothing.
    let replan =
      plan_database_views(&fixture.database.collab, &fixture.database.body, &parsed).unwrap();
    assert!(replan.is_empty());
    assert!(
      apply_database_views(&mut fixture.database.collab, &fixture.database.body, replan)
        .unwrap()
        .is_none()
    );
  }
}

#[tokio::test]
async fn without_a_source_starts_from_the_inline_view_defaults() {
  let mut fixture = grid_with_filter().await;
  let parsed = parse(&web_body(&fixture, 1, false));
  let plans =
    plan_database_views(&fixture.database.collab, &fixture.database.body, &parsed).unwrap();
  apply_database_views(&mut fixture.database.collab, &fixture.database.body, plans).unwrap();
  let database = database_json(&fixture.database.collab);
  let view = &database["views"][parsed.request.view.view_id.to_string()];
  assert!(view["filters"].as_array().unwrap().is_empty());
  assert_eq!(view["groups"].as_array().unwrap().len(), 1);
  let inline = &database["views"][database["metas"]["iid"].as_str().unwrap()];
  assert_eq!(view["row_orders"], inline["row_orders"]);
}

#[tokio::test]
async fn conflicting_or_unknown_views_are_rejected() {
  let fixture = grid_with_filter().await;

  let mut unknown_source = web_body(&fixture, 0, true);
  unknown_source["view"]["source_view_id"] = json!(Uuid::new_v4());
  assert!(matches!(
    plan_database_views(
      &fixture.database.collab,
      &fixture.database.body,
      &parse(&unknown_source)
    ),
    Err(AppError::InvalidRequest(_))
  ));

  // The requested id is already a view of another layout.
  let mut taken = web_body(&fixture, 1, true);
  taken["view"]["view_id"] = json!(fixture.source_view_id);
  assert!(matches!(
    plan_database_views(
      &fixture.database.collab,
      &fixture.database.body,
      &parse(&taken)
    ),
    Err(AppError::RecordAlreadyExists(_))
  ));

  let mut other_database = web_body(&fixture, 0, true);
  other_database["view"]["database_id"] = json!(Uuid::new_v4());
  assert!(matches!(
    plan_database_views(
      &fixture.database.collab,
      &fixture.database.body,
      &parse(&other_database)
    ),
    Err(AppError::RecordNotFound(_))
  ));
}

#[tokio::test]
async fn parses_the_web_request() {
  let fixture = grid_with_filter().await;
  let body = web_body(&fixture, 7, true);
  let parsed = parse(&body);
  assert_eq!(parsed.layout, DatabaseLayout::Form);
  assert_eq!(parsed.request.database_restore_id, ORIGINAL_RESTORE);
  assert_eq!(parsed.view_ids().len(), 2);
  assert_eq!(
    parsed.layout_of(&parsed.view_ids()[1]),
    DatabaseLayout::Grid
  );

  let mut no_responses = body.clone();
  no_responses["view"]["responses_view_id"] = Value::Null;
  assert!(matches!(
    parse_request(no_responses.to_string().as_bytes()),
    Err(AppError::InvalidRequest(_))
  ));
  let mut stray_responses = web_body(&fixture, 0, true);
  stray_responses["view"]["responses_view_id"] = json!(Uuid::new_v4());
  assert!(matches!(
    parse_request(stray_responses.to_string().as_bytes()),
    Err(AppError::InvalidRequest(_))
  ));
  let mut self_parent = web_body(&fixture, 0, true);
  self_parent["view"]["parent_view_id"] = self_parent["view"]["view_id"].clone();
  assert!(matches!(
    parse_request(self_parent.to_string().as_bytes()),
    Err(AppError::InvalidRequest(_))
  ));

  // The web's minimal body: no position, icon or extra.
  let mut minimal = web_body(&fixture, 0, false);
  let object = minimal.as_object_mut().unwrap();
  object.remove("position");
  object.remove("icon");
  object.remove("extra");
  assert!(parse_request(minimal.to_string().as_bytes()).is_ok());
}

#[tokio::test]
async fn folder_extra_merges_the_client_extra_on_the_requested_view() {
  let fixture = grid_with_filter().await;
  let mut body = web_body(&fixture, 7, true);
  body["extra"] = json!({"cover": {"type": "0", "value": "red"}, "database_id": "spoofed"});
  body["view"]["embedded"] = json!(true);
  let parsed = parse(&body);
  let [view_id, responses_view_id] = parsed.view_ids()[..] else {
    panic!("two views expected")
  };
  let extra: Value = serde_json::from_str(&parsed.folder_extra(&view_id)).unwrap();
  assert_eq!(extra["database_id"], fixture.database_id.to_string());
  assert_eq!(extra["embedded"], true);
  assert_eq!(extra["cover"]["value"], "red");
  let responses: Value = serde_json::from_str(&parsed.folder_extra(&responses_view_id)).unwrap();
  assert_eq!(
    responses,
    json!({"database_id": fixture.database_id.to_string(), "embedded": true})
  );
}

#[test]
fn receipt_result_matches_the_web_validation() {
  let result = CreateDatabaseViewResult {
    view_id: Uuid::new_v4(),
    view_ids: vec![Uuid::new_v4()],
    database_id: Uuid::new_v4(),
    database_restore_id: ORIGINAL_RESTORE.to_string(),
    database_version: None,
    database_update_v1: STANDARD.encode([1u8, 2, 3]),
    update_batch_id: Uuid::new_v4(),
  };
  let value = serde_json::to_value(&result).unwrap();
  assert!(value["database_version"].is_null());
  assert_eq!(value["database_restore_id"], ORIGINAL_RESTORE);
  let encoded = value["database_update_v1"].as_str().unwrap();
  assert!(!encoded.is_empty());
  assert!(encoded
    .chars()
    .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '='));
  assert_eq!(value["view_ids"].as_array().unwrap().len(), 1);
}

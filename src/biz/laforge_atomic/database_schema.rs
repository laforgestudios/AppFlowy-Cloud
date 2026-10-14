//! Default database schema and per-layout view settings of the atomic database creations.
//!
//! Every layout setting lives under `layout_settings[<DatabaseLayout number>]` with the keys the
//! web reads: Calendar `2` (`calendar-layout.ts`), Chart `3` (`chart.type.ts`, `selector.ts`),
//! List `4` (`list-layout.ts`), Gallery `5` (`gallery-layout.ts`), Timeline `8`
//! (`timeline-layout.ts`). Board views carry persisted groups, Feed views a CreatedTime sort when
//! the database has such a field (`feed-layout.ts`). Form views leave `form_field_settings` to the
//! client's first-open decision.

use anyhow::anyhow;
use app_error::AppError;
use collab::core::collab::default_client_id;
use collab_database::database::{
  gen_database_group_id, gen_database_sort_id, gen_field_id, timestamp, Database, DatabaseContext,
};
use collab_database::database_trait::NoPersistenceDatabaseCollabService;
use collab_database::entity::{
  CreateDatabaseParams, CreateViewParams, DatabaseView, EncodedDatabase, FieldType,
};
use collab_database::fields::date_type_option::DateTypeOption;
use collab_database::fields::select_type_option::{
  SelectOption, SelectOptionColor, SelectOptionIds, SelectTypeOption, SingleSelectTypeOption,
};
use collab_database::fields::{
  default_field_settings_by_layout_map, default_field_settings_for_fields, Field,
};
use collab_database::rows::{new_cell_builder, CreateRowParams, RowId};
use collab_database::template::entity::CELL_DATA;
use collab_database::views::{
  BoardLayoutSetting, CalendarLayoutSetting, DatabaseLayout, FieldSettingsByFieldIdMap, Group,
  GroupSetting, GroupSettingMap, LayoutSetting, LayoutSettings, SortMap, TimelineLayoutSetting,
};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use uuid::Uuid;
use yrs::Any;

/// Timezone of the date field when the client sends none.
const DEFAULT_TIMEZONE: &str = "Etc/UTC";
/// Web `FieldVisibility`.
const VISIBILITY_ALWAYS_SHOWN: i64 = 0;
const VISIBILITY_ALWAYS_HIDDEN: i64 = 2;
/// Web `LIST_DEFAULT_VISIBLE_FIELD_POSITION_COUNT`.
const LIST_VISIBLE_FIELD_POSITIONS: usize = 3;
/// Web `SortCondition.Descending`.
const SORT_DESCENDING: i64 = 1;
/// Web `DateGroupCondition.Month`.
const CHART_DATE_CONDITION_MONTH: i64 = 3;

/// `DatabaseLayout` from the web number (`DatabaseViewLayout`), rejecting unknown values
/// (`DatabaseLayout::from(i64)` silently maps them to Grid).
pub fn parse_database_layout(value: u8) -> Result<DatabaseLayout, AppError> {
  Ok(match value {
    0 => DatabaseLayout::Grid,
    1 => DatabaseLayout::Board,
    2 => DatabaseLayout::Calendar,
    3 => DatabaseLayout::Chart,
    4 => DatabaseLayout::List,
    5 => DatabaseLayout::Gallery,
    6 => DatabaseLayout::Feed,
    7 => DatabaseLayout::Form,
    8 => DatabaseLayout::Timeline,
    other => {
      return Err(AppError::InvalidRequest(format!(
        "unsupported database layout {}",
        other
      )))
    },
  })
}

/// Folder layout of a view showing a database layout (web `FOLDER_LAYOUTS`).
pub fn folder_view_layout(layout: DatabaseLayout) -> collab_folder::ViewLayout {
  match layout {
    DatabaseLayout::Grid => collab_folder::ViewLayout::Grid,
    DatabaseLayout::Board => collab_folder::ViewLayout::Board,
    DatabaseLayout::Calendar => collab_folder::ViewLayout::Calendar,
    DatabaseLayout::Chart => collab_folder::ViewLayout::Chart,
    DatabaseLayout::List => collab_folder::ViewLayout::List,
    DatabaseLayout::Gallery => collab_folder::ViewLayout::Gallery,
    DatabaseLayout::Feed => collab_folder::ViewLayout::Feed,
    DatabaseLayout::Form => collab_folder::ViewLayout::Form,
    DatabaseLayout::Timeline => collab_folder::ViewLayout::Timeline,
  }
}

/// Settings of a new database view, plus the fields its layout needs that the database lacks.
#[derive(Debug, Clone, Default)]
pub struct ViewSettings {
  pub layout_settings: LayoutSettings,
  pub field_settings: FieldSettingsByFieldIdMap,
  pub group_settings: Vec<GroupSettingMap>,
  pub sorts: Vec<SortMap>,
  pub filters: Vec<HashMap<String, Any>>,
  pub deps_fields: Vec<Field>,
}

impl ViewSettings {
  pub fn into_params(
    self,
    database_id: &str,
    view_id: &Uuid,
    name: &str,
    layout: DatabaseLayout,
  ) -> CreateViewParams {
    let now = timestamp();
    let deps_field_setting = self
      .deps_fields
      .iter()
      .map(|_| default_field_settings_by_layout_map())
      .collect();
    CreateViewParams {
      database_id: database_id.to_string(),
      view_id: view_id.to_string(),
      name: name.to_string(),
      layout,
      layout_settings: self.layout_settings,
      filters: self.filters,
      group_settings: self.group_settings,
      sorts: self.sorts,
      field_settings: self.field_settings,
      created_at: now,
      modified_at: now,
      deps_fields: self.deps_fields,
      deps_field_setting,
    }
  }
}

/// Fields in view order: ordered ids first, then the fields the order misses.
fn ordered_fields<'a>(fields: &'a [Field], field_order: &[String]) -> Vec<&'a Field> {
  let mut seen = HashSet::new();
  let mut ordered = Vec::with_capacity(fields.len());
  for id in field_order {
    if let Some(field) = fields.iter().find(|field| &field.id == id) {
      if seen.insert(field.id.as_str()) {
        ordered.push(field);
      }
    }
  }
  for field in fields {
    if seen.insert(field.id.as_str()) {
      ordered.push(field);
    }
  }
  ordered
}

fn field_type(field: &Field) -> FieldType {
  FieldType::from(field.field_type)
}

pub fn date_field(timezone_id: Option<&str>) -> Field {
  let type_option = DateTypeOption {
    timezone_id: timezone_id
      .filter(|timezone| !timezone.is_empty())
      .unwrap_or(DEFAULT_TIMEZONE)
      .to_string(),
    ..DateTypeOption::default_utc()
  };
  Field::new(
    gen_field_id(),
    "Date".to_string(),
    FieldType::DateTime.into(),
    false,
  )
  .with_type_option_data(FieldType::DateTime, type_option.into())
}

fn status_field(options: Vec<SelectOption>) -> Field {
  let mut type_option = SingleSelectTypeOption::default();
  type_option.options.extend(options);
  Field::new(
    gen_field_id(),
    "Status".to_string(),
    FieldType::SingleSelect.into(),
    false,
  )
  .with_type_option_data(FieldType::SingleSelect, type_option.into())
}

/// Board groups of a field: the "no value" group (the field id) then one group per option for
/// select fields, Yes/No for checkboxes.
fn board_group_setting(field: &Field) -> GroupSettingMap {
  let ty = field_type(field);
  let group_ids: Vec<String> = match ty {
    FieldType::Checkbox => vec!["Yes".to_string(), "No".to_string()],
    _ => {
      let options = field
        .type_options
        .get(&ty.to_string())
        .cloned()
        .map(|data| SelectTypeOption::from(data).options)
        .unwrap_or_default();
      std::iter::once(field.id.clone())
        .chain(options.into_iter().map(|option| option.id))
        .collect()
    },
  };
  GroupSetting {
    id: gen_database_group_id(),
    field_id: field.id.clone(),
    field_type: field.field_type,
    groups: group_ids.into_iter().map(Group::new).collect(),
    content: String::new(),
  }
  .into()
}

fn chart_layout_setting(ordered: &[&Field]) -> LayoutSetting {
  // Web `GROUPABLE_FIELD_TYPES`.
  let x_field = ordered.iter().find(|field| {
    matches!(
      field_type(field),
      FieldType::SingleSelect
        | FieldType::MultiSelect
        | FieldType::Checkbox
        | FieldType::DateTime
        | FieldType::LastEditedTime
        | FieldType::CreatedTime
    )
  });
  let mut setting = LayoutSetting::from([
    ("chartType".to_string(), Any::BigInt(0)),
    ("showEmptyValues".to_string(), Any::Bool(true)),
    ("aggregationType".to_string(), Any::BigInt(0)),
    ("cumulative".to_string(), Any::Bool(false)),
    (
      "dateCondition".to_string(),
      Any::BigInt(CHART_DATE_CONDITION_MONTH),
    ),
  ]);
  if let Some(field) = x_field {
    setting.insert("xFieldId".to_string(), field.id.clone().into());
  }
  setting
}

/// `visible_field_ids` is left out: the web stores it as a plain array that Yrs layout settings
/// would turn into a shared array, and nothing reads it.
fn list_layout_setting() -> LayoutSetting {
  LayoutSetting::from([
    ("display_mode".to_string(), Any::BigInt(1)),
    ("show_cover".to_string(), Any::Bool(true)),
    ("show_icon".to_string(), Any::Bool(true)),
    ("card_width".to_string(), Any::BigInt(0)),
    ("show_field_names".to_string(), Any::Bool(true)),
  ])
}

/// Web `DEFAULT_GALLERY_LAYOUT_SETTINGS`.
fn gallery_layout_setting() -> LayoutSetting {
  LayoutSetting::from([
    ("show_cover".to_string(), Any::Bool(true)),
    ("fit_image".to_string(), Any::Bool(false)),
    ("card_size".to_string(), Any::BigInt(1)),
    ("card_width".to_string(), Any::BigInt(0)),
    ("card_preview".to_string(), Any::BigInt(0)),
  ])
}

fn projected_field_settings(
  ordered: &[&Field],
  visible: impl Fn(usize, &Field) -> bool,
) -> FieldSettingsByFieldIdMap {
  ordered
    .iter()
    .enumerate()
    .map(|(index, field)| {
      let visibility = if field.is_primary || visible(index, field) {
        VISIBILITY_ALWAYS_SHOWN
      } else {
        VISIBILITY_ALWAYS_HIDDEN
      };
      (
        field.id.clone(),
        HashMap::from([
          ("visibility".to_string(), Any::BigInt(visibility)),
          ("wrap".to_string(), Any::Bool(false)),
        ]),
      )
    })
    .collect::<HashMap<_, _>>()
    .into()
}

/// Field settings of a fresh view: List shows the primary field and the first positions,
/// Gallery and Feed only the primary field (web `generate*FieldSettings`), the other layouts use
/// the collab defaults.
fn layout_field_settings(
  layout: DatabaseLayout,
  fields: &[Field],
  field_order: &[String],
) -> FieldSettingsByFieldIdMap {
  let ordered = ordered_fields(fields, field_order);
  match layout {
    DatabaseLayout::List => {
      projected_field_settings(&ordered, |index, _| index < LIST_VISIBLE_FIELD_POSITIONS)
    },
    DatabaseLayout::Gallery | DatabaseLayout::Feed => {
      projected_field_settings(&ordered, |_, _| false)
    },
    _ => default_field_settings_for_fields(fields, layout),
  }
}

/// Settings of a fresh view of `layout` over `fields`.
pub fn default_view_settings(
  layout: DatabaseLayout,
  fields: &[Field],
  field_order: &[String],
  timezone_id: Option<&str>,
) -> ViewSettings {
  let mut settings = ViewSettings::default();
  let mut all_fields = fields.to_vec();
  let ordered = ordered_fields(fields, field_order);
  match layout {
    DatabaseLayout::Board => {
      let group_field = ordered
        .iter()
        .find(|field| {
          matches!(
            field_type(field),
            FieldType::SingleSelect | FieldType::MultiSelect | FieldType::Checkbox
          )
        })
        .map(|field| (*field).clone())
        .unwrap_or_else(|| {
          let field = status_field(vec![]);
          settings.deps_fields.push(field.clone());
          all_fields.push(field.clone());
          field
        });
      settings
        .group_settings
        .push(board_group_setting(&group_field));
      settings
        .layout_settings
        .insert(layout, BoardLayoutSetting::new().into());
    },
    DatabaseLayout::Calendar | DatabaseLayout::Timeline => {
      let date_field_id = ordered
        .iter()
        .find(|field| field_type(field) == FieldType::DateTime)
        .map(|field| field.id.clone())
        .unwrap_or_else(|| {
          let field = date_field(timezone_id);
          let id = field.id.clone();
          settings.deps_fields.push(field.clone());
          all_fields.push(field);
          id
        });
      let setting: LayoutSetting = if layout == DatabaseLayout::Timeline {
        TimelineLayoutSetting::new(date_field_id).into()
      } else {
        CalendarLayoutSetting::new(date_field_id).into()
      };
      settings.layout_settings.insert(layout, setting);
    },
    DatabaseLayout::Chart => {
      settings
        .layout_settings
        .insert(layout, chart_layout_setting(&ordered));
    },
    DatabaseLayout::List => {
      settings
        .layout_settings
        .insert(layout, list_layout_setting());
    },
    DatabaseLayout::Gallery => {
      settings
        .layout_settings
        .insert(layout, gallery_layout_setting());
    },
    DatabaseLayout::Feed => {
      if let Some(created_time) = ordered
        .iter()
        .find(|field| field_type(field) == FieldType::CreatedTime)
      {
        settings.sorts.push(HashMap::from([
          ("id".to_string(), gen_database_sort_id().into()),
          ("field_id".to_string(), created_time.id.clone().into()),
          ("condition".to_string(), Any::BigInt(SORT_DESCENDING)),
        ]));
      }
    },
    DatabaseLayout::Grid | DatabaseLayout::Form => {},
  }
  settings.field_settings = layout_field_settings(layout, &all_fields, field_order);
  settings
}

/// Whether the source view's setting for `layout` can be reused as is.
fn source_layout_setting_usable(
  layout: DatabaseLayout,
  setting: &LayoutSetting,
  fields: &[Field],
) -> bool {
  if !layout.requires_date_field() {
    return true;
  }
  match setting.get("field_id") {
    Some(Any::String(field_id)) => fields
      .iter()
      .any(|field| field.id == field_id.as_ref() && field_type(field) == FieldType::DateTime),
    _ => false,
  }
}

/// Settings of a new view of `layout` duplicating `source`: filters, sorts, layout settings and,
/// for the same layout, field settings and groups. What the layout needs and the source lacks
/// (date field binding, Board groups, List/Gallery/Feed projections) comes from the defaults.
pub fn linked_view_settings(
  layout: DatabaseLayout,
  source: &DatabaseView,
  fields: &[Field],
  timezone_id: Option<&str>,
) -> ViewSettings {
  let field_order: Vec<String> = source
    .field_orders
    .iter()
    .map(|order| order.id.clone())
    .collect();
  let mut settings = default_view_settings(layout, fields, &field_order, timezone_id);
  let same_layout = source.layout == layout;

  let mut layout_settings = source.layout_settings.clone();
  match source.layout_settings.get(&layout) {
    Some(setting) if source_layout_setting_usable(layout, setting, fields) => {
      // The source binding is kept, so a date field created for the defaults is not needed.
      if layout.requires_date_field() {
        settings.deps_fields.clear();
      }
    },
    _ => {
      if let Some(setting) = settings.layout_settings.remove(&layout) {
        layout_settings.insert(layout, setting);
      }
    },
  }
  settings.layout_settings = layout_settings;

  let source_groups_usable = !source.group_settings.is_empty()
    && source.group_settings.iter().all(|group| {
      matches!(group.get("field_id"), Some(Any::String(field_id))
        if fields.iter().any(|field| field.id == field_id.as_ref()))
    });
  if layout == DatabaseLayout::Board && source_groups_usable {
    settings.group_settings = source.group_settings.clone();
    settings.deps_fields.clear();
  } else if same_layout && layout != DatabaseLayout::Board {
    settings.group_settings = source.group_settings.clone();
  }

  if same_layout {
    let mut field_settings = source.field_settings.clone().into_inner();
    for (field_id, setting) in settings.field_settings.into_inner() {
      field_settings.entry(field_id).or_insert(setting);
    }
    settings.field_settings = field_settings.into();
  }
  if !source.sorts.is_empty() {
    settings.sorts = source.sorts.clone();
  }
  settings.filters = source.filters.clone();
  settings
}

/// Ids and options of a new database page (web `DefaultDatabaseOptions`).
#[derive(Debug, Clone)]
pub struct DefaultDatabaseRecipe {
  pub database_id: Uuid,
  pub view_id: Uuid,
  pub name: String,
  pub layout: DatabaseLayout,
  pub responses_view_id: Option<Uuid>,
  pub row_ids: Vec<Uuid>,
  pub timezone_id: Option<String>,
}

pub const RESPONSES_VIEW_NAME: &str = "Responses";

struct DefaultSchema {
  fields: Vec<Field>,
  /// Board rows: (title field, status field, default option).
  board: Option<(String, String, String)>,
}

fn default_schema(layout: DatabaseLayout, timezone_id: Option<&str>) -> DefaultSchema {
  match layout {
    DatabaseLayout::Board => {
      let title = Field::from_field_type("Description", FieldType::RichText, true);
      let to_do = SelectOption::with_color("To Do", SelectOptionColor::Purple);
      let doing = SelectOption::with_color("Doing", SelectOptionColor::Orange);
      let done = SelectOption::with_color("Done", SelectOptionColor::Yellow);
      let default_option_id = to_do.id.clone();
      let status = status_field(vec![to_do, doing, done]);
      let board = Some((title.id.clone(), status.id.clone(), default_option_id));
      DefaultSchema {
        fields: vec![title, status],
        board,
      }
    },
    DatabaseLayout::Calendar | DatabaseLayout::Timeline => DefaultSchema {
      fields: vec![
        Field::from_field_type("Title", FieldType::RichText, true),
        date_field(timezone_id),
        Field::from_field_type("Tags", FieldType::MultiSelect, false),
      ],
      board: None,
    },
    _ => DefaultSchema {
      fields: vec![
        Field::from_field_type("Name", FieldType::RichText, true),
        Field::from_field_type("Type", FieldType::SingleSelect, false),
        Field::from_field_type("Done", FieldType::Checkbox, false),
      ],
      board: None,
    },
  }
}

fn default_rows(recipe: &DefaultDatabaseRecipe, schema: &DefaultSchema) -> Vec<CreateRowParams> {
  let database_id = recipe.database_id.to_string();
  recipe
    .row_ids
    .iter()
    .enumerate()
    .map(|(index, row_id)| {
      let mut row = CreateRowParams::new(RowId::from(row_id.to_string()), database_id.clone());
      if let Some((title_field_id, status_field_id, option_id)) = &schema.board {
        let mut title = new_cell_builder(FieldType::RichText);
        title.insert(CELL_DATA.into(), format!("Card {}", index + 1).into());
        row.cells.insert(title_field_id.clone(), title);
        row.cells.insert(
          status_field_id.clone(),
          SelectOptionIds::from(vec![option_id.clone()]).to_cell(FieldType::SingleSelect),
        );
      }
      row
    })
    .collect()
}

/// The `CreateDatabaseParams` of a new database page: the primary view (and the Form responses
/// Grid) over the layout's default fields, with the client's row ids.
pub fn default_database_params(recipe: &DefaultDatabaseRecipe) -> CreateDatabaseParams {
  let timezone_id = recipe.timezone_id.as_deref();
  let schema = default_schema(recipe.layout, timezone_id);
  let field_order: Vec<String> = schema.fields.iter().map(|field| field.id.clone()).collect();
  let database_id = recipe.database_id.to_string();

  let mut primary = default_view_settings(recipe.layout, &schema.fields, &field_order, timezone_id);
  if recipe.layout == DatabaseLayout::Board {
    // Every default card has a status.
    primary.layout_settings.insert(
      DatabaseLayout::Board,
      BoardLayoutSetting {
        hide_ungrouped_column: true,
        collapse_hidden_groups: true,
      }
      .into(),
    );
  }
  let mut views =
    vec![primary.into_params(&database_id, &recipe.view_id, &recipe.name, recipe.layout)];
  if let Some(responses_view_id) = &recipe.responses_view_id {
    views.push(
      default_view_settings(DatabaseLayout::Grid, &schema.fields, &field_order, None).into_params(
        &database_id,
        responses_view_id,
        RESPONSES_VIEW_NAME,
        DatabaseLayout::Grid,
      ),
    );
  }

  CreateDatabaseParams {
    database_id,
    rows: default_rows(recipe, &schema),
    fields: schema.fields,
    views,
  }
}

/// Encodes the database collab and its row collabs.
pub async fn encode_default_database(
  recipe: &DefaultDatabaseRecipe,
) -> Result<EncodedDatabase, AppError> {
  let params = default_database_params(recipe);
  let service = Arc::new(NoPersistenceDatabaseCollabService::new(default_client_id()));
  let context = DatabaseContext::new(service.clone(), service);
  let database = Database::create_with_view(params, context)
    .await
    .map_err(|err| AppError::Internal(anyhow!("Failed to create default database: {}", err)))?;
  database
    .encode_database_collabs()
    .await
    .map_err(|err| AppError::Internal(anyhow!("Failed to encode default database: {}", err)))
}

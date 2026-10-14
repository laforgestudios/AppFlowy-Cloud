# Laforge — Phase 1: serve the current AppFlowy-Web from this legacy server

The web client (`laforgestudios/AppFlowy-Web`, cloned at `../appflowy-web`, sources in
`src/`) targets AppFlowy-Cloud ≥ 0.18.10, which is closed source. This fork (legacy code,
frozen Aug 2025, v0.9.64) must gain the routes below. Paths prefixed `W/` are in
`../appflowy-web/src`, unprefixed paths are in this repo. Line numbers are indicative
(verify before editing).

## Ground rules (all tasks)

- **Envelope**: JSON routes return `{code, data?, message}`; `code 0` = success
  (`libs/shared-entity/src/response.rs:32-41`, web `W/application/services/js-services/http/core.ts:121-128`).
  Error codes: -2 not found, -3 exists, **-5 RetryLater (web retries)**, 1008 invalid request,
  1012 no permission, 1024 unauthorized (`libs/app-error/src/lib.rs:410-437`).
- **Versionless**: never send a collab version. `collab_version: ""` in batch results,
  `null` versions elsewhere. A versioned doc makes the web silently drop later WS updates
  (`W/.../useCollabMessageHandler.ts:140-160`).
- **Route order**: register new literal routes (`/v2/...`, `/views`, `/collab/{id}/permission`)
  BEFORE generic `/{workspace_id}/...` patterns in `src/api/workspace.rs`.
- **SQL**: use runtime `sqlx::query`/`query_as` (not the `query!` macros) for NEW SQL, so the
  offline cache in `.sqlx/` does not need regenerating. New tables go in `migrations/`
  (timestamped, additive only).
- **Build**: shared target dir. Always `export CARGO_TARGET_DIR=/Users/abdoul/development/laforgestudios/.af-target SQLX_OFFLINE=true CARGO_BUILD_JOBS=6`.
  Prefer `cargo check -p appflowy-cloud` / `cargo test -p <crate>`; builds are serialized by
  the shared lock — expect waits.
- **Local services** (docker, from `docker-compose.laforge.yml`): Postgres `localhost:5433`
  (postgres/password, db `postgres`), Redis `localhost:6380`, GoTrue `localhost:9999`,
  MinIO `localhost:9000`, the server image on `localhost:8000`.

## Tasks

### T0 — Fork AppFlowy-Collab, add layouts (L) — blocks layouts 3–8
- collab crates pinned to rev `e59260e5` (`Cargo.toml` ~310-317, `[patch.crates-io]`).
- Add `collab_folder::ViewLayout` variants Chart=5, List=6, Gallery=7, Feed=8, Form=9,
  Timeline=10 and `collab_database::DatabaseLayout` Chart=3, List=4, Gallery=5, Feed=6,
  Form=7, Timeline=8 (web: `W/application/types.ts:502-522` and `:928-943` — copy numbers from there).
- First check how an unknown integer is parsed today (dropped view / error / Grid default).
- Fix all exhaustive matches: `src/biz/collab/folder_view.rs:497-539`,
  `src/biz/workspace/page_view.rs:247-268, 1986-1997, 2163-2172`,
  `src/biz/collab/database.rs:40-44`, DTO `ViewLayout` in
  `libs/shared-entity/src/dto/workspace_dto.rs:400-427` (Deserialize_repr, accept 0-10).

### T1 — `GET /api/server-info` (+ `/auth-providers`) (S)
- Web: `W/application/services/js-services/http/auth-api.ts:19-45, 145-190`.
- Response: `{"version":"0.18.10+laforge","min_web_client_version":"0.0.0","enable_page_history":false,"enable_atomic_creation":true,"enable_atomic_creation_defaults":true,"enable_database_history":false,"ai_enabled":false,"self_hosted":true}`.
  Version must be ≥ 0.18.10 with `+build` metadata only (a `-suffix` = prerelease = "server too old").
  Omit `max_update_bytes` / `max_slow_sync_update_bytes`.
- auth-providers: `{count, providers:["password","magic_link",…], signup_disabled, mailer_autoconfirm}` from GoTrue `/settings` (optional; web falls back to password).
- CORS (`src/application.rs:573-589`): allow headers `X-Platform`, `Cache-Control`,
  `X-AppFlowy-Sync-Lane`; expose `ETag`, `Retry-After`, `X-Request-Id`.

### T2 — `GET /api/workspace/{ws}/collab/{object_id}/permission?collab_type=N` (S) — blocker for editing
- Web: `W/.../http/access-api.ts:241-249`, validation `permissionProbe.ts:25-44`, type `W/application/types.ts:2711-2720`:
  `{object_id, collab_type, governing_view_id (non-empty), access_level: 10|20|30|50|null, can_read, can_write, can_comment, can_share}`.
- Use `state.collab_access_control.enforce_action(.., Action::Read/Write)`; access_level 50 if writable else 10.
  governing_view_id = object id (for a database: first linked view from WorkspaceDatabase).

### T3 — View tree (M)
Reuse `biz::collab::ops::get_user_workspace_structure` (`src/biz/collab/ops.rs:252-288`),
`collab_folder_to_folder_view`/`to_folder_view` (`folder_view.rs:63-211`), `get_self_and_ancestor_views` (`:386-405`).
- `GET /{ws}/view/{view_id}?depth=N` → `data: View` (`W/application/types.ts:1929-1961`; callers
  `W/.../http/view-api.ts:14-27`). `view_id == ws` → root with spaces as children. Add
  `has_children: bool` (`#[serde(default)]`) to `FolderView`. Clamp depth (ops.rs:260 caps 10). Read uncached
  (read-your-writes after creation).
- `GET /{ws}/views?depth=1&view_ids=a,b,c` and `POST /{ws}/views {depth, view_ids}` → `{views: View[]}` (`view-api.ts:74-170`); skip unknown ids.
- `GET /{ws}/view/{view_id}/navigation?depth=0` → View tree rooted at the containing top-level space; every
  ancestor carries its full visible children (siblings collapsed with `has_children`), path down to target,
  target expanded to `depth` (`W/.../useWorkspaceData.ts:155-176, 222-276, 1483-1538`).

### T4 — `GET /api/workspace/{ws}/database?offset=&limit=` (M)
- Web `view-api.ts:35-72`, type `W/application/services/services.type.ts:6-25`:
  `{databases:[{database_id, views:[{view_id, layout, is_container, embedded, name, icon, parent_view_id}]}], has_more:false}`.
- Legacy `list_database_handler` (`workspace.rs:2561-2577`, `ops.rs:348-399`) returns `Vec<AFDatabase>`; return the
  new shape when `offset` is present; include folder container views (`extra.database_id` + `is_database_container` → `is_container:true`).

### T5 — `POST /api/workspace/v1/{ws}/collab/full-sync/batch` (protobuf) (M)
- Web `W/.../http/collab-api.ts:190-339`. Body protobuf `collab.CollabBatchSyncRequest`
  (`application/octet-stream`); response raw protobuf `CollabBatchSyncResponse`, HTTP 200, no envelope; 429/503 honored.
- Proto: from `W/proto/collab.proto` add to `libs/collab-rt-entity/proto/collab.proto`: `CollabDocStateParams.collab_version=6`,
  `database_restore_id=7`, `PayloadCompressionType.GZIP=2`, messages `CollabBatchSyncRequest/Result/Response` (+ `Rid`).
- Per item: if `doc_state` non-empty → decompress (none/gzip) → `CollabManager::publish_update`
  (`services/appflowy-collaborate/src/collab/collab_manager.rs:239-278`, Rid → message_id); then
  `get_latest_state(ws, oid, type, uid, StateVector::decode_v1(sv))` (`:186-225`) → `missing_update` + `server_state_vector`
  from the same snapshot. Empty doc_state allowed (read probe). `collab_version=""`. Per-item errors in `error`.
- Expose `Arc<CollabManager>` on `AppState` (`src/state.rs:40-64`; built near `src/application.rs:360`).

### T6 — Small stubs (S)
- `GET /{ws}/database/{db}/restore-state` → `{"database_restore_id":null,"version":null}`.
- `GET /{ws}/collab/{id}/collab-exists` → `{exists}` (alias of `get_row_document_collab_exists_handler`, `workspace.rs:1359-1378`).
- `get_page_view_collab_for_view_with_parent` (`page_view.rs:1986-2047`): resolve database id from `view.extra.database_id`
  first (container views); also expose `owner`/`last_editor` under `data` as the web reads (`W/.../collab-api.ts:377-410`).

### T7 — Atomic creation infra + `POST /{ws}/v2/page-view/document/default` (M)
- Web `W/.../http/atomic-creation-api.ts:8-13, 84-145`; receipt validation `W/application/atomic-creation/receipt.ts:24-98`,
  types `atomic-creation/types.ts:1-114`. Body = exact JSON string, retried byte-identical; `-5` = retry.
- Receipt: `{request_id, outcome:"applied"|"already_applied", result}`.
- Migration `af_atomic_creation_receipt(request_id PK, workspace_id, kind, request_sha256, receipt_json, created_at)`.
  Same request_id + same sha → stored receipt with `already_applied`; different sha → InvalidRequest.
- Serialize per workspace (folder lock) while resolving `position` (`start|end|after{view_id}|index{index}`); validate `after` sibling parent.
- Document request `{schema_version:"v1", request_id, parent_view_id, view_id, name, position, icon?, extra?, is_favorite?}`:
  `prepare_default_document_collab_param` (`page_view.rs:303-325`, collab_id=view_id) → insert folder view at index
  (`folder.body.views.insert(txn, view, index, uid)`, see `add_new_view_to_folder` `:733-754`) →
  `update_workspace_folder_data` (`:2375`). Result `{view_id, database_id:null}`.
- Hydration then reads `/view/{id}?depth=1` (T3) + full-sync (T5) and requires `data.document.page_id` → block `ty=="page"`
  with complete `children_map`/`text_map` (`W/.../created-page-hydration.ts:155-298`). Make sure reads right after commit see the update
  (else hold the receipt or return -5).

### T8 — `POST /{ws}/v2/page-view/database/default` (L) — needs T0 for layouts 3–8
- Request (`W/.../page-default-creation.ts:46-106`): `{…, container_id, database:{database_id, view_id, name, layout:0..8, embedded, responses_view_id|null, initial_row_ids[], timezone_id|null}}`.
- Build on `prepare_new_encoded_database` / `create_database_page` (`page_view.rs:352-396, 423-447, 1201-1295`), client row ids.
  Folder: container `container_id` (folder layout = primary, `extra {database_id, is_database_container:true, embedded}`), children
  `view_id` (+`responses_view_id` for Form) with `extra {database_id}`. Register views in WorkspaceDatabase (`:701-716`).
  Result `{view_id: container_id, database_id}`.
- Hydration (`created-page-hydration.ts:93-152`) needs a primary field, `metas.iid` → view with `row_orders`, matching layout numbers,
  rows with `data.data.{id,database_id,cells}` and `data.meta`. Calendar/Timeline: DateTime field + layout setting. Form: primary + responses view.
  Mirror keys from `W/application/database-yjs/{calendar,list,gallery,feed}-layout.ts`, `chart.type.ts`, `form-writer.ts`.

### T9 — `POST /{ws}/v2/page-view/database-view/default` (L)
- Request `{schema_version, request_id, database_restore_id, view:{database_id, view_id, parent_view_id, source_view_id, layout, name, embedded, responses_view_id, timezone_id}, position}`.
- Generalize `create_database_view` (`page_view.rs:2155-2306`): client view_id, parent, `extra {database_id}`, position, copy settings from
  `source_view_id`; add arms in `resolve_dependencies_when_create_database_linked_view` (`database.rs:36-44`).
- Result `{view_id, view_ids:[view_id, responses_view_id?], database_id, database_restore_id:<echo>, database_version:null, database_update_v1:<base64 yrs update>, update_batch_id:<uuid>}`;
  hydration (`W/.../create-default-view.ts:162-195`) requires folder parent == request parent and matching layout.

### T10 — `POST /{ws}/v2/page-view/row-document` (M)
- Request (`W/application/atomic-creation/types.ts:64-74`): `{request_id, database_id, database_restore_id, row_id, source_view_id, name, contribution:{kind:"default_schema", schema_version:"v1"}}`.
- Row not readable yet → -5. `document_id = meta_id_from_row_id(row_id, RowMetaKey::DocumentId)` (as in `src/biz/collab/database.rs:175`);
  create default document collab; don't mutate the row. `kind:"default_actions"` → InvalidRequest.
- Result `{document_id, row_id, database_id, database_restore_id, row_version:null, document_version:null, update_batch_id}`.
- Check `GET /api/workspace/v1/{ws}/collab/{id}` returns `{object_id, doc_state:number[], version:0}` (`row-source-readiness.ts:11-40`).

### T11 — Publish from the app (M) — needs T0
- `publish_page` (`page_view.rs:1663-1850`): accept new layouts, resolve database via container `extra.database_id`,
  default `visible_database_view_ids` to container children. Optional `GET/PATCH /{ws}/publish/{view_id}/config`.

### T12 — `GET /api/workspace/v2/published/{ns}/{name}/snapshot` (M/L)
- Web `W/application/services/js-services/publish-snapshot/http-json-adapter.ts:10-23`, types `publish-snapshot/types.ts:1-156`. Anonymous.
- Data: `published_collab_store.get_collab_metadata` (`workspace.rs:2116-2126`) + blob `get_collab_blob_by_publish_namespace` (`:2128-2138`);
  document blob = raw doc_state, database blob = `PublishDatabaseData` (`libs/shared-entity/src/dto/publish_dto.rs:44-61`).
- Response `{schemaVersion:1, kind, namespace, publishName, view:{viewId, name, icon|null, extra|null, layout, childViews?, ancestorViews?, visibleViewIds?, databaseRelations?}, …}`.
  Document: `document.children` = Slate `Descendant[]` — port `traverseBlock`/`deltaInsertToSlateNode`
  (`W/application/slate-yjs/utils/convert.ts:18-160`) to Rust. Database: `database:{databaseId, activeViewId, visibleViewIds, raw:{database:<JSON of root data.database>, rows:{rowId:<row root map JSON>}, row_documents:{}}}`
  (`W/.../database-yjs-render-bridge.ts:216-260`), via `Collab::to_json_value`.

### Already compatible (no work)
GoTrue auth, `/api/user/verify|profile|workspace`, workspace list/open, favorites/recent/trash, page-view mutations,
document load, database collab load, `/ws/v2/{ws}` (protobuf additive; no FolderChanged notifications in Phase 1).

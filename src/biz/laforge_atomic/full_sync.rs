//! `POST /api/workspace/v1/{workspace_id}/collab/full-sync/batch` (protobuf).
//!
//! Per item: a non-empty `doc_state` is published to the workspace update stream, then the
//! server state missing from `sv` and the server state vector are read from the same snapshot.
//! An empty `doc_state` is a read probe; created-page hydration uses it. This server is
//! versionless: `collab_version` is always empty.

use crate::state::AppState;
use access_control::act::Action;
use anyhow::anyhow;
use app_error::AppError;
use collab::core::origin::CollabOrigin;
use collab_entity::CollabType;
use collab_rt_entity::collab_proto::{
  CollabBatchSyncRequest, CollabBatchSyncResponse, CollabBatchSyncResult, CollabDocStateParams,
  PayloadCompressionType, Rid,
};
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use std::io::{Read, Write};
use uuid::Uuid;
use yrs::updates::decoder::Decode;
use yrs::{StateVector, Update};

pub const MAX_BATCH_ITEMS: usize = 500;
/// Upper bound of one decompressed payload, against compression bombs.
const MAX_DECOMPRESSED_BYTES: u64 = 256 * 1024 * 1024;
/// Items processed concurrently.
const CONCURRENCY: usize = 16;

pub fn compression_type(value: i32) -> Result<PayloadCompressionType, AppError> {
  PayloadCompressionType::try_from(value)
    .map_err(|_| AppError::InvalidRequest(format!("unknown payload compression {}", value)))
}

pub fn decompress(compression: PayloadCompressionType, data: Vec<u8>) -> Result<Vec<u8>, AppError> {
  if data.is_empty() {
    return Ok(data);
  }
  let invalid = |err: std::io::Error| AppError::InvalidRequest(format!("invalid payload: {}", err));
  let mut out = Vec::new();
  match compression {
    PayloadCompressionType::None => return Ok(data),
    PayloadCompressionType::Zstd => {
      zstd::stream::read::Decoder::new(data.as_slice())
        .map_err(invalid)?
        .take(MAX_DECOMPRESSED_BYTES + 1)
        .read_to_end(&mut out)
        .map_err(invalid)?;
    },
    PayloadCompressionType::Gzip => {
      GzDecoder::new(data.as_slice())
        .take(MAX_DECOMPRESSED_BYTES + 1)
        .read_to_end(&mut out)
        .map_err(invalid)?;
    },
  }
  if out.len() as u64 > MAX_DECOMPRESSED_BYTES {
    return Err(AppError::PayloadTooLarge(
      "decompressed payload is too large".to_string(),
    ));
  }
  Ok(out)
}

pub fn compress(compression: PayloadCompressionType, data: Vec<u8>) -> Result<Vec<u8>, AppError> {
  let internal = |err: std::io::Error| AppError::Internal(anyhow!("compression failed: {}", err));
  match compression {
    PayloadCompressionType::None => Ok(data),
    PayloadCompressionType::Zstd => zstd::encode_all(data.as_slice(), 3).map_err(internal),
    PayloadCompressionType::Gzip => {
      let mut encoder = GzEncoder::new(Vec::new(), flate2::Compression::default());
      encoder.write_all(&data).map_err(internal)?;
      encoder.finish().map_err(internal)
    },
  }
}

/// Decoded payloads of one item.
#[derive(Debug)]
pub struct SyncItem {
  pub object_id: Uuid,
  pub collab_type: CollabType,
  pub state_vector: StateVector,
  /// `None` for a read probe or an update that changes nothing.
  pub update: Option<Vec<u8>>,
}

pub fn decode_item(item: CollabDocStateParams) -> Result<SyncItem, AppError> {
  let object_id = Uuid::parse_str(&item.object_id)
    .map_err(|_| AppError::InvalidRequest(format!("invalid object_id {}", item.object_id)))?;
  let collab_type = CollabType::from(item.collab_type);
  if collab_type == CollabType::Unknown {
    return Err(AppError::InvalidRequest(format!(
      "unknown collab_type {}",
      item.collab_type
    )));
  }
  let compression = compression_type(item.compression)?;
  let sv = decompress(compression, item.sv)?;
  let doc_state = decompress(compression, item.doc_state)?;
  let state_vector = if sv.is_empty() {
    StateVector::default()
  } else {
    StateVector::decode_v1(&sv)
      .map_err(|err| AppError::InvalidRequest(format!("invalid state vector: {}", err)))?
  };
  let update = if doc_state.is_empty() {
    None
  } else {
    let decoded = Update::decode_v1(&doc_state)
      .map_err(|err| AppError::InvalidRequest(format!("invalid doc_state: {}", err)))?;
    (!decoded.is_empty()).then_some(doc_state)
  };
  Ok(SyncItem {
    object_id,
    collab_type,
    state_vector,
    update,
  })
}

pub fn error_result(object_id: String, collab_type: i32, err: &AppError) -> CollabBatchSyncResult {
  CollabBatchSyncResult {
    object_id,
    collab_type,
    compression: PayloadCompressionType::None as i32,
    error: err.to_string(),
    ..Default::default()
  }
}

async fn sync_item(
  state: &AppState,
  uid: i64,
  origin: &CollabOrigin,
  workspace_id: Uuid,
  item: CollabDocStateParams,
  response_compression: PayloadCompressionType,
) -> Result<CollabBatchSyncResult, AppError> {
  let object_id_str = item.object_id.clone();
  let collab_type_value = item.collab_type;
  let item = tokio::task::spawn_blocking(move || decode_item(item)).await??;

  let mut message_id = None;
  if let Some(update) = item.update {
    state
      .workspace_access_control
      .enforce_action(&uid, &workspace_id, Action::Write)
      .await?;
    state
      .collab_manager
      .enforce_write_collab(&workspace_id, &uid, &item.object_id)
      .await?;
    let rid = state
      .collab_manager
      .publish_update(
        workspace_id,
        item.object_id,
        item.collab_type,
        origin,
        update,
      )
      .await
      .map_err(AppError::Internal)?;
    message_id = Some(Rid {
      timestamp: rid.timestamp,
      counter: rid.seq_no as u32,
    });
  }

  // Missing update and state vector come from one snapshot, so that applying the update to the
  // client state yields exactly the reported vector.
  let latest = state
    .collab_manager
    .get_latest_state(
      workspace_id,
      item.object_id,
      item.collab_type,
      uid,
      item.state_vector,
    )
    .await?;
  let update = latest.update.to_vec();
  let state_vector = latest.state_vector;
  let (missing_update, server_state_vector) = tokio::task::spawn_blocking(move || {
    Ok::<_, AppError>((
      compress(response_compression, update)?,
      compress(response_compression, state_vector)?,
    ))
  })
  .await??;

  Ok(CollabBatchSyncResult {
    object_id: object_id_str,
    collab_type: collab_type_value,
    compression: response_compression as i32,
    missing_update,
    error: String::new(),
    server_state_vector,
    collab_version: String::new(),
    message_id,
  })
}

/// Item errors are reported in their result; only a malformed batch fails as a whole.
pub async fn collab_full_sync_batch(
  state: &AppState,
  uid: i64,
  origin: CollabOrigin,
  workspace_id: Uuid,
  request: CollabBatchSyncRequest,
) -> Result<CollabBatchSyncResponse, AppError> {
  let response_compression = compression_type(request.response_compression)?;
  if request.items.len() > MAX_BATCH_ITEMS {
    return Err(AppError::InvalidRequest(format!(
      "at most {} items per batch",
      MAX_BATCH_ITEMS
    )));
  }
  state
    .workspace_access_control
    .enforce_action(&uid, &workspace_id, Action::Read)
    .await?;

  let mut results = Vec::with_capacity(request.items.len());
  let mut items = request.items.into_iter().peekable();
  while items.peek().is_some() {
    let chunk = items.by_ref().take(CONCURRENCY).collect::<Vec<_>>();
    let synced = futures::future::join_all(chunk.into_iter().map(|item| {
      let object_id = item.object_id.clone();
      let collab_type = item.collab_type;
      let origin = &origin;
      async move {
        sync_item(state, uid, origin, workspace_id, item, response_compression)
          .await
          .unwrap_or_else(|err| error_result(object_id, collab_type, &err))
      }
    }))
    .await;
    results.extend(synced);
  }

  Ok(CollabBatchSyncResponse {
    results,
    response_compression: response_compression as i32,
  })
}

#[cfg(test)]
mod tests {
  use super::*;
  use prost::Message;
  use yrs::updates::encoder::Encode;
  use yrs::{Doc, GetString, ReadTxn, Text, Transact};

  fn doc_with_text(text: &str) -> Doc {
    let doc = Doc::new();
    let root = doc.get_or_insert_text("t");
    root.insert(&mut doc.transact_mut(), 0, text);
    doc
  }

  #[test]
  fn web_request_round_trips() {
    // Field numbers of the web `collab.CollabBatchSyncRequest`.
    let request = CollabBatchSyncRequest {
      items: vec![CollabDocStateParams {
        object_id: Uuid::new_v4().to_string(),
        collab_type: 0,
        compression: PayloadCompressionType::Gzip as i32,
        sv: vec![0],
        doc_state: vec![],
        collab_version: String::new(),
        database_restore_id: Some("00000000-0000-0000-0000-000000000000".to_string()),
      }],
      response_compression: PayloadCompressionType::None as i32,
    };
    let bytes = request.encode_to_vec();
    assert_eq!(
      CollabBatchSyncRequest::decode(bytes.as_slice()).unwrap(),
      request
    );
  }

  #[test]
  fn legacy_single_item_params_still_decode() {
    // Clients of `/collab/{id}/full-sync` send the first five fields only.
    let legacy = CollabDocStateParams {
      object_id: "a".to_string(),
      collab_type: 1,
      compression: PayloadCompressionType::Zstd as i32,
      sv: vec![1, 2],
      doc_state: vec![3],
      ..Default::default()
    };
    let decoded = CollabDocStateParams::decode(legacy.encode_to_vec().as_slice()).unwrap();
    assert_eq!(decoded.collab_version, "");
    assert_eq!(decoded.database_restore_id, None);
  }

  #[test]
  fn response_round_trips_with_message_id() {
    let response = CollabBatchSyncResponse {
      results: vec![
        CollabBatchSyncResult {
          object_id: "a".to_string(),
          collab_type: 0,
          compression: 0,
          missing_update: vec![1, 2, 3],
          error: String::new(),
          server_state_vector: vec![1, 5, 7],
          collab_version: String::new(),
          message_id: Some(Rid {
            timestamp: 1_760_000_000_000,
            counter: 3,
          }),
        },
        error_result(
          "b".to_string(),
          4,
          &AppError::RecordNotFound("missing".to_string()),
        ),
      ],
      response_compression: 0,
    };
    let decoded = CollabBatchSyncResponse::decode(response.encode_to_vec().as_slice()).unwrap();
    assert_eq!(decoded, response);
    assert!(decoded.results[1].error.contains("missing"));
    assert!(decoded.results[1].missing_update.is_empty());
    assert_eq!(decoded.results[1].message_id, None);
  }

  #[test]
  fn gzip_and_zstd_round_trip() {
    let data = b"hello hello hello hello".to_vec();
    for compression in [
      PayloadCompressionType::None,
      PayloadCompressionType::Gzip,
      PayloadCompressionType::Zstd,
    ] {
      let packed = compress(compression, data.clone()).unwrap();
      assert_eq!(decompress(compression, packed).unwrap(), data);
    }
    // Empty payloads stay empty whatever the compression (web sends empty probes).
    assert!(decompress(PayloadCompressionType::Gzip, vec![])
      .unwrap()
      .is_empty());
  }

  #[test]
  fn decodes_browser_gzip() {
    // `CompressionStream('gzip')` output for "abc".
    let gzip = vec![
      0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03, 0x4b, 0x4c, 0x4a, 0x06, 0x00,
      0xc2, 0x41, 0x24, 0x35, 0x03, 0x00, 0x00, 0x00,
    ];
    assert_eq!(
      decompress(PayloadCompressionType::Gzip, gzip).unwrap(),
      b"abc".to_vec()
    );
    assert!(matches!(
      decompress(PayloadCompressionType::Gzip, vec![1, 2, 3]),
      Err(AppError::InvalidRequest(_))
    ));
    assert!(compression_type(9).is_err());
  }

  #[test]
  fn decodes_items() {
    let doc = doc_with_text("abc");
    let update = doc
      .transact()
      .encode_state_as_update_v1(&StateVector::default());
    let sv = doc.transact().state_vector().encode_v1();
    let object_id = Uuid::new_v4();
    let item = decode_item(CollabDocStateParams {
      object_id: object_id.to_string(),
      collab_type: 0,
      compression: PayloadCompressionType::Gzip as i32,
      sv: compress(PayloadCompressionType::Gzip, sv).unwrap(),
      doc_state: compress(PayloadCompressionType::Gzip, update.clone()).unwrap(),
      ..Default::default()
    })
    .unwrap();
    assert_eq!(item.object_id, object_id);
    assert_eq!(item.collab_type, CollabType::Document);
    assert_eq!(item.update, Some(update));
    assert_eq!(item.state_vector, doc.transact().state_vector());

    // Read probe: empty doc_state, `[0]` (empty) state vector.
    let probe = decode_item(CollabDocStateParams {
      object_id: object_id.to_string(),
      collab_type: 4,
      sv: vec![0],
      ..Default::default()
    })
    .unwrap();
    assert_eq!(probe.update, None);
    assert_eq!(probe.state_vector, StateVector::default());

    // An update that changes nothing is not published.
    let noop = decode_item(CollabDocStateParams {
      object_id: object_id.to_string(),
      collab_type: 0,
      doc_state: vec![0, 0],
      ..Default::default()
    })
    .unwrap();
    assert_eq!(noop.update, None);
  }

  #[test]
  fn rejects_bad_items() {
    let bad_id = CollabDocStateParams {
      object_id: "nope".to_string(),
      ..Default::default()
    };
    assert!(matches!(
      decode_item(bad_id),
      Err(AppError::InvalidRequest(_))
    ));
    let bad_type = CollabDocStateParams {
      object_id: Uuid::new_v4().to_string(),
      collab_type: 99,
      ..Default::default()
    };
    assert!(matches!(
      decode_item(bad_type),
      Err(AppError::InvalidRequest(_))
    ));
    let bad_update = CollabDocStateParams {
      object_id: Uuid::new_v4().to_string(),
      doc_state: vec![255, 255, 255],
      ..Default::default()
    };
    assert!(matches!(
      decode_item(bad_update),
      Err(AppError::InvalidRequest(_))
    ));
  }

  #[test]
  fn missing_update_applied_to_an_empty_doc_matches_the_state_vector() {
    // The invariant the web hydration checks on a read probe.
    let doc = doc_with_text("hello");
    let txn = doc.transact();
    let missing = txn.encode_state_as_update_v1(&StateVector::default());
    let reported = txn.state_vector();
    let replica = Doc::new();
    replica
      .transact_mut()
      .apply_update(Update::decode_v1(&missing).unwrap())
      .unwrap();
    assert_eq!(replica.transact().state_vector(), reported);
    assert_eq!(
      replica
        .get_or_insert_text("t")
        .get_string(&replica.transact()),
      "hello"
    );
  }
}

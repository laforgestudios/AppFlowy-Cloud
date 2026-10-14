use app_error::AppError;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

/// Waiting longer than this for another creation of the same workspace answers RetryLater.
const WORKSPACE_LOCK_TIMEOUT: &str = "10s";
/// Postgres `lock_not_available`, raised when `lock_timeout` expires.
const LOCK_NOT_AVAILABLE: &str = "55P03";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AtomicCreationKind {
  Document,
  Database,
  DatabaseView,
  RowDocument,
}

impl AtomicCreationKind {
  pub fn as_str(&self) -> &'static str {
    match self {
      AtomicCreationKind::Document => "document",
      AtomicCreationKind::Database => "database",
      AtomicCreationKind::DatabaseView => "database-view",
      AtomicCreationKind::RowDocument => "row-document",
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AtomicCreationOutcome {
  Applied,
  AlreadyApplied,
}

/// `{request_id, outcome, result}` as validated by the web (`atomic-creation/receipt.ts`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AtomicCreationReceipt<R> {
  pub request_id: Uuid,
  pub outcome: AtomicCreationOutcome,
  pub result: R,
}

/// Lowercase hex SHA-256 of the exact request body bytes.
pub fn request_sha256_hex(body: &[u8]) -> String {
  Sha256::digest(body)
    .iter()
    .map(|byte| format!("{:02x}", byte))
    .collect()
}

#[derive(Debug, Clone)]
pub struct StoredReceipt {
  pub workspace_id: Uuid,
  pub kind: String,
  pub request_sha256: String,
  pub receipt_json: Value,
}

#[derive(Debug, PartialEq)]
pub enum ReceiptDecision {
  /// No receipt yet: apply the creation.
  Apply,
  /// Same request already applied: answer with this receipt (outcome `already_applied`).
  Replay(Value),
}

/// A request_id identifies one exact request body of one kind in one workspace.
pub fn decide_receipt(
  stored: Option<StoredReceipt>,
  workspace_id: &Uuid,
  kind: AtomicCreationKind,
  request_sha256: &str,
) -> Result<ReceiptDecision, AppError> {
  match stored {
    None => Ok(ReceiptDecision::Apply),
    Some(stored)
      if stored.workspace_id == *workspace_id
        && stored.kind == kind.as_str()
        && stored.request_sha256 == request_sha256 =>
    {
      Ok(ReceiptDecision::Replay(with_outcome(
        stored.receipt_json,
        AtomicCreationOutcome::AlreadyApplied,
      )))
    },
    Some(_) => Err(AppError::InvalidRequest(
      "request_id was already used by a different creation request".to_string(),
    )),
  }
}

fn with_outcome(mut receipt: Value, outcome: AtomicCreationOutcome) -> Value {
  if let Some(object) = receipt.as_object_mut() {
    object.insert(
      "outcome".to_string(),
      serde_json::to_value(outcome).unwrap_or(Value::Null),
    );
  }
  receipt
}

/// Advisory lock key serializing the atomic creations of one workspace.
pub fn workspace_lock_key(workspace_id: &Uuid) -> i64 {
  let digest = Sha256::digest(format!("af_atomic_creation:{}", workspace_id).as_bytes());
  let mut key = [0u8; 8];
  key.copy_from_slice(&digest[..8]);
  i64::from_be_bytes(key)
}

pub enum AtomicCreationStart {
  /// The request was already applied; this is its stored receipt.
  Replay(Value),
  /// The caller must apply the creation, then call [AtomicCreation::commit].
  Apply(AtomicCreation),
}

/// An open creation. Holds the workspace lock (a transaction-scoped advisory lock) until it is
/// committed or dropped, so position resolution and Folder insertion of concurrent creations in
/// the same workspace never interleave. Dropping it without commit stores no receipt.
pub struct AtomicCreation {
  tx: Transaction<'static, Postgres>,
  workspace_id: Uuid,
  kind: AtomicCreationKind,
  request_id: Uuid,
  request_sha256: String,
}

impl AtomicCreation {
  pub async fn begin(
    pg_pool: &PgPool,
    workspace_id: Uuid,
    kind: AtomicCreationKind,
    request_id: Uuid,
    body: &[u8],
  ) -> Result<AtomicCreationStart, AppError> {
    let request_sha256 = request_sha256_hex(body);
    let mut tx = pg_pool.begin().await?;
    sqlx::query(&format!(
      "SET LOCAL lock_timeout = '{}'",
      WORKSPACE_LOCK_TIMEOUT
    ))
    .execute(&mut *tx)
    .await?;
    if let Err(err) = sqlx::query("SELECT pg_advisory_xact_lock($1)")
      .bind(workspace_lock_key(&workspace_id))
      .execute(&mut *tx)
      .await
    {
      let busy = matches!(&err, sqlx::Error::Database(db) if db.code().as_deref() == Some(LOCK_NOT_AVAILABLE));
      return Err(if busy {
        AppError::RetryLater(anyhow::anyhow!(
          "another creation is in progress in this workspace"
        ))
      } else {
        err.into()
      });
    }

    let stored = select_receipt(&mut tx, &request_id).await?;
    match decide_receipt(stored, &workspace_id, kind, &request_sha256)? {
      ReceiptDecision::Replay(receipt) => {
        tx.rollback().await?;
        Ok(AtomicCreationStart::Replay(receipt))
      },
      ReceiptDecision::Apply => Ok(AtomicCreationStart::Apply(Self {
        tx,
        workspace_id,
        kind,
        request_id,
        request_sha256,
      })),
    }
  }

  pub fn workspace_id(&self) -> Uuid {
    self.workspace_id
  }

  pub fn request_id(&self) -> Uuid {
    self.request_id
  }

  /// Stores the receipt, releases the workspace lock and returns the receipt JSON.
  /// Call it only once the created objects are readable (see `folder::await_folder_views`).
  pub async fn commit<R: Serialize>(mut self, result: R) -> Result<Value, AppError> {
    let receipt = serde_json::to_value(AtomicCreationReceipt {
      request_id: self.request_id,
      outcome: AtomicCreationOutcome::Applied,
      result,
    })?;
    sqlx::query(
      r#"
        INSERT INTO af_atomic_creation_receipt
          (request_id, workspace_id, kind, request_sha256, receipt_json)
        VALUES ($1, $2, $3, $4, $5)
      "#,
    )
    .bind(self.request_id)
    .bind(self.workspace_id)
    .bind(self.kind.as_str())
    .bind(&self.request_sha256)
    .bind(&receipt)
    .execute(&mut *self.tx)
    .await?;
    self.tx.commit().await?;
    Ok(receipt)
  }
}

async fn select_receipt(
  tx: &mut Transaction<'static, Postgres>,
  request_id: &Uuid,
) -> Result<Option<StoredReceipt>, AppError> {
  let row: Option<(Uuid, String, String, Value)> = sqlx::query_as(
    r#"
      SELECT workspace_id, kind, request_sha256, receipt_json
      FROM af_atomic_creation_receipt
      WHERE request_id = $1
    "#,
  )
  .bind(request_id)
  .fetch_optional(&mut **tx)
  .await?;
  Ok(row.map(
    |(workspace_id, kind, request_sha256, receipt_json)| StoredReceipt {
      workspace_id,
      kind,
      request_sha256,
      receipt_json,
    },
  ))
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;

  fn stored(workspace_id: Uuid, kind: &str, sha: &str) -> StoredReceipt {
    StoredReceipt {
      workspace_id,
      kind: kind.to_string(),
      request_sha256: sha.to_string(),
      receipt_json: json!({
        "request_id": "8b0b2b8e-5f6c-4a39-9c43-2f0d7d4b1a01",
        "outcome": "applied",
        "result": {"view_id": "v", "database_id": null}
      }),
    }
  }

  #[test]
  fn sha256_is_lowercase_hex_of_exact_bytes() {
    assert_eq!(
      request_sha256_hex(b"abc"),
      "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    // Formatting differences are different requests.
    assert_ne!(
      request_sha256_hex(br#"{"a":1}"#),
      request_sha256_hex(br#"{"a": 1}"#)
    );
  }

  #[test]
  fn no_receipt_applies() {
    let ws = Uuid::new_v4();
    assert_eq!(
      decide_receipt(None, &ws, AtomicCreationKind::Document, "x").unwrap(),
      ReceiptDecision::Apply
    );
  }

  #[test]
  fn same_request_replays_with_already_applied() {
    let ws = Uuid::new_v4();
    let decision = decide_receipt(
      Some(stored(ws, "document", "abc")),
      &ws,
      AtomicCreationKind::Document,
      "abc",
    )
    .unwrap();
    match decision {
      ReceiptDecision::Replay(receipt) => {
        assert_eq!(receipt["outcome"], "already_applied");
        assert_eq!(receipt["result"]["view_id"], "v");
        assert_eq!(
          receipt["request_id"],
          "8b0b2b8e-5f6c-4a39-9c43-2f0d7d4b1a01"
        );
      },
      other => panic!("unexpected {:?}", other),
    }
  }

  #[test]
  fn different_body_is_rejected() {
    let ws = Uuid::new_v4();
    let err = decide_receipt(
      Some(stored(ws, "document", "abc")),
      &ws,
      AtomicCreationKind::Document,
      "abd",
    )
    .unwrap_err();
    assert!(matches!(err, AppError::InvalidRequest(_)));
  }

  #[test]
  fn other_kind_or_workspace_is_rejected() {
    let ws = Uuid::new_v4();
    assert!(matches!(
      decide_receipt(
        Some(stored(ws, "document", "abc")),
        &ws,
        AtomicCreationKind::RowDocument,
        "abc",
      ),
      Err(AppError::InvalidRequest(_))
    ));
    assert!(matches!(
      decide_receipt(
        Some(stored(Uuid::new_v4(), "document", "abc")),
        &ws,
        AtomicCreationKind::Document,
        "abc",
      ),
      Err(AppError::InvalidRequest(_))
    ));
  }

  #[test]
  fn lock_key_is_stable_per_workspace() {
    let ws = Uuid::parse_str("2f6c8d3e-1b6a-4c55-8f1e-0a6b4c2d9e10").unwrap();
    assert_eq!(workspace_lock_key(&ws), workspace_lock_key(&ws));
    assert_ne!(workspace_lock_key(&ws), workspace_lock_key(&Uuid::new_v4()));
  }

  /// Against a database with the migration applied and an `af_workspace` row for the workspace:
  /// `LAFORGE_TEST_DATABASE_URL=postgres://… LAFORGE_TEST_WORKSPACE_ID=<uuid> cargo test …`.
  /// Skipped when the variables are absent.
  #[tokio::test]
  async fn receipt_store_round_trip_against_postgres() {
    let (Ok(url), Ok(workspace_id)) = (
      std::env::var("LAFORGE_TEST_DATABASE_URL"),
      std::env::var("LAFORGE_TEST_WORKSPACE_ID"),
    ) else {
      eprintln!("LAFORGE_TEST_DATABASE_URL not set, skipping");
      return;
    };
    let workspace_id = Uuid::parse_str(&workspace_id).unwrap();
    let pool = PgPool::connect(&url).await.unwrap();
    let request_id = Uuid::new_v4();
    let body = format!(r#"{{"request_id":"{}"}}"#, request_id);
    let kind = AtomicCreationKind::Document;

    let creation =
      match AtomicCreation::begin(&pool, workspace_id, kind, request_id, body.as_bytes())
        .await
        .unwrap()
      {
        AtomicCreationStart::Apply(creation) => creation,
        AtomicCreationStart::Replay(_) => panic!("fresh request replayed"),
      };
    // Another creation of the same workspace waits for the lock; a dropped creation stores
    // nothing, so commit and check the replay instead.
    let receipt = creation
      .commit(json!({"view_id": "v", "database_id": null}))
      .await
      .unwrap();
    assert_eq!(receipt["outcome"], "applied");

    match AtomicCreation::begin(&pool, workspace_id, kind, request_id, body.as_bytes())
      .await
      .unwrap()
    {
      AtomicCreationStart::Replay(replayed) => {
        assert_eq!(replayed["outcome"], "already_applied");
        assert_eq!(replayed["result"], receipt["result"]);
      },
      AtomicCreationStart::Apply(_) => panic!("applied twice"),
    }
    assert!(matches!(
      AtomicCreation::begin(&pool, workspace_id, kind, request_id, b"{}").await,
      Err(AppError::InvalidRequest(_))
    ));

    // Lock contention: a held creation makes a second one of the same workspace wait.
    let other = Uuid::new_v4();
    let held = match AtomicCreation::begin(&pool, workspace_id, kind, other, b"a")
      .await
      .unwrap()
    {
      AtomicCreationStart::Apply(creation) => creation,
      AtomicCreationStart::Replay(_) => panic!("fresh request replayed"),
    };
    let pool2 = pool.clone();
    let waiter = tokio::spawn(async move {
      AtomicCreation::begin(&pool2, workspace_id, kind, Uuid::new_v4(), b"b")
        .await
        .map(|start| matches!(start, AtomicCreationStart::Apply(_)))
    });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(!waiter.is_finished());
    drop(held);
    assert!(waiter.await.unwrap().unwrap());

    sqlx::query("DELETE FROM af_atomic_creation_receipt WHERE request_id = $1")
      .bind(request_id)
      .execute(&pool)
      .await
      .unwrap();
  }

  #[test]
  fn kinds_match_web_paths() {
    assert_eq!(AtomicCreationKind::Document.as_str(), "document");
    assert_eq!(AtomicCreationKind::Database.as_str(), "database");
    assert_eq!(AtomicCreationKind::DatabaseView.as_str(), "database-view");
    assert_eq!(AtomicCreationKind::RowDocument.as_str(), "row-document");
  }
}

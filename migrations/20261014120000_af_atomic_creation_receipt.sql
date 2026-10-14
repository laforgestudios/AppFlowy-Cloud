-- Receipts of atomic object creations (default page, database, database view, row document).
-- A client retries a creation with the byte-identical request body; the stored receipt is
-- returned for the same request_id + request_sha256, and a reused request_id with another body
-- is rejected.
CREATE TABLE IF NOT EXISTS af_atomic_creation_receipt (
  request_id UUID PRIMARY KEY,
  workspace_id UUID NOT NULL REFERENCES af_workspace(workspace_id) ON DELETE CASCADE,
  kind TEXT NOT NULL,
  -- lowercase hex SHA-256 of the exact request body bytes
  request_sha256 TEXT NOT NULL,
  receipt_json JSONB NOT NULL,
  created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_af_atomic_creation_receipt_workspace_created_at
  ON af_atomic_creation_receipt (workspace_id, created_at);

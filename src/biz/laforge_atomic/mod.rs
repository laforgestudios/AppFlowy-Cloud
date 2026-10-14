//! Atomic object creation for the current web client.
//!
//! A creation request carries client-chosen ids and a `request_id`. The web retries the
//! byte-identical body until it gets a receipt, so every creation must be idempotent:
//! - [receipt]: per-workspace serialization (Postgres advisory lock) and the receipt store.
//! - [position]: resolution of `start | end | after{view_id} | index{index}` in a Folder parent.
//! - [folder]: Folder insertion and the read-your-writes check of the published Folder update.
//! - [collab]: presence check and insertion of client-id collabs, readability check.
//!
//! [full_sync] is the batch full-sync used by the web to hydrate created objects.
//!
//! Ordering rule for every kind: collabs are committed first, then the Folder update is
//! published, then the receipt is stored. A failure in between leaves state that a retry of the
//! same request converges on (existing collabs are never overwritten, existing Folder views
//! under the requested parent are adopted), and the receipt only exists once everything is
//! readable.

pub mod collab;
pub mod folder;
pub mod full_sync;
pub mod position;
pub mod receipt;

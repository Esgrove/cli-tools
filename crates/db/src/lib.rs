//! Shared `SQLite` database connection, per-component migrations, and cross-tool active file locks.
//!
//! All database-backed tools in `cli-tools` share the unified `cli-tools.db` file managed by [`sqlite`]
//! and coordinate concurrent file operations through the [`locks`] lease store.

#![cfg_attr(test, allow(clippy::panic_in_result_fn))]

pub mod locks;
pub mod sqlite;

pub use locks::{AcquireOutcome, ActiveLockStore, DEFAULT_LOCK_TTL_SECS, FileLockGuard, LockInfo};

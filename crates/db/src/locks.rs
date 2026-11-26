//! Cross-tool active file locks stored in `cli-tools.db`.
//!
//! Coordinates concurrent executions of `vconvert` and sibling tools (`vres`, `thumbs`, `dirmove`, `dupefind`)
//! so two processes do not convert, move, rename, or delete the same file at the same time.
//! Locks carry an owner ID, process ID, host name, and sliding TTL lease refreshed by a background heartbeat thread.
//! Stale locks from crashed processes expire automatically via TTL or same-host PID liveness checks.

use std::env;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};

use crate::sqlite;

/// Schema migration component name for `active_locks`.
const COMPONENT_NAME: &str = "core_locks";
/// Current schema version for `active_locks`.
const SCHEMA_VERSION: i64 = 1;
/// Default lock lease time-to-live in seconds.
pub const DEFAULT_LOCK_TTL_SECS: i64 = 120;
/// Process-wide sequence counter ensuring distinct owner IDs within one process.
static OWNER_SEQUENCE: AtomicU64 = AtomicU64::new(1);

/// Result of attempting to acquire a cross-tool lock set.
#[derive(Debug)]
pub enum AcquireOutcome {
    /// All requested paths were locked for this owner.
    Acquired(FileLockGuard),
    /// At least one requested path is already held by another live owner.
    AlreadyLocked(LockInfo),
}

/// Metadata describing an active lock row in `active_locks`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockInfo {
    /// Canonicalized or normalized path protected by this lock.
    pub canonical_path: String,
    /// Unique owner identifier (`hostname:pid:sequence`).
    pub owner_id: String,
    /// Name of the CLI tool holding the lock.
    pub tool: String,
    /// Operation being performed (for example `"convert"`, `"remux"`, `"rename"`).
    pub operation: String,
    /// Operating system process ID of the lock holder.
    pub pid: u32,
    /// Host name of the machine holding the lock.
    pub hostname: String,
    /// Unix timestamp (seconds) when the lock was acquired.
    pub started_at: i64,
    /// Unix timestamp (seconds) of the last heartbeat refresh.
    pub heartbeat_at: i64,
    /// Unix timestamp (seconds) after which the lock is considered expired.
    pub expires_at: i64,
}

/// Shared state used to stop a guard's background heartbeat thread promptly on drop.
#[derive(Debug)]
struct HeartbeatStop {
    mutex: Mutex<bool>,
    condvar: Condvar,
}

/// RAII guard holding one or more `active_locks` rows until dropped or explicitly released.
#[derive(Debug)]
pub struct FileLockGuard {
    db_path: PathBuf,
    owner_id: String,
    locked_paths: Vec<String>,
    stop: Arc<HeartbeatStop>,
    heartbeat_handle: Option<JoinHandle<()>>,
    released: bool,
}

/// Store for acquiring, inspecting, and releasing cross-tool locks in `cli-tools.db`.
#[derive(Debug, Clone)]
pub struct ActiveLockStore {
    db_path: PathBuf,
    owner_id: String,
    tool: String,
    pid: u32,
    hostname: String,
    ttl_secs: i64,
}

impl FileLockGuard {
    /// Return the normalized path keys protected by this guard.
    #[must_use]
    pub fn locked_paths(&self) -> &[String] {
        &self.locked_paths
    }

    /// Release the locks immediately instead of waiting for `Drop`.
    pub fn release(mut self) {
        self.release_inner();
    }

    /// Stop the heartbeat thread and delete all lock rows owned by this guard.
    fn release_inner(&mut self) {
        if self.released {
            return;
        }
        self.released = true;

        if let Ok(mut stopped) = self.stop.mutex.lock() {
            *stopped = true;
            self.stop.condvar.notify_all();
        }
        if let Some(handle) = self.heartbeat_handle.take() {
            let _ = handle.join();
        }

        let _ = release_paths_at(&self.db_path, &self.owner_id, &self.locked_paths);
    }
}

impl Drop for FileLockGuard {
    fn drop(&mut self) {
        self.release_inner();
    }
}

impl ActiveLockStore {
    /// Open the cross-tool lock store against the default `cli-tools.db`.
    ///
    /// # Errors
    /// Returns an error if the database path cannot be resolved or initialized.
    pub fn open_default(tool: impl Into<String>) -> Result<Self> {
        let db_path = sqlite::database_path()?;
        Self::open_at(db_path, tool)
    }

    /// Open the cross-tool lock store against a specific database path.
    ///
    /// # Errors
    /// Returns an error if the database cannot be opened or migrated.
    pub fn open_at(db_path: PathBuf, tool: impl Into<String>) -> Result<Self> {
        let mut connection = sqlite::open_at(&db_path)?;
        ensure_schema(&mut connection)?;
        drop(connection);

        let pid = std::process::id();
        let hostname = current_hostname();
        let seq = OWNER_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let owner_id = format!("{hostname}:{pid}:{seq}");

        Ok(Self {
            db_path,
            owner_id,
            tool: tool.into(),
            pid,
            hostname,
            ttl_secs: DEFAULT_LOCK_TTL_SECS,
        })
    }

    /// Return this store's unique owner identifier.
    #[must_use]
    pub fn owner_id(&self) -> &str {
        &self.owner_id
    }

    /// Override the lease TTL in seconds (useful in tests).
    #[must_use]
    pub const fn with_ttl_secs(mut self, ttl_secs: i64) -> Self {
        self.ttl_secs = if ttl_secs < 1 { 1 } else { ttl_secs };
        self
    }

    /// Atomically acquire locks on all `paths` for `operation`, or report the blocking lock.
    ///
    /// Expired locks and locks held by dead processes on the same host are pruned automatically.
    ///
    /// # Errors
    /// Returns an error if the database transaction fails.
    pub fn try_acquire(&self, paths: &[PathBuf], operation: &str) -> Result<AcquireOutcome> {
        let normalized_paths = normalize_unique_lock_paths(paths);
        if normalized_paths.is_empty() {
            return Ok(AcquireOutcome::Acquired(FileLockGuard {
                db_path: self.db_path.clone(),
                owner_id: self.owner_id.clone(),
                locked_paths: Vec::new(),
                stop: Arc::new(HeartbeatStop {
                    mutex: Mutex::new(true),
                    condvar: Condvar::new(),
                }),
                heartbeat_handle: None,
                released: true,
            }));
        }

        let mut connection = sqlite::open_at(&self.db_path)?;
        let now = unix_now_secs();
        let expires_at = now.saturating_add(self.ttl_secs);

        let tx = sqlite::immediate_transaction(&mut connection)?;
        prune_stale_locks(&tx, now, &self.hostname)?;

        for key in &normalized_paths {
            if let Some(existing) = query_lock(&tx, key)?
                && existing.owner_id != self.owner_id
            {
                tx.commit().context("Failed to commit stale-lock cleanup")?;
                return Ok(AcquireOutcome::AlreadyLocked(existing));
            }
        }

        for key in &normalized_paths {
            tx.execute(
                r"
                INSERT INTO active_locks (
                    canonical_path, owner_id, tool, operation, pid, hostname, started_at, heartbeat_at, expires_at
                )
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                ON CONFLICT(canonical_path) DO UPDATE SET
                    owner_id = excluded.owner_id,
                    tool = excluded.tool,
                    operation = excluded.operation,
                    pid = excluded.pid,
                    hostname = excluded.hostname,
                    heartbeat_at = excluded.heartbeat_at,
                    expires_at = excluded.expires_at
                ",
                params![
                    key,
                    &self.owner_id,
                    &self.tool,
                    operation,
                    i64::from(self.pid),
                    &self.hostname,
                    now,
                    now,
                    expires_at,
                ],
            )
            .with_context(|| format!("Failed to acquire active lock for {key}"))?;
        }

        tx.commit().context("Failed to commit active lock acquisition")?;

        let stop = Arc::new(HeartbeatStop {
            mutex: Mutex::new(false),
            condvar: Condvar::new(),
        });
        let heartbeat_handle = spawn_heartbeat(
            self.db_path.clone(),
            self.owner_id.clone(),
            normalized_paths.clone(),
            self.ttl_secs,
            Arc::clone(&stop),
        );

        Ok(AcquireOutcome::Acquired(FileLockGuard {
            db_path: self.db_path.clone(),
            owner_id: self.owner_id.clone(),
            locked_paths: normalized_paths,
            stop,
            heartbeat_handle: Some(heartbeat_handle),
            released: false,
        }))
    }

    /// Check whether `path` is currently locked by a live owner other than this store.
    ///
    /// # Errors
    /// Returns an error if the database query fails.
    pub fn is_locked_by_other(&self, path: &Path) -> Result<Option<LockInfo>> {
        self.is_any_locked_by_other(&[path.to_path_buf()])
    }

    /// Check whether any path in `paths` is currently locked by a live owner other than this store.
    ///
    /// # Errors
    /// Returns an error if the database query fails.
    pub fn is_any_locked_by_other(&self, paths: &[PathBuf]) -> Result<Option<LockInfo>> {
        let keys = normalize_unique_lock_paths(paths);
        if keys.is_empty() {
            return Ok(None);
        }

        let Some(connection) = sqlite::open_read_only_at(&self.db_path)? else {
            return Ok(None);
        };
        if !sqlite::table_exists(&connection, "main", "active_locks")? {
            return Ok(None);
        }

        let now = unix_now_secs();
        for key in &keys {
            if let Some(info) = query_lock(&connection, key)?
                && info.owner_id != self.owner_id
                && is_lock_live(&info, now, &self.hostname)
            {
                return Ok(Some(info));
            }
        }
        Ok(None)
    }

    /// Release every lock row currently owned by this store (useful in signal handlers).
    ///
    /// # Errors
    /// Returns an error if the database delete fails.
    pub fn release_all_owned(&self) -> Result<usize> {
        let mut connection = sqlite::open_at(&self.db_path)?;
        let tx = sqlite::immediate_transaction(&mut connection)?;
        let deleted = tx
            .execute("DELETE FROM active_locks WHERE owner_id = ?1", params![&self.owner_id])
            .context("Failed to release owned active locks")?;
        tx.commit().context("Failed to commit active lock release")?;
        Ok(deleted)
    }

    /// List all currently live locks in the database.
    ///
    /// # Errors
    /// Returns an error if querying the database fails.
    pub fn list_live_locks(&self) -> Result<Vec<LockInfo>> {
        let Some(connection) = sqlite::open_read_only_at(&self.db_path)? else {
            return Ok(Vec::new());
        };
        if !sqlite::table_exists(&connection, "main", "active_locks")? {
            return Ok(Vec::new());
        }
        let now = unix_now_secs();
        let mut statement = connection.prepare(
            r"
            SELECT canonical_path, owner_id, tool, operation, pid, hostname, started_at, heartbeat_at, expires_at
            FROM active_locks
            ORDER BY started_at ASC
            ",
        )?;
        let mut rows = statement.query([])?;
        let mut locks = Vec::new();
        while let Some(row) = rows.next()? {
            let info = lock_info_from_row(row)?;
            if is_lock_live(&info, now, &self.hostname) {
                locks.push(info);
            }
        }
        Ok(locks)
    }
}

/// Ensure the `active_locks` table and indexes exist in `connection`.
///
/// # Errors
/// Returns an error if the migration fails.
pub fn ensure_schema(connection: &mut Connection) -> Result<()> {
    sqlite::migrate_component(connection, COMPONENT_NAME, SCHEMA_VERSION, |tx, current_version| {
        if current_version < 1 {
            tx.execute_batch(
                r"
                CREATE TABLE IF NOT EXISTS active_locks (
                    canonical_path TEXT PRIMARY KEY,
                    owner_id TEXT NOT NULL,
                    tool TEXT NOT NULL,
                    operation TEXT NOT NULL,
                    pid INTEGER NOT NULL,
                    hostname TEXT NOT NULL,
                    started_at INTEGER NOT NULL,
                    heartbeat_at INTEGER NOT NULL,
                    expires_at INTEGER NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_active_locks_owner ON active_locks(owner_id);
                CREATE INDEX IF NOT EXISTS idx_active_locks_expires ON active_locks(expires_at);
                ",
            )
            .context("Failed to create active_locks table")?;
        }
        Ok(())
    })
}

/// Check whether `path` is currently locked by any live process in the default `cli-tools.db`.
///
/// Intended for read-mostly or lightweight callers that only want to skip actively locked files
/// without holding a long-lived `ActiveLockStore`.
#[must_use]
pub fn is_path_locked_in_default_db(path: &Path) -> Option<LockInfo> {
    let db_path = sqlite::database_path().ok()?;
    let connection = sqlite::open_read_only_at(&db_path).ok()??;
    if !sqlite::table_exists(&connection, "main", "active_locks").ok()? {
        return None;
    }
    let key = normalize_lock_path(path);
    let info = query_lock(&connection, &key).ok()??;
    let now = unix_now_secs();
    let hostname = current_hostname();
    is_lock_live(&info, now, &hostname).then_some(info)
}

/// Return whether operating system process `pid` appears to be alive on the local machine.
#[must_use]
pub fn is_process_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(unix)]
    {
        let Ok(pid_i32) = i32::try_from(pid) else {
            return false;
        };
        // SAFETY: `kill(pid, 0)` performs existence and permission checks without delivering a signal.
        #[allow(unsafe_code)]
        let result = unsafe { libc::kill(pid_i32, 0) };
        if result == 0 {
            return true;
        }
        std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };

        const STILL_ACTIVE: u32 = 259;

        // SAFETY: `OpenProcess` and `GetExitCodeProcess` read process state and `CloseHandle` releases the handle.
        #[allow(unsafe_code)]
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if handle.is_null() {
                return false;
            }
            let mut exit_code: u32 = 0;
            let ok = GetExitCodeProcess(handle, &raw mut exit_code) != 0;
            let _ = CloseHandle(handle);
            ok && exit_code == STILL_ACTIVE
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        true
    }
}

/// Normalize a file path into a stable canonical lock key, even when the target does not exist yet.
#[must_use]
pub fn normalize_lock_path(path: &Path) -> String {
    if let Ok(canonical) = dunce::canonicalize(path) {
        return canonical.to_string_lossy().into_owned();
    }
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
        && let Ok(canonical_parent) = dunce::canonicalize(parent)
        && let Some(file_name) = path.file_name()
    {
        return canonical_parent.join(file_name).to_string_lossy().into_owned();
    }
    path.to_string_lossy().into_owned()
}

/// Normalize and deduplicate a list of paths into stable lock keys.
fn normalize_unique_lock_paths(paths: &[PathBuf]) -> Vec<String> {
    let mut keys = Vec::with_capacity(paths.len());
    for path in paths {
        let key = normalize_lock_path(path);
        if !keys.contains(&key) {
            keys.push(key);
        }
    }
    keys
}

/// Spawn the background heartbeat refresher thread for a lock guard.
fn spawn_heartbeat(
    db_path: PathBuf,
    owner_id: String,
    locked_paths: Vec<String>,
    ttl_secs: i64,
    stop: Arc<HeartbeatStop>,
) -> JoinHandle<()> {
    let interval_secs = u64::try_from((ttl_secs / 4).max(1)).unwrap_or(1);
    let interval = Duration::from_secs(interval_secs);

    thread::spawn(move || {
        let Ok(mut stopped) = stop.mutex.lock() else {
            return;
        };
        while !*stopped {
            let wait_result = stop.condvar.wait_timeout(stopped, interval);
            match wait_result {
                Ok((guard, _)) => {
                    stopped = guard;
                    if *stopped {
                        break;
                    }
                }
                Err(_) => break,
            }
            drop(stopped);
            let _ = refresh_heartbeat(&db_path, &owner_id, &locked_paths, ttl_secs);
            stopped = match stop.mutex.lock() {
                Ok(guard) => guard,
                Err(_) => break,
            };
        }
    })
}

/// Extend `heartbeat_at` and `expires_at` for all lock rows held by `owner_id`.
fn refresh_heartbeat(db_path: &Path, owner_id: &str, locked_paths: &[String], ttl_secs: i64) -> Result<()> {
    let mut connection = sqlite::open_at(db_path)?;
    let now = unix_now_secs();
    let expires_at = now.saturating_add(ttl_secs);
    let tx = sqlite::immediate_transaction(&mut connection)?;
    for key in locked_paths {
        tx.execute(
            r"
            UPDATE active_locks
            SET heartbeat_at = ?1, expires_at = ?2
            WHERE canonical_path = ?3 AND owner_id = ?4
            ",
            params![now, expires_at, key, owner_id],
        )?;
    }
    tx.commit()?;
    Ok(())
}

/// Delete the specified locked paths owned by `owner_id`.
fn release_paths_at(db_path: &Path, owner_id: &str, locked_paths: &[String]) -> Result<()> {
    let mut connection = sqlite::open_at(db_path)?;
    let tx = sqlite::immediate_transaction(&mut connection)?;
    for key in locked_paths {
        tx.execute(
            "DELETE FROM active_locks WHERE canonical_path = ?1 AND owner_id = ?2",
            params![key, owner_id],
        )?;
    }
    tx.commit()?;
    Ok(())
}

/// Prune expired locks and locks whose same-host process ID is no longer alive.
fn prune_stale_locks(tx: &rusqlite::Transaction<'_>, now: i64, local_hostname: &str) -> Result<()> {
    tx.execute("DELETE FROM active_locks WHERE expires_at <= ?1", params![now])
        .context("Failed to delete expired active locks")?;

    let mut statement = tx.prepare("SELECT canonical_path, pid FROM active_locks WHERE hostname = ?1")?;
    let mut dead_paths = Vec::new();
    let mut rows = statement.query(params![local_hostname])?;
    while let Some(row) = rows.next()? {
        let path: String = row.get(0)?;
        let pid_i64: i64 = row.get(1)?;
        let pid = u32::try_from(pid_i64).unwrap_or(0);
        if !is_process_alive(pid) {
            dead_paths.push(path);
        }
    }
    drop(rows);
    drop(statement);

    for path in dead_paths {
        tx.execute("DELETE FROM active_locks WHERE canonical_path = ?1", params![path])?;
    }
    Ok(())
}

/// Query a single lock row by canonical path key.
fn query_lock(connection: &Connection, canonical_path: &str) -> Result<Option<LockInfo>> {
    connection
        .query_row(
            r"
            SELECT canonical_path, owner_id, tool, operation, pid, hostname, started_at, heartbeat_at, expires_at
            FROM active_locks
            WHERE canonical_path = ?1
            ",
            params![canonical_path],
            lock_info_from_row,
        )
        .optional()
        .context("Failed to query active_locks")
}

/// Decode a [`LockInfo`] from a SQLite row.
fn lock_info_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<LockInfo> {
    let pid_i64: i64 = row.get(4)?;
    Ok(LockInfo {
        canonical_path: row.get(0)?,
        owner_id: row.get(1)?,
        tool: row.get(2)?,
        operation: row.get(3)?,
        pid: u32::try_from(pid_i64).unwrap_or(0),
        hostname: row.get(5)?,
        started_at: row.get(6)?,
        heartbeat_at: row.get(7)?,
        expires_at: row.get(8)?,
    })
}

/// Return whether `info` has not expired and, on the local host, belongs to a live PID.
fn is_lock_live(info: &LockInfo, now: i64, local_hostname: &str) -> bool {
    if info.expires_at <= now {
        return false;
    }
    if info.hostname == local_hostname {
        return is_process_alive(info.pid);
    }
    true
}

/// Current Unix timestamp in seconds.
fn unix_now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| i64::try_from(duration.as_secs()).unwrap_or(i64::MAX))
}

/// Best-effort local host name without adding extra crate dependencies.
fn current_hostname() -> String {
    env::var("HOSTNAME")
        .or_else(|_| env::var("COMPUTERNAME"))
        .ok()
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| "localhost".to_string())
}

#[cfg(test)]
mod test_active_locks {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn acquires_blocks_second_owner_and_releases_on_drop() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("cli-tools.db");
        let file = dir.path().join("video.mp4");
        std::fs::write(&file, b"video").unwrap();

        let owner_one = ActiveLockStore::open_at(db_path.clone(), "vconvert").unwrap();
        let owner_two = ActiveLockStore::open_at(db_path, "vres").unwrap();

        let guard = match owner_one.try_acquire(std::slice::from_ref(&file), "convert").unwrap() {
            AcquireOutcome::Acquired(guard) => guard,
            AcquireOutcome::AlreadyLocked(info) => panic!("unexpected lock: {info:?}"),
        };

        let blocked = owner_two.is_locked_by_other(&file).unwrap();
        assert!(blocked.is_some());
        assert_eq!(blocked.unwrap().tool, "vconvert");

        let second_attempt = owner_two.try_acquire(std::slice::from_ref(&file), "rename").unwrap();
        assert!(matches!(second_attempt, AcquireOutcome::AlreadyLocked(_)));

        drop(guard);

        assert!(owner_two.is_locked_by_other(&file).unwrap().is_none());
        let acquired_after = owner_two.try_acquire(&[file], "rename").unwrap();
        assert!(matches!(acquired_after, AcquireOutcome::Acquired(_)));
    }

    #[test]
    fn reclaims_stale_expired_and_dead_pid_locks() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("cli-tools.db");
        let file = dir.path().join("clip.mkv");
        std::fs::write(&file, b"clip").unwrap();

        let store = ActiveLockStore::open_at(db_path.clone(), "vconvert").unwrap();
        let key = normalize_lock_path(&file);

        // Insert a stale lock whose PID cannot exist and whose TTL has expired.
        let conn = sqlite::open_at(&db_path).unwrap();
        conn.execute(
            r"
            INSERT INTO active_locks (
                canonical_path, owner_id, tool, operation, pid, hostname, started_at, heartbeat_at, expires_at
            )
            VALUES (?1, 'dead:999999:1', 'vconvert', 'convert', 999999, ?2, 1, 1, 2)
            ",
            params![key, current_hostname()],
        )
        .unwrap();
        drop(conn);

        assert!(store.is_locked_by_other(&file).unwrap().is_none());
        let outcome = store.try_acquire(&[file], "convert").unwrap();
        assert!(matches!(outcome, AcquireOutcome::Acquired(_)));
    }

    #[test]
    fn multi_threaded_workers_acquire_disjoint_locks_safely() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("cli-tools.db");
        let file = dir.path().join("shared.mkv");
        std::fs::write(&file, b"data").unwrap();

        // Initialize schema once before concurrent workers race.
        let _init = ActiveLockStore::open_at(db_path.clone(), "init").unwrap();

        let acquired_count = Arc::new(AtomicU64::new(0));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let db_clone = db_path.clone();
            let file_clone = file.clone();
            let counter = Arc::clone(&acquired_count);
            handles.push(thread::spawn(move || {
                let store = ActiveLockStore::open_at(db_clone, "vconvert").unwrap();
                if let AcquireOutcome::Acquired(_guard) = store.try_acquire(&[file_clone], "convert").unwrap() {
                    counter.fetch_add(1, Ordering::SeqCst);
                    thread::sleep(Duration::from_millis(60));
                }
            }));
        }

        for handle in handles {
            handle.join().unwrap();
        }
        assert_eq!(acquired_count.load(Ordering::SeqCst), 1);
    }
}

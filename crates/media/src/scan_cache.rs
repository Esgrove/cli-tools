//! Shared `SQLite`-backed cache for video analysis results and file content hashes.
//!
//! All media tools (`vconvert`, `dupefind`, `vstats`, `vres`, `thumbs`) share the `scanned_files`
//! and `file_hashes` tables inside the unified `cli-tools.db` database.
//! Files already probed or hashed by one tool are reused immediately by every other tool
//! as long as their file size and modification timestamp remain unchanged.

#![allow(clippy::cast_possible_wrap)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use cli_tools_common::sqlite;
use rusqlite::{Connection, params};

use crate::Resolution;
use crate::file_hash::{FileFingerprint, fingerprint_file};
use crate::video_info::VideoInfo;

/// Schema migration component name for the media scan and hash cache.
const COMPONENT_NAME: &str = "media_cache";
/// Current schema version for `scanned_files` and `file_hashes`.
const SCHEMA_VERSION: i64 = 2;
/// Legacy database filename imported on first migration into `cli-tools.db`.
const LEGACY_VCONVERT_DB: &str = "vconvert.db";

/// Summary counts and disk size of the shared media cache.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CacheStats {
    /// Number of rows currently in `scanned_files`.
    pub scanned_files_count: u64,
    /// Number of rows currently in `file_hashes`.
    pub file_hashes_count: u64,
    /// Database file size in bytes (zero for in-memory databases).
    pub database_size_bytes: u64,
}

/// A cached BLAKE3 hash and the file metadata used to determine whether it is still valid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedFileHash {
    /// File size when the hash was calculated.
    pub size_bytes: u64,
    /// File modification time as nanoseconds since the Unix epoch.
    pub modified_time_ns: i64,
    /// Lowercase hexadecimal BLAKE3 hash.
    pub blake3_hash: String,
}

/// A row from the `scanned_files` cache with non-optional fields matching the database schema.
#[derive(Debug, Clone, Default)]
pub struct CachedVideoInfo {
    /// File size in bytes (used with `modified_time_ns` to detect file changes).
    pub size_bytes: u64,
    /// Video codec name.
    pub codec: String,
    /// Video bitrate in kbps.
    pub bitrate_kbps: u64,
    /// Duration in seconds.
    pub duration: f64,
    /// Displayed video width in pixels (after applying stream rotation).
    pub width: u32,
    /// Displayed video height in pixels (after applying stream rotation).
    pub height: u32,
    /// Raw coded video width before rotation (0 when not recorded).
    pub coded_width: u32,
    /// Raw coded video height before rotation (0 when not recorded).
    pub coded_height: u32,
    /// Stream rotation in degrees (0, 90, 180, or 270).
    pub rotation: i32,
    /// Video frames per second.
    pub frames_per_second: f64,
    /// Per-channel video bit depth, for example 8 or 10, or 0 when unknown.
    pub bit_depth: u8,
    /// File modification time as nanoseconds since the Unix epoch, zero when not recorded.
    pub modified_time_ns: i64,
}

/// Read/write handle to the shared `scanned_files` and `file_hashes` tables in `cli-tools.db`.
pub struct ScanCache {
    connection: Connection,
    db_path: Option<PathBuf>,
}

impl ScanCache {
    /// Open (or create) the shared cache inside the default `cli-tools.db`.
    ///
    /// On first initialization, imports any existing `scanned_files`
    /// and `file_hashes` rows from a sibling legacy `vconvert.db` without modifying the legacy file.
    ///
    /// # Errors
    /// Returns an error if the database cannot be opened or initialized.
    pub fn open() -> Result<Self> {
        let path = sqlite::database_path()?;
        Self::open_at(&path)
    }

    /// Open (or create) the cache database at a specific path.
    ///
    /// # Errors
    /// Returns an error if the database cannot be opened or initialized.
    pub fn open_at(path: &Path) -> Result<Self> {
        let mut connection = sqlite::open_at(path)?;
        let legacy = if path.file_name().and_then(|name| name.to_str()) == Some(sqlite::DATABASE_FILENAME) {
            sqlite::legacy_database_path(path, LEGACY_VCONVERT_DB)
        } else {
            None
        };
        ensure_schema(&mut connection, legacy.as_deref())?;
        Ok(Self {
            connection,
            db_path: Some(path.to_path_buf()),
        })
    }

    /// Open the default `cli-tools.db` in read-only mode, returning `Ok(None)` if it does not exist yet.
    ///
    /// # Errors
    /// Returns an error if an existing database cannot be opened.
    pub fn open_read_only() -> Result<Option<Self>> {
        let path = sqlite::database_path()?;
        Self::open_read_only_at(&path)
    }

    /// Open a cache database at `path` in read-only mode, returning `Ok(None)` if `path` does not exist.
    ///
    /// # Errors
    /// Returns an error if an existing database cannot be opened.
    pub fn open_read_only_at(path: &Path) -> Result<Option<Self>> {
        let Some(connection) = sqlite::open_read_only_at(path)? else {
            return Ok(None);
        };
        Ok(Some(Self {
            connection,
            db_path: Some(path.to_path_buf()),
        }))
    }

    /// Open an in-memory cache database for testing.
    ///
    /// # Errors
    /// Returns an error if the in-memory database cannot be created.
    pub fn open_in_memory() -> Result<Self> {
        let mut connection = sqlite::open_in_memory()?;
        ensure_schema(&mut connection, None)?;
        Ok(Self {
            connection,
            db_path: None,
        })
    }

    /// Load every cached video entry into a `HashMap` keyed by full path string.
    ///
    /// # Errors
    /// Returns an error if the database query fails.
    pub fn get_all(&self) -> Result<HashMap<String, CachedVideoInfo>> {
        if !sqlite::table_exists(&self.connection, "main", "scanned_files")? {
            return Ok(HashMap::new());
        }
        let has_coded_columns = sqlite::column_exists(&self.connection, "main", "scanned_files", "coded_width")?;
        let query = if has_coded_columns {
            r"
            SELECT full_path, size_bytes, codec, bitrate_kbps, duration, width, height,
                   coded_width, coded_height, rotation, frames_per_second, bit_depth, modified_time_ns
            FROM scanned_files
            "
        } else {
            r"
            SELECT full_path, size_bytes, codec, bitrate_kbps, duration, width, height,
                   0, 0, 0, frames_per_second, bit_depth, modified_time_ns
            FROM scanned_files
            "
        };

        let mut statement = self.connection.prepare(query)?;
        let mut rows = statement.query([])?;
        let mut entries = HashMap::new();
        while let Some(row) = rows.next()? {
            let path: String = row.get(0)?;
            let width = row.get::<_, i64>(5)? as u32;
            let height = row.get::<_, i64>(6)? as u32;
            let coded_width = row.get::<_, i64>(7)? as u32;
            let coded_height = row.get::<_, i64>(8)? as u32;
            let entry = CachedVideoInfo {
                size_bytes: row.get::<_, i64>(1)? as u64,
                codec: row.get(2)?,
                bitrate_kbps: row.get::<_, i64>(3)? as u64,
                duration: row.get(4)?,
                width,
                height,
                coded_width: if coded_width > 0 { coded_width } else { width },
                coded_height: if coded_height > 0 { coded_height } else { height },
                rotation: row.get::<_, i64>(9)? as i32,
                frames_per_second: row.get(10)?,
                bit_depth: row.get::<_, i64>(11)? as u8,
                modified_time_ns: row.get(12)?,
            };
            entries.insert(path, entry);
        }

        Ok(entries)
    }

    /// Load every cached BLAKE3 hash into a `HashMap` keyed by full path string.
    ///
    /// # Errors
    /// Returns an error if the database query fails.
    pub fn get_all_hashes(&self) -> Result<HashMap<String, CachedFileHash>> {
        if !sqlite::table_exists(&self.connection, "main", "file_hashes")? {
            return Ok(HashMap::new());
        }
        let mut statement = self.connection.prepare(
            r"
            SELECT full_path, size_bytes, modified_time_ns, blake3_hash
            FROM file_hashes
            ",
        )?;

        let mut rows = statement.query([])?;
        let mut entries = HashMap::new();
        while let Some(row) = rows.next()? {
            let path: String = row.get(0)?;
            let entry = CachedFileHash {
                size_bytes: row.get::<_, i64>(1)? as u64,
                modified_time_ns: row.get(2)?,
                blake3_hash: row.get(3)?,
            };
            entries.insert(path, entry);
        }

        Ok(entries)
    }

    /// Insert or update multiple cached BLAKE3 hashes in a single `IMMEDIATE` transaction.
    ///
    /// Returns the number of entries written.
    ///
    /// # Errors
    /// Returns an error if the transaction cannot be started or committed.
    pub fn batch_upsert_hashes(&mut self, entries: &[(&Path, &CachedFileHash)]) -> Result<usize> {
        if entries.is_empty() {
            return Ok(0);
        }

        let now = unix_now_secs();
        let transaction = sqlite::immediate_transaction(&mut self.connection)?;
        let mut count = 0;
        for (path, hash) in entries {
            let Some(path_str) = path.to_str() else {
                continue;
            };
            transaction
                .execute(
                    r"
                    INSERT INTO file_hashes (full_path, size_bytes, modified_time_ns, blake3_hash, hashed_time)
                    VALUES (?1, ?2, ?3, ?4, ?5)
                    ON CONFLICT(full_path) DO UPDATE SET
                        size_bytes = excluded.size_bytes,
                        modified_time_ns = excluded.modified_time_ns,
                        blake3_hash = excluded.blake3_hash,
                        hashed_time = excluded.hashed_time
                    ",
                    params![
                        path_str,
                        hash.size_bytes as i64,
                        hash.modified_time_ns,
                        hash.blake3_hash,
                        now,
                    ],
                )
                .context("Failed to upsert file hash")?;
            count += 1;
        }

        transaction.commit()?;
        Ok(count)
    }

    /// Insert or update multiple scanned file entries in a single `IMMEDIATE` transaction.
    ///
    /// All filesystem `fingerprint_file` calls happen before opening the write transaction
    /// so slow or network drives never hold the `SQLite` write lock.
    ///
    /// # Errors
    /// Returns an error if the transaction cannot be started or committed.
    pub fn batch_upsert(&mut self, entries: &[(&Path, &VideoInfo)]) -> Result<usize> {
        if entries.is_empty() {
            return Ok(0);
        }
        // Perform all filesystem metadata reads before acquiring the SQLite write lock.
        let prepared: Vec<(&Path, &VideoInfo, i64)> = entries
            .iter()
            .filter_map(|(path, info)| {
                let _ = path.to_str()?;
                let _ = info.size_bytes?;
                let modified_time_ns = fingerprint_file(path).map_or(0, |fp| fp.modified_time_ns);
                Some((*path, *info, modified_time_ns))
            })
            .collect();
        self.write_scanned_entries(&prepared)
    }

    /// Insert or update multiple scanned entries using pre-probe fingerprints captured before ffprobe ran.
    ///
    /// This avoids both filesystem I/O inside the transaction and post-probe mtime races.
    ///
    /// # Errors
    /// Returns an error if the transaction cannot be started or committed.
    pub fn batch_upsert_with_fingerprints(
        &mut self,
        entries: &[(&Path, &VideoInfo, FileFingerprint)],
    ) -> Result<usize> {
        if entries.is_empty() {
            return Ok(0);
        }
        let prepared: Vec<(&Path, &VideoInfo, i64)> = entries
            .iter()
            .filter_map(|(path, info, fingerprint)| {
                let _ = path.to_str()?;
                let _ = info.size_bytes?;
                Some((*path, *info, fingerprint.modified_time_ns))
            })
            .collect();
        self.write_scanned_entries(&prepared)
    }

    /// Update cached path keys after a file is renamed or moved on disk.
    ///
    /// Keeps `scanned_files`, `file_hashes`, and `pending_files` (when present) synchronized.
    ///
    /// # Errors
    /// Returns an error if the update transaction fails.
    pub fn move_cached_path(&mut self, old_path: &Path, new_path: &Path) -> Result<()> {
        let (Some(old_str), Some(new_str)) = (old_path.to_str(), new_path.to_str()) else {
            return Ok(());
        };
        if old_str == new_str {
            return Ok(());
        }
        let has_pending = sqlite::table_exists(&self.connection, "main", "pending_files")?;
        let tx = sqlite::immediate_transaction(&mut self.connection)?;
        tx.execute("DELETE FROM scanned_files WHERE full_path = ?1", params![new_str])?;
        tx.execute(
            "UPDATE scanned_files SET full_path = ?1 WHERE full_path = ?2",
            params![new_str, old_str],
        )?;
        tx.execute("DELETE FROM file_hashes WHERE full_path = ?1", params![new_str])?;
        tx.execute(
            "UPDATE file_hashes SET full_path = ?1 WHERE full_path = ?2",
            params![new_str, old_str],
        )?;
        if has_pending {
            tx.execute("DELETE FROM pending_files WHERE full_path = ?1", params![new_str])?;
            tx.execute(
                "UPDATE pending_files SET full_path = ?1 WHERE full_path = ?2",
                params![new_str, old_str],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Remove cached scan and hash rows for a deleted file path.
    ///
    /// # Errors
    /// Returns an error if the delete transaction fails.
    pub fn remove_cached_path(&mut self, path: &Path) -> Result<()> {
        let Some(path_str) = path.to_str() else {
            return Ok(());
        };
        let tx = sqlite::immediate_transaction(&mut self.connection)?;
        tx.execute("DELETE FROM scanned_files WHERE full_path = ?1", params![path_str])?;
        tx.execute("DELETE FROM file_hashes WHERE full_path = ?1", params![path_str])?;
        tx.commit()?;
        Ok(())
    }

    /// Remove entries from `scanned_files` whose files no longer exist on mounted volumes.
    ///
    /// Existence checks happen outside any write transaction, and paths on unmounted drives
    /// or paths that fail with permission/IO errors are preserved.
    ///
    /// # Errors
    /// Returns an error if querying or deleting fails.
    pub fn remove_missing_scanned_files(&mut self) -> Result<usize> {
        self.remove_missing_from_table("scanned_files")
    }

    /// Remove entries from `file_hashes` whose files no longer exist on mounted volumes.
    ///
    /// # Errors
    /// Returns an error if querying or deleting fails.
    pub fn remove_missing_file_hashes(&mut self) -> Result<usize> {
        self.remove_missing_from_table("file_hashes")
    }

    /// Return the total number of rows in `scanned_files`.
    ///
    /// # Errors
    /// Returns an error if the count query fails.
    pub fn scanned_file_count(&self) -> Result<u64> {
        if !sqlite::table_exists(&self.connection, "main", "scanned_files")? {
            return Ok(0);
        }
        let count: i64 = self
            .connection
            .query_row("SELECT COUNT(*) FROM scanned_files", [], |row| row.get(0))?;
        Ok(u64::try_from(count).unwrap_or(0))
    }

    /// Return the total number of rows in `file_hashes`.
    ///
    /// # Errors
    /// Returns an error if the count query fails.
    pub fn file_hash_count(&self) -> Result<u64> {
        if !sqlite::table_exists(&self.connection, "main", "file_hashes")? {
            return Ok(0);
        }
        let count: i64 = self
            .connection
            .query_row("SELECT COUNT(*) FROM file_hashes", [], |row| row.get(0))?;
        Ok(u64::try_from(count).unwrap_or(0))
    }

    /// Return summary statistics for the shared cache tables and database file size.
    ///
    /// # Errors
    /// Returns an error if counting rows fails.
    pub fn cache_stats(&self) -> Result<CacheStats> {
        let database_size_bytes = self
            .db_path
            .as_deref()
            .and_then(|path| std::fs::metadata(path).ok())
            .map_or(0, |meta| meta.len());
        Ok(CacheStats {
            scanned_files_count: self.scanned_file_count()?,
            file_hashes_count: self.file_hash_count()?,
            database_size_bytes,
        })
    }

    /// Write pre-fingerprinted video rows inside one `IMMEDIATE` transaction.
    fn write_scanned_entries(&mut self, prepared: &[(&Path, &VideoInfo, i64)]) -> Result<usize> {
        if prepared.is_empty() {
            return Ok(0);
        }
        let now = unix_now_secs();
        let transaction = sqlite::immediate_transaction(&mut self.connection)?;
        let mut count = 0;

        for &(path, info, modified_time_ns) in prepared {
            let Some(path_str) = path.to_str() else {
                continue;
            };
            let Some(size_bytes) = info.size_bytes else {
                continue;
            };

            let codec = info.codec.as_deref().unwrap_or("");
            let bitrate_kbps = info.bitrate_kbps.unwrap_or(0);
            let duration = info.duration.unwrap_or(0.0);
            let (width, height) = info.resolution.map_or((0u32, 0u32), |res| (res.width, res.height));
            let (coded_width, coded_height) = info
                .coded_resolution
                .map_or((width, height), |res| (res.width, res.height));
            let rotation = info.rotation.unwrap_or(0);
            let frames_per_second = info.frames_per_second.unwrap_or(0.0);
            let bit_depth = i64::from(info.bit_depth.unwrap_or(0));

            transaction
                .execute(
                    r"
                    INSERT INTO scanned_files (
                        full_path, size_bytes, codec, bitrate_kbps, duration, width, height,
                        coded_width, coded_height, rotation, frames_per_second, bit_depth,
                        scanned_time, modified_time_ns
                    )
                    VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
                    ON CONFLICT(full_path) DO UPDATE SET
                        frames_per_second = CASE
                            WHEN excluded.frames_per_second > 0 THEN excluded.frames_per_second
                            WHEN scanned_files.size_bytes = excluded.size_bytes
                                AND scanned_files.modified_time_ns != 0
                                AND scanned_files.modified_time_ns = excluded.modified_time_ns
                            THEN scanned_files.frames_per_second
                            ELSE 0
                        END,
                        bit_depth = CASE
                            WHEN excluded.bit_depth > 0 THEN excluded.bit_depth
                            WHEN scanned_files.size_bytes = excluded.size_bytes
                                AND scanned_files.modified_time_ns != 0
                                AND scanned_files.modified_time_ns = excluded.modified_time_ns
                            THEN scanned_files.bit_depth
                            ELSE 0
                        END,
                        size_bytes = excluded.size_bytes,
                        codec = excluded.codec,
                        bitrate_kbps = excluded.bitrate_kbps,
                        duration = excluded.duration,
                        width = excluded.width,
                        height = excluded.height,
                        coded_width = excluded.coded_width,
                        coded_height = excluded.coded_height,
                        rotation = excluded.rotation,
                        scanned_time = excluded.scanned_time,
                        modified_time_ns = excluded.modified_time_ns
                    ",
                    params![
                        path_str,
                        size_bytes as i64,
                        codec,
                        bitrate_kbps as i64,
                        duration,
                        width,
                        height,
                        coded_width,
                        coded_height,
                        rotation,
                        frames_per_second,
                        bit_depth,
                        now,
                        modified_time_ns,
                    ],
                )
                .context("Failed to upsert scanned file")?;
            count += 1;
        }

        transaction.commit()?;
        Ok(count)
    }

    /// Remove rows with missing files from `table_name` without holding a write lock during filesystem checks.
    fn remove_missing_from_table(&mut self, table_name: &str) -> Result<usize> {
        if !sqlite::table_exists(&self.connection, "main", table_name)? {
            return Ok(0);
        }

        let paths: Vec<String> = {
            let mut statement = self
                .connection
                .prepare(&format!("SELECT full_path FROM {table_name}"))?;
            let mut rows = statement.query([])?;
            let mut collected = Vec::new();
            while let Some(row) = rows.next()? {
                collected.push(row.get(0)?);
            }
            collected
        };

        let missing: Vec<String> = paths
            .into_iter()
            .filter(|path_str| is_definitively_missing(Path::new(path_str)))
            .collect();

        if missing.is_empty() {
            return Ok(0);
        }

        let tx = sqlite::immediate_transaction(&mut self.connection)?;
        let mut removed = 0usize;
        let delete_sql = format!("DELETE FROM {table_name} WHERE full_path = ?1");
        for path_str in &missing {
            removed += tx.execute(&delete_sql, params![path_str])?;
        }
        tx.commit()?;
        Ok(removed)
    }
}

impl CachedFileHash {
    /// Return whether this hash still describes a file with the given metadata.
    #[must_use]
    pub const fn is_current(&self, size_bytes: u64, modified_time_ns: i64) -> bool {
        self.size_bytes == size_bytes && self.modified_time_ns == modified_time_ns
    }
}

impl CachedVideoInfo {
    /// Return whether this entry still describes the file with the given fingerprint.
    ///
    /// A file replaced by another of the same size has a new modification time,
    /// and an entry stored without a modification time is never current.
    #[must_use]
    pub const fn is_current(&self, fingerprint: &FileFingerprint) -> bool {
        self.modified_time_ns != 0
            && self.size_bytes == fingerprint.size_bytes
            && self.modified_time_ns == fingerprint.modified_time_ns
    }

    /// Return whether this cached entry carries complete video metadata usable by `vconvert`.
    #[must_use]
    pub fn has_complete_video_metadata(&self) -> bool {
        !self.codec.is_empty()
            && self.bitrate_kbps > 0
            && self.duration > 0.0
            && self.width > 0
            && self.height > 0
            && self.bit_depth >= 8
            && (1.0..=240.0).contains(&self.frames_per_second)
    }

    /// Convert to the shared [`VideoInfo`] type used by the rest of the library.
    #[must_use]
    pub fn to_video_info(&self) -> VideoInfo {
        let display_resolution = if self.width > 0 && self.height > 0 {
            Resolution::from_options(Some(self.width), Some(self.height))
        } else {
            None
        };
        let coded_resolution = if self.coded_width > 0 && self.coded_height > 0 {
            Resolution::from_options(Some(self.coded_width), Some(self.coded_height))
        } else {
            display_resolution
        };
        VideoInfo {
            size_bytes: Some(self.size_bytes),
            resolution: display_resolution,
            coded_resolution,
            rotation: (self.rotation != 0).then_some(self.rotation),
            duration: (self.duration > 0.0).then_some(self.duration),
            codec: (!self.codec.is_empty()).then(|| self.codec.clone()),
            bitrate_kbps: (self.bitrate_kbps > 0).then_some(self.bitrate_kbps),
            frames_per_second: (self.frames_per_second > 0.0).then_some(self.frames_per_second),
            bit_depth: (self.bit_depth > 0).then_some(self.bit_depth),
        }
    }
}

/// Ensure the `scanned_files` and `file_hashes` tables exist and migrate from legacy `vconvert.db` once.
///
/// # Errors
/// Returns an error if schema migration or legacy data import fails.
pub fn ensure_schema(connection: &mut Connection, legacy_vconvert_db: Option<&Path>) -> Result<()> {
    let was_uninitialized = sqlite::current_component_version(connection, COMPONENT_NAME)? == 0;

    sqlite::migrate_component(connection, COMPONENT_NAME, SCHEMA_VERSION, |tx, current_version| {
        if current_version < 1 {
            tx.execute_batch(
                r"
                CREATE TABLE IF NOT EXISTS scanned_files (
                    id INTEGER PRIMARY KEY,
                    full_path TEXT NOT NULL UNIQUE,
                    size_bytes INTEGER NOT NULL,
                    codec TEXT NOT NULL,
                    bitrate_kbps INTEGER NOT NULL,
                    duration REAL NOT NULL,
                    width INTEGER NOT NULL,
                    height INTEGER NOT NULL,
                    coded_width INTEGER NOT NULL DEFAULT 0,
                    coded_height INTEGER NOT NULL DEFAULT 0,
                    rotation INTEGER NOT NULL DEFAULT 0,
                    frames_per_second REAL NOT NULL,
                    bit_depth INTEGER NOT NULL DEFAULT 0,
                    scanned_time INTEGER NOT NULL,
                    modified_time_ns INTEGER NOT NULL DEFAULT 0
                );

                CREATE TABLE IF NOT EXISTS file_hashes (
                    id INTEGER PRIMARY KEY,
                    full_path TEXT NOT NULL UNIQUE,
                    size_bytes INTEGER NOT NULL,
                    modified_time_ns INTEGER NOT NULL,
                    blake3_hash TEXT NOT NULL,
                    hashed_time INTEGER NOT NULL
                );

                CREATE INDEX IF NOT EXISTS idx_file_hashes_hash ON file_hashes(blake3_hash);
                ",
            )
            .context("Failed to create scanned_files and file_hashes tables")?;
        }

        if current_version < 2 {
            sqlite::add_column_if_missing(tx, "scanned_files", "bit_depth", "INTEGER NOT NULL DEFAULT 0")?;
            sqlite::add_column_if_missing(tx, "scanned_files", "modified_time_ns", "INTEGER NOT NULL DEFAULT 0")?;
            sqlite::add_column_if_missing(tx, "scanned_files", "coded_width", "INTEGER NOT NULL DEFAULT 0")?;
            sqlite::add_column_if_missing(tx, "scanned_files", "coded_height", "INTEGER NOT NULL DEFAULT 0")?;
            sqlite::add_column_if_missing(tx, "scanned_files", "rotation", "INTEGER NOT NULL DEFAULT 0")?;
            tx.execute_batch(
                r"
                DROP INDEX IF EXISTS idx_scanned_path;
                DROP INDEX IF EXISTS idx_file_hashes_path;
                DELETE FROM scanned_files WHERE (frames_per_second > 0.0 AND frames_per_second < 1.0) OR frames_per_second > 240.0;
                ",
            )
            .context("Failed to finalize media_cache v2 migration")?;
        }
        Ok(())
    })?;

    if was_uninitialized && let Some(legacy_path) = legacy_vconvert_db {
        import_legacy_media_tables(connection, legacy_path)?;
    }

    Ok(())
}

/// Return whether `path` is confirmed missing on a currently mounted volume.
///
/// Paths on unmounted external/network volumes
/// or paths whose existence check fails with a transient permission/IO error return `false`
/// so valid cache entries are not wiped.
#[must_use]
pub fn is_definitively_missing(path: &Path) -> bool {
    if !mount_root_is_available(path) {
        return false;
    }
    matches!(path.try_exists(), Ok(false))
}

/// Return whether the drive or mount root containing `path` appears to be online/mounted.
#[must_use]
pub fn mount_root_is_available(path: &Path) -> bool {
    let Some(mount_root) = volume_mount_root(path) else {
        return true;
    };
    matches!(mount_root.try_exists(), Ok(true))
}

/// Extract the mount root for removable/network volumes so offline mounts can be detected.
fn volume_mount_root(path: &Path) -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let mut components = path.components();
        if matches!(components.next(), Some(std::path::Component::RootDir))
            && matches!(components.next(), Some(std::path::Component::Normal(name)) if name == "Volumes")
            && let Some(std::path::Component::Normal(volume)) = components.next()
        {
            return Some(Path::new("/Volumes").join(volume));
        }
        None
    }
    #[cfg(target_os = "linux")]
    {
        let mut components = path.components();
        if matches!(components.next(), Some(std::path::Component::RootDir))
            && let Some(std::path::Component::Normal(first)) = components.next()
        {
            if first == "mnt"
                && let Some(std::path::Component::Normal(mount)) = components.next()
            {
                return Some(Path::new("/mnt").join(mount));
            }
            if first == "media"
                && let Some(std::path::Component::Normal(user)) = components.next()
                && let Some(std::path::Component::Normal(mount)) = components.next()
            {
                return Some(Path::new("/media").join(user).join(mount));
            }
        }
        None
    }
    #[cfg(windows)]
    {
        let mut components = path.components();
        if let Some(std::path::Component::Prefix(prefix)) = components.next() {
            let mut root = PathBuf::from(prefix.as_os_str());
            root.push(std::path::MAIN_SEPARATOR_STR);
            return Some(root);
        }
        None
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    {
        let _ = path;
        None
    }
}

/// Copy `scanned_files` and `file_hashes` rows from a legacy `vconvert.db` into `cli-tools.db`.
fn import_legacy_media_tables(connection: &mut Connection, legacy_path: &Path) -> Result<()> {
    sqlite::import_from_legacy_database(connection, legacy_path, |tx| {
        if sqlite::table_exists(tx, "legacy", "scanned_files")? {
            let has_bit_depth = sqlite::column_exists(tx, "legacy", "scanned_files", "bit_depth")?;
            let has_mtime_ns = sqlite::column_exists(tx, "legacy", "scanned_files", "modified_time_ns")?;
            let bit_depth_expr = if has_bit_depth { "bit_depth" } else { "0" };
            let mtime_expr = if has_mtime_ns { "modified_time_ns" } else { "0" };
            tx.execute(
                &format!(
                    r"
                    INSERT OR IGNORE INTO main.scanned_files (
                        full_path, size_bytes, codec, bitrate_kbps, duration, width, height,
                        coded_width, coded_height, rotation, frames_per_second, bit_depth,
                        scanned_time, modified_time_ns
                    )
                    SELECT full_path, size_bytes, codec, bitrate_kbps, duration, width, height,
                           width, height, 0, frames_per_second, {bit_depth_expr},
                           scanned_time, {mtime_expr}
                    FROM legacy.scanned_files
                    "
                ),
                [],
            )
            .context("Failed to import legacy scanned_files rows")?;
        }
        if sqlite::table_exists(tx, "legacy", "file_hashes")? {
            tx.execute(
                r"
                INSERT OR IGNORE INTO main.file_hashes (
                    full_path, size_bytes, modified_time_ns, blake3_hash, hashed_time
                )
                SELECT full_path, size_bytes, modified_time_ns, blake3_hash, hashed_time
                FROM legacy.file_hashes
                ",
                [],
            )
            .context("Failed to import legacy file_hashes rows")?;
        }
        Ok(())
    })
}

/// Current Unix timestamp in seconds.
fn unix_now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs() as i64)
}

#[cfg(test)]
mod test_scan_cache_open {
    use super::*;

    #[test]
    fn opens_in_memory_database() {
        assert!(ScanCache::open_in_memory().is_ok());
    }

    #[test]
    fn open_read_only_returns_none_when_database_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.db");
        assert!(ScanCache::open_read_only_at(&path).unwrap().is_none());
        assert!(!path.exists());
    }

    #[test]
    fn empty_cache_returns_empty_maps() {
        let cache = ScanCache::open_in_memory().unwrap();
        assert!(cache.get_all().unwrap().is_empty());
        assert!(cache.get_all_hashes().unwrap().is_empty());
        assert_eq!(cache.scanned_file_count().unwrap(), 0);
        assert_eq!(cache.file_hash_count().unwrap(), 0);
    }
}

#[cfg(test)]
mod test_scan_cache_upsert {
    use super::*;

    fn full_video_info() -> VideoInfo {
        VideoInfo {
            size_bytes: Some(1_000_000),
            resolution: Resolution::from_options(Some(1920), Some(1080)),
            coded_resolution: Resolution::from_options(Some(1920), Some(1080)),
            rotation: None,
            duration: Some(120.5),
            codec: Some("h264".to_string()),
            bitrate_kbps: Some(5000),
            frames_per_second: Some(29.97),
            bit_depth: Some(10),
        }
    }

    #[test]
    fn bidirectional_cache_preserves_fps_bit_depth_and_rotation() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("portrait.mp4");
        std::fs::write(&file, b"video").unwrap();
        let fingerprint = fingerprint_file(&file).unwrap();

        let info = VideoInfo {
            size_bytes: Some(fingerprint.size_bytes),
            resolution: Some(Resolution::new(1080, 1920)),
            coded_resolution: Some(Resolution::new(1920, 1080)),
            rotation: Some(270),
            duration: Some(45.0),
            codec: Some("hevc".to_string()),
            bitrate_kbps: Some(8_000),
            frames_per_second: Some(60.0),
            bit_depth: Some(10),
        };

        let mut cache = ScanCache::open_in_memory().unwrap();
        assert_eq!(
            cache
                .batch_upsert_with_fingerprints(&[(file.as_path(), &info, fingerprint)])
                .unwrap(),
            1
        );

        let all = cache.get_all().unwrap();
        let cached = all.get(file.to_str().unwrap()).expect("cached row");
        assert!(cached.is_current(&fingerprint));
        assert!(cached.has_complete_video_metadata());
        assert_eq!(cached.bit_depth, 10);
        assert!((cached.frames_per_second - 60.0).abs() < f64::EPSILON);
        assert_eq!((cached.width, cached.height), (1080, 1920));
        assert_eq!((cached.coded_width, cached.coded_height), (1920, 1080));
        assert_eq!(cached.rotation, 270);

        let roundtripped = cached.to_video_info();
        assert_eq!(roundtripped.resolution, Some(Resolution::new(1080, 1920)));
        assert_eq!(roundtripped.coded_resolution, Some(Resolution::new(1920, 1080)));
        assert_eq!(roundtripped.rotation, Some(270));
        assert_eq!(roundtripped.bit_depth, Some(10));
        assert_eq!(roundtripped.frames_per_second, Some(60.0));
    }

    #[test]
    fn move_and_remove_cached_paths_keep_tables_synchronized() {
        let dir = tempfile::tempdir().unwrap();
        let old_path = dir.path().join("old.mkv");
        let new_path = dir.path().join("new.mkv");
        let info = full_video_info();
        let hash = CachedFileHash {
            size_bytes: 1_000_000,
            modified_time_ns: 42,
            blake3_hash: "abc123".to_string(),
        };

        let mut cache = ScanCache::open_in_memory().unwrap();
        cache.batch_upsert(&[(old_path.as_path(), &info)]).unwrap();
        cache.batch_upsert_hashes(&[(old_path.as_path(), &hash)]).unwrap();

        cache.move_cached_path(&old_path, &new_path).unwrap();
        let videos = cache.get_all().unwrap();
        assert!(!videos.contains_key(old_path.to_str().unwrap()));
        assert!(videos.contains_key(new_path.to_str().unwrap()));
        let hashes = cache.get_all_hashes().unwrap();
        assert!(hashes.contains_key(new_path.to_str().unwrap()));

        cache.remove_cached_path(&new_path).unwrap();
        assert!(cache.get_all().unwrap().is_empty());
        assert!(cache.get_all_hashes().unwrap().is_empty());
    }

    #[test]
    fn migrates_legacy_vconvert_db_into_cli_tools_db() {
        let dir = tempfile::tempdir().unwrap();
        let legacy_path = dir.path().join("vconvert.db");
        let unified_path = dir.path().join("cli-tools.db");

        let legacy_conn = sqlite::open_at(&legacy_path).unwrap();
        legacy_conn
            .execute_batch(
                r"
                CREATE TABLE scanned_files (
                    id INTEGER PRIMARY KEY,
                    full_path TEXT NOT NULL UNIQUE,
                    size_bytes INTEGER NOT NULL,
                    codec TEXT NOT NULL,
                    bitrate_kbps INTEGER NOT NULL,
                    duration REAL NOT NULL,
                    width INTEGER NOT NULL,
                    height INTEGER NOT NULL,
                    frames_per_second REAL NOT NULL,
                    bit_depth INTEGER NOT NULL DEFAULT 8,
                    scanned_time INTEGER NOT NULL,
                    modified_time_ns INTEGER NOT NULL DEFAULT 555
                );
                INSERT INTO scanned_files (
                    full_path, size_bytes, codec, bitrate_kbps, duration, width, height,
                    frames_per_second, bit_depth, scanned_time, modified_time_ns
                ) VALUES ('/videos/legacy.mkv', 5000, 'hevc', 4000, 60.0, 1920, 1080, 24.0, 10, 100, 555);
                CREATE TABLE file_hashes (
                    id INTEGER PRIMARY KEY,
                    full_path TEXT NOT NULL UNIQUE,
                    size_bytes INTEGER NOT NULL,
                    modified_time_ns INTEGER NOT NULL,
                    blake3_hash TEXT NOT NULL,
                    hashed_time INTEGER NOT NULL
                );
                INSERT INTO file_hashes (full_path, size_bytes, modified_time_ns, blake3_hash, hashed_time)
                VALUES ('/videos/legacy.mkv', 5000, 555, 'deadbeef', 100);
                ",
            )
            .unwrap();
        drop(legacy_conn);

        let cache = ScanCache::open_at(&unified_path).unwrap();
        let videos = cache.get_all().unwrap();
        let entry = videos.get("/videos/legacy.mkv").expect("legacy scanned row imported");
        assert_eq!(entry.codec, "hevc");
        assert_eq!(entry.bit_depth, 10);
        assert_eq!(entry.modified_time_ns, 555);

        let hashes = cache.get_all_hashes().unwrap();
        assert_eq!(hashes.get("/videos/legacy.mkv").unwrap().blake3_hash, "deadbeef");
        assert!(legacy_path.is_file());
    }

    #[test]
    fn remove_missing_prunes_deleted_local_files_and_keeps_existing() {
        let dir = tempfile::tempdir().unwrap();
        let existing = dir.path().join("present.mp4");
        let missing = dir.path().join("gone.mp4");
        std::fs::write(&existing, b"keep").unwrap();

        let info = full_video_info();
        let hash = CachedFileHash {
            size_bytes: 4,
            modified_time_ns: 1,
            blake3_hash: "aa".to_string(),
        };

        let mut cache = ScanCache::open_in_memory().unwrap();
        cache
            .batch_upsert(&[(existing.as_path(), &info), (missing.as_path(), &info)])
            .unwrap();
        cache
            .batch_upsert_hashes(&[(existing.as_path(), &hash), (missing.as_path(), &hash)])
            .unwrap();

        assert_eq!(cache.remove_missing_scanned_files().unwrap(), 1);
        assert_eq!(cache.remove_missing_file_hashes().unwrap(), 1);
        assert_eq!(cache.scanned_file_count().unwrap(), 1);
        assert_eq!(cache.file_hash_count().unwrap(), 1);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn unmounted_macos_volume_is_not_treated_as_missing() {
        let offline = Path::new("/Volumes/DefinitelyUnmountedTestDrive99999/movie.mkv");
        assert!(!mount_root_is_available(offline));
        assert!(!is_definitively_missing(offline));
    }

    #[cfg(unix)]
    #[test]
    fn skips_non_utf8_paths_to_avoid_cache_key_collisions() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let mut cache = ScanCache::open_in_memory().unwrap();
        let first = Path::new(OsStr::from_bytes(b"/videos/clip_\xff.mp4"));
        let second = Path::new(OsStr::from_bytes(b"/videos/clip_\xfe.mp4"));
        let info = full_video_info();

        assert_eq!(cache.batch_upsert(&[(first, &info), (second, &info)]).unwrap(), 0);
        assert!(cache.get_all().unwrap().is_empty());
    }
}

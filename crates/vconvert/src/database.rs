//! `SQLite` database operations for video convert.
//!
//! Stores pending files that need conversion, remuxing, or subtitle processing.
//! The scanned file cache lives in the `scanned_files` submodule.

#![allow(clippy::cast_possible_wrap)]

mod scanned_files;

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use cli_tools_common::sqlite;
use cli_tools_media::scan_cache::{ensure_schema as ensure_media_cache_schema, is_definitively_missing};
use rusqlite::{Connection, OptionalExtension, params};

use crate::bitrate_limit::MinimumBitrate;
use crate::options::SortOrder;
use crate::types::VideoInfo;

/// Schema migration component name for `pending_files`.
const PENDING_COMPONENT: &str = "vconvert_pending";
/// Current schema version for `pending_files`.
const PENDING_SCHEMA_VERSION: i64 = 2;
/// Legacy database filename imported on first migration into `cli-tools.db`.
const LEGACY_VCONVERT_DB: &str = "vconvert.db";

/// Action type for a pending file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingAction {
    /// File needs full conversion to HEVC.
    Convert,
    /// File is already HEVC but needs remuxing to MP4 container.
    Remux,
    /// File needs external subtitle sidecars muxed into the movie container.
    SubtitleMux,
}

/// Database wrapper for pending file and shared scan cache operations on `cli-tools.db`.
pub struct Database {
    connection: Connection,
    db_path: Option<PathBuf>,
}

/// A pending file entry from the database.
#[derive(Debug, Clone)]
pub struct PendingFile {
    /// Database row ID.
    pub id: i64,
    /// Full path to the video file.
    pub full_path: PathBuf,
    /// File extension (for example "mp4", "mkv").
    pub extension: String,
    /// Video codec name (for example "hevc", "h264").
    pub codec: String,
    /// Video bitrate in kbps.
    pub bitrate_kbps: u64,
    /// File size in bytes.
    pub size_bytes: u64,
    /// Duration in seconds.
    pub duration: f64,
    /// Video width in pixels.
    pub width: u32,
    /// Video height in pixels.
    pub height: u32,
    /// Framerate in frames per second.
    pub frames_per_second: f64,
    /// Video bit depth per color channel.
    pub bit_depth: u8,
    /// Action to perform: `"convert"`, `"remux"`, or `"subtitle_mux"`.
    pub action: PendingAction,
    /// Whether this pending item was queued in movie mode.
    pub movie_mode: bool,
    /// Worker owner identifier currently leasing this row, if any.
    pub claimed_by: Option<String>,
    /// Unix timestamp (seconds) when the current lease was taken or refreshed.
    pub claimed_at: Option<i64>,
    /// When the entry was added to the database.
    #[allow(dead_code)]
    pub created_time: i64,
    /// File modification time in whole seconds since the Unix epoch, if recorded.
    pub modified_time: Option<i64>,
    /// File modification time in nanoseconds since the Unix epoch (0 if unrecorded).
    pub modified_time_ns: i64,
    /// On-disk byte size recorded when the file was scanned, if available.
    pub disk_size_bytes: Option<u64>,
}

/// Statistics about the database contents.
#[derive(Debug, Default, Clone)]
pub struct DatabaseStats {
    /// Total number of pending files.
    pub total_files: u64,
    /// Number of files needing conversion.
    pub convert_count: u64,
    /// Number of files needing remux.
    pub remux_count: u64,
    /// Number of files needing external subtitle muxing.
    pub subtitle_mux_count: u64,
    /// Total size of all pending files in bytes.
    pub total_size: u64,
}

/// Statistics about file extensions in the database.
#[derive(Debug, Clone)]
pub struct ExtensionStats {
    /// File extension.
    pub extension: String,
    /// Number of files with this extension.
    pub count: u64,
    /// Total size of files with this extension.
    pub total_size: u64,
}

/// Pre-stat entry ready to write inside an `IMMEDIATE` transaction.
struct PreparedPendingEntry<'a> {
    path_str: &'a str,
    extension: String,
    info: &'a VideoInfo,
    action: PendingAction,
    modified_time: Option<i64>,
    modified_time_ns: i64,
    disk_size_bytes: Option<u64>,
}

/// Filter options for querying pending files.
#[derive(Debug, Default, Clone)]
pub struct PendingFileFilter {
    /// Filter by action type.
    pub action: Option<PendingAction>,
    /// Filter by file extensions.
    pub extensions: Vec<String>,
    /// Optional path prefixes restricting matched rows to specific directories or files.
    pub path_prefixes: Vec<PathBuf>,
    /// Minimum bitrate.
    pub min_bitrate: Option<MinimumBitrate>,
    /// Maximum bitrate in kbps.
    pub max_bitrate: Option<u64>,
    /// Minimum duration in seconds.
    pub min_duration: Option<f64>,
    /// Maximum duration in seconds.
    pub max_duration: Option<f64>,
    /// Minimum equivalent 16:9 resolution height in pixels (conversions only).
    pub min_resolution: Option<u32>,
    /// Include files whose name matches at least one of these glob or substring patterns.
    pub include: Vec<String>,
    /// Exclude files whose name matches any of these glob or substring patterns.
    pub exclude: Vec<String>,
    /// Limit number of results.
    pub limit: Option<usize>,
    /// Sort order for results.
    pub sort: Option<SortOrder>,
}

impl PendingAction {
    /// Parse action from string.
    fn from_str(value: &str) -> Self {
        match value {
            "remux" => Self::Remux,
            "subtitle_mux" => Self::SubtitleMux,
            // Default to Convert for unknown values
            _ => Self::Convert,
        }
    }

    /// Convert action to string for database storage.
    const fn as_str(self) -> &'static str {
        match self {
            Self::Convert => "convert",
            Self::Remux => "remux",
            Self::SubtitleMux => "subtitle_mux",
        }
    }
}

impl std::fmt::Display for PendingAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl Database {
    /// Open or create the unified `cli-tools.db` database at the default path.
    ///
    /// # Errors
    /// Returns an error if the database cannot be opened or initialized.
    pub fn open_default() -> Result<Self> {
        let path = sqlite::database_path()?;
        Self::open_at(&path)
    }

    /// Open or create the database at a specific path.
    ///
    /// # Errors
    /// Returns an error if the database cannot be opened or initialized.
    pub fn open_at(path: &Path) -> Result<Self> {
        let connection = sqlite::open_at(path)?;
        let legacy = if path.file_name().and_then(|name| name.to_str()) == Some(sqlite::DATABASE_FILENAME) {
            sqlite::legacy_database_path(path, LEGACY_VCONVERT_DB)
        } else {
            None
        };
        let mut database = Self {
            connection,
            db_path: Some(path.to_path_buf()),
        };
        database.initialize_with_legacy(legacy.as_deref())?;
        Ok(database)
    }

    /// Open the default unified database in read-only mode, returning `Ok(None)` if it does not exist yet.
    ///
    /// # Errors
    /// Returns an error if an existing database cannot be opened.
    pub fn open_read_only() -> Result<Option<Self>> {
        let path = sqlite::database_path()?;
        Self::open_read_only_at(&path)
    }

    /// Open a database at `path` in read-only mode, returning `Ok(None)` if `path` does not exist.
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

    /// Open an empty in-memory database, for tests and for runs that keep no state.
    ///
    /// # Errors
    /// Returns an error if the database cannot be created.
    pub fn open_in_memory() -> Result<Self> {
        let connection = sqlite::open_in_memory()?;
        let mut database = Self {
            connection,
            db_path: None,
        };
        database.initialize_with_legacy(None)?;
        Ok(database)
    }

    /// Get the database path for display purposes.
    #[must_use]
    pub fn path() -> PathBuf {
        sqlite::database_path().unwrap_or_else(|_| PathBuf::from(sqlite::DATABASE_FILENAME))
    }

    /// Return the on-disk path of this open database, or `None` for an in-memory database.
    #[must_use]
    pub fn db_path(&self) -> Option<&Path> {
        self.db_path.as_deref()
    }

    /// Insert or update a pending file entry (defaulting `movie_mode` to `false`).
    ///
    /// # Errors
    /// Returns an error if the database operation fails.
    pub fn upsert_pending_file(
        &self,
        path: &Path,
        extension: &str,
        info: &VideoInfo,
        action: PendingAction,
    ) -> Result<i64> {
        self.upsert_pending_file_with_mode(path, extension, info, action, false)
    }

    /// Insert or update a pending file entry with explicit movie-mode state.
    ///
    /// # Errors
    /// Returns an error if the database operation fails.
    pub fn upsert_pending_file_with_mode(
        &self,
        path: &Path,
        extension: &str,
        info: &VideoInfo,
        action: PendingAction,
        movie_mode: bool,
    ) -> Result<i64> {
        let Some(path_str) = path.to_str() else {
            return Ok(0);
        };
        let now = unix_now_secs();
        let (modified_time, modified_time_ns, disk_size_bytes) = read_file_disk_metadata(path);

        self.connection
            .execute(
                r"
                INSERT INTO pending_files (
                    full_path, extension, codec, bitrate_kbps, size_bytes, duration, width, height,
                    frames_per_second, bit_depth, action, movie_mode, created_time,
                    modified_time, modified_time_ns, disk_size_bytes
                )
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)
                ON CONFLICT(full_path) DO UPDATE SET
                    extension = excluded.extension,
                    codec = excluded.codec,
                    bitrate_kbps = excluded.bitrate_kbps,
                    size_bytes = excluded.size_bytes,
                    duration = excluded.duration,
                    width = excluded.width,
                    height = excluded.height,
                    frames_per_second = excluded.frames_per_second,
                    bit_depth = excluded.bit_depth,
                    action = excluded.action,
                    movie_mode = excluded.movie_mode,
                    modified_time = excluded.modified_time,
                    modified_time_ns = excluded.modified_time_ns,
                    disk_size_bytes = excluded.disk_size_bytes
                ",
                params![
                    path_str,
                    extension.to_lowercase(),
                    info.codec,
                    info.bitrate_kbps as i64,
                    info.size_bytes as i64,
                    info.duration,
                    info.width,
                    info.height,
                    info.frames_per_second,
                    i64::from(info.bit_depth),
                    action.as_str(),
                    i64::from(movie_mode),
                    now,
                    modified_time,
                    modified_time_ns,
                    disk_size_bytes.map(|size| size as i64),
                ],
            )
            .context("Failed to insert pending file")?;

        Ok(self.connection.last_insert_rowid())
    }

    /// Insert or update multiple pending files in a single `IMMEDIATE` transaction.
    ///
    /// All filesystem metadata reads happen before opening the write transaction.
    ///
    /// # Errors
    /// Returns an error if the transaction cannot be started or committed.
    pub fn batch_upsert_pending_files(
        &mut self,
        entries: &[(&Path, &str, &VideoInfo, PendingAction)],
    ) -> Result<usize> {
        self.batch_upsert_pending_files_with_mode(entries, false)
    }

    /// Insert or update multiple pending files with explicit movie-mode state in a single `IMMEDIATE` transaction.
    ///
    /// # Errors
    /// Returns an error if the transaction cannot be started or committed.
    pub fn batch_upsert_pending_files_with_mode(
        &mut self,
        entries: &[(&Path, &str, &VideoInfo, PendingAction)],
        movie_mode: bool,
    ) -> Result<usize> {
        if entries.is_empty() {
            return Ok(0);
        }

        // Collect all filesystem stat calls before taking the SQLite write lock.
        let prepared: Vec<PreparedPendingEntry<'_>> = entries
            .iter()
            .filter_map(|&(path, extension, info, action)| {
                let path_str = path.to_str()?;
                let (modified_time, modified_time_ns, disk_size_bytes) = read_file_disk_metadata(path);
                Some(PreparedPendingEntry {
                    path_str,
                    extension: extension.to_lowercase(),
                    info,
                    action,
                    modified_time,
                    modified_time_ns,
                    disk_size_bytes,
                })
            })
            .collect();

        if prepared.is_empty() {
            return Ok(0);
        }

        let now = unix_now_secs();
        let transaction = sqlite::immediate_transaction(&mut self.connection)?;
        let mut count = 0;

        for entry in prepared {
            let entry_movie_mode = movie_mode || entry.action == PendingAction::SubtitleMux;
            transaction
                .execute(
                    r"
                    INSERT INTO pending_files (
                        full_path, extension, codec, bitrate_kbps, size_bytes, duration, width, height,
                        frames_per_second, bit_depth, action, movie_mode, created_time,
                        modified_time, modified_time_ns, disk_size_bytes
                    )
                    VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)
                    ON CONFLICT(full_path) DO UPDATE SET
                        extension = excluded.extension,
                        codec = excluded.codec,
                        bitrate_kbps = excluded.bitrate_kbps,
                        size_bytes = excluded.size_bytes,
                        duration = excluded.duration,
                        width = excluded.width,
                        height = excluded.height,
                        frames_per_second = excluded.frames_per_second,
                        bit_depth = excluded.bit_depth,
                        action = excluded.action,
                        movie_mode = excluded.movie_mode,
                        modified_time = excluded.modified_time,
                        modified_time_ns = excluded.modified_time_ns,
                        disk_size_bytes = excluded.disk_size_bytes
                    ",
                    params![
                        entry.path_str,
                        entry.extension,
                        entry.info.codec,
                        entry.info.bitrate_kbps as i64,
                        entry.info.size_bytes as i64,
                        entry.info.duration,
                        entry.info.width,
                        entry.info.height,
                        entry.info.frames_per_second,
                        i64::from(entry.info.bit_depth),
                        entry.action.as_str(),
                        i64::from(entry_movie_mode),
                        now,
                        entry.modified_time,
                        entry.modified_time_ns,
                        entry.disk_size_bytes.map(|size| size as i64),
                    ],
                )
                .context("Failed to insert pending file")?;
            count += 1;
        }

        transaction.commit()?;
        Ok(count)
    }

    /// Atomically claim a specific pending file row for `owner_id` if unclaimed or lease-expired.
    ///
    /// Returns `Ok(true)` if the claim succeeded or is already held by `owner_id`,
    /// and `Ok(false)` if another worker holds a live claim or the row no longer exists.
    ///
    /// # Errors
    /// Returns an error if the database transaction fails.
    pub fn claim_pending_file(&mut self, path: &Path, owner_id: &str, lease_ttl_secs: i64) -> Result<bool> {
        let Some(path_str) = path.to_str() else {
            return Ok(false);
        };
        let now = unix_now_secs();
        let stale_before = now.saturating_sub(lease_ttl_secs);
        let tx = sqlite::immediate_transaction(&mut self.connection)?;
        let updated = tx.execute(
            r"
            UPDATE pending_files
            SET claimed_by = ?1, claimed_at = ?2
            WHERE full_path = ?3
              AND (claimed_by IS NULL OR claimed_by = ?1 OR COALESCE(claimed_at, 0) <= ?4)
            ",
            params![owner_id, now, path_str, stale_before],
        )?;
        tx.commit()?;
        Ok(updated > 0)
    }

    /// Release a pending-file claim owned by `owner_id`.
    ///
    /// # Errors
    /// Returns an error if the update fails.
    pub fn release_pending_claim(&mut self, path: &Path, owner_id: &str) -> Result<()> {
        let Some(path_str) = path.to_str() else {
            return Ok(());
        };
        let tx = sqlite::immediate_transaction(&mut self.connection)?;
        tx.execute(
            "UPDATE pending_files SET claimed_by = NULL, claimed_at = NULL WHERE full_path = ?1 AND claimed_by = ?2",
            params![path_str, owner_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Release every pending-file claim owned by `owner_id`.
    ///
    /// # Errors
    /// Returns an error if the update fails.
    pub fn release_all_pending_claims(&mut self, owner_id: &str) -> Result<usize> {
        let tx = sqlite::immediate_transaction(&mut self.connection)?;
        let count = tx.execute(
            "UPDATE pending_files SET claimed_by = NULL, claimed_at = NULL WHERE claimed_by = ?1",
            params![owner_id],
        )?;
        tx.commit()?;
        Ok(count)
    }

    /// Get pending files with optional filtering.
    ///
    /// # Errors
    /// Returns an error if the database operation fails.
    pub fn get_pending_files(&self, filter: &PendingFileFilter) -> Result<Vec<PendingFile>> {
        if !sqlite::table_exists(&self.connection, "main", "pending_files")? {
            return Ok(Vec::new());
        }
        let (where_clause, params_vec) = build_pending_where_clause(filter);

        let has_post_filters =
            !filter.include.is_empty() || !filter.exclude.is_empty() || !filter.path_prefixes.is_empty();
        let limit_clause = if has_post_filters {
            String::new()
        } else {
            filter.limit.map_or_default(|limit| format!("LIMIT {limit}"))
        };

        let order_clause = filter.sort.map_or_else(
            || "ORDER BY action, size_bytes DESC".to_string(),
            |sort| format!("ORDER BY {}", sort.sql_order_clause()),
        );

        let sql = format!(
            r"
            SELECT id, full_path, extension, codec, bitrate_kbps, size_bytes, duration, width, height,
                   frames_per_second, bit_depth, action, movie_mode, claimed_by, claimed_at,
                   created_time, modified_time, modified_time_ns, disk_size_bytes
            FROM pending_files
            {where_clause}
            {order_clause}
            {limit_clause}
            "
        );

        let mut statement = self.connection.prepare(&sql)?;
        let params_refs: Vec<&dyn rusqlite::ToSql> = params_vec.iter().map(std::convert::AsRef::as_ref).collect();
        let rows = statement.query_map(params_refs.as_slice(), row_to_pending_file)?;

        let mut files = rows
            .collect::<Result<Vec<_>, _>>()
            .context("Failed to query pending files")?;

        if !filter.path_prefixes.is_empty() {
            files.retain(|file| {
                filter
                    .path_prefixes
                    .iter()
                    .any(|prefix| file.full_path.starts_with(prefix))
            });
        }

        if !filter.include.is_empty() || !filter.exclude.is_empty() {
            let name_filter = cli_tools_common::NameFilter::new(&filter.include, &filter.exclude)?;
            files.retain(|file| name_filter.matches_path(&file.full_path));
        }

        if has_post_filters && let Some(limit) = filter.limit {
            files.truncate(limit);
        }

        Ok(files)
    }

    /// Get a single pending file by path.
    ///
    /// # Errors
    /// Returns an error if the database operation fails.
    pub fn get_pending_file(&self, path: &Path) -> Result<Option<PendingFile>> {
        if !sqlite::table_exists(&self.connection, "main", "pending_files")? {
            return Ok(None);
        }
        let Some(path_str) = path.to_str() else {
            return Ok(None);
        };

        let mut statement = self.connection.prepare(
            r"
            SELECT id, full_path, extension, codec, bitrate_kbps, size_bytes, duration, width, height,
                   frames_per_second, bit_depth, action, movie_mode, claimed_by, claimed_at,
                   created_time, modified_time, modified_time_ns, disk_size_bytes
            FROM pending_files
            WHERE full_path = ?1
            ",
        )?;

        let file = statement
            .query_row(params![path_str], row_to_pending_file)
            .optional()
            .context("Failed to query pending file")?;

        Ok(file)
    }

    /// Remove a pending file by path.
    ///
    /// Returns `true` if a file was removed.
    ///
    /// # Errors
    /// Returns an error if the database operation fails.
    pub fn remove_pending_file(&self, path: &Path) -> Result<bool> {
        let Some(path_str) = path.to_str() else {
            return Ok(false);
        };

        let rows_affected = self
            .connection
            .execute("DELETE FROM pending_files WHERE full_path = ?1", params![path_str])
            .context("Failed to remove pending file")?;

        Ok(rows_affected > 0)
    }

    /// Remove a pending file by ID.
    ///
    /// Returns `true` if a file was removed.
    ///
    /// # Errors
    /// Returns an error if the database operation fails.
    pub fn remove_pending_file_by_id(&self, id: i64) -> Result<bool> {
        let rows_affected = self
            .connection
            .execute("DELETE FROM pending_files WHERE id = ?1", params![id])
            .context("Failed to remove pending file")?;

        Ok(rows_affected > 0)
    }

    /// Remove all pending files that no longer exist on mounted volumes.
    ///
    /// Existence checks happen outside the `SQLite` write transaction,
    /// and paths on offline/unmounted drives are preserved.
    ///
    /// # Errors
    /// Returns an error if the database operation fails.
    pub fn remove_missing_files(&mut self) -> Result<usize> {
        let files = self.get_pending_files(&PendingFileFilter::default())?;
        let missing_ids: Vec<i64> = files
            .into_iter()
            .filter(|file| is_definitively_missing(&file.full_path))
            .map(|file| file.id)
            .collect();

        if missing_ids.is_empty() {
            return Ok(0);
        }

        let transaction = sqlite::immediate_transaction(&mut self.connection)?;
        let mut removed_count = 0;
        for id in missing_ids {
            removed_count += transaction.execute("DELETE FROM pending_files WHERE id = ?1", params![id])?;
        }
        transaction.commit()?;
        Ok(removed_count)
    }

    /// Remove all pending files that were modified on disk after they were scanned.
    ///
    /// Filesystem `stat()` checks happen outside the `SQLite` write transaction.
    ///
    /// # Errors
    /// Returns an error if the database operation fails.
    pub fn remove_changed_files(&mut self) -> Result<usize> {
        let files = self.get_pending_files(&PendingFileFilter::default())?;
        let changed_ids: Vec<i64> = files
            .into_iter()
            .filter(PendingFile::is_changed_on_disk)
            .map(|file| file.id)
            .collect();

        if changed_ids.is_empty() {
            return Ok(0);
        }

        let transaction = sqlite::immediate_transaction(&mut self.connection)?;
        let mut removed_count = 0;
        for id in changed_ids {
            removed_count += transaction.execute("DELETE FROM pending_files WHERE id = ?1", params![id])?;
        }
        transaction.commit()?;
        Ok(removed_count)
    }

    /// Clear all pending files from the database.
    ///
    /// Returns the number of files removed.
    ///
    /// # Errors
    /// Returns an error if the database operation fails.
    pub fn clear(&self) -> Result<usize> {
        let rows_affected = self
            .connection
            .execute("DELETE FROM pending_files", [])
            .context("Failed to clear pending files")?;

        Ok(rows_affected)
    }

    /// Get statistics about the database contents.
    ///
    /// # Errors
    /// Returns an error if the database operation fails.
    pub fn get_stats(&self) -> Result<DatabaseStats> {
        let mut stats = DatabaseStats::default();
        if !sqlite::table_exists(&self.connection, "main", "pending_files")? {
            return Ok(stats);
        }

        // Get total count and size
        let mut statement = self
            .connection
            .prepare("SELECT COUNT(*), COALESCE(SUM(size_bytes), 0) FROM pending_files")?;
        let (total_files, total_size): (i64, i64) = statement.query_row([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        stats.total_files = total_files as u64;
        stats.total_size = total_size as u64;

        // Get counts by action
        let mut statement = self
            .connection
            .prepare("SELECT action, COUNT(*) FROM pending_files GROUP BY action")?;
        let rows = statement.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)))?;

        for row in rows {
            let (action, count) = row?;
            match action.as_str() {
                "convert" => stats.convert_count = count as u64,
                "remux" => stats.remux_count = count as u64,
                "subtitle_mux" => stats.subtitle_mux_count = count as u64,
                _ => {}
            }
        }

        Ok(stats)
    }

    /// Get statistics grouped by file extension.
    ///
    /// # Errors
    /// Returns an error if the database operation fails.
    pub fn get_extension_stats(&self) -> Result<Vec<ExtensionStats>> {
        if !sqlite::table_exists(&self.connection, "main", "pending_files")? {
            return Ok(Vec::new());
        }
        let mut statement = self.connection.prepare(
            r"
            SELECT extension, COUNT(*), COALESCE(SUM(size_bytes), 0)
            FROM pending_files
            GROUP BY extension
            ORDER BY COUNT(*) DESC
            ",
        )?;

        let rows = statement.query_map([], |row| {
            Ok(ExtensionStats {
                extension: row.get(0)?,
                count: row.get::<_, i64>(1)? as u64,
                total_size: row.get::<_, i64>(2)? as u64,
            })
        })?;

        let stats = rows
            .collect::<Result<Vec<_>, _>>()
            .context("Failed to query extension stats")?;

        Ok(stats)
    }

    /// Get list of unique extensions in the database.
    ///
    /// # Errors
    /// Returns an error if the database operation fails.
    #[cfg(test)]
    pub fn get_extensions(&self) -> Result<Vec<String>> {
        if !sqlite::table_exists(&self.connection, "main", "pending_files")? {
            return Ok(Vec::new());
        }
        let mut statement = self
            .connection
            .prepare("SELECT DISTINCT extension FROM pending_files ORDER BY extension")?;

        let rows = statement.query_map([], |row| row.get(0))?;

        let extensions = rows
            .collect::<Result<Vec<_>, _>>()
            .context("Failed to query extensions")?;

        Ok(extensions)
    }

    /// Initialize shared media cache, active locks, and `pending_files` schemas via versioned migrations.
    fn initialize_with_legacy(&mut self, legacy_vconvert_db: Option<&Path>) -> Result<()> {
        cli_tools_common::locks::ensure_schema(&mut self.connection)?;
        ensure_media_cache_schema(&mut self.connection, legacy_vconvert_db)?;

        let was_uninitialized = sqlite::current_component_version(&self.connection, PENDING_COMPONENT)? == 0;
        sqlite::migrate_component(
            &mut self.connection,
            PENDING_COMPONENT,
            PENDING_SCHEMA_VERSION,
            |tx, current_version| {
                if current_version < 1 {
                    tx.execute_batch(
                        r"
                        CREATE TABLE IF NOT EXISTS pending_files (
                            id INTEGER PRIMARY KEY,
                            full_path TEXT NOT NULL UNIQUE,
                            extension TEXT NOT NULL,
                            codec TEXT NOT NULL,
                            bitrate_kbps INTEGER NOT NULL,
                            size_bytes INTEGER NOT NULL,
                            duration REAL NOT NULL,
                            width INTEGER NOT NULL,
                            height INTEGER NOT NULL,
                            frames_per_second REAL NOT NULL,
                            bit_depth INTEGER NOT NULL DEFAULT 0,
                            action TEXT NOT NULL,
                            movie_mode INTEGER NOT NULL DEFAULT 0,
                            claimed_by TEXT,
                            claimed_at INTEGER,
                            created_time INTEGER NOT NULL,
                            modified_time INTEGER,
                            modified_time_ns INTEGER NOT NULL DEFAULT 0,
                            disk_size_bytes INTEGER
                        );

                        CREATE INDEX IF NOT EXISTS idx_pending_action ON pending_files(action);
                        CREATE INDEX IF NOT EXISTS idx_pending_extension ON pending_files(extension);
                        CREATE INDEX IF NOT EXISTS idx_pending_bitrate ON pending_files(bitrate_kbps);
                        CREATE INDEX IF NOT EXISTS idx_pending_duration ON pending_files(duration);
                        ",
                    )
                    .context("Failed to create pending_files table")?;
                }
                if current_version < 2 {
                    sqlite::add_column_if_missing(tx, "pending_files", "bit_depth", "INTEGER NOT NULL DEFAULT 0")?;
                    sqlite::add_column_if_missing(
                        tx,
                        "pending_files",
                        "modified_time_ns",
                        "INTEGER NOT NULL DEFAULT 0",
                    )?;
                    sqlite::add_column_if_missing(tx, "pending_files", "disk_size_bytes", "INTEGER")?;
                    sqlite::add_column_if_missing(tx, "pending_files", "movie_mode", "INTEGER NOT NULL DEFAULT 0")?;
                    sqlite::add_column_if_missing(tx, "pending_files", "claimed_by", "TEXT")?;
                    sqlite::add_column_if_missing(tx, "pending_files", "claimed_at", "INTEGER")?;
                    tx.execute_batch(
                        "DELETE FROM pending_files WHERE frames_per_second < 1.0 OR frames_per_second > 240.0;",
                    )
                    .context("Failed to prune invalid fps rows from pending_files")?;
                }
                Ok(())
            },
        )?;

        if was_uninitialized && let Some(legacy_path) = legacy_vconvert_db {
            import_legacy_pending_files(&mut self.connection, legacy_path)?;
        }

        Ok(())
    }

    #[cfg(test)]
    fn initialize(&mut self) -> Result<()> {
        self.initialize_with_legacy(None)
    }
}

impl PendingFile {
    /// Convert to `VideoInfo` for processing.
    #[must_use]
    pub fn to_video_info(&self) -> VideoInfo {
        VideoInfo {
            codec: self.codec.clone(),
            bitrate_kbps: self.bitrate_kbps,
            size_bytes: self.size_bytes,
            duration: self.duration,
            width: self.width,
            height: self.height,
            frames_per_second: self.frames_per_second,
            bit_depth: self.bit_depth.max(8),
            warning: None,
        }
    }

    /// Whether the file on disk has a different size or modification time than when it was scanned.
    ///
    /// Compare the stored size independently of the timestamp, using signed nanoseconds when available.
    /// Files whose metadata cannot be read count as unchanged here.
    #[must_use]
    pub fn is_changed_on_disk(&self) -> bool {
        let Ok(metadata) = std::fs::metadata(&self.full_path) else {
            return false;
        };
        if let Some(stored_disk_size) = self.disk_size_bytes
            && metadata.len() != stored_disk_size
        {
            return true;
        }
        let Ok(modified) = metadata.modified() else {
            return false;
        };
        if self.modified_time_ns != 0 {
            return system_time_to_unix_nanos(modified) != self.modified_time_ns;
        }
        self.modified_time.is_some_and(|stored_secs| {
            modified
                .duration_since(SystemTime::UNIX_EPOCH)
                .map_or(true, |duration| duration.as_secs() as i64 != stored_secs)
        })
    }
}

impl std::fmt::Display for DatabaseStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "Database Statistics:")?;
        writeln!(f, "  Total files:    {}", self.total_files)?;
        writeln!(f, "  To convert:     {}", self.convert_count)?;
        writeln!(f, "  To remux:       {}", self.remux_count)?;
        writeln!(f, "  To subtitle mux: {}", self.subtitle_mux_count)?;
        write!(
            f,
            "  Total size:     {}",
            cli_tools_common::format_size(self.total_size)
        )
    }
}

impl std::fmt::Display for ExtensionStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            ".{:<3} {:>4} files  {}",
            self.extension,
            self.count,
            cli_tools_common::format_size(self.total_size)
        )
    }
}

/// Build the SQL `WHERE` clause and bound parameter list for [`PendingFileFilter`].
fn build_pending_where_clause(filter: &PendingFileFilter) -> (String, Vec<Box<dyn rusqlite::ToSql>>) {
    let mut conditions = Vec::new();
    let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

    if let Some(action) = filter.action {
        params_vec.push(Box::new(action.as_str().to_string()));
        conditions.push(format!("action = ?{}", params_vec.len()));
    }

    if !filter.extensions.is_empty() {
        let placeholders: Vec<String> = filter
            .extensions
            .iter()
            .map(|ext| {
                params_vec.push(Box::new(ext.to_lowercase()));
                format!("?{}", params_vec.len())
            })
            .collect();
        conditions.push(format!("extension IN ({})", placeholders.join(", ")));
    }

    if let Some(min_bitrate) = filter.min_bitrate {
        let threshold = min_bitrate.sql_threshold(&mut params_vec);
        conditions.push(format!(
            "(action != '{}' OR bitrate_kbps >= {threshold})",
            PendingAction::Convert.as_str(),
        ));
    }

    if let Some(max_bitrate) = filter.max_bitrate {
        params_vec.push(Box::new(max_bitrate as i64));
        conditions.push(format!(
            "(action != '{}' OR bitrate_kbps <= ?{})",
            PendingAction::Convert.as_str(),
            params_vec.len()
        ));
    }

    if let Some(min_duration) = filter.min_duration {
        params_vec.push(Box::new(min_duration));
        conditions.push(format!(
            "(action != '{}' OR duration >= ?{})",
            PendingAction::Convert.as_str(),
            params_vec.len()
        ));
    }

    if let Some(max_duration) = filter.max_duration {
        params_vec.push(Box::new(max_duration));
        conditions.push(format!(
            "(action != '{}' OR duration <= ?{})",
            PendingAction::Convert.as_str(),
            params_vec.len()
        ));
    }

    if let Some(min_resolution) = filter.min_resolution {
        params_vec.push(Box::new(i64::from(min_resolution)));
        let param_index = params_vec.len();
        conditions.push(format!(
            "(action != '{}' OR MAX(MIN(width, height), (MAX(width, height) * 9) / 16) >= ?{param_index})",
            PendingAction::Convert.as_str(),
        ));
    }

    let where_clause = if conditions.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", conditions.join(" AND "))
    };
    (where_clause, params_vec)
}

/// Convert a database row to a `PendingFile`.
fn row_to_pending_file(row: &rusqlite::Row<'_>) -> rusqlite::Result<PendingFile> {
    Ok(PendingFile {
        id: row.get(0)?,
        full_path: PathBuf::from(row.get::<_, String>(1)?),
        extension: row.get(2)?,
        codec: row.get(3)?,
        bitrate_kbps: row.get::<_, i64>(4)? as u64,
        size_bytes: row.get::<_, i64>(5)? as u64,
        duration: row.get(6)?,
        width: row.get::<_, i64>(7)? as u32,
        height: row.get::<_, i64>(8)? as u32,
        frames_per_second: row.get(9)?,
        bit_depth: row.get::<_, i64>(10)? as u8,
        action: PendingAction::from_str(&row.get::<_, String>(11)?),
        movie_mode: row.get::<_, i64>(12)? != 0,
        claimed_by: row.get(13)?,
        claimed_at: row.get(14)?,
        created_time: row.get(15)?,
        modified_time: row.get(16)?,
        modified_time_ns: row.get(17)?,
        disk_size_bytes: row.get::<_, Option<i64>>(18)?.map(|size| size as u64),
    })
}

/// Copy `pending_files` rows from a legacy `vconvert.db` into `cli-tools.db`.
fn import_legacy_pending_files(connection: &mut Connection, legacy_path: &Path) -> Result<()> {
    sqlite::import_from_legacy_database(connection, legacy_path, |tx| {
        if !sqlite::table_exists(tx, "legacy", "pending_files")? {
            return Ok(());
        }
        let has_bit_depth = sqlite::column_exists(tx, "legacy", "pending_files", "bit_depth")?;
        let has_mtime_ns = sqlite::column_exists(tx, "legacy", "pending_files", "modified_time_ns")?;
        let has_disk_size = sqlite::column_exists(tx, "legacy", "pending_files", "disk_size_bytes")?;
        let bit_depth_expr = if has_bit_depth { "bit_depth" } else { "0" };
        let mtime_ns_expr = if has_mtime_ns { "modified_time_ns" } else { "0" };
        let disk_size_expr = if has_disk_size { "disk_size_bytes" } else { "NULL" };
        tx.execute(
            &format!(
                r"
                INSERT OR IGNORE INTO main.pending_files (
                    full_path, extension, codec, bitrate_kbps, size_bytes, duration, width, height,
                    frames_per_second, bit_depth, action, movie_mode, created_time,
                    modified_time, modified_time_ns, disk_size_bytes
                )
                SELECT full_path, extension, codec, bitrate_kbps, size_bytes, duration, width, height,
                       frames_per_second, {bit_depth_expr}, action,
                       CASE WHEN action = 'subtitle_mux' THEN 1 ELSE 0 END,
                       created_time, modified_time, {mtime_ns_expr}, {disk_size_expr}
                FROM legacy.pending_files
                WHERE frames_per_second >= 1.0 AND frames_per_second <= 240.0
                "
            ),
            [],
        )
        .context("Failed to import legacy pending_files rows")?;
        Ok(())
    })
}

/// Read modified time (seconds and nanoseconds) and disk size in a single `stat()` call.
fn read_file_disk_metadata(path: &Path) -> (Option<i64>, i64, Option<u64>) {
    let Ok(metadata) = std::fs::metadata(path) else {
        return (None, 0, None);
    };
    let disk_size = Some(metadata.len());
    let Ok(modified) = metadata.modified() else {
        return (None, 0, disk_size);
    };
    let modified_secs = modified
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_secs() as i64);
    let modified_ns = system_time_to_unix_nanos(modified);
    (modified_secs, modified_ns, disk_size)
}

/// Current Unix timestamp in seconds.
fn unix_now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs() as i64)
}

/// File modification time in signed nanoseconds relative to the Unix epoch, or `0` if unavailable.
pub(crate) fn file_modified_time_ns(path: &Path) -> i64 {
    std::fs::metadata(path)
        .ok()
        .and_then(|metadata| metadata.modified().ok())
        .map_or(0, system_time_to_unix_nanos)
}

/// Convert a [`SystemTime`] to signed nanoseconds relative to the Unix epoch.
fn system_time_to_unix_nanos(time: SystemTime) -> i64 {
    match time.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(duration) => i64::try_from(duration.as_nanos()).unwrap_or(i64::MAX),
        Err(error) => {
            let before_epoch = error.duration().as_nanos();
            i64::try_from(before_epoch).map_or(i64::MIN, |nanos| -nanos)
        }
    }
}

#[cfg(test)]
mod test_database {
    use super::*;

    fn create_test_video_info() -> VideoInfo {
        VideoInfo {
            codec: "h264".to_string(),
            bitrate_kbps: 8000,
            size_bytes: 1_000_000_000,
            duration: 3600.0,
            width: 1920,
            height: 1080,
            frames_per_second: 24.0,
            bit_depth: 8,
            warning: None,
        }
    }

    #[test]
    fn test_open_in_memory() {
        let database = Database::open_in_memory();
        assert!(database.is_ok());
    }

    #[test]
    fn initialize_preserves_shared_scan_cache_rows_written_by_dupefind() {
        let mut database = Database::open_in_memory().expect("open initial");
        database
            .connection
            .execute(
                "INSERT INTO scanned_files (full_path, size_bytes, codec, bitrate_kbps, duration, width, height, frames_per_second, bit_depth, scanned_time)
                 VALUES ('/videos/dupefind_entry.mkv', 5000, 'hevc', 4000, 60.0, 1920, 1080, 0.0, 0, 100)",
                [],
            )
            .expect("insert dupefind row");

        database.initialize().expect("reinitialize");
        let entries = database.get_all_scanned_files().expect("read scanned");
        assert!(
            entries.contains_key("/videos/dupefind_entry.mkv"),
            "vconvert startup must not delete rows written by dupefind"
        );
    }

    #[test]
    fn pending_file_is_changed_on_disk_detects_same_second_size_change() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let file_path = temp_dir.path().join("clip.mkv");
        std::fs::write(&file_path, b"12345").expect("write initial");

        let database = Database::open_in_memory().expect("open db");
        let mut info = create_test_video_info();
        info.size_bytes = 5;
        database
            .upsert_pending_file(&file_path, "mkv", &info, PendingAction::Convert)
            .expect("insert");

        // Rewrite with different size within the same second.
        std::fs::write(&file_path, b"123456789").expect("rewrite larger");
        let pending = database
            .get_pending_file(&file_path)
            .expect("query")
            .expect("row exists");
        assert!(
            pending.is_changed_on_disk(),
            "same-second size change must be detected by is_changed_on_disk"
        );
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_paths_are_skipped_by_upserts() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let mut database = Database::open_in_memory().expect("open db");
        let bad_path = Path::new(OsStr::from_bytes(b"/videos/clip_\xff.mkv"));
        let info = create_test_video_info();

        let pending_written = database
            .batch_upsert_pending_files(&[(bad_path, "mkv", &info, PendingAction::Convert)])
            .expect("batch pending");
        assert_eq!(pending_written, 0);

        let scanned_written = database
            .batch_upsert_scanned_files(&[(bad_path, &info)])
            .expect("batch scanned");
        assert_eq!(scanned_written, 0);
    }

    #[test]
    fn test_insert_and_get_pending_file() {
        let database = Database::open_in_memory().expect("Failed to open database");
        let info = create_test_video_info();
        let path = PathBuf::from("/test/video.mp4");

        let result = database.upsert_pending_file(&path, "mp4", &info, PendingAction::Convert);
        assert!(result.is_ok());

        let files = database
            .get_pending_files(&PendingFileFilter::default())
            .expect("Failed to get files");
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].full_path, path);
        assert_eq!(files[0].extension, "mp4");
        assert_eq!(files[0].codec, "h264");
        assert_eq!(files[0].action, PendingAction::Convert);
    }

    #[test]
    fn test_upsert_updates_existing() {
        let database = Database::open_in_memory().expect("Failed to open database");
        let path = PathBuf::from("/test/video.mp4");

        let info1 = VideoInfo {
            codec: "h264".to_string(),
            bitrate_kbps: 8000,
            size_bytes: 1_000_000_000,
            duration: 3600.0,
            width: 1920,
            height: 1080,
            frames_per_second: 24.0,
            bit_depth: 8,
            warning: None,
        };

        database
            .upsert_pending_file(&path, "mp4", &info1, PendingAction::Convert)
            .expect("Failed to insert");

        let info2 = VideoInfo {
            codec: "hevc".to_string(),
            bitrate_kbps: 4000,
            size_bytes: 500_000_000,
            duration: 3600.0,
            width: 1920,
            height: 1080,
            frames_per_second: 24.0,
            bit_depth: 8,
            warning: None,
        };

        database
            .upsert_pending_file(&path, "mp4", &info2, PendingAction::Remux)
            .expect("Failed to update");

        let files = database
            .get_pending_files(&PendingFileFilter::default())
            .expect("Failed to get files");
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].codec, "hevc");
        assert_eq!(files[0].action, PendingAction::Remux);
    }

    #[test]
    fn test_filter_by_action() {
        let database = Database::open_in_memory().expect("Failed to open database");
        let info = create_test_video_info();

        database
            .upsert_pending_file(&PathBuf::from("/test/a.mp4"), "mp4", &info, PendingAction::Convert)
            .expect("Failed to insert");
        database
            .upsert_pending_file(&PathBuf::from("/test/b.mkv"), "mkv", &info, PendingAction::Remux)
            .expect("Failed to insert");
        database
            .upsert_pending_file(&PathBuf::from("/test/c.mp4"), "mp4", &info, PendingAction::Convert)
            .expect("Failed to insert");

        let convert_files = database
            .get_pending_files(&PendingFileFilter {
                action: Some(PendingAction::Convert),
                ..Default::default()
            })
            .expect("Failed to get files");
        assert_eq!(convert_files.len(), 2);

        let remux_files = database
            .get_pending_files(&PendingFileFilter {
                action: Some(PendingAction::Remux),
                ..Default::default()
            })
            .expect("Failed to get files");
        assert_eq!(remux_files.len(), 1);
    }

    #[test]
    fn test_filter_by_extension() {
        let database = Database::open_in_memory().expect("Failed to open database");
        let info = create_test_video_info();

        database
            .upsert_pending_file(&PathBuf::from("/test/a.mp4"), "mp4", &info, PendingAction::Convert)
            .expect("Failed to insert");
        database
            .upsert_pending_file(&PathBuf::from("/test/b.mkv"), "mkv", &info, PendingAction::Convert)
            .expect("Failed to insert");
        database
            .upsert_pending_file(&PathBuf::from("/test/c.avi"), "avi", &info, PendingAction::Convert)
            .expect("Failed to insert");

        let mp4_files = database
            .get_pending_files(&PendingFileFilter {
                extensions: vec!["mp4".to_string()],
                ..Default::default()
            })
            .expect("Failed to get files");
        assert_eq!(mp4_files.len(), 1);

        let mp4_mkv_files = database
            .get_pending_files(&PendingFileFilter {
                extensions: vec!["mp4".to_string(), "mkv".to_string()],
                ..Default::default()
            })
            .expect("Failed to get files");
        assert_eq!(mp4_mkv_files.len(), 2);
    }

    #[test]
    fn test_filter_by_bitrate() {
        let database = Database::open_in_memory().expect("Failed to open database");

        let info_low = VideoInfo {
            codec: "h264".to_string(),
            bitrate_kbps: 2000,
            size_bytes: 500_000_000,
            duration: 3600.0,
            width: 1920,
            height: 1080,
            frames_per_second: 24.0,
            bit_depth: 8,
            warning: None,
        };

        let info_high = VideoInfo {
            codec: "h264".to_string(),
            bitrate_kbps: 15000,
            size_bytes: 2_000_000_000,
            duration: 3600.0,
            width: 1920,
            height: 1080,
            frames_per_second: 24.0,
            bit_depth: 8,
            warning: None,
        };

        database
            .upsert_pending_file(
                &PathBuf::from("/test/low.mp4"),
                "mp4",
                &info_low,
                PendingAction::Convert,
            )
            .expect("Failed to insert");
        database
            .upsert_pending_file(
                &PathBuf::from("/test/high.mp4"),
                "mp4",
                &info_high,
                PendingAction::Convert,
            )
            .expect("Failed to insert");

        let high_bitrate = database
            .get_pending_files(&PendingFileFilter {
                min_bitrate: Some(MinimumBitrate::Fixed(10000)),
                ..Default::default()
            })
            .expect("Failed to get files");
        assert_eq!(high_bitrate.len(), 1);
        assert_eq!(high_bitrate[0].bitrate_kbps, 15000);

        let low_bitrate = database
            .get_pending_files(&PendingFileFilter {
                max_bitrate: Some(5000),
                ..Default::default()
            })
            .expect("Failed to get files");
        assert_eq!(low_bitrate.len(), 1);
        assert_eq!(low_bitrate[0].bitrate_kbps, 2000);
    }

    #[test]
    fn test_filter_by_duration() {
        let database = Database::open_in_memory().expect("Failed to open database");

        let info_short = VideoInfo {
            codec: "h264".to_string(),
            bitrate_kbps: 8000,
            size_bytes: 500_000_000,
            duration: 600.0, // 10 minutes
            width: 1920,
            height: 1080,
            frames_per_second: 24.0,
            bit_depth: 8,
            warning: None,
        };

        let info_long = VideoInfo {
            codec: "h264".to_string(),
            bitrate_kbps: 8000,
            size_bytes: 2_000_000_000,
            duration: 7200.0, // 2 hours
            width: 1920,
            height: 1080,
            frames_per_second: 24.0,
            bit_depth: 8,
            warning: None,
        };

        database
            .upsert_pending_file(
                &PathBuf::from("/test/short.mp4"),
                "mp4",
                &info_short,
                PendingAction::Convert,
            )
            .expect("Failed to insert");
        database
            .upsert_pending_file(
                &PathBuf::from("/test/long.mp4"),
                "mp4",
                &info_long,
                PendingAction::Convert,
            )
            .expect("Failed to insert");

        let long_videos = database
            .get_pending_files(&PendingFileFilter {
                min_duration: Some(3600.0),
                ..Default::default()
            })
            .expect("Failed to get files");
        assert_eq!(long_videos.len(), 1);
        cli_tools_common::assert_f64_eq(long_videos[0].duration, 7200.0);
    }

    #[test]
    fn test_remove_pending_file() {
        let database = Database::open_in_memory().expect("Failed to open database");
        let info = create_test_video_info();
        let path = PathBuf::from("/test/video.mp4");

        database
            .upsert_pending_file(&path, "mp4", &info, PendingAction::Convert)
            .expect("Failed to insert");

        let removed = database.remove_pending_file(&path).expect("Failed to remove");
        assert!(removed);

        let files = database
            .get_pending_files(&PendingFileFilter::default())
            .expect("Failed to get files");
        assert!(files.is_empty());
    }

    #[test]
    fn test_clear_database() {
        let database = Database::open_in_memory().expect("Failed to open database");
        let info = create_test_video_info();

        database
            .upsert_pending_file(&PathBuf::from("/test/a.mp4"), "mp4", &info, PendingAction::Convert)
            .expect("Failed to insert");
        database
            .upsert_pending_file(&PathBuf::from("/test/b.mp4"), "mp4", &info, PendingAction::Convert)
            .expect("Failed to insert");

        let cleared = database.clear().expect("Failed to clear");
        assert_eq!(cleared, 2);

        let files = database
            .get_pending_files(&PendingFileFilter::default())
            .expect("Failed to get files");
        assert!(files.is_empty());
    }

    #[test]
    fn test_get_stats() {
        let database = Database::open_in_memory().expect("Failed to open database");
        let info = create_test_video_info();

        database
            .upsert_pending_file(&PathBuf::from("/test/a.mp4"), "mp4", &info, PendingAction::Convert)
            .expect("Failed to insert");
        database
            .upsert_pending_file(&PathBuf::from("/test/b.mkv"), "mkv", &info, PendingAction::Remux)
            .expect("Failed to insert");
        database
            .upsert_pending_file(&PathBuf::from("/test/c.mp4"), "mp4", &info, PendingAction::Convert)
            .expect("Failed to insert");

        let stats = database.get_stats().expect("Failed to get stats");
        assert_eq!(stats.total_files, 3);
        assert_eq!(stats.convert_count, 2);
        assert_eq!(stats.remux_count, 1);
        assert_eq!(stats.total_size, 3_000_000_000);
    }

    #[test]
    fn test_get_extension_stats() {
        let database = Database::open_in_memory().expect("Failed to open database");
        let info = create_test_video_info();

        database
            .upsert_pending_file(&PathBuf::from("/test/a.mp4"), "mp4", &info, PendingAction::Convert)
            .expect("Failed to insert");
        database
            .upsert_pending_file(&PathBuf::from("/test/b.mkv"), "mkv", &info, PendingAction::Convert)
            .expect("Failed to insert");
        database
            .upsert_pending_file(&PathBuf::from("/test/c.mp4"), "mp4", &info, PendingAction::Convert)
            .expect("Failed to insert");

        let ext_stats = database.get_extension_stats().expect("Failed to get extension stats");
        assert_eq!(ext_stats.len(), 2);

        let mp4_stats = ext_stats.iter().find(|s| s.extension == "mp4").expect("mp4 not found");
        assert_eq!(mp4_stats.count, 2);

        let mkv_stats = ext_stats.iter().find(|s| s.extension == "mkv").expect("mkv not found");
        assert_eq!(mkv_stats.count, 1);
    }

    #[test]
    fn test_get_extensions() {
        let database = Database::open_in_memory().expect("Failed to open database");
        let info = create_test_video_info();

        database
            .upsert_pending_file(&PathBuf::from("/test/a.mp4"), "mp4", &info, PendingAction::Convert)
            .expect("Failed to insert");
        database
            .upsert_pending_file(&PathBuf::from("/test/b.mkv"), "mkv", &info, PendingAction::Convert)
            .expect("Failed to insert");
        database
            .upsert_pending_file(&PathBuf::from("/test/c.avi"), "avi", &info, PendingAction::Convert)
            .expect("Failed to insert");

        let extensions = database.get_extensions().expect("Failed to get extensions");
        assert_eq!(extensions.len(), 3);
        assert!(extensions.contains(&"avi".to_string()));
        assert!(extensions.contains(&"mkv".to_string()));
        assert!(extensions.contains(&"mp4".to_string()));
    }

    #[test]
    fn test_pending_action_display() {
        assert_eq!(PendingAction::Convert.to_string(), "convert");
        assert_eq!(PendingAction::Remux.to_string(), "remux");
        assert_eq!(PendingAction::SubtitleMux.to_string(), "subtitle_mux");
    }

    #[test]
    fn test_pending_action_from_str() {
        assert_eq!(PendingAction::from_str("convert"), PendingAction::Convert);
        assert_eq!(PendingAction::from_str("remux"), PendingAction::Remux);
        assert_eq!(PendingAction::from_str("subtitle_mux"), PendingAction::SubtitleMux);
        assert_eq!(PendingAction::from_str("unknown"), PendingAction::Convert);
    }

    #[test]
    fn test_to_video_info() {
        let pending = PendingFile {
            id: 1,
            full_path: PathBuf::from("/test/video.mp4"),
            extension: "mp4".to_string(),
            codec: "h264".to_string(),
            bitrate_kbps: 8000,
            size_bytes: 1_000_000_000,
            duration: 3600.0,
            width: 1920,
            height: 1080,
            frames_per_second: 24.0,
            bit_depth: 10,
            action: PendingAction::Convert,
            movie_mode: false,
            claimed_by: None,
            claimed_at: None,
            created_time: 0,
            modified_time: None,
            modified_time_ns: 0,
            disk_size_bytes: None,
        };

        let info = pending.to_video_info();
        assert_eq!(info.codec, "h264");
        assert_eq!(info.bitrate_kbps, 8000);
        assert_eq!(info.size_bytes, 1_000_000_000);
        cli_tools_common::assert_f64_eq(info.duration, 3600.0);
        assert_eq!(info.width, 1920);
        assert_eq!(info.height, 1080);
        cli_tools_common::assert_f64_eq(info.frames_per_second, 24.0);
        assert_eq!(info.bit_depth, 10);
    }

    #[test]
    fn test_filter_with_limit() {
        let database = Database::open_in_memory().expect("Failed to open database");
        let info = create_test_video_info();

        for i in 0..10 {
            database
                .upsert_pending_file(
                    &PathBuf::from(format!("/test/video{i}.mp4")),
                    "mp4",
                    &info,
                    PendingAction::Convert,
                )
                .expect("Failed to insert");
        }

        let limited = database
            .get_pending_files(&PendingFileFilter {
                limit: Some(5),
                ..Default::default()
            })
            .expect("Failed to get files");
        assert_eq!(limited.len(), 5);
    }

    #[test]
    fn test_combined_filters() {
        let database = Database::open_in_memory().expect("Failed to open database");

        let info_low = VideoInfo {
            codec: "h264".to_string(),
            bitrate_kbps: 5000,
            size_bytes: 500_000_000,
            duration: 1800.0,
            width: 1920,
            height: 1080,
            frames_per_second: 24.0,
            bit_depth: 8,
            warning: None,
        };

        let info_high = VideoInfo {
            codec: "h264".to_string(),
            bitrate_kbps: 15000,
            size_bytes: 2_000_000_000,
            duration: 7200.0,
            width: 1920,
            height: 1080,
            frames_per_second: 24.0,
            bit_depth: 8,
            warning: None,
        };

        database
            .upsert_pending_file(&PathBuf::from("/test/a.mp4"), "mp4", &info_low, PendingAction::Convert)
            .expect("Failed to insert");
        database
            .upsert_pending_file(&PathBuf::from("/test/b.mkv"), "mkv", &info_high, PendingAction::Convert)
            .expect("Failed to insert");
        database
            .upsert_pending_file(&PathBuf::from("/test/c.mp4"), "mp4", &info_high, PendingAction::Remux)
            .expect("Failed to insert");

        // Filter: mp4 files with high bitrate that need conversion
        let filtered = database
            .get_pending_files(&PendingFileFilter {
                action: Some(PendingAction::Convert),
                extensions: vec!["mp4".to_string()],
                min_bitrate: Some(MinimumBitrate::Fixed(10000)),
                ..Default::default()
            })
            .expect("Failed to get files");
        // a.mp4 has low bitrate, c.mp4 is remux
        assert_eq!(filtered.len(), 0);

        // Filter: any file with high bitrate
        let filtered = database
            .get_pending_files(&PendingFileFilter {
                min_bitrate: Some(MinimumBitrate::Fixed(10000)),
                ..Default::default()
            })
            .expect("Failed to get files");
        assert_eq!(filtered.len(), 2);
    }

    #[test]
    fn test_sort_by_bitrate() {
        let database = Database::open_in_memory().expect("Failed to open database");

        let info_low = VideoInfo {
            codec: "h264".to_string(),
            bitrate_kbps: 2000,
            size_bytes: 500_000_000,
            duration: 3600.0,
            width: 1920,
            height: 1080,
            frames_per_second: 24.0,
            bit_depth: 8,
            warning: None,
        };

        let info_mid = VideoInfo {
            codec: "h264".to_string(),
            bitrate_kbps: 8000,
            size_bytes: 1_000_000_000,
            duration: 3600.0,
            width: 1920,
            height: 1080,
            frames_per_second: 24.0,
            bit_depth: 8,
            warning: None,
        };

        let info_high = VideoInfo {
            codec: "h264".to_string(),
            bitrate_kbps: 15000,
            size_bytes: 2_000_000_000,
            duration: 3600.0,
            width: 1920,
            height: 1080,
            frames_per_second: 24.0,
            bit_depth: 8,
            warning: None,
        };

        database
            .upsert_pending_file(
                &PathBuf::from("/test/low.mp4"),
                "mp4",
                &info_low,
                PendingAction::Convert,
            )
            .expect("Failed to insert");
        database
            .upsert_pending_file(
                &PathBuf::from("/test/mid.mp4"),
                "mp4",
                &info_mid,
                PendingAction::Convert,
            )
            .expect("Failed to insert");
        database
            .upsert_pending_file(
                &PathBuf::from("/test/high.mp4"),
                "mp4",
                &info_high,
                PendingAction::Convert,
            )
            .expect("Failed to insert");

        // Sort by bitrate descending (highest first)
        let sorted_desc = database
            .get_pending_files(&PendingFileFilter {
                sort: Some(SortOrder::Bitrate),
                ..Default::default()
            })
            .expect("Failed to get files");
        assert_eq!(sorted_desc.len(), 3);
        assert_eq!(sorted_desc[0].bitrate_kbps, 15000);
        assert_eq!(sorted_desc[1].bitrate_kbps, 8000);
        assert_eq!(sorted_desc[2].bitrate_kbps, 2000);
    }

    #[test]
    fn test_sort_by_name() {
        let database = Database::open_in_memory().expect("Failed to open database");
        let info = create_test_video_info();

        database
            .upsert_pending_file(
                &PathBuf::from("/test/charlie.mp4"),
                "mp4",
                &info,
                PendingAction::Convert,
            )
            .expect("Failed to insert");
        database
            .upsert_pending_file(&PathBuf::from("/test/alpha.mp4"), "mp4", &info, PendingAction::Convert)
            .expect("Failed to insert");
        database
            .upsert_pending_file(&PathBuf::from("/test/bravo.mp4"), "mp4", &info, PendingAction::Convert)
            .expect("Failed to insert");

        // Sort by name ascending (alphabetical)
        let sorted = database
            .get_pending_files(&PendingFileFilter {
                sort: Some(SortOrder::Name),
                ..Default::default()
            })
            .expect("Failed to get files");
        assert_eq!(sorted.len(), 3);
        assert!(sorted[0].full_path.to_string_lossy().contains("alpha"));
        assert!(sorted[1].full_path.to_string_lossy().contains("bravo"));
        assert!(sorted[2].full_path.to_string_lossy().contains("charlie"));
    }

    #[test]
    fn test_sort_by_impact() {
        let database = Database::open_in_memory().expect("Failed to open database");

        // Low impact: low bitrate, short duration, high fps
        // Impact = (2000 / 60) * 600 = 20,000
        let info_low = VideoInfo {
            codec: "h264".to_string(),
            bitrate_kbps: 2000,
            size_bytes: 150_000_000,
            duration: 600.0,
            width: 1920,
            height: 1080,
            frames_per_second: 60.0,
            bit_depth: 8,
            warning: None,
        };

        // Medium impact: medium bitrate, medium duration, normal fps
        // Impact = (8000 / 30) * 1800 = 480,000
        let info_mid = VideoInfo {
            codec: "h264".to_string(),
            bitrate_kbps: 8000,
            size_bytes: 1_800_000_000,
            duration: 1800.0,
            width: 1920,
            height: 1080,
            frames_per_second: 30.0,
            bit_depth: 8,
            warning: None,
        };

        // High impact: high bitrate, long duration, low fps
        // Impact = (15000 / 24) * 7200 = 4,500,000
        let info_high = VideoInfo {
            codec: "h264".to_string(),
            bitrate_kbps: 15000,
            size_bytes: 13_500_000_000,
            duration: 7200.0,
            width: 1920,
            height: 1080,
            frames_per_second: 24.0,
            bit_depth: 8,
            warning: None,
        };

        database
            .upsert_pending_file(
                &PathBuf::from("/test/low_impact.mp4"),
                "mp4",
                &info_low,
                PendingAction::Convert,
            )
            .expect("Failed to insert");
        database
            .upsert_pending_file(
                &PathBuf::from("/test/mid_impact.mp4"),
                "mp4",
                &info_mid,
                PendingAction::Convert,
            )
            .expect("Failed to insert");
        database
            .upsert_pending_file(
                &PathBuf::from("/test/high_impact.mp4"),
                "mp4",
                &info_high,
                PendingAction::Convert,
            )
            .expect("Failed to insert");

        // Sort by impact descending (highest potential savings first)
        let sorted = database
            .get_pending_files(&PendingFileFilter {
                sort: Some(SortOrder::Impact),
                ..Default::default()
            })
            .expect("Failed to get files");
        assert_eq!(sorted.len(), 3);
        assert!(sorted[0].full_path.to_string_lossy().contains("high_impact"));
        assert!(sorted[1].full_path.to_string_lossy().contains("mid_impact"));
        assert!(sorted[2].full_path.to_string_lossy().contains("low_impact"));
    }

    #[test]
    fn test_bitrate_filter_does_not_exclude_remux_files() {
        let database = Database::open_in_memory().expect("Failed to open database");

        let info_low = VideoInfo {
            codec: "hevc".to_string(),
            bitrate_kbps: 2000,
            size_bytes: 500_000_000,
            duration: 3600.0,
            width: 1920,
            height: 1080,
            frames_per_second: 24.0,
            bit_depth: 8,
            warning: None,
        };

        let info_high = VideoInfo {
            codec: "h264".to_string(),
            bitrate_kbps: 15000,
            size_bytes: 2_000_000_000,
            duration: 3600.0,
            width: 1920,
            height: 1080,
            frames_per_second: 24.0,
            bit_depth: 8,
            warning: None,
        };

        // Low bitrate remux file
        database
            .upsert_pending_file(
                &PathBuf::from("/test/remux_low.mkv"),
                "mkv",
                &info_low,
                PendingAction::Remux,
            )
            .expect("Failed to insert");

        // Low bitrate convert file
        database
            .upsert_pending_file(
                &PathBuf::from("/test/convert_low.mp4"),
                "mp4",
                &info_low,
                PendingAction::Convert,
            )
            .expect("Failed to insert");

        // High bitrate convert file
        database
            .upsert_pending_file(
                &PathBuf::from("/test/convert_high.mp4"),
                "mp4",
                &info_high,
                PendingAction::Convert,
            )
            .expect("Failed to insert");

        // Filter with min_bitrate above the low bitrate value
        // Should exclude the low bitrate convert file but keep the low bitrate remux file
        let filtered = database
            .get_pending_files(&PendingFileFilter {
                min_bitrate: Some(MinimumBitrate::Fixed(10000)),
                ..Default::default()
            })
            .expect("Failed to get files");
        assert_eq!(filtered.len(), 2);
        assert!(
            filtered
                .iter()
                .any(|f| f.full_path.to_string_lossy().contains("remux_low"))
        );
        assert!(
            filtered
                .iter()
                .any(|f| f.full_path.to_string_lossy().contains("convert_high"))
        );

        // Filter with max_bitrate below the high bitrate value
        // Should exclude the high bitrate convert file but keep the low bitrate remux file
        let filtered = database
            .get_pending_files(&PendingFileFilter {
                max_bitrate: Some(5000),
                ..Default::default()
            })
            .expect("Failed to get files");
        assert_eq!(filtered.len(), 2);
        assert!(
            filtered
                .iter()
                .any(|f| f.full_path.to_string_lossy().contains("remux_low"))
        );
        assert!(
            filtered
                .iter()
                .any(|f| f.full_path.to_string_lossy().contains("convert_low"))
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn test_duration_filter_does_not_exclude_remux_files() {
        let database = Database::open_in_memory().expect("Failed to open database");

        let info_short = VideoInfo {
            codec: "hevc".to_string(),
            bitrate_kbps: 8000,
            size_bytes: 500_000_000,
            duration: 30.0,
            width: 1920,
            height: 1080,
            frames_per_second: 24.0,
            bit_depth: 8,
            warning: None,
        };

        let info_long = VideoInfo {
            codec: "h264".to_string(),
            bitrate_kbps: 8000,
            size_bytes: 2_000_000_000,
            duration: 7200.0,
            width: 1920,
            height: 1080,
            frames_per_second: 24.0,
            bit_depth: 8,
            warning: None,
        };

        let info_mid = VideoInfo {
            codec: "h264".to_string(),
            bitrate_kbps: 8000,
            size_bytes: 1_000_000_000,
            duration: 3600.0,
            width: 1920,
            height: 1080,
            frames_per_second: 24.0,
            bit_depth: 8,
            warning: None,
        };

        // Short duration remux file
        database
            .upsert_pending_file(
                &PathBuf::from("/test/remux_short.mkv"),
                "mkv",
                &info_short,
                PendingAction::Remux,
            )
            .expect("Failed to insert");

        // Short duration convert file
        database
            .upsert_pending_file(
                &PathBuf::from("/test/convert_short.mp4"),
                "mp4",
                &info_short,
                PendingAction::Convert,
            )
            .expect("Failed to insert");

        // Mid duration convert file
        database
            .upsert_pending_file(
                &PathBuf::from("/test/convert_mid.mp4"),
                "mp4",
                &info_mid,
                PendingAction::Convert,
            )
            .expect("Failed to insert");

        // Long duration convert file
        database
            .upsert_pending_file(
                &PathBuf::from("/test/convert_long.mp4"),
                "mp4",
                &info_long,
                PendingAction::Convert,
            )
            .expect("Failed to insert");

        // Filter with min_duration above the short duration
        // Should exclude the short convert file but keep the short remux file
        let filtered = database
            .get_pending_files(&PendingFileFilter {
                min_duration: Some(60.0),
                ..Default::default()
            })
            .expect("Failed to get files");
        assert_eq!(filtered.len(), 3);
        assert!(
            filtered
                .iter()
                .any(|f| f.full_path.to_string_lossy().contains("remux_short"))
        );
        assert!(
            filtered
                .iter()
                .any(|f| f.full_path.to_string_lossy().contains("convert_mid"))
        );
        assert!(
            filtered
                .iter()
                .any(|f| f.full_path.to_string_lossy().contains("convert_long"))
        );

        // Filter with max_duration below the long duration
        // Should exclude the long convert file but keep the short remux file
        let filtered = database
            .get_pending_files(&PendingFileFilter {
                max_duration: Some(5000.0),
                ..Default::default()
            })
            .expect("Failed to get files");
        assert_eq!(filtered.len(), 3);
        assert!(
            filtered
                .iter()
                .any(|f| f.full_path.to_string_lossy().contains("remux_short"))
        );
        assert!(
            filtered
                .iter()
                .any(|f| f.full_path.to_string_lossy().contains("convert_short"))
        );
        assert!(
            filtered
                .iter()
                .any(|f| f.full_path.to_string_lossy().contains("convert_mid"))
        );
    }

    #[test]
    fn test_upsert_and_find_scanned_file() {
        let database = Database::open_in_memory().expect("Failed to open in-memory database");
        let info = create_test_video_info();
        let path = PathBuf::from("/videos/test.mkv");

        database
            .upsert_scanned_file(&path, &info)
            .expect("Failed to upsert scanned file");

        let found = database
            .find_scanned_file(&path, info.size_bytes)
            .expect("Failed to query scanned file");

        let found = found.expect("Expected a cached result");
        assert_eq!(found.codec, info.codec);
        assert_eq!(found.bitrate_kbps, info.bitrate_kbps);
        assert_eq!(found.size_bytes, info.size_bytes);
        cli_tools_common::assert_f64_eq(found.duration, info.duration);
        assert_eq!(found.width, info.width);
        assert_eq!(found.height, info.height);
        cli_tools_common::assert_f64_eq(found.frames_per_second, info.frames_per_second);
        assert!(found.warning.is_none());
    }

    #[test]
    fn test_find_scanned_file_returns_none_for_unknown_path() {
        let database = Database::open_in_memory().expect("Failed to open in-memory database");

        let result = database
            .find_scanned_file(&PathBuf::from("/videos/unknown.mkv"), 1_000_000)
            .expect("Failed to query scanned file");

        assert!(result.is_none());
    }

    #[test]
    fn test_find_scanned_file_returns_none_when_size_differs() {
        let database = Database::open_in_memory().expect("Failed to open in-memory database");
        let info = create_test_video_info();
        let path = PathBuf::from("/videos/test.mkv");

        database
            .upsert_scanned_file(&path, &info)
            .expect("Failed to upsert scanned file");

        // Query with a different size. Should not match.
        let result = database
            .find_scanned_file(&path, info.size_bytes + 1)
            .expect("Failed to query scanned file");

        assert!(result.is_none());
    }

    #[test]
    fn test_upsert_scanned_file_updates_existing_entry() {
        let database = Database::open_in_memory().expect("Failed to open in-memory database");
        let path = PathBuf::from("/videos/test.mkv");

        let original = create_test_video_info();
        database
            .upsert_scanned_file(&path, &original)
            .expect("Failed to upsert original scanned file");

        // Upsert an updated entry with a different codec and size.
        let updated = VideoInfo {
            codec: "hevc".to_string(),
            bitrate_kbps: 4000,
            size_bytes: 500_000_000,
            duration: 1800.0,
            width: 1280,
            height: 720,
            frames_per_second: 30.0,
            bit_depth: 8,
            warning: None,
        };
        database
            .upsert_scanned_file(&path, &updated)
            .expect("Failed to upsert updated scanned file");

        let found = database
            .find_scanned_file(&path, updated.size_bytes)
            .expect("Failed to query scanned file")
            .expect("Expected a cached result after update");

        assert_eq!(found.codec, "hevc");
        assert_eq!(found.bitrate_kbps, 4000);
        assert_eq!(found.size_bytes, 500_000_000);

        // The original size no longer matches.
        let stale = database
            .find_scanned_file(&path, original.size_bytes)
            .expect("Failed to query scanned file");
        assert!(stale.is_none());
    }

    #[test]
    fn test_get_all_scanned_files_returns_all_entries() {
        let database = Database::open_in_memory().expect("Failed to open in-memory database");

        let info_a = create_test_video_info();
        let path_a = PathBuf::from("/videos/a.mkv");
        database
            .upsert_scanned_file(&path_a, &info_a)
            .expect("Failed to upsert scanned file");

        let info_b = VideoInfo {
            codec: "hevc".to_string(),
            bitrate_kbps: 4000,
            size_bytes: 500_000_000,
            duration: 1800.0,
            width: 1280,
            height: 720,
            frames_per_second: 30.0,
            bit_depth: 8,
            warning: None,
        };
        let path_b = PathBuf::from("/videos/b.mp4");
        database
            .upsert_scanned_file(&path_b, &info_b)
            .expect("Failed to upsert scanned file");

        let all = database
            .get_all_scanned_files()
            .expect("Failed to get all scanned files");

        assert_eq!(all.len(), 2);

        let found_a = all.get("/videos/a.mkv").expect("Expected entry for a.mkv");
        assert_eq!(found_a.codec, info_a.codec);
        assert_eq!(found_a.size_bytes, info_a.size_bytes);
        cli_tools_common::assert_f64_eq(found_a.frames_per_second, info_a.frames_per_second);

        let found_b = all.get("/videos/b.mp4").expect("Expected entry for b.mp4");
        assert_eq!(found_b.codec, "hevc");
        assert_eq!(found_b.size_bytes, 500_000_000);
        cli_tools_common::assert_f64_eq(found_b.frames_per_second, 30.0);
    }

    #[test]
    fn test_get_all_scanned_files_returns_empty_map_when_no_entries() {
        let database = Database::open_in_memory().expect("Failed to open in-memory database");

        let all = database
            .get_all_scanned_files()
            .expect("Failed to get all scanned files");

        assert!(all.is_empty());
    }

    #[test]
    fn test_batch_upsert_scanned_files_writes_all_entries() {
        let mut database = Database::open_in_memory().expect("Failed to open in-memory database");

        let info_a = create_test_video_info();
        let path_a = PathBuf::from("/videos/a.mkv");

        let info_b = VideoInfo {
            codec: "hevc".to_string(),
            bitrate_kbps: 4000,
            size_bytes: 500_000_000,
            duration: 1800.0,
            width: 1280,
            height: 720,
            frames_per_second: 30.0,
            bit_depth: 8,
            warning: None,
        };
        let path_b = PathBuf::from("/videos/b.mp4");

        let entries: Vec<(&Path, &VideoInfo)> = vec![(path_a.as_path(), &info_a), (path_b.as_path(), &info_b)];

        let count = database
            .batch_upsert_scanned_files(&entries)
            .expect("Failed to batch upsert scanned files");

        assert_eq!(count, 2);

        let all = database
            .get_all_scanned_files()
            .expect("Failed to get all scanned files");

        assert_eq!(all.len(), 2);

        let found_a = all.get("/videos/a.mkv").expect("Expected entry for a.mkv");
        assert_eq!(found_a.codec, info_a.codec);
        assert_eq!(found_a.size_bytes, info_a.size_bytes);

        let found_b = all.get("/videos/b.mp4").expect("Expected entry for b.mp4");
        assert_eq!(found_b.codec, "hevc");
        assert_eq!(found_b.size_bytes, 500_000_000);
    }

    #[test]
    fn test_batch_upsert_scanned_files_returns_zero_for_empty_input() {
        let mut database = Database::open_in_memory().expect("Failed to open in-memory database");

        let count = database
            .batch_upsert_scanned_files(&[])
            .expect("Failed to batch upsert scanned files");

        assert_eq!(count, 0);
    }

    #[test]
    fn test_batch_upsert_scanned_files_updates_existing_entries() {
        let mut database = Database::open_in_memory().expect("Failed to open in-memory database");

        let original = create_test_video_info();
        let path = PathBuf::from("/videos/test.mkv");

        database
            .upsert_scanned_file(&path, &original)
            .expect("Failed to upsert original");

        let updated = VideoInfo {
            codec: "hevc".to_string(),
            bitrate_kbps: 4000,
            size_bytes: 500_000_000,
            duration: 1800.0,
            width: 1280,
            height: 720,
            frames_per_second: 30.0,
            bit_depth: 8,
            warning: None,
        };

        let entries: Vec<(&Path, &VideoInfo)> = vec![(path.as_path(), &updated)];

        let count = database
            .batch_upsert_scanned_files(&entries)
            .expect("Failed to batch upsert");

        assert_eq!(count, 1);

        let found = database
            .find_scanned_file(&path, updated.size_bytes)
            .expect("Failed to query scanned file")
            .expect("Expected a cached result after batch update");

        assert_eq!(found.codec, "hevc");
        assert_eq!(found.bitrate_kbps, 4000);
        assert_eq!(found.size_bytes, 500_000_000);
    }

    #[test]
    fn test_batch_upsert_pending_files_writes_all_entries() {
        let mut database = Database::open_in_memory().expect("Failed to open in-memory database");
        let info = create_test_video_info();

        let path_a = PathBuf::from("/test/a.mp4");
        let path_b = PathBuf::from("/test/b.mkv");

        let entries: Vec<(&Path, &str, &VideoInfo, PendingAction)> = vec![
            (path_a.as_path(), "mp4", &info, PendingAction::Convert),
            (path_b.as_path(), "mkv", &info, PendingAction::Remux),
        ];

        let count = database
            .batch_upsert_pending_files(&entries)
            .expect("Failed to batch upsert pending files");

        assert_eq!(count, 2);

        let files = database
            .get_pending_files(&PendingFileFilter::default())
            .expect("Failed to get pending files");

        assert_eq!(files.len(), 2);
        assert!(files.iter().any(|f| f.full_path.to_string_lossy().contains("a.mp4")));
        assert!(files.iter().any(|f| f.full_path.to_string_lossy().contains("b.mkv")));
    }

    #[test]
    fn test_batch_upsert_pending_files_returns_zero_for_empty_input() {
        let mut database = Database::open_in_memory().expect("Failed to open in-memory database");

        let count = database
            .batch_upsert_pending_files(&[])
            .expect("Failed to batch upsert pending files");

        assert_eq!(count, 0);
    }

    #[test]
    fn test_batch_upsert_pending_files_updates_existing_entries() {
        let mut database = Database::open_in_memory().expect("Failed to open in-memory database");
        let info = create_test_video_info();
        let path = PathBuf::from("/test/video.mp4");

        database
            .upsert_pending_file(&path, "mp4", &info, PendingAction::Convert)
            .expect("Failed to upsert original");

        // Batch update changing action to Remux
        let entries: Vec<(&Path, &str, &VideoInfo, PendingAction)> =
            vec![(path.as_path(), "mp4", &info, PendingAction::Remux)];

        let count = database
            .batch_upsert_pending_files(&entries)
            .expect("Failed to batch upsert");

        assert_eq!(count, 1);

        let files = database
            .get_pending_files(&PendingFileFilter::default())
            .expect("Failed to get pending files");

        assert_eq!(files.len(), 1);
        assert_eq!(files[0].action, PendingAction::Remux);
    }

    #[test]
    fn test_scanned_file_count_empty() {
        let database = Database::open_in_memory().expect("Failed to open in-memory database");
        let count = database.scanned_file_count().expect("Failed to get count");
        assert_eq!(count, 0);
    }

    #[test]
    fn test_scanned_file_count_after_inserts() {
        let database = Database::open_in_memory().expect("Failed to open in-memory database");
        let info = create_test_video_info();

        database
            .upsert_scanned_file(&PathBuf::from("/videos/a.mkv"), &info)
            .expect("Failed to upsert");
        database
            .upsert_scanned_file(&PathBuf::from("/videos/b.mkv"), &info)
            .expect("Failed to upsert");

        let count = database.scanned_file_count().expect("Failed to get count");
        assert_eq!(count, 2);
    }

    #[test]
    fn test_remove_missing_scanned_files_removes_nonexistent() {
        let mut database = Database::open_in_memory().expect("Failed to open in-memory database");
        let info = create_test_video_info();

        // Insert paths that definitely do not exist on disk.
        database
            .upsert_scanned_file(&PathBuf::from("/nonexistent/aaa.mkv"), &info)
            .expect("Failed to upsert");
        database
            .upsert_scanned_file(&PathBuf::from("/nonexistent/bbb.mkv"), &info)
            .expect("Failed to upsert");

        let removed = database
            .remove_missing_scanned_files()
            .expect("Failed to remove missing");

        assert_eq!(removed, 2);

        let count = database.scanned_file_count().expect("Failed to get count");
        assert_eq!(count, 0);
    }

    #[test]
    fn test_remove_missing_scanned_files_keeps_existing() {
        let mut database = Database::open_in_memory().expect("Failed to open in-memory database");
        let info = create_test_video_info();

        // Use a path that actually exists (the database module source file itself).
        let existing_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/database.rs");
        assert!(existing_path.exists(), "Test prerequisite: source file must exist");

        database
            .upsert_scanned_file(&existing_path, &info)
            .expect("Failed to upsert existing");
        database
            .upsert_scanned_file(&PathBuf::from("/nonexistent/gone.mkv"), &info)
            .expect("Failed to upsert missing");

        let removed = database
            .remove_missing_scanned_files()
            .expect("Failed to remove missing");

        assert_eq!(removed, 1);

        let count = database.scanned_file_count().expect("Failed to get count");
        assert_eq!(count, 1);
    }

    #[test]
    fn test_remove_missing_scanned_files_returns_zero_when_empty() {
        let mut database = Database::open_in_memory().expect("Failed to open in-memory database");

        let removed = database
            .remove_missing_scanned_files()
            .expect("Failed to remove missing");

        assert_eq!(removed, 0);
    }
}

#[cfg(test)]
mod test_tiered_bitrate_filter {
    use super::*;

    fn video_info(width: u32, height: u32, frames_per_second: f64, bitrate_kbps: u64) -> VideoInfo {
        VideoInfo {
            codec: "h264".to_string(),
            bitrate_kbps,
            size_bytes: 1_000_000_000,
            duration: 3600.0,
            width,
            height,
            frames_per_second,
            bit_depth: 8,
            warning: None,
        }
    }

    #[test]
    fn sql_filter_matches_rust_thresholds() {
        let database = Database::open_in_memory().expect("Failed to open in-memory database");
        let min_bitrate = MinimumBitrate::default();
        let dimensions = [
            (640, 480),
            (1280, 720),
            (720, 1280),
            (1600, 900),
            (1602, 901),
            (1440, 1080),
            (1920, 800),
            (1920, 1080),
            (2240, 1260),
            (2242, 1261),
            (2560, 1440),
            (3840, 2160),
        ];
        let frame_rates = [23.976, 30.0, 50.0, 59.94];
        let bitrates = [3500, 5000, 7000, 9000, 13000, 20000, 30000];

        let mut expected = Vec::new();
        for (width, height) in dimensions {
            for frames_per_second in frame_rates {
                for bitrate in bitrates {
                    let path = PathBuf::from(format!("/test/{width}x{height}_{frames_per_second}_{bitrate}.mp4"));
                    let info = video_info(width, height, frames_per_second, bitrate);
                    database
                        .upsert_pending_file(&path, "mp4", &info, PendingAction::Convert)
                        .expect("Failed to insert");
                    if bitrate >= min_bitrate.threshold(width, height, frames_per_second) {
                        expected.push(path);
                    }
                }
            }
        }

        let mut filtered: Vec<PathBuf> = database
            .get_pending_files(&PendingFileFilter {
                min_bitrate: Some(min_bitrate),
                ..Default::default()
            })
            .expect("Failed to get files")
            .into_iter()
            .map(|file| file.full_path)
            .collect();
        filtered.sort();
        expected.sort();

        assert_eq!(filtered, expected);
    }

    #[test]
    fn tiered_filter_does_not_apply_to_remux() {
        let database = Database::open_in_memory().expect("Failed to open in-memory database");
        database
            .upsert_pending_file(
                &PathBuf::from("/test/remux.mkv"),
                "mkv",
                &video_info(3840, 2160, 60.0, 1000),
                PendingAction::Remux,
            )
            .expect("Failed to insert");

        let filtered = database
            .get_pending_files(&PendingFileFilter {
                min_bitrate: Some(MinimumBitrate::default()),
                ..Default::default()
            })
            .expect("Failed to get files");

        assert_eq!(filtered.len(), 1);
    }
}

#[cfg(test)]
mod test_schema_updates {
    use super::*;

    #[test]
    fn add_column_if_missing_adds_column_only_once() {
        let mut connection = sqlite::open_in_memory().expect("Failed to open in-memory database");
        connection
            .execute("CREATE TABLE legacy_files (id INTEGER PRIMARY KEY)", [])
            .expect("Failed to create legacy table");

        let tx = sqlite::immediate_transaction(&mut connection).expect("begin tx");
        sqlite::add_column_if_missing(&tx, "legacy_files", "bit_depth", "INTEGER NOT NULL DEFAULT 8")
            .expect("Failed to add missing column");
        sqlite::add_column_if_missing(&tx, "legacy_files", "bit_depth", "INTEGER NOT NULL DEFAULT 8")
            .expect("Existing column should be left unchanged");
        tx.commit().expect("commit tx");

        let mut statement = connection
            .prepare("PRAGMA table_info(legacy_files)")
            .expect("Failed to inspect legacy table");
        let bit_depth_columns = statement
            .query_map([], |row| row.get::<_, String>(1))
            .expect("Failed to query legacy columns")
            .filter_map(std::result::Result::ok)
            .filter(|name| name == "bit_depth")
            .count();

        assert_eq!(bit_depth_columns, 1);
    }

    #[test]
    fn migration_v2_removes_entries_with_invalid_frame_rates() {
        let mut database = Database::open_in_memory().expect("Failed to open in-memory database");
        let invalid_info = VideoInfo {
            codec: "h264".to_string(),
            bitrate_kbps: 8_000,
            size_bytes: 1_000,
            duration: 60.0,
            width: 1920,
            height: 1080,
            frames_per_second: 0.5,
            bit_depth: 8,
            warning: None,
        };
        let pending_path = PathBuf::from("invalid-pending.mkv");
        let scanned_path = PathBuf::from("invalid-scanned.mkv");
        database
            .upsert_pending_file(&pending_path, "mkv", &invalid_info, PendingAction::Convert)
            .expect("Failed to insert invalid pending entry");
        database
            .upsert_scanned_file(&scanned_path, &invalid_info)
            .expect("Failed to insert invalid scanned entry");

        // Reset migration versions to 1 to exercise the v1 -> v2 cleanup migration.
        database
            .connection
            .execute("UPDATE schema_migrations SET version = 1", [])
            .expect("reset schema version");
        database.initialize().expect("Failed to reinitialize database");

        assert!(
            database
                .get_pending_files(&PendingFileFilter::default())
                .expect("Failed to query pending entries")
                .is_empty()
        );
        assert_eq!(
            database.scanned_file_count().expect("Failed to count scanned entries"),
            0
        );
    }
}

#[cfg(test)]
mod test_pending_file_lookup_and_removal {
    use super::*;

    fn video_info() -> VideoInfo {
        VideoInfo {
            codec: "h264".to_string(),
            bitrate_kbps: 8_000,
            size_bytes: 1_048_576,
            duration: 90.0,
            width: 1920,
            height: 1080,
            frames_per_second: 24.0,
            bit_depth: 10,
            warning: None,
        }
    }

    #[test]
    fn gets_pending_file_by_path_and_returns_none_for_unknown_path() {
        let database = Database::open_in_memory().expect("Failed to open in-memory database");
        let path = PathBuf::from("known-video.mkv");
        database
            .upsert_pending_file(&path, "MKV", &video_info(), PendingAction::SubtitleMux)
            .expect("Failed to insert pending file");

        let pending = database
            .get_pending_file(&path)
            .expect("Failed to query pending file")
            .expect("Expected pending file");

        assert_eq!(pending.full_path, path);
        assert_eq!(pending.extension, "mkv");
        assert_eq!(pending.bit_depth, 10);
        assert_eq!(pending.action, PendingAction::SubtitleMux);
        assert!(
            database
                .get_pending_file(Path::new("unknown-video.mkv"))
                .expect("Failed to query unknown path")
                .is_none()
        );
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_paths_do_not_match_a_literal_replacement_character_queue_key() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let directory = tempfile::tempdir().unwrap();
        let raw = directory.path().join(OsStr::from_bytes(b"clip_\xff.mkv"));
        let unicode = directory.path().join("clip_�.mkv");
        let database = Database::open_in_memory().unwrap();
        database
            .upsert_pending_file(&unicode, "mkv", &video_info(), PendingAction::Convert)
            .unwrap();

        assert!(!database.remove_pending_file(&raw).unwrap());
        assert!(database.get_pending_file(&unicode).unwrap().is_some());
        assert!(database.get_pending_file(&raw).unwrap().is_none());
    }

    #[test]
    fn removes_pending_file_by_id_and_reports_missing_entries() {
        let database = Database::open_in_memory().expect("Failed to open in-memory database");
        let path = PathBuf::from("remove-by-id.mkv");
        database
            .upsert_pending_file(&path, "mkv", &video_info(), PendingAction::Convert)
            .expect("Failed to insert pending file");
        let pending = database
            .get_pending_file(&path)
            .expect("Failed to query pending file")
            .expect("Expected pending file");

        assert!(
            database
                .remove_pending_file_by_id(pending.id)
                .expect("Failed to remove pending file by ID")
        );
        assert!(
            !database
                .remove_pending_file_by_id(pending.id)
                .expect("Repeated removal should not fail")
        );
        assert!(
            !database
                .remove_pending_file(Path::new("unknown-video.mkv"))
                .expect("Unknown path removal should not fail")
        );
    }

    #[test]
    fn remove_missing_files_keeps_existing_paths() {
        let temporary_directory = tempfile::tempdir().expect("Failed to create temporary directory");
        let existing_path = temporary_directory.path().join("existing.mkv");
        let missing_path = temporary_directory.path().join("missing.mkv");
        std::fs::write(&existing_path, b"video").expect("Failed to create existing test file");
        let mut database = Database::open_in_memory().expect("Failed to open in-memory database");
        let info = video_info();
        database
            .upsert_pending_file(&existing_path, "mkv", &info, PendingAction::Convert)
            .expect("Failed to insert existing path");
        database
            .upsert_pending_file(&missing_path, "mkv", &info, PendingAction::Convert)
            .expect("Failed to insert missing path");

        assert_eq!(
            database.remove_missing_files().expect("Failed to remove missing files"),
            1
        );
        assert_eq!(
            database
                .remove_missing_files()
                .expect("Failed to recheck pending files"),
            0
        );

        let remaining = database
            .get_pending_files(&PendingFileFilter::default())
            .expect("Failed to query remaining files");
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].full_path, existing_path);
        assert!(remaining[0].modified_time.is_some());
    }

    /// Move the file modification time one hour into the past.
    fn backdate_modified_time(path: &Path) {
        let file = std::fs::File::options()
            .write(true)
            .open(path)
            .expect("Failed to open test file");
        let modified = file
            .metadata()
            .and_then(|metadata| metadata.modified())
            .expect("Failed to read modification time");
        file.set_modified(modified - std::time::Duration::from_secs(3600))
            .expect("Failed to set modification time");
    }

    #[test]
    fn remove_changed_files_drops_entries_modified_after_scan() {
        let temporary_directory = tempfile::tempdir().expect("Failed to create temporary directory");
        let unchanged_path = temporary_directory.path().join("unchanged.mkv");
        let changed_path = temporary_directory.path().join("changed.mkv");
        std::fs::write(&unchanged_path, b"video").expect("Failed to create unchanged test file");
        std::fs::write(&changed_path, b"video").expect("Failed to create changed test file");
        let mut database = Database::open_in_memory().expect("Failed to open in-memory database");
        let info = video_info();
        database
            .upsert_pending_file(&unchanged_path, "mkv", &info, PendingAction::Convert)
            .expect("Failed to insert unchanged path");
        database
            .upsert_pending_file(&changed_path, "mkv", &info, PendingAction::Convert)
            .expect("Failed to insert changed path");

        backdate_modified_time(&changed_path);

        assert_eq!(
            database.remove_changed_files().expect("Failed to remove changed files"),
            1
        );
        let remaining = database
            .get_pending_files(&PendingFileFilter::default())
            .expect("Failed to query remaining files");
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].full_path, unchanged_path);
    }

    #[test]
    fn pre_epoch_pending_rows_are_checked_for_signed_time_and_disk_size() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("older.mkv");
        let file = std::fs::File::create(&path).unwrap();
        let first_time = SystemTime::UNIX_EPOCH - std::time::Duration::from_secs(60);
        file.set_modified(first_time).unwrap();
        let database = Database::open_in_memory().unwrap();
        database
            .upsert_pending_file(&path, "mkv", &video_info(), PendingAction::Convert)
            .unwrap();
        let pending = database.get_pending_file(&path).unwrap().unwrap();
        assert!(pending.modified_time.is_none());
        assert!(pending.modified_time_ns < 0);
        assert!(!pending.is_changed_on_disk());

        file.set_modified(first_time - std::time::Duration::from_secs(1))
            .unwrap();
        assert!(pending.is_changed_on_disk(), "the signed modification time changed");

        file.set_len(16).unwrap();
        file.set_modified(first_time).unwrap();
        assert!(pending.is_changed_on_disk(), "the size changed at the stored time");
    }

    #[test]
    fn entries_without_stored_or_readable_time_count_as_unchanged() {
        let temporary_directory = tempfile::tempdir().expect("Failed to create temporary directory");
        let existing_path = temporary_directory.path().join("existing.mkv");
        std::fs::write(&existing_path, b"video").expect("Failed to create test file");
        let database = Database::open_in_memory().expect("Failed to open in-memory database");
        database
            .upsert_pending_file(&existing_path, "mkv", &video_info(), PendingAction::Remux)
            .expect("Failed to insert pending file");
        let mut pending = database
            .get_pending_file(&existing_path)
            .expect("Failed to query pending file")
            .expect("Expected pending file");

        assert!(!pending.is_changed_on_disk());

        pending.modified_time = None;
        pending.modified_time_ns = 0;
        backdate_modified_time(&existing_path);
        assert!(!pending.is_changed_on_disk());

        pending.modified_time = Some(0);
        pending.full_path = temporary_directory.path().join("missing.mkv");
        assert!(!pending.is_changed_on_disk());
    }
}

#[cfg(test)]
mod test_database_statistics_display {
    use super::*;

    fn video_info() -> VideoInfo {
        VideoInfo {
            codec: "h264".to_string(),
            bitrate_kbps: 8_000,
            size_bytes: 1_048_576,
            duration: 90.0,
            width: 1920,
            height: 1080,
            frames_per_second: 24.0,
            bit_depth: 8,
            warning: None,
        }
    }

    #[test]
    fn statistics_count_subtitle_mux_and_ignore_unknown_actions() {
        let database = Database::open_in_memory().expect("Failed to open in-memory database");
        let info = video_info();
        for (name, action) in [
            ("convert.mkv", PendingAction::Convert),
            ("remux.mkv", PendingAction::Remux),
            ("subtitles.mkv", PendingAction::SubtitleMux),
            ("unknown.mkv", PendingAction::Convert),
        ] {
            database
                .upsert_pending_file(Path::new(name), "mkv", &info, action)
                .expect("Failed to insert pending file");
        }
        database
            .connection
            .execute(
                "UPDATE pending_files SET action = 'future_action' WHERE full_path = 'unknown.mkv'",
                [],
            )
            .expect("Failed to set unknown action");

        let stats = database.get_stats().expect("Failed to query database statistics");

        assert_eq!(stats.total_files, 4);
        assert_eq!(stats.convert_count, 1);
        assert_eq!(stats.remux_count, 1);
        assert_eq!(stats.subtitle_mux_count, 1);
        assert_eq!(stats.total_size, 4_194_304);
    }

    #[test]
    fn database_stats_display_includes_all_totals() {
        let stats = DatabaseStats {
            total_files: 6,
            convert_count: 3,
            remux_count: 2,
            subtitle_mux_count: 1,
            total_size: 1_048_576,
        };

        assert_eq!(
            stats.to_string(),
            "Database Statistics:\n  Total files:    6\n  To convert:     3\n  To remux:       2\n  To subtitle mux: 1\n  Total size:     1.00 MB"
        );
    }

    #[test]
    fn extension_stats_display_formats_extension_count_and_size() {
        let stats = ExtensionStats {
            extension: "mkv".to_string(),
            count: 2,
            total_size: 1_048_576,
        };
        let displayed = stats.to_string();

        assert!(displayed.starts_with(".mkv"));
        assert!(displayed.contains("2 files"));
        assert!(displayed.ends_with("1.00 MB"));
    }

    #[test]
    fn pending_file_video_info_uses_minimum_bit_depth() {
        let pending = PendingFile {
            id: 1,
            full_path: PathBuf::from("legacy.mkv"),
            extension: "mkv".to_string(),
            codec: "h264".to_string(),
            bitrate_kbps: 8_000,
            size_bytes: 1_048_576,
            duration: 90.0,
            width: 1920,
            height: 1080,
            frames_per_second: 24.0,
            bit_depth: 0,
            action: PendingAction::Convert,
            movie_mode: false,
            claimed_by: None,
            claimed_at: None,
            created_time: 0,
            modified_time: None,
            modified_time_ns: 0,
            disk_size_bytes: None,
        };

        assert_eq!(pending.to_video_info().bit_depth, 8);
    }
}

#[cfg(test)]
mod test_unified_db_and_leasing {
    use super::*;

    fn sample_info() -> VideoInfo {
        VideoInfo {
            codec: "h264".to_string(),
            bitrate_kbps: 12_000,
            size_bytes: 2_000_000,
            duration: 120.0,
            width: 1920,
            height: 800,
            frames_per_second: 24.0,
            bit_depth: 8,
            warning: None,
        }
    }

    #[test]
    fn claims_block_other_workers_until_released_or_expired() {
        let mut db = Database::open_in_memory().unwrap();
        let path = PathBuf::from("/videos/movie.mkv");
        db.upsert_pending_file_with_mode(&path, "mkv", &sample_info(), PendingAction::Convert, true)
            .unwrap();

        assert!(db.claim_pending_file(&path, "worker-a", 120).unwrap());
        assert!(!db.claim_pending_file(&path, "worker-b", 120).unwrap());
        assert!(db.claim_pending_file(&path, "worker-a", 120).unwrap());

        db.release_pending_claim(&path, "worker-a").unwrap();
        assert!(db.claim_pending_file(&path, "worker-b", 120).unwrap());

        let row = db.get_pending_file(&path).unwrap().expect("row exists");
        assert!(row.movie_mode);
        assert_eq!(row.claimed_by.as_deref(), Some("worker-b"));
    }

    #[test]
    fn filters_by_path_prefix_globs_and_ultrawide_equivalent_height() {
        let db = Database::open_in_memory().unwrap();
        let movie_a = PathBuf::from("/media/movies/Scope.Movie.mkv");
        let movie_b = PathBuf::from("/media/tv/Episode.mkv");
        let info = sample_info(); // 1920x800 -> equivalent height 1080
        db.upsert_pending_file(&movie_a, "mkv", &info, PendingAction::Convert)
            .unwrap();
        db.upsert_pending_file(&movie_b, "mkv", &info, PendingAction::Convert)
            .unwrap();

        let filter = PendingFileFilter {
            path_prefixes: vec![PathBuf::from("/media/movies")],
            min_resolution: Some(1000),
            include: vec!["*.mkv".to_string()],
            ..PendingFileFilter::default()
        };
        let results = db.get_pending_files(&filter).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].full_path, movie_a);
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
                CREATE TABLE pending_files (
                    id INTEGER PRIMARY KEY,
                    full_path TEXT NOT NULL UNIQUE,
                    extension TEXT NOT NULL,
                    codec TEXT NOT NULL,
                    bitrate_kbps INTEGER NOT NULL,
                    size_bytes INTEGER NOT NULL,
                    duration REAL NOT NULL,
                    width INTEGER NOT NULL,
                    height INTEGER NOT NULL,
                    frames_per_second REAL NOT NULL,
                    bit_depth INTEGER NOT NULL DEFAULT 10,
                    action TEXT NOT NULL,
                    created_time INTEGER NOT NULL,
                    modified_time INTEGER
                );
                INSERT INTO pending_files (
                    full_path, extension, codec, bitrate_kbps, size_bytes, duration, width, height,
                    frames_per_second, bit_depth, action, created_time
                ) VALUES ('/legacy/clip.mkv', 'mkv', 'h264', 9000, 1000, 60.0, 1920, 1080, 24.0, 10, 'convert', 100);
                ",
            )
            .unwrap();
        drop(legacy_conn);

        let db = Database::open_at(&unified_path).unwrap();
        let row = db
            .get_pending_file(Path::new("/legacy/clip.mkv"))
            .unwrap()
            .expect("legacy pending row imported");
        assert_eq!(row.bit_depth, 10);
        assert_eq!(row.action, PendingAction::Convert);
        assert!(legacy_path.is_file());
    }
}

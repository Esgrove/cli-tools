//! `SQLite` database operations for video convert.
//!
//! Stores pending video files that need conversion or remuxing.

#![allow(clippy::cast_possible_wrap)]

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};

use crate::SortOrder;
use crate::convert::VideoInfo;

/// Default database filename.
const DATABASE_FILENAME: &str = "vconvert.db";

/// Database wrapper for pending file operations.
pub struct Database {
    connection: Connection,
}

/// A pending file entry from the database.
#[derive(Debug, Clone)]
pub struct PendingFile {
    /// Database row ID.
    pub id: i64,
    /// Full path to the video file.
    pub full_path: PathBuf,
    /// File extension (e.g., "mp4", "mkv").
    pub extension: String,
    /// Video codec name (e.g., "hevc", "h264").
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
    /// Action to perform: "convert" or "remux".
    pub action: PendingAction,
    /// When the entry was added to the database.
    #[allow(dead_code)]
    pub created_time: i64,
    /// File's modification time (for detecting changes).
    #[allow(dead_code)]
    pub modified_time: Option<i64>,
}

/// Action type for a pending file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingAction {
    /// File needs full conversion to HEVC.
    Convert,
    /// File is already HEVC but needs remuxing to MP4 container.
    Remux,
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

/// Filter options for querying pending files.
#[derive(Debug, Default, Clone)]
pub struct PendingFileFilter {
    /// Filter by action type.
    pub action: Option<PendingAction>,
    /// Filter by file extension(s).
    pub extensions: Vec<String>,
    /// Minimum bitrate in kbps.
    pub min_bitrate: Option<u64>,
    /// Maximum bitrate in kbps.
    pub max_bitrate: Option<u64>,
    /// Minimum duration in seconds.
    pub min_duration: Option<f64>,
    /// Maximum duration in seconds.
    pub max_duration: Option<f64>,
    /// Limit number of results.
    pub limit: Option<usize>,
    /// Sort order for results.
    pub sort: Option<SortOrder>,
}

impl PendingAction {
    /// Convert action to string for database storage.
    const fn as_str(self) -> &'static str {
        match self {
            Self::Convert => "convert",
            Self::Remux => "remux",
        }
    }

    /// Parse action from string.
    fn from_str(value: &str) -> Self {
        match value {
            "remux" => Self::Remux,
            // Default to Convert for unknown values
            _ => Self::Convert,
        }
    }
}

impl std::fmt::Display for PendingAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl Database {
    /// Get the database path.
    ///
    /// Uses the platform-specific local data directory:
    /// - Windows: `%LOCALAPPDATA%\cli-tools\vconvert.db`
    /// - macOS: `~/Library/Application Support/cli-tools/vconvert.db`
    /// - Linux: `~/.local/share/cli-tools/vconvert.db`
    fn database_path() -> PathBuf {
        let data_dir = dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("cli-tools");
        data_dir.join(DATABASE_FILENAME)
    }

    /// Get the database path for display purposes.
    pub fn path() -> PathBuf {
        Self::database_path()
    }

    /// Open or create the database at the default path.
    ///
    /// # Errors
    /// Returns an error if the database cannot be opened or initialized.
    pub fn open_default() -> Result<Self> {
        let path = Self::database_path();

        // Ensure parent directory exists
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create database directory: {}", parent.display()))?;
        }

        let connection =
            Connection::open(&path).with_context(|| format!("Failed to open database: {}", path.display()))?;

        // Set busy timeout for concurrent access (5 seconds)
        // WAL mode allows concurrent reads, but writes need to wait for each other
        connection
            .busy_timeout(std::time::Duration::from_secs(5))
            .context("Failed to set busy timeout")?;

        let database = Self { connection };
        database.initialize()?;

        Ok(database)
    }

    /// Open an in-memory database for testing.
    ///
    /// # Errors
    /// Returns an error if the database cannot be created.
    #[cfg(test)]
    fn open_in_memory() -> Result<Self> {
        let connection = Connection::open_in_memory().context("Failed to open in-memory database")?;

        let database = Self { connection };
        database.initialize()?;

        Ok(database)
    }

    /// Initialize the database schema.
    fn initialize(&self) -> Result<()> {
        self.connection
            .execute_batch(
                r"
                -- Pending video files to process
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
                    action TEXT NOT NULL,
                    created_time INTEGER NOT NULL,
                    modified_time INTEGER
                );

                -- Indexes for fast querying
                CREATE INDEX IF NOT EXISTS idx_pending_action ON pending_files(action);
                CREATE INDEX IF NOT EXISTS idx_pending_extension ON pending_files(extension);
                CREATE INDEX IF NOT EXISTS idx_pending_bitrate ON pending_files(bitrate_kbps);
                CREATE INDEX IF NOT EXISTS idx_pending_duration ON pending_files(duration);

                -- Performance optimizations
                PRAGMA journal_mode = WAL;
                PRAGMA synchronous = NORMAL;
                PRAGMA cache_size = -8000;
                ",
            )
            .context("Failed to initialize database schema")?;

        Ok(())
    }

    /// Insert or update a pending file entry.
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
        let path_str = path.to_string_lossy();
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        let modified_time = std::fs::metadata(path)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64);

        self.connection
            .execute(
                r"
                INSERT INTO pending_files (full_path, extension, codec, bitrate_kbps, size_bytes, duration, width, height, frames_per_second, action, created_time, modified_time)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
                ON CONFLICT(full_path) DO UPDATE SET
                    extension = excluded.extension,
                    codec = excluded.codec,
                    bitrate_kbps = excluded.bitrate_kbps,
                    size_bytes = excluded.size_bytes,
                    duration = excluded.duration,
                    width = excluded.width,
                    height = excluded.height,
                    frames_per_second = excluded.frames_per_second,
                    action = excluded.action,
                    modified_time = excluded.modified_time
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
                    action.as_str(),
                    now,
                    modified_time,
                ],
            )
            .context("Failed to insert pending file")?;

        Ok(self.connection.last_insert_rowid())
    }

    /// Get pending files with optional filtering.
    ///
    /// # Errors
    /// Returns an error if the database operation fails.
    pub fn get_pending_files(&self, filter: &PendingFileFilter) -> Result<Vec<PendingFile>> {
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
            params_vec.push(Box::new(min_bitrate as i64));
            conditions.push(format!("bitrate_kbps >= ?{}", params_vec.len()));
        }

        if let Some(max_bitrate) = filter.max_bitrate {
            params_vec.push(Box::new(max_bitrate as i64));
            conditions.push(format!("bitrate_kbps <= ?{}", params_vec.len()));
        }

        if let Some(min_duration) = filter.min_duration {
            params_vec.push(Box::new(min_duration));
            conditions.push(format!("duration >= ?{}", params_vec.len()));
        }

        if let Some(max_duration) = filter.max_duration {
            params_vec.push(Box::new(max_duration));
            conditions.push(format!("duration <= ?{}", params_vec.len()));
        }

        let where_clause = if conditions.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", conditions.join(" AND "))
        };

        let limit_clause = filter.limit.map(|limit| format!("LIMIT {limit}")).unwrap_or_default();

        let order_clause = filter.sort.map_or_else(
            || "ORDER BY action, size_bytes DESC".to_string(),
            |sort| format!("ORDER BY {}", sort.sql_order_clause()),
        );

        let sql = format!(
            r"
            SELECT id, full_path, extension, codec, bitrate_kbps, size_bytes, duration, width, height, frames_per_second, action, created_time, modified_time
            FROM pending_files
            {where_clause}
            {order_clause}
            {limit_clause}
            "
        );

        let mut statement = self.connection.prepare(&sql)?;

        let params_refs: Vec<&dyn rusqlite::ToSql> = params_vec.iter().map(std::convert::AsRef::as_ref).collect();

        let rows = statement.query_map(params_refs.as_slice(), row_to_pending_file)?;

        let files = rows
            .collect::<Result<Vec<_>, _>>()
            .context("Failed to query pending files")?;

        Ok(files)
    }

    /// Get a single pending file by path.
    ///
    /// # Errors
    /// Returns an error if the database operation fails.
    #[expect(dead_code, reason = "Part of public API for future use")]
    pub fn get_pending_file(&self, path: &Path) -> Result<Option<PendingFile>> {
        let path_str = path.to_string_lossy();

        let mut statement = self.connection.prepare(
            r"
            SELECT id, full_path, extension, codec, bitrate_kbps, size_bytes, duration, width, height, frames_per_second, action, created_time, modified_time
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
        let path_str = path.to_string_lossy();

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
    #[expect(dead_code, reason = "Part of public API for future use")]
    pub fn remove_pending_file_by_id(&self, id: i64) -> Result<bool> {
        let rows_affected = self
            .connection
            .execute("DELETE FROM pending_files WHERE id = ?1", params![id])
            .context("Failed to remove pending file")?;

        Ok(rows_affected > 0)
    }

    /// Remove all pending files that no longer exist on disk.
    ///
    /// Returns the number of files removed.
    ///
    /// # Errors
    /// Returns an error if the database operation fails.
    pub fn remove_missing_files(&mut self) -> Result<usize> {
        let files = self.get_pending_files(&PendingFileFilter::default())?;
        let mut removed_count = 0;

        let transaction = self.connection.transaction()?;

        for file in files {
            if !file.full_path.exists() {
                transaction.execute("DELETE FROM pending_files WHERE id = ?1", params![file.id])?;
                removed_count += 1;
            }
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
        let mut statement = self
            .connection
            .prepare("SELECT DISTINCT extension FROM pending_files ORDER BY extension")?;

        let rows = statement.query_map([], |row| row.get(0))?;

        let extensions = rows
            .collect::<Result<Vec<_>, _>>()
            .context("Failed to query extensions")?;

        Ok(extensions)
    }
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
        action: PendingAction::from_str(&row.get::<_, String>(10)?),
        created_time: row.get(11)?,
        modified_time: row.get(12)?,
    })
}

impl PendingFile {
    /// Convert to `VideoInfo` for processing.
    pub fn to_video_info(&self) -> VideoInfo {
        VideoInfo {
            codec: self.codec.clone(),
            bitrate_kbps: self.bitrate_kbps,
            size_bytes: self.size_bytes,
            duration: self.duration,
            width: self.width,
            height: self.height,
            frames_per_second: self.frames_per_second,
            warning: None,
        }
    }
}

impl std::fmt::Display for DatabaseStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "Database Statistics:")?;
        writeln!(f, "  Total files:    {}", self.total_files)?;
        writeln!(f, "  To convert:     {}", self.convert_count)?;
        writeln!(f, "  To remux:       {}", self.remux_count)?;
        write!(f, "  Total size:     {}", cli_tools::format_size(self.total_size))
    }
}

impl std::fmt::Display for ExtensionStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            ".{:<3} {:>4} files  {}",
            self.extension,
            self.count,
            cli_tools::format_size(self.total_size)
        )
    }
}

#[cfg(test)]
mod tests {
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
            warning: None,
        }
    }

    #[test]
    fn test_open_in_memory() {
        let database = Database::open_in_memory();
        assert!(database.is_ok());
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
                min_bitrate: Some(10000),
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
        cli_tools::assert_f64_eq(long_videos[0].duration, 7200.0);
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
    }

    #[test]
    fn test_pending_action_from_str() {
        assert_eq!(PendingAction::from_str("convert"), PendingAction::Convert);
        assert_eq!(PendingAction::from_str("remux"), PendingAction::Remux);
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
            action: PendingAction::Convert,
            created_time: 0,
            modified_time: None,
        };

        let info = pending.to_video_info();
        assert_eq!(info.codec, "h264");
        assert_eq!(info.bitrate_kbps, 8000);
        assert_eq!(info.size_bytes, 1_000_000_000);
        assert_eq!(info.width, 1920);
        assert_eq!(info.height, 1080);
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
                min_bitrate: Some(10000),
                ..Default::default()
            })
            .expect("Failed to get files");
        assert_eq!(filtered.len(), 0); // a.mp4 has low bitrate, c.mp4 is remux

        // Filter: any file with high bitrate
        let filtered = database
            .get_pending_files(&PendingFileFilter {
                min_bitrate: Some(10000),
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
}

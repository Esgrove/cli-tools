//! Shared SQLite-backed cache for file analysis results.
//!
//! Both `vconvert` and `dupefind` use ffprobe to gather video metadata.
//! This module caches those results so files already scanned by one tool do not need to be re-analysed by the other.
//! It also stores BLAKE3 hashes used by `dupefind` for exact-content comparisons.
//!
//! The cache lives in the same database file that `vconvert` uses,
//! `vconvert.db` in the platform-specific local data directory.
//! It reads and writes the `scanned_files` and `file_hashes` tables.

#![allow(clippy::cast_possible_wrap)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};
use rusqlite::{Connection, params};

use crate::Resolution;
use crate::video_info::VideoInfo;

/// Default database filename (shared with `vconvert`).
const DATABASE_FILENAME: &str = "vconvert.db";

/// Read/write handle to the scanned-files cache table.
pub struct ScanCache {
    connection: Connection,
}

impl ScanCache {
    /// Open (or create) the shared cache database at the default path.
    ///
    /// The `scanned_files` table is created if it does not already exist,
    /// so this is safe to call even when `vconvert` has never been run.
    ///
    /// # Errors
    /// Returns an error if the database cannot be opened or initialised.
    pub fn open() -> Result<Self> {
        Self::open_at(&Self::database_path())
    }

    /// Open (or create) the cache database at a specific path.
    ///
    /// # Errors
    /// Returns an error if the database cannot be opened or initialised.
    pub fn open_at(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create database directory: {}", parent.display()))?;
        }

        let connection =
            Connection::open(path).with_context(|| format!("Failed to open database: {}", path.display()))?;

        connection
            .busy_timeout(std::time::Duration::from_secs(5))
            .context("Failed to set busy timeout")?;

        let cache = Self { connection };
        cache.initialise()?;
        Ok(cache)
    }

    /// Open an in-memory database for testing.
    ///
    /// # Errors
    /// Returns an error if the database cannot be created.
    #[cfg(test)]
    fn open_in_memory() -> Result<Self> {
        let connection = Connection::open_in_memory().context("Failed to open in-memory database")?;
        let cache = Self { connection };
        cache.initialise()?;
        Ok(cache)
    }

    /// Platform-specific database path.
    ///
    /// - Windows: `%LOCALAPPDATA%\cli-tools\vconvert.db`
    /// - macOS:   `~/Library/Application Support/cli-tools/vconvert.db`
    /// - Linux:   `~/.local/share/cli-tools/vconvert.db`
    fn database_path() -> PathBuf {
        let data_dir = dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("cli-tools");
        data_dir.join(DATABASE_FILENAME)
    }

    /// Ensure the `scanned_files` table exists.
    ///
    /// The schema is identical to the one created by `vconvert` so both
    /// tools can read and write the same table.
    fn initialise(&self) -> Result<()> {
        self.connection
            .execute_batch(
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
                    frames_per_second REAL NOT NULL,
                    scanned_time INTEGER NOT NULL
                );

                CREATE INDEX IF NOT EXISTS idx_scanned_path ON scanned_files(full_path);

                CREATE TABLE IF NOT EXISTS file_hashes (
                    id INTEGER PRIMARY KEY,
                    full_path TEXT NOT NULL UNIQUE,
                    size_bytes INTEGER NOT NULL,
                    modified_time_ns INTEGER NOT NULL,
                    blake3_hash TEXT NOT NULL,
                    hashed_time INTEGER NOT NULL
                );

                CREATE INDEX IF NOT EXISTS idx_file_hashes_path ON file_hashes(full_path);
                CREATE INDEX IF NOT EXISTS idx_file_hashes_hash ON file_hashes(blake3_hash);

                PRAGMA journal_mode = WAL;
                PRAGMA synchronous = NORMAL;
                PRAGMA cache_size = -8000;
                ",
            )
            .context("Failed to initialise scanned_files table")?;
        Ok(())
    }

    /// Load every cached entry into a `HashMap` keyed by full path string.
    ///
    /// This allows callers to do O(1) lookups and decide which files still
    /// need to be probed.
    ///
    /// # Errors
    /// Returns an error if the database query fails.
    pub fn get_all(&self) -> Result<HashMap<String, CachedVideoInfo>> {
        let mut statement = self.connection.prepare(
            r"
            SELECT full_path, size_bytes, codec, bitrate_kbps, duration, width, height, frames_per_second
            FROM scanned_files
            ",
        )?;

        let entries = statement
            .query_map([], |row| {
                let path: String = row.get(0)?;
                let entry = CachedVideoInfo {
                    size_bytes: row.get::<_, i64>(1)? as u64,
                    codec: row.get(2)?,
                    bitrate_kbps: row.get::<_, i64>(3)? as u64,
                    duration: row.get(4)?,
                    width: row.get::<_, i64>(5)? as u32,
                    height: row.get::<_, i64>(6)? as u32,
                    frames_per_second: row.get(7)?,
                };
                Ok((path, entry))
            })?
            .filter_map(std::result::Result::ok)
            .collect();

        Ok(entries)
    }

    /// Load every cached BLAKE3 hash into a `HashMap` keyed by full path string.
    ///
    /// # Errors
    /// Returns an error if the database query fails.
    pub fn get_all_hashes(&self) -> Result<HashMap<String, CachedFileHash>> {
        let mut statement = self.connection.prepare(
            r"
            SELECT full_path, size_bytes, modified_time_ns, blake3_hash
            FROM file_hashes
            ",
        )?;

        let entries = statement
            .query_map([], |row| {
                let path: String = row.get(0)?;
                let entry = CachedFileHash {
                    size_bytes: row.get::<_, i64>(1)? as u64,
                    modified_time_ns: row.get(2)?,
                    blake3_hash: row.get(3)?,
                };
                Ok((path, entry))
            })?
            .filter_map(std::result::Result::ok)
            .collect();

        Ok(entries)
    }

    /// Insert or update multiple cached BLAKE3 hashes in a single transaction.
    ///
    /// Returns the number of entries written.
    ///
    /// # Errors
    /// Returns an error if the transaction cannot be started or committed.
    pub fn batch_upsert_hashes(&mut self, entries: &[(&Path, &CachedFileHash)]) -> Result<usize> {
        if entries.is_empty() {
            return Ok(0);
        }

        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |duration| duration.as_secs() as i64);

        let transaction = self.connection.transaction()?;
        for (path, hash) in entries {
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
                        path.to_string_lossy(),
                        hash.size_bytes as i64,
                        hash.modified_time_ns,
                        hash.blake3_hash,
                        now,
                    ],
                )
                .context("Failed to upsert file hash")?;
        }

        transaction.commit()?;
        Ok(entries.len())
    }

    /// Insert or update multiple scanned file entries in a single transaction.
    ///
    /// Returns the number of entries written.
    ///
    /// # Errors
    /// Returns an error if the transaction cannot be started or committed.
    pub fn batch_upsert(&mut self, entries: &[(&Path, &VideoInfo)]) -> Result<usize> {
        if entries.is_empty() {
            return Ok(0);
        }

        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |duration| duration.as_secs() as i64);

        let transaction = self.connection.transaction()?;
        let mut count = 0;

        for (path, info) in entries {
            let path_str = path.to_string_lossy();

            let Some(size_bytes) = info.size_bytes else {
                continue;
            };

            let codec = info.codec.as_deref().unwrap_or("");
            let bitrate_kbps = info.bitrate_kbps.unwrap_or(0);
            let duration = info.duration.unwrap_or(0.0);
            let (width, height) = info.resolution.map_or((0u32, 0u32), |r| (r.width, r.height));

            transaction
                .execute(
                    r"
                    INSERT INTO scanned_files (full_path, size_bytes, codec, bitrate_kbps, duration, width, height, frames_per_second, scanned_time)
                    VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                    ON CONFLICT(full_path) DO UPDATE SET
                        size_bytes = excluded.size_bytes,
                        codec = excluded.codec,
                        bitrate_kbps = excluded.bitrate_kbps,
                        duration = excluded.duration,
                        width = excluded.width,
                        height = excluded.height,
                        frames_per_second = excluded.frames_per_second,
                        scanned_time = excluded.scanned_time
                    ",
                    params![
                        path_str,
                        size_bytes as i64,
                        codec,
                        bitrate_kbps as i64,
                        duration,
                        width,
                        height,
                        0.0f64, // frames_per_second (not available in shared VideoInfo)
                        now,
                    ],
                )
                .context("Failed to upsert scanned file")?;
            count += 1;
        }

        transaction.commit()?;
        Ok(count)
    }
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

impl CachedFileHash {
    /// Return whether this hash still describes a file with the given metadata.
    #[must_use]
    pub const fn is_current(&self, size_bytes: u64, modified_time_ns: i64) -> bool {
        self.size_bytes == size_bytes && self.modified_time_ns == modified_time_ns
    }
}

/// A row from the `scanned_files` cache with non-optional fields matching the
/// database schema.
///
/// This is intentionally separate from `VideoInfo` (which uses `Option` fields)
/// because the database columns are `NOT NULL`.
#[derive(Debug, Clone)]
pub struct CachedVideoInfo {
    /// File size in bytes (used to detect file changes).
    pub size_bytes: u64,
    /// Video codec name.
    pub codec: String,
    /// Video bitrate in kbps.
    pub bitrate_kbps: u64,
    /// Duration in seconds.
    pub duration: f64,
    /// Video width in pixels.
    pub width: u32,
    /// Video height in pixels.
    pub height: u32,
    /// Frames per second.
    pub frames_per_second: f64,
}

impl CachedVideoInfo {
    /// Convert to the shared `VideoInfo` type used by the rest of the library.
    #[must_use]
    pub fn to_video_info(&self) -> VideoInfo {
        VideoInfo {
            size_bytes: Some(self.size_bytes),
            resolution: Resolution::from_options(Some(self.width), Some(self.height)),
            duration: if self.duration > 0.0 { Some(self.duration) } else { None },
            codec: if self.codec.is_empty() {
                None
            } else {
                Some(self.codec.clone())
            },
            bitrate_kbps: if self.bitrate_kbps > 0 {
                Some(self.bitrate_kbps)
            } else {
                None
            },
        }
    }
}

#[cfg(test)]
mod test_scan_cache_open {
    use super::*;

    #[test]
    fn opens_in_memory_database() {
        let cache = ScanCache::open_in_memory();
        assert!(cache.is_ok());
    }

    #[test]
    fn empty_cache_returns_empty_map() {
        let cache = ScanCache::open_in_memory().expect("Failed to open in-memory database");
        let all = cache.get_all().expect("Failed to get entries");
        assert!(all.is_empty());
    }

    #[test]
    fn empty_cache_returns_empty_hash_map() {
        let cache = ScanCache::open_in_memory().expect("Failed to open in-memory database");
        let hashes = cache.get_all_hashes().expect("Failed to get hashes");
        assert!(hashes.is_empty());
    }
}

#[cfg(test)]
mod test_scan_cache_upsert {
    use super::*;

    fn sample_video_info() -> VideoInfo {
        VideoInfo {
            size_bytes: Some(1_000_000),
            resolution: Resolution::from_options(Some(1920), Some(1080)),
            duration: Some(120.5),
            codec: Some("h264".to_string()),
            bitrate_kbps: Some(5000),
        }
    }

    #[test]
    fn upsert_and_retrieve_single_entry() {
        let mut cache = ScanCache::open_in_memory().expect("Failed to open in-memory database");
        let info = sample_video_info();
        let path = Path::new("/videos/test.mp4");

        let count = cache.batch_upsert(&[(path, &info)]).expect("Failed to upsert");
        assert_eq!(count, 1);

        let all = cache.get_all().expect("Failed to get entries");
        assert_eq!(all.len(), 1);

        let cached = all.get("/videos/test.mp4").expect("Entry not found");
        assert_eq!(cached.size_bytes, 1_000_000);
        assert_eq!(cached.codec, "h264");
        assert_eq!(cached.bitrate_kbps, 5000);
        assert_eq!(cached.width, 1920);
        assert_eq!(cached.height, 1080);
        assert!((cached.duration - 120.5).abs() < f64::EPSILON);
    }

    #[test]
    fn upsert_updates_existing_entry() {
        let mut cache = ScanCache::open_in_memory().expect("Failed to open in-memory database");
        let path = Path::new("/videos/test.mp4");

        let info_v1 = sample_video_info();
        cache.batch_upsert(&[(path, &info_v1)]).expect("Failed to upsert v1");

        let info_v2 = VideoInfo {
            size_bytes: Some(2_000_000),
            resolution: Resolution::from_options(Some(3840), Some(2160)),
            duration: Some(240.0),
            codec: Some("hevc".to_string()),
            bitrate_kbps: Some(10_000),
        };
        cache.batch_upsert(&[(path, &info_v2)]).expect("Failed to upsert v2");

        let all = cache.get_all().expect("Failed to get entries");
        assert_eq!(all.len(), 1);

        let cached = all.get("/videos/test.mp4").expect("Entry not found");
        assert_eq!(cached.size_bytes, 2_000_000);
        assert_eq!(cached.codec, "hevc");
        assert_eq!(cached.width, 3840);
        assert_eq!(cached.height, 2160);
    }

    #[test]
    fn upsert_multiple_entries() {
        let mut cache = ScanCache::open_in_memory().expect("Failed to open in-memory database");

        let info_a = sample_video_info();
        let info_b = VideoInfo {
            size_bytes: Some(500_000),
            resolution: Resolution::from_options(Some(1280), Some(720)),
            duration: Some(60.0),
            codec: Some("hevc".to_string()),
            bitrate_kbps: Some(3000),
        };

        let path_a = Path::new("/videos/a.mp4");
        let path_b = Path::new("/videos/b.mkv");

        let count = cache
            .batch_upsert(&[(path_a, &info_a), (path_b, &info_b)])
            .expect("Failed to upsert");
        assert_eq!(count, 2);

        let all = cache.get_all().expect("Failed to get entries");
        assert_eq!(all.len(), 2);
        assert!(all.contains_key("/videos/a.mp4"));
        assert!(all.contains_key("/videos/b.mkv"));
    }

    #[test]
    fn upsert_skips_entry_without_size() {
        let mut cache = ScanCache::open_in_memory().expect("Failed to open in-memory database");
        let info = VideoInfo {
            size_bytes: None,
            resolution: Resolution::from_options(Some(1920), Some(1080)),
            duration: Some(120.5),
            codec: Some("h264".to_string()),
            bitrate_kbps: Some(5000),
        };
        let path = Path::new("/videos/test.mp4");

        let count = cache.batch_upsert(&[(path, &info)]).expect("Failed to upsert");
        assert_eq!(count, 0);

        let all = cache.get_all().expect("Failed to get entries");
        assert!(all.is_empty());
    }

    #[test]
    fn upsert_empty_input_returns_zero() {
        let mut cache = ScanCache::open_in_memory().expect("Failed to open in-memory database");
        let count = cache.batch_upsert(&[]).expect("Failed to upsert");
        assert_eq!(count, 0);
    }

    #[test]
    fn upsert_mixed_entries_skips_only_entry_without_size() {
        let mut cache = ScanCache::open_in_memory().expect("Failed to open in-memory database");
        let valid = sample_video_info();
        let missing_size = VideoInfo {
            size_bytes: None,
            ..valid.clone()
        };
        let valid_path = Path::new("/videos/valid.mp4");
        let skipped_path = Path::new("/videos/skipped.mp4");

        let count = cache
            .batch_upsert(&[(skipped_path, &missing_size), (valid_path, &valid)])
            .expect("Failed to upsert mixed entries");
        let all = cache.get_all().expect("Failed to get entries");

        assert_eq!(count, 1);
        assert!(all.contains_key("/videos/valid.mp4"));
        assert!(!all.contains_key("/videos/skipped.mp4"));
    }
}

#[cfg(test)]
mod test_file_hash_cache {
    use super::*;

    #[test]
    fn upserts_and_retrieves_hashes() {
        let mut cache = ScanCache::open_in_memory().expect("Failed to open in-memory database");
        let path = Path::new("/videos/test.mp4");
        let hash = CachedFileHash {
            size_bytes: 1_024,
            modified_time_ns: 123_456,
            blake3_hash: "abc123".to_string(),
        };

        let count = cache
            .batch_upsert_hashes(&[(path, &hash)])
            .expect("Failed to upsert hash");
        assert_eq!(count, 1);

        let hashes = cache.get_all_hashes().expect("Failed to retrieve hashes");
        assert_eq!(hashes.get("/videos/test.mp4"), Some(&hash));
    }

    #[test]
    fn validity_requires_matching_size_and_modification_time() {
        let hash = CachedFileHash {
            size_bytes: 1_024,
            modified_time_ns: 123_456,
            blake3_hash: "abc123".to_string(),
        };

        assert!(hash.is_current(1_024, 123_456));
        assert!(!hash.is_current(2_048, 123_456));
        assert!(!hash.is_current(1_024, 654_321));
    }

    #[test]
    fn upserts_multiple_hashes_in_one_batch() {
        let mut cache = ScanCache::open_in_memory().expect("Failed to open in-memory database");
        let first_path = Path::new("/videos/first.mp4");
        let second_path = Path::new("/videos/second.mp4");
        let first_hash = CachedFileHash {
            size_bytes: 100,
            modified_time_ns: 1,
            blake3_hash: "first".to_string(),
        };
        let second_hash = CachedFileHash {
            size_bytes: 200,
            modified_time_ns: 2,
            blake3_hash: "second".to_string(),
        };

        let count = cache
            .batch_upsert_hashes(&[(first_path, &first_hash), (second_path, &second_hash)])
            .expect("Failed to upsert hashes");
        let hashes = cache.get_all_hashes().expect("Failed to retrieve hashes");

        assert_eq!(count, 2);
        assert_eq!(hashes.get("/videos/first.mp4"), Some(&first_hash));
        assert_eq!(hashes.get("/videos/second.mp4"), Some(&second_hash));
    }
}

#[cfg(test)]
mod test_cached_video_info_conversion {
    use super::*;

    #[test]
    fn converts_all_fields() {
        let cached = CachedVideoInfo {
            size_bytes: 1_000_000,
            codec: "h264".to_string(),
            bitrate_kbps: 5000,
            duration: 120.5,
            width: 1920,
            height: 1080,
            frames_per_second: 29.97,
        };

        let video_info = cached.to_video_info();
        assert_eq!(video_info.size_bytes, Some(1_000_000));
        assert_eq!(video_info.codec, Some("h264".to_string()));
        assert_eq!(video_info.bitrate_kbps, Some(5000));
        assert_eq!(video_info.duration, Some(120.5));

        let resolution = video_info.resolution.expect("should have resolution");
        assert_eq!(resolution.width, 1920);
        assert_eq!(resolution.height, 1080);
    }

    #[test]
    fn zero_bitrate_becomes_none() {
        let cached = CachedVideoInfo {
            size_bytes: 1_000,
            codec: "h264".to_string(),
            bitrate_kbps: 0,
            duration: 10.0,
            width: 640,
            height: 480,
            frames_per_second: 24.0,
        };

        let video_info = cached.to_video_info();
        assert_eq!(video_info.bitrate_kbps, None);
    }

    #[test]
    fn zero_duration_becomes_none() {
        let cached = CachedVideoInfo {
            size_bytes: 1_000,
            codec: "h264".to_string(),
            bitrate_kbps: 5000,
            duration: 0.0,
            width: 640,
            height: 480,
            frames_per_second: 24.0,
        };

        let video_info = cached.to_video_info();
        assert_eq!(video_info.duration, None);
    }

    #[test]
    fn empty_codec_becomes_none() {
        let cached = CachedVideoInfo {
            size_bytes: 1_000,
            codec: String::new(),
            bitrate_kbps: 5000,
            duration: 10.0,
            width: 640,
            height: 480,
            frames_per_second: 24.0,
        };

        let video_info = cached.to_video_info();
        assert_eq!(video_info.codec, None);
    }

    #[test]
    fn roundtrip_through_database() {
        let mut cache = ScanCache::open_in_memory().expect("Failed to open in-memory database");
        let original = VideoInfo {
            size_bytes: Some(5_000_000),
            resolution: Resolution::from_options(Some(3840), Some(2160)),
            duration: Some(300.0),
            codec: Some("hevc".to_string()),
            bitrate_kbps: Some(15_000),
        };
        let path = Path::new("/videos/roundtrip.mkv");

        cache.batch_upsert(&[(path, &original)]).expect("Failed to upsert");

        let all = cache.get_all().expect("Failed to get entries");
        let cached = all.get("/videos/roundtrip.mkv").expect("Entry not found");
        let restored = cached.to_video_info();

        assert_eq!(restored.size_bytes, original.size_bytes);
        assert_eq!(restored.codec, original.codec);
        assert_eq!(restored.bitrate_kbps, original.bitrate_kbps);
        assert_eq!(restored.duration, original.duration);
        assert_eq!(restored.resolution, original.resolution);
    }
}

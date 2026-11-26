//! Scanned file and hash cache operations for `vconvert` on the unified `cli-tools.db`.
//!
//! Stores ffprobe results by path, size, and nanosecond modification timestamp so later scans
//! and sibling media tools (`dupefind`, `vstats`, `vres`, `thumbs`) reuse cached metadata
//! without re-running ffprobe.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use cli_tools_db::sqlite;
use cli_tools_media::scan_cache::is_definitively_missing;
use rayon::prelude::*;
use rusqlite::params;

use super::{Database, file_modified_time_ns, unix_now_secs};
use crate::types::VideoInfo;

impl Database {
    /// Insert or update a single scanned file cache entry.
    ///
    /// # Errors
    /// Returns an error if the database operation fails.
    pub fn upsert_scanned_file(&self, path: &Path, info: &VideoInfo) -> Result<()> {
        let Some(path_str) = path.to_str() else {
            return Ok(());
        };
        let now = unix_now_secs();
        let modified_time_ns = file_modified_time_ns(path);

        self.connection
            .execute(
                r"
                INSERT INTO scanned_files (
                    full_path, size_bytes, codec, bitrate_kbps, duration, width, height,
                    coded_width, coded_height, rotation, frames_per_second, bit_depth,
                    scanned_time, modified_time_ns
                )
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 0, ?10, ?11, ?12, ?13)
                ON CONFLICT(full_path) DO UPDATE SET
                    size_bytes = excluded.size_bytes,
                    codec = excluded.codec,
                    bitrate_kbps = excluded.bitrate_kbps,
                    duration = excluded.duration,
                    width = excluded.width,
                    height = excluded.height,
                    coded_width = excluded.coded_width,
                    coded_height = excluded.coded_height,
                    frames_per_second = excluded.frames_per_second,
                    bit_depth = excluded.bit_depth,
                    scanned_time = excluded.scanned_time,
                    modified_time_ns = excluded.modified_time_ns
                ",
                params![
                    path_str,
                    info.size_bytes as i64,
                    info.codec,
                    info.bitrate_kbps as i64,
                    info.duration,
                    info.width,
                    info.height,
                    info.width,
                    info.height,
                    info.frames_per_second,
                    i64::from(info.bit_depth),
                    now,
                    modified_time_ns,
                ],
            )
            .context("Failed to upsert scanned file")?;

        Ok(())
    }

    /// Insert or update multiple scanned file cache entries in a single `IMMEDIATE` transaction.
    ///
    /// All filesystem `stat()` calls run before taking the `SQLite` write lock.
    ///
    /// # Errors
    /// Returns an error if the transaction cannot be started or committed.
    pub fn batch_upsert_scanned_files(&mut self, entries: &[(&Path, &VideoInfo)]) -> Result<usize> {
        if entries.is_empty() {
            return Ok(0);
        }

        let prepared: Vec<(&str, &VideoInfo, i64)> = entries
            .iter()
            .filter_map(|&(path, info)| {
                let path_str = path.to_str()?;
                let modified_time_ns = file_modified_time_ns(path);
                Some((path_str, info, modified_time_ns))
            })
            .collect();

        if prepared.is_empty() {
            return Ok(0);
        }

        let now = unix_now_secs();
        let transaction = sqlite::immediate_transaction(&mut self.connection)?;
        let mut count = 0;

        for (path_str, info, modified_time_ns) in prepared {
            transaction
                .execute(
                    r"
                    INSERT INTO scanned_files (
                        full_path, size_bytes, codec, bitrate_kbps, duration, width, height,
                        coded_width, coded_height, rotation, frames_per_second, bit_depth,
                        scanned_time, modified_time_ns
                    )
                    VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 0, ?10, ?11, ?12, ?13)
                    ON CONFLICT(full_path) DO UPDATE SET
                        size_bytes = excluded.size_bytes,
                        codec = excluded.codec,
                        bitrate_kbps = excluded.bitrate_kbps,
                        duration = excluded.duration,
                        width = excluded.width,
                        height = excluded.height,
                        coded_width = excluded.coded_width,
                        coded_height = excluded.coded_height,
                        frames_per_second = excluded.frames_per_second,
                        bit_depth = excluded.bit_depth,
                        scanned_time = excluded.scanned_time,
                        modified_time_ns = excluded.modified_time_ns
                    ",
                    params![
                        path_str,
                        info.size_bytes as i64,
                        info.codec,
                        info.bitrate_kbps as i64,
                        info.duration,
                        info.width,
                        info.height,
                        info.width,
                        info.height,
                        info.frames_per_second,
                        i64::from(info.bit_depth),
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

    /// Update cached path keys in `scanned_files`, `file_hashes`, and `pending_files` after a rename.
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
        let tx = sqlite::immediate_transaction(&mut self.connection)?;
        for table in ["scanned_files", "file_hashes", "pending_files"] {
            if sqlite::table_exists(&tx, "main", table)? {
                tx.execute(&format!("DELETE FROM {table} WHERE full_path = ?1"), params![new_str])?;
                tx.execute(
                    &format!("UPDATE {table} SET full_path = ?1 WHERE full_path = ?2"),
                    params![new_str, old_str],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Remove cached scan and hash rows for a deleted or replaced file path.
    ///
    /// # Errors
    /// Returns an error if the delete fails.
    pub fn remove_cached_path(&self, path: &Path) -> Result<()> {
        let Some(path_str) = path.to_str() else {
            return Ok(());
        };
        if sqlite::table_exists(&self.connection, "main", "scanned_files")? {
            self.connection
                .execute("DELETE FROM scanned_files WHERE full_path = ?1", params![path_str])?;
        }
        if sqlite::table_exists(&self.connection, "main", "file_hashes")? {
            self.connection
                .execute("DELETE FROM file_hashes WHERE full_path = ?1", params![path_str])?;
        }
        Ok(())
    }

    /// Look up a previously scanned file by path and size.
    ///
    /// # Errors
    /// Returns an error if the database operation fails.
    #[cfg(test)]
    pub fn find_scanned_file(&self, path: &Path, size_bytes: u64) -> Result<Option<VideoInfo>> {
        use rusqlite::OptionalExtension;

        let path_str = path.to_string_lossy();

        let mut statement = self.connection.prepare(
            r"
            SELECT codec, bitrate_kbps, size_bytes, duration, width, height, frames_per_second, bit_depth
            FROM scanned_files
            WHERE full_path = ?1 AND size_bytes = ?2
            ",
        )?;

        let info = statement
            .query_row(params![path_str, size_bytes as i64], |row| {
                Ok(VideoInfo {
                    codec: row.get(0)?,
                    bitrate_kbps: row.get::<_, i64>(1)? as u64,
                    size_bytes: row.get::<_, i64>(2)? as u64,
                    duration: row.get(3)?,
                    width: row.get::<_, i64>(4)? as u32,
                    height: row.get::<_, i64>(5)? as u32,
                    frames_per_second: row.get(6)?,
                    bit_depth: row.get::<_, i64>(7)? as u8,
                    warning: None,
                })
            })
            .optional()
            .context("Failed to query scanned file")?;

        Ok(info)
    }

    /// Load all scanned file cache entries into a `HashMap` keyed by full path.
    ///
    /// # Errors
    /// Returns an error if the database operation fails.
    pub fn get_all_scanned_files(&self) -> Result<HashMap<String, VideoInfo>> {
        if !sqlite::table_exists(&self.connection, "main", "scanned_files")? {
            return Ok(HashMap::new());
        }
        let mut statement = self.connection.prepare(
            r"
            SELECT full_path, codec, bitrate_kbps, size_bytes, duration, width, height, frames_per_second, bit_depth
            FROM scanned_files
            ",
        )?;

        let mut rows = statement.query([])?;
        let mut entries = HashMap::new();
        while let Some(row) = rows.next()? {
            let path: String = row.get(0)?;
            let info = VideoInfo {
                codec: row.get(1)?,
                bitrate_kbps: row.get::<_, i64>(2)? as u64,
                size_bytes: row.get::<_, i64>(3)? as u64,
                duration: row.get(4)?,
                width: row.get::<_, i64>(5)? as u32,
                height: row.get::<_, i64>(6)? as u32,
                frames_per_second: row.get(7)?,
                bit_depth: row.get::<_, i64>(8)? as u8,
                warning: None,
            };
            entries.insert(path, info);
        }

        Ok(entries)
    }

    /// Load all scanned file cache entries alongside their recorded nanosecond modification times.
    ///
    /// # Errors
    /// Returns an error if the database query fails.
    pub(crate) fn get_all_scanned_files_with_mtime(&self) -> Result<HashMap<String, (VideoInfo, i64)>> {
        if !sqlite::table_exists(&self.connection, "main", "scanned_files")? {
            return Ok(HashMap::new());
        }
        let mut statement = self.connection.prepare(
            r"
            SELECT full_path, codec, bitrate_kbps, size_bytes, duration, width, height, frames_per_second, bit_depth, modified_time_ns
            FROM scanned_files
            ",
        )?;

        let mut rows = statement.query([])?;
        let mut entries = HashMap::new();
        while let Some(row) = rows.next()? {
            let path: String = row.get(0)?;
            let info = VideoInfo {
                codec: row.get(1)?,
                bitrate_kbps: row.get::<_, i64>(2)? as u64,
                size_bytes: row.get::<_, i64>(3)? as u64,
                duration: row.get(4)?,
                width: row.get::<_, i64>(5)? as u32,
                height: row.get::<_, i64>(6)? as u32,
                frames_per_second: row.get(7)?,
                bit_depth: row.get::<_, i64>(8)? as u8,
                warning: None,
            };
            let modified_time_ns: i64 = row.get(9)?;
            entries.insert(path, (info, modified_time_ns));
        }

        Ok(entries)
    }

    /// Remove scanned cache entries for files confirmed missing on mounted volumes.
    ///
    /// Groups paths by drive root so checks run in parallel via Rayon without holding a write lock,
    /// and skips paths on unmounted/offline volumes.
    ///
    /// # Errors
    /// Returns an error if the database operation fails.
    pub fn remove_missing_scanned_files(&mut self) -> Result<usize> {
        self.remove_missing_cache_table("scanned_files")
    }

    /// Remove `file_hashes` entries for files confirmed missing on mounted volumes.
    ///
    /// # Errors
    /// Returns an error if the database operation fails.
    pub fn remove_missing_file_hashes(&mut self) -> Result<usize> {
        self.remove_missing_cache_table("file_hashes")
    }

    /// Return the total number of entries in the scanned files cache.
    ///
    /// # Errors
    /// Returns an error if the database operation fails.
    pub fn scanned_file_count(&self) -> Result<u64> {
        if !sqlite::table_exists(&self.connection, "main", "scanned_files")? {
            return Ok(0);
        }
        let count: i64 = self
            .connection
            .query_row("SELECT COUNT(*) FROM scanned_files", [], |row| row.get(0))?;
        Ok(u64::try_from(count).unwrap_or(0))
    }

    /// Return the total number of entries in the shared file hashes cache.
    ///
    /// # Errors
    /// Returns an error if the database operation fails.
    pub fn file_hash_count(&self) -> Result<u64> {
        if !sqlite::table_exists(&self.connection, "main", "file_hashes")? {
            return Ok(0);
        }
        let count: i64 = self
            .connection
            .query_row("SELECT COUNT(*) FROM file_hashes", [], |row| row.get(0))?;
        Ok(u64::try_from(count).unwrap_or(0))
    }

    /// Prune definitively missing file rows from a cache table keyed by `full_path`.
    fn remove_missing_cache_table(&mut self, table_name: &str) -> Result<usize> {
        if !sqlite::table_exists(&self.connection, "main", table_name)? {
            return Ok(0);
        }
        let all_paths: Vec<PathBuf> = {
            let mut statement = self
                .connection
                .prepare(&format!("SELECT full_path FROM {table_name}"))?;
            let mut rows = statement.query([])?;
            let mut collected = Vec::new();
            while let Some(row) = rows.next()? {
                collected.push(PathBuf::from(row.get::<_, String>(0)?));
            }
            collected
        };

        if all_paths.is_empty() {
            return Ok(0);
        }

        let drive_groups = group_paths_by_drive(&all_paths);
        let missing_paths: Vec<PathBuf> = drive_groups
            .into_par_iter()
            .flat_map(|(_drive, paths)| {
                paths
                    .into_iter()
                    .filter(|path| is_definitively_missing(path))
                    .collect::<Vec<_>>()
            })
            .collect();

        if missing_paths.is_empty() {
            return Ok(0);
        }

        let transaction = sqlite::immediate_transaction(&mut self.connection)?;
        let delete_sql = format!("DELETE FROM {table_name} WHERE full_path = ?1");
        let mut removed = 0usize;
        for path in &missing_paths {
            if let Some(path_str) = path.to_str() {
                removed += transaction.execute(&delete_sql, params![path_str])?;
            }
        }
        transaction.commit()?;

        Ok(removed)
    }
}

/// Extract a drive or mount-point key from a path for grouping.
fn drive_key(path: &Path) -> String {
    let path_str = path.to_string_lossy();

    if let Some(without_prefix) = path_str.strip_prefix(r"\\") {
        let mut parts = without_prefix.splitn(3, '\\');
        return match (parts.next(), parts.next()) {
            (Some(server), Some(share)) => format!(r"\\{server}\{share}"),
            _ => r"\\".to_string(),
        };
    }

    if let Some(drive) = path_str.get(..2).filter(|drive| drive.ends_with(':')) {
        return drive.to_uppercase();
    }

    "/".to_string()
}

/// Group a slice of paths by their drive or mount-point key.
fn group_paths_by_drive(paths: &[PathBuf]) -> Vec<(String, Vec<PathBuf>)> {
    let mut groups: HashMap<String, Vec<PathBuf>> = HashMap::new();
    for path in paths {
        let key = drive_key(path);
        groups.entry(key).or_default().push(path.clone());
    }
    groups.into_iter().collect()
}

#[cfg(test)]
mod test_drive_key {
    use super::*;

    #[test]
    fn windows_drive_letter() {
        let path = PathBuf::from(r"C:\Users\video.mkv");
        assert_eq!(drive_key(&path), "C:");
    }

    #[test]
    fn windows_drive_letter_lowercase() {
        let path = PathBuf::from(r"d:\media\video.mkv");
        assert_eq!(drive_key(&path), "D:");
    }

    #[test]
    fn windows_unc_path() {
        let path = PathBuf::from(r"\\server\share\folder\file.mkv");
        assert_eq!(drive_key(&path), r"\\server\share");
    }

    #[test]
    fn windows_unc_path_server_only() {
        let path = PathBuf::from(r"\\server");
        assert_eq!(drive_key(&path), r"\\");
    }

    #[test]
    fn unix_path() {
        let path = PathBuf::from("/home/user/video.mkv");
        assert_eq!(drive_key(&path), "/");
    }

    #[test]
    fn relative_path() {
        let path = PathBuf::from("videos/file.mkv");
        assert_eq!(drive_key(&path), "/");
    }
}

#[cfg(test)]
mod test_group_paths_by_drive {
    use super::*;

    #[test]
    fn groups_single_drive() {
        let paths = vec![PathBuf::from(r"C:\a.mkv"), PathBuf::from(r"C:\b.mkv")];
        let groups = group_paths_by_drive(&paths);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].1.len(), 2);
    }

    #[test]
    fn groups_multiple_drives() {
        let paths = vec![
            PathBuf::from(r"C:\a.mkv"),
            PathBuf::from(r"D:\b.mkv"),
            PathBuf::from(r"C:\c.mkv"),
            PathBuf::from(r"D:\d.mkv"),
            PathBuf::from(r"E:\e.mkv"),
        ];
        let groups = group_paths_by_drive(&paths);
        assert_eq!(groups.len(), 3);

        let groups_map: HashMap<String, Vec<PathBuf>> = groups.into_iter().collect();
        assert_eq!(groups_map.get("C:").expect("Expected C: group").len(), 2);
        assert_eq!(groups_map.get("D:").expect("Expected D: group").len(), 2);
        assert_eq!(groups_map.get("E:").expect("Expected E: group").len(), 1);
    }

    #[test]
    fn groups_empty_input() {
        let paths: Vec<PathBuf> = vec![];
        let groups = group_paths_by_drive(&paths);
        assert_eq!(groups, [] as [(String, Vec<PathBuf>); 0]);
    }

    #[test]
    fn groups_unix_paths_into_single_group() {
        let paths = vec![
            PathBuf::from("/home/user/a.mkv"),
            PathBuf::from("/mnt/data/b.mkv"),
            PathBuf::from("/tmp/c.mkv"),
        ];
        let groups = group_paths_by_drive(&paths);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].0, "/");
        assert_eq!(groups[0].1.len(), 3);
    }

    #[test]
    fn groups_mixed_unc_and_drive_paths() {
        let paths = vec![
            PathBuf::from(r"C:\local.mkv"),
            PathBuf::from(r"\\server\share\remote.mkv"),
            PathBuf::from(r"C:\other.mkv"),
        ];
        let groups = group_paths_by_drive(&paths);
        assert_eq!(groups.len(), 2);

        let groups_map: HashMap<String, Vec<PathBuf>> = groups.into_iter().collect();
        assert_eq!(groups_map.get("C:").expect("Expected C: group").len(), 2);
        assert_eq!(groups_map.get(r"\\server\share").expect("Expected UNC group").len(), 1);
    }
}

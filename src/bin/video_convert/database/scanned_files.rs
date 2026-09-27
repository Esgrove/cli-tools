//! Scanned file cache for video convert.
//!
//! Stores ffprobe results by path and size so later scans can skip probing unchanged files,
//! and prunes entries for files that no longer exist, checking each drive in parallel.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};
use rayon::prelude::*;
use rusqlite::params;

use super::Database;
use crate::types::VideoInfo;

impl Database {
    /// Insert or update a scanned file cache entry.
    ///
    /// Called after every ffprobe run so the result can be reused on subsequent scans without re-running ffprobe,
    /// as long as the file size has not changed.
    ///
    /// # Errors
    /// Returns an error if the database operation fails.
    #[cfg(test)]
    pub fn upsert_scanned_file(&self, path: &Path, info: &VideoInfo) -> Result<()> {
        let path_str = path.to_string_lossy();
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |duration| duration.as_secs() as i64);

        self.connection
            .execute(
                r"
                INSERT INTO scanned_files (full_path, size_bytes, codec, bitrate_kbps, duration, width, height, frames_per_second, bit_depth, scanned_time)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                ON CONFLICT(full_path) DO UPDATE SET
                    size_bytes = excluded.size_bytes,
                    codec = excluded.codec,
                    bitrate_kbps = excluded.bitrate_kbps,
                    duration = excluded.duration,
                    width = excluded.width,
                    height = excluded.height,
                    frames_per_second = excluded.frames_per_second,
                    bit_depth = excluded.bit_depth,
                    scanned_time = excluded.scanned_time
                ",
                params![
                    path_str,
                    info.size_bytes as i64,
                    info.codec,
                    info.bitrate_kbps as i64,
                    info.duration,
                    info.width,
                    info.height,
                    info.frames_per_second,
                    i64::from(info.bit_depth),
                    now,
                ],
            )
            .context("Failed to upsert scanned file")?;

        Ok(())
    }

    /// Insert or update multiple scanned file cache entries in a single transaction.
    ///
    /// Wrapping all writes in one transaction reduces disk syncs from O(N) to O(1),
    /// which is significantly faster for large batches.
    ///
    /// Returns the number of entries successfully written.
    ///
    /// # Errors
    /// Returns an error if the transaction cannot be started or committed.
    pub fn batch_upsert_scanned_files(&mut self, entries: &[(&Path, &VideoInfo)]) -> Result<usize> {
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
            transaction
                .execute(
                    r"
                    INSERT INTO scanned_files (full_path, size_bytes, codec, bitrate_kbps, duration, width, height, frames_per_second, bit_depth, scanned_time)
                    VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                    ON CONFLICT(full_path) DO UPDATE SET
                        size_bytes = excluded.size_bytes,
                        codec = excluded.codec,
                        bitrate_kbps = excluded.bitrate_kbps,
                        duration = excluded.duration,
                        width = excluded.width,
                        height = excluded.height,
                        frames_per_second = excluded.frames_per_second,
                        bit_depth = excluded.bit_depth,
                        scanned_time = excluded.scanned_time
                    ",
                    params![
                        path_str,
                        info.size_bytes as i64,
                        info.codec,
                        info.bitrate_kbps as i64,
                        info.duration,
                        info.width,
                        info.height,
                        info.frames_per_second,
                        i64::from(info.bit_depth),
                        now,
                    ],
                )
                .context("Failed to upsert scanned file")?;
            count += 1;
        }

        transaction.commit()?;
        Ok(count)
    }

    /// Look up a previously scanned file by path and size.
    ///
    /// Returns the cached `VideoInfo` only when both the path and size match,
    /// ensuring stale entries for modified files are never used.
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
    /// This allows O(1) lookups during the analysis phase instead of issuing individual SQL queries per file.
    ///
    /// # Errors
    /// Returns an error if the database operation fails.
    pub fn get_all_scanned_files(&self) -> Result<HashMap<String, VideoInfo>> {
        let mut statement = self.connection.prepare(
            r"
            SELECT full_path, codec, bitrate_kbps, size_bytes, duration, width, height, frames_per_second, bit_depth
            FROM scanned_files
            ",
        )?;

        let entries = statement
            .query_map([], |row| {
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
                Ok((path, info))
            })?
            .filter_map(std::result::Result::ok)
            .collect();

        Ok(entries)
    }

    /// Remove scanned cache entries for files that no longer exist on disk.
    ///
    /// Groups paths by drive letter (on Windows) or mount-point prefix
    /// so that filesystem `exists()` checks for different drives run in parallel via Rayon,
    /// avoiding head-of-line blocking when one drive is slow or offline.
    /// The resulting list of missing paths is then deleted in a single database transaction.
    ///
    /// Returns the number of entries removed.
    ///
    /// # Errors
    /// Returns an error if the database operation fails.
    pub fn remove_missing_scanned_files(&mut self) -> Result<usize> {
        let all_paths: Vec<PathBuf> = {
            let mut statement = self.connection.prepare("SELECT full_path FROM scanned_files")?;
            statement
                .query_map([], |row| row.get::<_, String>(0))?
                .filter_map(std::result::Result::ok)
                .map(PathBuf::from)
                .collect()
        };

        if all_paths.is_empty() {
            return Ok(0);
        }

        // Group paths by drive root so each drive can be checked in parallel.
        let drive_groups = group_paths_by_drive(&all_paths);

        // Check existence in parallel, one thread per drive group.
        let missing_paths: Vec<PathBuf> = drive_groups
            .into_par_iter()
            .flat_map(|(_drive, paths)| paths.into_iter().filter(|path| !path.exists()).collect::<Vec<_>>())
            .collect();

        if missing_paths.is_empty() {
            return Ok(0);
        }

        // Batch-delete in a single transaction.
        let transaction = self.connection.transaction()?;
        for path in &missing_paths {
            let path_str = path.to_string_lossy();
            transaction.execute("DELETE FROM scanned_files WHERE full_path = ?1", params![path_str])?;
        }
        transaction.commit()?;

        Ok(missing_paths.len())
    }

    /// Return the total number of entries in the scanned files cache.
    ///
    /// # Errors
    /// Returns an error if the database operation fails.
    pub fn scanned_file_count(&self) -> Result<u64> {
        let count: i64 = self
            .connection
            .query_row("SELECT COUNT(*) FROM scanned_files", [], |row| row.get(0))?;
        Ok(count as u64)
    }
}

/// Extract a drive or mount-point key from a path for grouping.
///
/// On Windows this returns the drive prefix (e.g. `C:` or `\\server\share`).
/// On other platforms it returns `"/"` since all local paths share one root.
fn drive_key(path: &Path) -> String {
    let path_str = path.to_string_lossy();

    // UNC paths: \\server\share\...
    if let Some(without_prefix) = path_str.strip_prefix(r"\\") {
        // Take server\share as the key
        let mut parts = without_prefix.splitn(3, '\\');
        return match (parts.next(), parts.next()) {
            (Some(server), Some(share)) => format!(r"\\{server}\{share}"),
            _ => r"\\".to_string(),
        };
    }

    // Drive letter paths: C:\...
    if let Some(drive) = path_str.get(..2).filter(|drive| drive.ends_with(':')) {
        return drive.to_uppercase();
    }

    // Unix / fallback. Everything is on one root
    "/".to_string()
}

/// Group a slice of paths by their drive or mount-point key.
///
/// Returns a `Vec` of (key, paths) pairs suitable for parallel iteration.
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
        assert!(groups.is_empty());
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

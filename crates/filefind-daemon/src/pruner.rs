//! Database pruning functionality.
//!
//! This module provides functions for removing database entries
//! that reference files or directories that no longer exist on disk.
//! It optimizes by first checking parent directories to avoid unnecessary
//! filesystem queries for children of missing directories.
//!
//! Pruning is parallelized by volume/drive to maximize throughput.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::Path;

use anyhow::{Context, Result};
use filefind_common::types::IndexedVolume;
use filefind_common::{Database, extract_drive_letter_from_str};
use rayon::prelude::*;
use tracing::{debug, info, trace, warn};

/// Batch size for delete operations to avoid SQL statement size limits.
const DELETE_BATCH_SIZE: usize = 500;

/// Outcome of probing whether an indexed filesystem entry still exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathPresence {
    /// The entry exists on disk.
    Present,
    /// The filesystem confirmed the entry is absent (`NotFound`).
    Missing,
    /// Existence could not be confirmed due to permission errors, timeouts, or I/O failures.
    Unknown,
}

/// Statistics from a prune operation.
#[derive(Debug, Default, Clone)]
pub struct PruneStats {
    /// Number of files removed from the database.
    pub files_removed: u64,
    /// Number of directories removed from the database.
    pub directories_removed: u64,
    /// Number of entries checked.
    pub entries_checked: u64,
    /// Number of filesystem checks skipped due to missing parent directories.
    pub checks_skipped: u64,
}

/// Entry from the database to check for existence.
struct EntryToCheck {
    /// Database row ID.
    id: i64,
    /// Owning volume ID.
    volume_id: i64,
    /// Full path of the entry.
    full_path: String,
    /// Whether this is a directory.
    is_directory: bool,
}

/// Results from checking a single volume's entries.
struct VolumeCheckResult {
    /// IDs of entries to delete.
    ids_to_delete: Vec<i64>,
    /// Count of directories to be removed.
    directories_removed: u64,
    /// Count of files to be removed.
    files_removed: u64,
    /// Count of entries checked.
    entries_checked: u64,
    /// Count of filesystem checks skipped due to missing parent.
    checks_skipped: u64,
}

/// Prune database entries for files and directories that no longer exist.
///
/// This function efficiently removes stale entries by:
/// 1. Grouping entries by volume/drive for parallel processing
/// 2. For each volume, checking directories first (sorted by path length, shortest first)
/// 3. Tracking which directories are known to be missing
/// 4. For files, skipping filesystem checks if their parent directory is missing
///
/// # Arguments
/// * `database` - The database to prune
/// * `verbose` - Whether to print verbose progress information
///
/// # Errors
/// Returns an error if database operations fail.
pub fn prune_missing_entries(database: &Database, verbose: bool) -> Result<PruneStats> {
    prune_missing_entries_with_probe(database, verbose, &probe_path_presence)
}

/// Prune missing entries using a custom presence probe (used by tests to inject errors).
fn prune_missing_entries_with_probe(
    database: &Database,
    verbose: bool,
    probe: &(impl Fn(&Path) -> PathPresence + Sync),
) -> Result<PruneStats> {
    // Get all entries from the database and drop any belonging to offline or unreachable volume roots.
    let all_entries = filter_online_volume_entries(database, get_all_entries(database)?, probe)?;

    if all_entries.is_empty() {
        info!("Prune complete: no entries to check");
        return Ok(PruneStats::default());
    }

    // Group entries by volume (drive letter or UNC root)
    let entries_by_volume = group_entries_by_volume(all_entries);
    let volume_count = entries_by_volume.len();

    if verbose {
        let total_entries: usize = entries_by_volume.values().map(Vec::len).sum();
        info!(
            "Checking {} entries across {} volume(s) in parallel...",
            total_entries, volume_count
        );
    }

    // Process each volume in parallel using rayon
    let volume_results: Vec<VolumeCheckResult> = entries_by_volume
        .into_par_iter()
        .map(|(volume_key, entries)| {
            trace!("Processing volume: {}", volume_key);
            check_volume_entries_with_probe(entries, probe)
        })
        .collect();

    // Aggregate results
    let mut stats = PruneStats::default();
    let mut all_ids_to_delete: Vec<i64> = Vec::new();

    for result in volume_results {
        stats.directories_removed += result.directories_removed;
        stats.files_removed += result.files_removed;
        stats.entries_checked += result.entries_checked;
        stats.checks_skipped += result.checks_skipped;
        all_ids_to_delete.extend(result.ids_to_delete);
    }

    // Delete all missing entries from database
    if !all_ids_to_delete.is_empty() {
        let deleted = delete_entries_by_ids(database, &all_ids_to_delete)?;
        debug!("Deleted {} entries from database", deleted);
    }

    info!(
        "Prune complete: checked {} entries, removed {} files and {} directories ({} checks skipped)",
        stats.entries_checked, stats.files_removed, stats.directories_removed, stats.checks_skipped
    );

    Ok(stats)
}

/// Prune entries for a specific volume only.
///
/// This is useful for targeted cleanup after changes to a specific drive.
///
/// # Arguments
/// * `database` - The database to prune
/// * `volume_id` - The volume ID to prune entries for
/// * `verbose` - Whether to print verbose progress information
///
/// # Errors
/// Returns an error if database operations fail.
#[allow(dead_code, reason = "public API for single-volume pruning, used in tests")]
pub fn prune_volume_entries(database: &Database, volume_id: i64, verbose: bool) -> Result<PruneStats> {
    let entries = filter_online_volume_entries(
        database,
        get_entries_for_volume(database, volume_id, None)?,
        &probe_path_presence,
    )?;

    if entries.is_empty() {
        if verbose {
            info!("No entries to prune for volume {}", volume_id);
        }
        return Ok(PruneStats::default());
    }

    if verbose {
        info!("Checking {} entries for volume {}...", entries.len(), volume_id);
    }

    let result = check_volume_entries_with_probe(entries, &probe_path_presence);

    let stats = PruneStats {
        directories_removed: result.directories_removed,
        files_removed: result.files_removed,
        entries_checked: result.entries_checked,
        checks_skipped: result.checks_skipped,
    };

    if !result.ids_to_delete.is_empty() {
        delete_entries_by_ids(database, &result.ids_to_delete)?;
    }

    info!(
        "Volume {} prune: checked {} entries, removed {} files and {} directories",
        volume_id, stats.entries_checked, stats.files_removed, stats.directories_removed
    );

    Ok(stats)
}

/// Prune entries for multiple volumes efficiently in parallel.
///
/// This function is optimized for pruning multiple volumes by:
/// 1. Reading entries for all volumes from the database (sequential)
/// 2. Checking filesystem existence in parallel across all volumes using rayon
/// 3. Deleting stale entries from the database (sequential)
///
/// This is more efficient than calling `prune_volume_entries` multiple times
/// because it batches the database operations and maximizes parallel I/O.
///
/// # Arguments
/// * `database` - The database to prune
/// * `volume_ids` - The volume IDs to prune entries for
/// * `max_id` - Optional maximum file ID to prune. Entries with higher IDs are skipped.
///   This is useful to skip entries that were just inserted during the current scan.
/// * `verbose` - Whether to print verbose progress information
///
/// # Errors
/// Returns an error if database operations fail.
pub fn prune_multiple_volumes(
    database: &Database,
    volume_ids: &[i64],
    max_id: Option<i64>,
    verbose: bool,
) -> Result<PruneStats> {
    if volume_ids.is_empty() {
        return Ok(PruneStats::default());
    }

    // Step 1: Read entries for all volumes from the database (sequential)
    let mut all_entries: Vec<EntryToCheck> = Vec::new();
    for &volume_id in volume_ids {
        let entries = get_entries_for_volume(database, volume_id, max_id)?;
        if verbose && !entries.is_empty() {
            debug!("Volume {}: {} entries to check", volume_id, entries.len());
        }
        all_entries.extend(entries);
    }

    let all_entries = filter_online_volume_entries(database, all_entries, &probe_path_presence)?;

    if all_entries.is_empty() {
        if verbose {
            info!("No entries to prune across {} volume(s)", volume_ids.len());
        }
        return Ok(PruneStats::default());
    }

    if verbose {
        info!(
            "Checking {} entries across {} volume(s) in parallel...",
            all_entries.len(),
            volume_ids.len()
        );
    }

    // Step 2: Group by volume key and check filesystem in parallel
    let entries_by_volume = group_entries_by_volume(all_entries);

    let volume_results: Vec<VolumeCheckResult> = entries_by_volume
        .into_par_iter()
        .map(|(volume_key, entries)| {
            trace!("Processing volume: {}", volume_key);
            check_volume_entries_with_probe(entries, &probe_path_presence)
        })
        .collect();

    // Step 3: Aggregate results and delete from database (sequential)
    let mut stats = PruneStats::default();
    let mut all_ids_to_delete: Vec<i64> = Vec::new();

    for result in volume_results {
        stats.directories_removed += result.directories_removed;
        stats.files_removed += result.files_removed;
        stats.entries_checked += result.entries_checked;
        stats.checks_skipped += result.checks_skipped;
        all_ids_to_delete.extend(result.ids_to_delete);
    }

    if !all_ids_to_delete.is_empty() {
        let deleted = delete_entries_by_ids(database, &all_ids_to_delete)?;
        debug!("Deleted {} entries from database", deleted);
    }

    info!(
        "Multi-volume prune complete: checked {} entries, removed {} files and {} directories ({} checks skipped)",
        stats.entries_checked, stats.files_removed, stats.directories_removed, stats.checks_skipped
    );

    Ok(stats)
}

/// Probe a path with `symlink_metadata`, distinguishing confirmed absence from access or I/O errors.
fn probe_path_presence(path: &Path) -> PathPresence {
    match std::fs::symlink_metadata(path) {
        Ok(_) => PathPresence::Present,
        Err(error) if error.kind() == io::ErrorKind::NotFound => PathPresence::Missing,
        Err(_) => PathPresence::Unknown,
    }
}

/// Drop entries whose owning volume is marked offline or whose accessible mount point is currently missing.
fn filter_online_volume_entries(
    database: &Database,
    entries: Vec<EntryToCheck>,
    probe: &(impl Fn(&Path) -> PathPresence + Sync),
) -> Result<Vec<EntryToCheck>> {
    if entries.is_empty() {
        return Ok(entries);
    }

    let volumes: HashMap<i64, IndexedVolume> = database
        .get_all_volumes()?
        .into_iter()
        .filter_map(|volume| volume.id.map(|id| (id, volume)))
        .collect();

    let mut volumes_with_root_entries: HashSet<i64> = HashSet::new();
    for entry in &entries {
        if let Some(volume) = volumes.get(&entry.volume_id)
            && path_is_within_mount(&entry.full_path, &volume.mount_point)
        {
            volumes_with_root_entries.insert(entry.volume_id);
        }
    }

    let mut offline_volumes: HashSet<i64> = HashSet::new();
    for (volume_id, volume) in &volumes {
        if !volume.is_online {
            warn!("Skipping prune for offline volume {}", volume.mount_point);
            offline_volumes.insert(*volume_id);
            continue;
        }
        if volumes_with_root_entries.contains(volume_id)
            && probe(Path::new(&volume.mount_point)) != PathPresence::Present
        {
            warn!("Skipping prune for unreachable volume root {}", volume.mount_point);
            offline_volumes.insert(*volume_id);
        }
    }

    Ok(entries
        .into_iter()
        .filter(|entry| !offline_volumes.contains(&entry.volume_id))
        .collect())
}

/// Return whether `path` is `mount_point` or a descendant of it.
fn path_is_within_mount(path: &str, mount_point: &str) -> bool {
    if mount_point.is_empty() {
        return false;
    }
    if cfg!(windows) || mount_point.starts_with(r"\\") || extract_drive_letter_from_str(mount_point).is_some() {
        let normalized_path = path.replace('/', "\\").to_lowercase();
        let normalized_mount = mount_point.replace('/', "\\").to_lowercase();
        let trimmed = normalized_mount.trim_end_matches('\\');
        return normalized_path == trimmed
            || normalized_path
                .strip_prefix(trimmed)
                .is_some_and(|rest| rest.starts_with('\\'));
    }

    let trimmed = mount_point.trim_end_matches('/');
    if trimmed.is_empty() {
        return path.starts_with('/');
    }
    path == trimmed || path.strip_prefix(trimmed).is_some_and(|rest| rest.starts_with('/'))
}

/// Check entries for a single volume and return IDs to delete.
///
/// Processes directories first (sorted by path length) to enable parent-directory caching.
/// Only confirmed `Missing` entries are scheduled for deletion.
/// `Unknown` outcomes are preserved.
fn check_volume_entries_with_probe(
    entries: Vec<EntryToCheck>,
    probe: &(impl Fn(&Path) -> PathPresence + Sync),
) -> VolumeCheckResult {
    let mut result = VolumeCheckResult {
        ids_to_delete: Vec::new(),
        directories_removed: 0,
        files_removed: 0,
        entries_checked: 0,
        checks_skipped: 0,
    };

    // Separate directories and files
    let (mut directories, files): (Vec<_>, Vec<_>) = entries.into_iter().partition(|entry| entry.is_directory);

    // Sort directories by path length (shortest first) for parent-first checking
    directories.sort_by_key(|entry| entry.full_path.len());

    // Track confirmed missing, present, and unknown directories for parent-chain caching.
    let mut missing_directories: HashSet<String> = HashSet::new();
    let mut existing_directories: HashSet<String> = HashSet::new();
    let mut unknown_directories: HashSet<String> = HashSet::new();

    // Check directories first
    for entry in directories {
        result.entries_checked += 1;

        if is_parent_missing(&entry.full_path, &missing_directories) {
            trace!("Skipping {} - parent directory is missing", entry.full_path);
            result.checks_skipped += 1;
            result.ids_to_delete.push(entry.id);
            result.directories_removed += 1;
            missing_directories.insert(entry.full_path);
            continue;
        }

        if is_parent_missing(&entry.full_path, &unknown_directories) {
            trace!("Preserving {} - parent directory status is unknown", entry.full_path);
            result.checks_skipped += 1;
            unknown_directories.insert(entry.full_path);
            continue;
        }

        match probe(Path::new(&entry.full_path)) {
            PathPresence::Present => {
                existing_directories.insert(entry.full_path);
            }
            PathPresence::Missing => {
                debug!("Directory no longer exists: {}", entry.full_path);
                result.ids_to_delete.push(entry.id);
                result.directories_removed += 1;
                missing_directories.insert(entry.full_path);
            }
            PathPresence::Unknown => {
                debug!("Preserving inaccessible directory: {}", entry.full_path);
                unknown_directories.insert(entry.full_path);
            }
        }
    }

    // Check files
    for entry in files {
        result.entries_checked += 1;

        if is_parent_missing(&entry.full_path, &missing_directories) {
            trace!("Skipping {} - parent directory is missing", entry.full_path);
            result.checks_skipped += 1;
            result.ids_to_delete.push(entry.id);
            result.files_removed += 1;
            continue;
        }

        if is_parent_missing(&entry.full_path, &unknown_directories) {
            trace!("Preserving {} - parent directory status is unknown", entry.full_path);
            result.checks_skipped += 1;
            continue;
        }

        let parent = Path::new(&entry.full_path)
            .parent()
            .map(|path| path.to_string_lossy().to_string());

        let parent_presence = parent.as_deref().map_or(PathPresence::Present, |parent_path| {
            probe_cached_parent(
                parent_path,
                &mut existing_directories,
                &mut missing_directories,
                &mut unknown_directories,
                probe,
            )
        });

        match parent_presence {
            PathPresence::Missing => {
                trace!("Skipping {} - parent directory doesn't exist", entry.full_path);
                result.checks_skipped += 1;
                result.ids_to_delete.push(entry.id);
                result.files_removed += 1;
                continue;
            }
            PathPresence::Unknown => {
                trace!("Preserving {} - parent directory status is unknown", entry.full_path);
                result.checks_skipped += 1;
                continue;
            }
            PathPresence::Present => {}
        }

        if probe(Path::new(&entry.full_path)) == PathPresence::Missing {
            debug!("File no longer exists: {}", entry.full_path);
            result.ids_to_delete.push(entry.id);
            result.files_removed += 1;
        }
    }

    result
}

/// Look up or probe a parent directory's presence and cache the result.
fn probe_cached_parent(
    parent_path: &str,
    existing_directories: &mut HashSet<String>,
    missing_directories: &mut HashSet<String>,
    unknown_directories: &mut HashSet<String>,
    probe: &(impl Fn(&Path) -> PathPresence + Sync),
) -> PathPresence {
    if existing_directories.contains(parent_path) {
        return PathPresence::Present;
    }
    if missing_directories.contains(parent_path) {
        return PathPresence::Missing;
    }
    if unknown_directories.contains(parent_path) {
        return PathPresence::Unknown;
    }
    let presence = probe(Path::new(parent_path));
    match presence {
        PathPresence::Present => {
            existing_directories.insert(parent_path.to_string());
        }
        PathPresence::Missing => {
            missing_directories.insert(parent_path.to_string());
        }
        PathPresence::Unknown => {
            unknown_directories.insert(parent_path.to_string());
        }
    }
    presence
}

/// Check if any parent directory of the given path is in the missing set.
fn is_parent_missing(path: &str, missing_directories: &HashSet<String>) -> bool {
    let path = Path::new(path);
    let mut current = path.parent();

    while let Some(parent) = current {
        let parent_str = parent.to_string_lossy();
        if missing_directories.contains(parent_str.as_ref()) {
            return true;
        }
        current = parent.parent();
    }

    false
}

/// Extract volume key from a path (drive letter, UNC root, or Unix mount point).
fn extract_volume_key(path: &str) -> String {
    // Handle UNC paths: \\server\share -> \\server\share
    if path.starts_with("\\\\") {
        let parts: Vec<&str> = path.trim_start_matches("\\\\").splitn(3, '\\').collect();
        if let [server, share, ..] = parts.as_slice() {
            return format!("\\\\{server}\\{share}");
        }
        return path.to_string();
    }

    // Handle drive letters: C:\... -> C:
    if let Some(drive_letter) = extract_drive_letter_from_str(path) {
        return format!("{drive_letter}:");
    }

    // Handle absolute Unix paths: group by macOS mount point under /Volumes,
    // otherwise by the root filesystem
    if let Some(remainder) = path.strip_prefix("/Volumes/") {
        let volume_name = remainder.split('/').next().unwrap_or_default();
        if !volume_name.is_empty() {
            return format!("/Volumes/{volume_name}");
        }
    }
    if path.starts_with('/') {
        return "/".to_string();
    }

    // Fallback for unknown path formats
    "UNKNOWN".to_string()
}

/// Group entries by their volume (drive letter or UNC root).
fn group_entries_by_volume(entries: Vec<EntryToCheck>) -> HashMap<String, Vec<EntryToCheck>> {
    let mut grouped: HashMap<String, Vec<EntryToCheck>> = HashMap::new();

    for entry in entries {
        let volume_key = extract_volume_key(&entry.full_path);
        grouped.entry(volume_key).or_default().push(entry);
    }

    grouped
}

/// Get all entries from the database.
fn get_all_entries(database: &Database) -> Result<Vec<EntryToCheck>> {
    let connection = database.connection();

    let mut statement = connection
        .prepare(
            r"
            SELECT id, volume_id, full_path, is_directory
            FROM files
            ",
        )
        .context("Failed to prepare entries query")?;

    let entries = statement
        .query_map([], |row| {
            Ok(EntryToCheck {
                id: row.get(0)?,
                volume_id: row.get(1)?,
                full_path: row.get(2)?,
                is_directory: row.get(3)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()
        .context("Failed to query entries")?;

    Ok(entries)
}

/// Get entries for a specific volume from the database.
///
/// If `max_id` is provided, only returns entries with `id <= max_id`.
/// This is useful to skip entries that were just inserted during the current scan.
fn get_entries_for_volume(database: &Database, volume_id: i64, max_id: Option<i64>) -> Result<Vec<EntryToCheck>> {
    let connection = database.connection();

    let entries = if let Some(max) = max_id {
        let mut statement = connection
            .prepare(
                r"
                SELECT id, volume_id, full_path, is_directory
                FROM files
                WHERE volume_id = ?1 AND id <= ?2
                ",
            )
            .context("Failed to prepare volume entries query")?;

        statement
            .query_map([volume_id, max], |row| {
                Ok(EntryToCheck {
                    id: row.get(0)?,
                    volume_id: row.get(1)?,
                    full_path: row.get(2)?,
                    is_directory: row.get(3)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()
            .context("Failed to query volume entries")?
    } else {
        let mut statement = connection
            .prepare(
                r"
                SELECT id, volume_id, full_path, is_directory
                FROM files
                WHERE volume_id = ?1
                ",
            )
            .context("Failed to prepare volume entries query")?;

        statement
            .query_map([volume_id], |row| {
                Ok(EntryToCheck {
                    id: row.get(0)?,
                    volume_id: row.get(1)?,
                    full_path: row.get(2)?,
                    is_directory: row.get(3)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()
            .context("Failed to query volume entries")?
    };

    Ok(entries)
}

/// Delete entries from the database by their IDs.
fn delete_entries_by_ids(database: &Database, ids: &[i64]) -> Result<u64> {
    if ids.is_empty() {
        return Ok(0);
    }

    let connection = database.connection();

    // Use a transaction for better performance
    let transaction = connection.unchecked_transaction()?;

    let mut deleted = 0u64;

    for chunk in ids.chunks(DELETE_BATCH_SIZE) {
        // Build a parameterized query with placeholders
        let placeholders: Vec<&str> = chunk.iter().map(|_| "?").collect();
        let query = format!("DELETE FROM files WHERE id IN ({})", placeholders.join(", "));

        let mut statement = transaction.prepare(&query)?;

        // Bind all the IDs
        for (index, id) in chunk.iter().enumerate() {
            statement.raw_bind_parameter(index + 1, *id)?;
        }

        let rows = statement.raw_execute()?;
        deleted += rows as u64;
    }

    transaction.commit()?;

    Ok(deleted)
}

#[cfg(test)]
mod tests {
    use std::fs::{self, File};

    use filefind_common::FileEntry;
    use filefind_common::types::{IndexedVolume, VolumeType};
    use tempfile::tempdir;

    use super::*;

    fn setup_test_database() -> (Database, tempfile::TempDir) {
        let database = Database::open_in_memory().expect("Failed to create in-memory database");
        let temp_dir = tempdir().expect("Failed to create temp directory");
        (database, temp_dir)
    }

    fn insert_test_volume(database: &Database) -> i64 {
        let volume = IndexedVolume::new("TEST_SERIAL".to_string(), "T:".to_string(), VolumeType::Local);

        database.upsert_volume(&volume).expect("Failed to insert volume")
    }

    fn insert_test_file(database: &mut Database, volume_id: i64, path: &str, is_directory: bool) {
        let name = Path::new(path)
            .file_name()
            .map_or_default(|name| name.to_string_lossy().to_string());

        let entry = FileEntry::new(volume_id, name, path.to_string(), is_directory);

        let entries = vec![entry];
        database.insert_files_batch(&entries).expect("Failed to insert file");
    }

    #[test]
    fn test_prune_removes_missing_files() {
        let (mut database, temp_dir) = setup_test_database();
        let volume_id = insert_test_volume(&database);

        // Create a real file
        let existing_file_path = temp_dir.path().join("existing.txt");
        File::create(&existing_file_path).expect("Failed to create file");

        // Add entries to the database
        insert_test_file(&mut database, volume_id, &existing_file_path.to_string_lossy(), false);
        insert_test_file(
            &mut database,
            volume_id,
            &temp_dir.path().join("missing.txt").to_string_lossy(),
            false,
        );

        // Also add the temp dir itself as an existing directory
        insert_test_file(&mut database, volume_id, &temp_dir.path().to_string_lossy(), true);

        let stats = prune_missing_entries(&database, false).expect("Prune failed");

        assert_eq!(stats.files_removed, 1);
        assert_eq!(stats.directories_removed, 0);
    }

    #[test]
    fn test_prune_removes_missing_directories() {
        let (mut database, temp_dir) = setup_test_database();
        let volume_id = insert_test_volume(&database);

        // Create a real directory
        let existing_dir_path = temp_dir.path().join("existing_dir");
        fs::create_dir(&existing_dir_path).expect("Failed to create directory");

        // Add entries to the database
        insert_test_file(&mut database, volume_id, &existing_dir_path.to_string_lossy(), true);
        insert_test_file(
            &mut database,
            volume_id,
            &temp_dir.path().join("missing_dir").to_string_lossy(),
            true,
        );

        let stats = prune_missing_entries(&database, false).expect("Prune failed");

        assert_eq!(stats.directories_removed, 1);
    }

    #[test]
    fn test_prune_skips_children_of_missing_directories() {
        let (mut database, _temp_dir) = setup_test_database();
        let volume_id = insert_test_volume(&database);

        // Add a missing parent directory and several children
        let missing_parent = "Z:\\nonexistent_parent";
        insert_test_file(&mut database, volume_id, missing_parent, true);
        insert_test_file(&mut database, volume_id, "Z:\\nonexistent_parent\\child1.txt", false);
        insert_test_file(&mut database, volume_id, "Z:\\nonexistent_parent\\child2.txt", false);
        insert_test_file(&mut database, volume_id, "Z:\\nonexistent_parent\\subdir", true);
        insert_test_file(
            &mut database,
            volume_id,
            "Z:\\nonexistent_parent\\subdir\\nested.txt",
            false,
        );

        let stats = prune_missing_entries(&database, false).expect("Prune failed");

        // All entries should be removed
        assert_eq!(stats.directories_removed, 2); // parent + subdir
        assert_eq!(stats.files_removed, 3); // child1, child2, nested

        // Most checks should be skipped due to missing parent
        assert!(
            stats.checks_skipped >= 3,
            "Expected at least 3 skipped checks, got {}",
            stats.checks_skipped
        );
    }

    #[test]
    fn test_prune_empty_database() {
        let (database, _temp_dir) = setup_test_database();

        let stats = prune_missing_entries(&database, false).expect("Prune failed");

        assert_eq!(stats.files_removed, 0);
        assert_eq!(stats.directories_removed, 0);
        assert_eq!(stats.entries_checked, 0);
    }

    #[test]
    fn test_is_parent_missing() {
        let root = if cfg!(windows) { "C:\\" } else { "/" };
        let parent_dir = Path::new(root).join("parent");
        let child_path = parent_dir.join("child.txt");
        let deep_path = parent_dir.join("sub").join("deep.txt");
        let other_path = Path::new(root).join("other").join("file.txt");

        let mut missing: HashSet<String> = HashSet::new();
        missing.insert(parent_dir.to_string_lossy().to_string());

        assert!(is_parent_missing(&child_path.to_string_lossy(), &missing));
        assert!(is_parent_missing(&deep_path.to_string_lossy(), &missing));
        assert!(!is_parent_missing(&other_path.to_string_lossy(), &missing));
        // The path itself, not its parent
        assert!(!is_parent_missing(&parent_dir.to_string_lossy(), &missing));
    }

    #[test]
    fn test_prune_stats_default() {
        let stats = PruneStats::default();

        assert_eq!(stats.files_removed, 0);
        assert_eq!(stats.directories_removed, 0);
        assert_eq!(stats.entries_checked, 0);
        assert_eq!(stats.checks_skipped, 0);
    }

    #[test]
    fn test_extract_volume_key_drive_letter() {
        assert_eq!(extract_volume_key("C:\\Users\\test.txt"), "C:");
        assert_eq!(extract_volume_key("D:\\folder\\file.txt"), "D:");
        assert_eq!(extract_volume_key("c:\\lowercase"), "C:");
    }

    #[test]
    fn test_extract_volume_key_unc_path() {
        assert_eq!(extract_volume_key("\\\\server\\share\\file.txt"), "\\\\server\\share");
        assert_eq!(extract_volume_key("\\\\nas\\data\\docs"), "\\\\nas\\data");
    }

    #[test]
    fn test_extract_volume_key_unix_paths() {
        assert_eq!(extract_volume_key("/Users/test/file.txt"), "/");
        assert_eq!(extract_volume_key("/Volumes/USB/photos/img.jpg"), "/Volumes/USB");
        assert_eq!(
            extract_volume_key("/Volumes/Backup Drive/data"),
            "/Volumes/Backup Drive"
        );
        // Bare /Volumes/ with no volume name falls back to the root filesystem
        assert_eq!(extract_volume_key("/Volumes/"), "/");
    }

    #[test]
    fn test_extract_volume_key_unknown_format() {
        assert_eq!(extract_volume_key("relative/path/file.txt"), "UNKNOWN");
    }

    #[test]
    fn test_extract_volume_key_short_drive_paths() {
        assert_eq!(extract_volume_key("C:"), "C:");
        assert_eq!(extract_volume_key("z:file.txt"), "Z:");
        assert_eq!(extract_volume_key("C"), "UNKNOWN");
        assert_eq!(extract_volume_key(""), "UNKNOWN");
    }

    #[test]
    fn test_extract_volume_key_non_ascii_prefix() {
        // Multibyte first characters must not be sliced at a fixed two-byte offset.
        assert_eq!(extract_volume_key("日:\\file.txt"), "UNKNOWN");
        assert_eq!(extract_volume_key("é:\\file.txt"), "UNKNOWN");
        assert_eq!(extract_volume_key("1:\\file.txt"), "UNKNOWN");
    }

    #[test]
    fn test_extract_volume_key_malformed_unc() {
        assert_eq!(extract_volume_key("\\\\server"), "\\\\server");
        assert_eq!(extract_volume_key("\\\\server\\share"), "\\\\server\\share");
    }

    #[test]
    fn test_group_entries_by_volume() {
        let entries = vec![
            EntryToCheck {
                id: 1,
                volume_id: 1,
                full_path: "C:\\file1.txt".to_string(),
                is_directory: false,
            },
            EntryToCheck {
                id: 2,
                volume_id: 1,
                full_path: "C:\\file2.txt".to_string(),
                is_directory: false,
            },
            EntryToCheck {
                id: 3,
                volume_id: 2,
                full_path: "D:\\file3.txt".to_string(),
                is_directory: false,
            },
        ];

        let grouped = group_entries_by_volume(entries);

        assert_eq!(grouped.len(), 2);
        assert_eq!(grouped.get("C:").map(Vec::len), Some(2));
        assert_eq!(grouped.get("D:").map(Vec::len), Some(1));
    }

    #[test]
    fn test_prune_missing_entries_removes_missing_files() {
        let (mut database, temp_dir) = setup_test_database();
        let volume_id = insert_test_volume(&database);

        // Create some real files
        let existing_file = temp_dir.path().join("existing.txt");
        File::create(&existing_file).expect("Failed to create file");

        // Add entries
        insert_test_file(&mut database, volume_id, &existing_file.to_string_lossy(), false);
        insert_test_file(&mut database, volume_id, &temp_dir.path().to_string_lossy(), true);
        insert_test_file(
            &mut database,
            volume_id,
            &temp_dir.path().join("missing.txt").to_string_lossy(),
            false,
        );

        let stats = prune_missing_entries(&database, false).expect("Prune failed");

        assert_eq!(stats.files_removed, 1);
        assert_eq!(stats.directories_removed, 0);
    }

    #[test]
    fn test_prune_volume_entries() {
        let (mut database, temp_dir) = setup_test_database();
        let volume_id = insert_test_volume(&database);

        // Create a real file
        let existing_file = temp_dir.path().join("existing.txt");
        File::create(&existing_file).expect("Failed to create file");

        // Add entries
        insert_test_file(&mut database, volume_id, &existing_file.to_string_lossy(), false);
        insert_test_file(&mut database, volume_id, &temp_dir.path().to_string_lossy(), true);
        insert_test_file(
            &mut database,
            volume_id,
            &temp_dir.path().join("missing.txt").to_string_lossy(),
            false,
        );

        let stats = prune_volume_entries(&database, volume_id, false).expect("Volume prune failed");

        assert_eq!(stats.files_removed, 1);
        assert_eq!(stats.directories_removed, 0);
    }

    #[test]
    fn test_prune_multiple_volumes() {
        let (mut database, temp_dir) = setup_test_database();

        // Create two volumes
        let volume1 = IndexedVolume::new("VOL1".to_string(), "C:".to_string(), VolumeType::Local);
        let volume2 = IndexedVolume::new("VOL2".to_string(), "D:".to_string(), VolumeType::Local);

        let volume_id1 = database.upsert_volume(&volume1).expect("Failed to insert volume 1");
        let volume_id2 = database.upsert_volume(&volume2).expect("Failed to insert volume 2");

        // Create real files for volume 1
        let existing_file1 = temp_dir.path().join("vol1_existing.txt");
        File::create(&existing_file1).expect("Failed to create file");

        // Create real files for volume 2
        let existing_file2 = temp_dir.path().join("vol2_existing.txt");
        File::create(&existing_file2).expect("Failed to create file");

        // Add entries for volume 1 (1 existing, 1 missing)
        insert_test_file(&mut database, volume_id1, &existing_file1.to_string_lossy(), false);
        insert_test_file(
            &mut database,
            volume_id1,
            &temp_dir.path().join("vol1_missing.txt").to_string_lossy(),
            false,
        );

        // Add entries for volume 2 (1 existing, 2 missing)
        insert_test_file(&mut database, volume_id2, &existing_file2.to_string_lossy(), false);
        insert_test_file(
            &mut database,
            volume_id2,
            &temp_dir.path().join("vol2_missing1.txt").to_string_lossy(),
            false,
        );
        insert_test_file(
            &mut database,
            volume_id2,
            &temp_dir.path().join("vol2_missing2.txt").to_string_lossy(),
            false,
        );

        // Prune both volumes at once
        let stats = prune_multiple_volumes(&database, &[volume_id1, volume_id2], None, false)
            .expect("Multi-volume prune failed");

        // Should remove 3 missing files total (1 from vol1 + 2 from vol2)
        assert_eq!(stats.files_removed, 3);
        assert_eq!(stats.directories_removed, 0);
        assert_eq!(stats.entries_checked, 5);
    }

    #[test]
    fn test_prune_multiple_volumes_empty_list() {
        let (database, _temp_dir) = setup_test_database();

        let stats = prune_multiple_volumes(&database, &[], None, false).expect("Empty prune failed");

        assert_eq!(stats.files_removed, 0);
        assert_eq!(stats.directories_removed, 0);
        assert_eq!(stats.entries_checked, 0);
    }
}

#[cfg(test)]
mod test_prune_presence_safety {
    use std::fs::{self, File};

    use filefind_common::FileEntry;
    use filefind_common::types::{IndexedVolume, VolumeType};
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn offline_volume_root_preserves_all_entries() {
        let database = Database::open_in_memory().unwrap();
        let temp = tempdir().unwrap();
        let offline_root = temp.path().join("unmounted-drive");
        let root_str = offline_root.to_string_lossy().into_owned();
        let volume_id = database
            .upsert_volume(&IndexedVolume::new("OFFLINE-VOL".into(), root_str, VolumeType::Network))
            .unwrap();
        let nested_dir = offline_root.join("Movies");
        let nested_file = nested_dir.join("film.mkv");
        database
            .insert_file(&FileEntry::new(
                volume_id,
                "Movies".into(),
                nested_dir.to_string_lossy().into_owned(),
                true,
            ))
            .unwrap();
        database
            .insert_file(&FileEntry::new(
                volume_id,
                "film.mkv".into(),
                nested_file.to_string_lossy().into_owned(),
                false,
            ))
            .unwrap();

        let stats = prune_missing_entries(&database, false).unwrap();
        assert_eq!(stats.files_removed, 0);
        assert_eq!(stats.directories_removed, 0);
        assert!(
            database
                .get_file_by_path(&nested_dir.to_string_lossy())
                .unwrap()
                .is_some()
        );
        assert!(
            database
                .get_file_by_path(&nested_file.to_string_lossy())
                .unwrap()
                .is_some()
        );
    }

    #[cfg(unix)]
    #[test]
    fn permission_denied_directory_preserves_indexed_descendants() {
        use std::os::unix::fs::PermissionsExt;

        let database = Database::open_in_memory().unwrap();
        let temp = tempdir().unwrap();
        let volume_id = database
            .upsert_volume(&IndexedVolume::new(
                "PERM-VOL".into(),
                temp.path().to_string_lossy().into_owned(),
                VolumeType::Local,
            ))
            .unwrap();

        let restricted = temp.path().join("restricted");
        fs::create_dir(&restricted).unwrap();
        let child = restricted.join("secret.txt");
        File::create(&child).unwrap();
        let gone = temp.path().join("gone.txt");

        database
            .insert_file(&FileEntry::new(
                volume_id,
                "restricted".into(),
                restricted.to_string_lossy().into_owned(),
                true,
            ))
            .unwrap();
        database
            .insert_file(&FileEntry::new(
                volume_id,
                "secret.txt".into(),
                child.to_string_lossy().into_owned(),
                false,
            ))
            .unwrap();
        database
            .insert_file(&FileEntry::new(
                volume_id,
                "gone.txt".into(),
                gone.to_string_lossy().into_owned(),
                false,
            ))
            .unwrap();

        fs::set_permissions(&restricted, fs::Permissions::from_mode(0o000)).unwrap();
        let is_effective_root = fs::symlink_metadata(&child).is_ok();
        let result = prune_missing_entries(&database, false);
        fs::set_permissions(&restricted, fs::Permissions::from_mode(0o755)).unwrap();

        let stats = result.unwrap();
        assert_eq!(stats.files_removed, 1);
        assert!(database.get_file_by_path(&gone.to_string_lossy()).unwrap().is_none());
        if !is_effective_root {
            assert!(
                database
                    .get_file_by_path(&restricted.to_string_lossy())
                    .unwrap()
                    .is_some()
            );
            assert!(database.get_file_by_path(&child.to_string_lossy()).unwrap().is_some());
        }
    }

    #[test]
    fn distinguishes_present_missing_and_unknown_with_descendants() {
        let database = Database::open_in_memory().unwrap();
        let volume_id = database
            .upsert_volume(&IndexedVolume::new(
                "PROBE-VOL".into(),
                "/data".into(),
                VolumeType::Local,
            ))
            .unwrap();

        let entries = [
            ("/data/present", true),
            ("/data/present/kept.txt", false),
            ("/data/missing", true),
            ("/data/missing/child.txt", false),
            ("/data/unknown", true),
            ("/data/unknown/nested", true),
            ("/data/unknown/nested/preserved.txt", false),
        ];
        for (path, is_directory) in entries {
            let name = Path::new(path).file_name().unwrap().to_string_lossy().into_owned();
            database
                .insert_file(&FileEntry::new(volume_id, name, path.into(), is_directory))
                .unwrap();
        }

        let probe = |path: &Path| match path.to_string_lossy().as_ref() {
            "/data" | "/data/present" | "/data/present/kept.txt" => PathPresence::Present,
            "/data/missing" | "/data/missing/child.txt" => PathPresence::Missing,
            _ => PathPresence::Unknown,
        };

        let stats = prune_missing_entries_with_probe(&database, false, &probe).unwrap();
        assert_eq!(stats.directories_removed, 1);
        assert_eq!(stats.files_removed, 1);
        assert!(database.get_file_by_path("/data/present/kept.txt").unwrap().is_some());
        assert!(database.get_file_by_path("/data/missing").unwrap().is_none());
        assert!(database.get_file_by_path("/data/missing/child.txt").unwrap().is_none());
        assert!(database.get_file_by_path("/data/unknown").unwrap().is_some());
        assert!(database.get_file_by_path("/data/unknown/nested").unwrap().is_some());
        assert!(
            database
                .get_file_by_path("/data/unknown/nested/preserved.txt")
                .unwrap()
                .is_some()
        );
    }
}

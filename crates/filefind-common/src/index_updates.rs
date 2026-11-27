//! Updating the filefind index after other tools move, rename, or remove files.
//!
//! cli-tools tools record what they did as [`PathChange`] values and apply them with an [`IndexUpdater`],
//! so the index follows their changes immediately instead of waiting for the daemon,
//! which only notices them through the USN journal on NTFS drives or the next rescan elsewhere.
//!
//! The updater only moves and removes entries that are already indexed.
//! It never adds new entries, since the daemon's exclude rules decide what belongs in the index.
//! Every change is idempotent, so a change the daemon already applied is reported as not indexed.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use cli_tools_common::PathChange;

use crate::config::UserConfig;
use crate::database::Database;

/// What applying one change did to the index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChangeOutcome {
    /// Indexed entries moved to the new path.
    Moved,
    /// Indexed entries were removed.
    Removed,
    /// The source path was not in the index, so nothing changed.
    NotIndexed,
}

/// Applies completed file system changes to the filefind index database.
pub struct IndexUpdater {
    /// The opened index database.
    database: Database,
    /// The configured index roots, where an empty list means every indexed volume.
    indexed_roots: Vec<String>,
}

/// Counts of what applying a batch of changes did to the index.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct IndexUpdateReport {
    /// Changes that moved indexed entries to their new paths.
    pub moved: usize,
    /// Changes that removed indexed entries, including moves out of the indexed paths.
    pub removed: usize,
    /// Changes whose source path was not indexed.
    pub not_indexed: usize,
    /// Changes that failed, each described with its path and error.
    pub errors: Vec<String>,
}

impl IndexUpdater {
    /// Open the index from the filefind user config.
    ///
    /// Returns `None` when the index database does not exist, for example when filefind is not used.
    ///
    /// # Errors
    /// Returns an error if the existing database cannot be opened.
    pub fn open_default() -> Result<Option<Self>> {
        Self::open_with_config(&UserConfig::load())
    }

    /// Open the index database and the indexed paths named by `config`.
    ///
    /// Returns `None` when the index database does not exist.
    ///
    /// # Errors
    /// Returns an error if the existing database cannot be opened.
    pub fn open_with_config(config: &UserConfig) -> Result<Option<Self>> {
        let database_path = config.database_path();
        if !database_path.is_file() {
            return Ok(None);
        }
        let database = Database::open(&database_path)
            .with_context(|| format!("Failed to open the filefind index: {}", database_path.display()))?;
        Ok(Some(Self {
            database,
            indexed_roots: config.daemon.paths.clone(),
        }))
    }

    /// Apply `changes` in order, continuing past failed changes.
    ///
    /// Relative paths are resolved against the current directory,
    /// since the index stores absolute paths.
    #[must_use]
    pub fn apply(&self, changes: &[PathChange]) -> IndexUpdateReport {
        let mut report = IndexUpdateReport::default();
        for change in changes {
            match self.apply_change(change) {
                Ok(ChangeOutcome::Moved) => report.moved += 1,
                Ok(ChangeOutcome::Removed) => report.removed += 1,
                Ok(ChangeOutcome::NotIndexed) => report.not_indexed += 1,
                Err(error) => report.errors.push(format!("{}: {error:#}", change.source().display())),
            }
        }
        report
    }

    /// Apply one change to the index.
    fn apply_change(&self, change: &PathChange) -> Result<ChangeOutcome> {
        match change {
            PathChange::Moved { from, to, .. } => {
                self.apply_move(&absolute_path_string(from)?, &absolute_path_string(to)?)
            }
            PathChange::Removed { path, is_directory } => {
                self.apply_removal(&absolute_path_string(path)?, *is_directory)
            }
        }
    }

    /// Move the indexed entry at `from`, and everything under it for a directory, to `to`.
    fn apply_move(&self, from: &str, to: &str) -> Result<ChangeOutcome> {
        let Some(entry) = self.database.get_file_by_path(from)? else {
            return Ok(ChangeOutcome::NotIndexed);
        };
        if !self.is_indexed_location(to)? {
            if entry.is_directory {
                self.database.delete_files_under_path(from)?;
            } else {
                self.database.delete_file_by_path(from)?;
            }
            return Ok(ChangeOutcome::Removed);
        }

        let new_name = Path::new(to)
            .file_name()
            .map_or_else(|| to.to_string(), |name| name.to_string_lossy().to_string());
        if entry.is_directory {
            // Entries already indexed under the new path are stale, since the moved directory now occupies it.
            self.database.delete_files_under_path(to)?;
            self.database.rebase_files_under_path(from, to, &new_name)?;
            return Ok(ChangeOutcome::Moved);
        }
        if self.database.reconcile_moved_file(from, to, &new_name, true)? {
            Ok(ChangeOutcome::Moved)
        } else {
            Ok(ChangeOutcome::NotIndexed)
        }
    }

    /// Remove the indexed entry at `path`, and everything under it for a directory.
    fn apply_removal(&self, path: &str, is_directory: bool) -> Result<ChangeOutcome> {
        let indexed_directory = self
            .database
            .get_file_by_path(path)?
            .is_some_and(|entry| entry.is_directory);
        let removed = if is_directory || indexed_directory {
            self.database.delete_files_under_path(path)? > 0
        } else {
            self.database.delete_file_by_path(path)?
        };
        Ok(if removed {
            ChangeOutcome::Removed
        } else {
            ChangeOutcome::NotIndexed
        })
    }

    /// Check whether `path` is on an indexed volume and inside the configured index roots.
    fn is_indexed_location(&self, path: &str) -> Result<bool> {
        if self.database.get_volume_id_for_path(path)?.is_none() {
            return Ok(false);
        }
        Ok(self.indexed_roots.is_empty() || self.indexed_roots.iter().any(|root| is_within_root(path, root)))
    }
}

/// Resolve `path` against the current directory and return it as the string the index stores.
fn absolute_path_string(path: &Path) -> Result<String> {
    let absolute: PathBuf = std::path::absolute(path)
        .with_context(|| format!("Failed to resolve the absolute path of {}", path.display()))?;
    Ok(absolute.to_string_lossy().to_string())
}

/// Check whether `path` is `root` itself or inside it.
///
/// Windows paths compare case-insensitively and accept either separator.
fn is_within_root(path: &str, root: &str) -> bool {
    let normalize = |value: &str| {
        if cfg!(windows) {
            value.replace('/', "\\").to_lowercase()
        } else {
            value.to_string()
        }
    };
    let separator = if cfg!(windows) { '\\' } else { '/' };
    let path = normalize(path);
    let root = normalize(root);
    let root = root.trim_end_matches(separator);
    if root.is_empty() {
        return path.starts_with(separator);
    }
    path == root || path.strip_prefix(root).is_some_and(|rest| rest.starts_with(separator))
}

#[cfg(test)]
mod test_is_within_root {
    use super::*;

    #[test]
    #[cfg(windows)]
    fn matches_drive_roots_and_directories_case_insensitively() {
        assert!(is_within_root(r"Z:\DATA\video.mkv", "Z:"));
        assert!(is_within_root(r"Z:\DATA\video.mkv", r"z:\data"));
        assert!(is_within_root(r"Z:\DATA\video.mkv", r"Z:\DATA\"));
        assert!(is_within_root(r"Z:\DATA", r"Z:\DATA"));
        assert!(is_within_root("Z:/DATA/video.mkv", r"Z:\DATA"));
        assert!(!is_within_root(r"Z:\DATABASE\video.mkv", r"Z:\DATA"));
        assert!(!is_within_root(r"Y:\DATA\video.mkv", "Z:"));
    }

    #[test]
    #[cfg(windows)]
    fn matches_unc_roots() {
        assert!(is_within_root(r"\\server\share\dir\file.txt", r"\\server\share"));
        assert!(!is_within_root(r"\\server\shared\file.txt", r"\\server\share"));
    }

    #[test]
    #[cfg(not(windows))]
    fn matches_unix_directories_case_sensitively() {
        assert!(is_within_root("/data/videos/a.mkv", "/data"));
        assert!(is_within_root("/data/videos/a.mkv", "/data/"));
        assert!(is_within_root("/data", "/data"));
        assert!(!is_within_root("/database/a.mkv", "/data"));
        assert!(!is_within_root("/Data/a.mkv", "/data"));
        assert!(is_within_root("/anything", "/"));
    }
}

#[cfg(test)]
mod test_absolute_path_string {
    use super::*;

    #[test]
    fn resolves_relative_paths_against_the_current_directory() {
        let expected = std::env::current_dir().unwrap().join("dir").join("file.txt");

        assert_eq!(
            absolute_path_string(&Path::new("dir").join("file.txt")).unwrap(),
            expected.to_string_lossy()
        );
    }

    #[test]
    #[cfg(windows)]
    fn uses_backslashes_like_the_index() {
        assert_eq!(
            absolute_path_string(Path::new("C:/data/file.txt")).unwrap(),
            r"C:\data\file.txt"
        );
        assert_eq!(
            absolute_path_string(Path::new(r"C:\data\.\file.txt")).unwrap(),
            r"C:\data\file.txt"
        );
    }

    #[test]
    #[cfg(not(windows))]
    fn keeps_absolute_unix_paths() {
        assert_eq!(
            absolute_path_string(Path::new("/data/file.txt")).unwrap(),
            "/data/file.txt"
        );
    }
}

#[cfg(test)]
mod test_index_updater {
    use std::fs;

    use tempfile::TempDir;

    use super::*;
    use crate::types::{FileEntry, IndexedVolume, VolumeType};

    /// A temporary index with an indexed root directory and the matching filefind config.
    struct TestIndex {
        temp: TempDir,
        config: UserConfig,
    }

    impl TestIndex {
        /// Create an index whose volume and configured root is the `indexed` directory of a temporary tree.
        fn new() -> Self {
            let temp = TempDir::new().unwrap();
            fs::create_dir_all(temp.path().join("indexed")).unwrap();
            fs::create_dir_all(temp.path().join("outside")).unwrap();
            let mut config = UserConfig::default();
            config.daemon.database_path = Some(temp.path().join("filefind.db"));
            config.daemon.paths = vec![temp.path().join("indexed").to_string_lossy().to_string()];
            let index = Self { temp, config };
            let database = index.open_database();
            database
                .upsert_volume(&IndexedVolume {
                    id: None,
                    serial_number: "INDEX-UPDATES".into(),
                    label: None,
                    mount_point: index.temp.path().to_string_lossy().to_string(),
                    volume_type: VolumeType::Local,
                    is_online: true,
                    last_scan_time: None,
                    last_usn: None,
                })
                .unwrap();
            index
        }

        fn open_database(&self) -> Database {
            Database::open(&self.config.database_path()).unwrap()
        }

        /// Join a relative path written with forward slashes using the platform separator.
        fn path(&self, relative: &str) -> PathBuf {
            relative
                .split('/')
                .filter(|component| !component.is_empty())
                .fold(self.temp.path().to_path_buf(), |path, component| path.join(component))
        }

        fn path_string(&self, relative: &str) -> String {
            self.path(relative).to_string_lossy().to_string()
        }

        /// Index the given relative paths, marking those ending in a slash as directories.
        fn index(&self, entries: &[&str]) {
            let database = self.open_database();
            let volume_id = database
                .get_volume_id_for_path(&self.path_string("indexed"))
                .unwrap()
                .unwrap();
            for entry in entries {
                let is_directory = entry.ends_with('/');
                let full_path = self.path_string(entry.trim_end_matches('/'));
                let name = Path::new(&full_path).file_name().unwrap().to_string_lossy().to_string();
                database
                    .insert_file(&FileEntry {
                        id: None,
                        volume_id,
                        parent_id: None,
                        name,
                        full_path,
                        is_directory,
                        size: 7,
                        created_time: None,
                        modified_time: None,
                        mft_reference: Some(42),
                    })
                    .unwrap();
            }
        }

        /// The indexed paths relative to the temporary root, with forward slashes, sorted.
        fn indexed(&self) -> Vec<String> {
            let root = self.temp.path().to_string_lossy().to_string();
            let mut paths: Vec<String> = self
                .open_database()
                .search_by_path("", 1000)
                .unwrap()
                .into_iter()
                .map(|entry| {
                    entry
                        .full_path
                        .strip_prefix(&root)
                        .unwrap()
                        .trim_start_matches(['/', '\\'])
                        .replace('\\', "/")
                })
                .collect();
            paths.sort();
            paths
        }

        fn updater(&self) -> IndexUpdater {
            IndexUpdater::open_with_config(&self.config).unwrap().unwrap()
        }
    }

    #[test]
    fn missing_index_database_opens_as_none() {
        let temp = TempDir::new().unwrap();
        let mut config = UserConfig::default();
        config.daemon.database_path = Some(temp.path().join("missing.db"));

        assert!(IndexUpdater::open_with_config(&config).unwrap().is_none());
        assert!(!temp.path().join("missing.db").exists());
    }

    #[test]
    fn moved_file_keeps_its_entry_under_the_new_path() {
        let index = TestIndex::new();
        index.index(&["indexed/a/", "indexed/a/video.mkv", "indexed/b/"]);

        let report = index.updater().apply(&[PathChange::moved_file(
            index.path("indexed/a/video.mkv"),
            index.path("indexed/b/Video.Renamed.mkv"),
        )]);

        assert_eq!(report.moved, 1);
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(
            index.indexed(),
            vec!["indexed/a", "indexed/b", "indexed/b/Video.Renamed.mkv"]
        );
        let entry = index
            .open_database()
            .get_file_by_path(&index.path_string("indexed/b/Video.Renamed.mkv"))
            .unwrap()
            .unwrap();
        assert_eq!(entry.name, "Video.Renamed.mkv");
        assert_eq!(entry.size, 7);
        assert_eq!(
            entry.mft_reference, None,
            "a moved file may have a different MFT reference"
        );
    }

    #[test]
    fn moved_directory_moves_everything_under_it() {
        let index = TestIndex::new();
        index.index(&[
            "indexed/old/",
            "indexed/old/a.txt",
            "indexed/old/sub/",
            "indexed/old/sub/b.txt",
            "indexed/oldx.txt",
        ]);

        let report = index.updater().apply(&[PathChange::moved_directory(
            index.path("indexed/old"),
            index.path("indexed/new"),
        )]);

        assert_eq!(report.moved, 1);
        assert_eq!(
            index.indexed(),
            vec![
                "indexed/new",
                "indexed/new/a.txt",
                "indexed/new/sub",
                "indexed/new/sub/b.txt",
                "indexed/oldx.txt"
            ]
        );
    }

    #[test]
    fn moved_file_replaces_a_stale_entry_at_the_destination() {
        let index = TestIndex::new();
        index.index(&["indexed/new.txt", "indexed/old.txt"]);

        let report = index.updater().apply(&[PathChange::moved_file(
            index.path("indexed/old.txt"),
            index.path("indexed/new.txt"),
        )]);

        assert_eq!(report.moved, 1);
        assert_eq!(index.indexed(), vec!["indexed/new.txt"]);
    }

    #[test]
    fn moved_directory_replaces_stale_entries_at_the_destination() {
        let index = TestIndex::new();
        index.index(&[
            "indexed/new/",
            "indexed/new/stale.txt",
            "indexed/old/",
            "indexed/old/a.txt",
        ]);

        let report = index.updater().apply(&[PathChange::moved_directory(
            index.path("indexed/old"),
            index.path("indexed/new"),
        )]);

        assert_eq!(report.moved, 1);
        assert_eq!(index.indexed(), vec!["indexed/new", "indexed/new/a.txt"]);
    }

    #[test]
    fn removed_file_and_directory_leave_the_index() {
        let index = TestIndex::new();
        index.index(&[
            "indexed/gone.txt",
            "indexed/dir/",
            "indexed/dir/a.txt",
            "indexed/dir/sub/b.txt",
            "indexed/kept.txt",
        ]);

        let report = index.updater().apply(&[
            PathChange::removed_file(index.path("indexed/gone.txt")),
            PathChange::removed_directory(index.path("indexed/dir")),
        ]);

        assert_eq!(report.removed, 2);
        assert_eq!(index.indexed(), vec!["indexed/kept.txt"]);
    }

    #[test]
    fn removed_directory_reported_as_a_file_still_removes_its_subtree() {
        let index = TestIndex::new();
        index.index(&["indexed/dir/", "indexed/dir/a.txt"]);

        let report = index
            .updater()
            .apply(&[PathChange::removed_file(index.path("indexed/dir"))]);

        assert_eq!(report.removed, 1);
        assert_eq!(index.indexed(), Vec::<String>::new());
    }

    #[test]
    fn move_out_of_the_indexed_paths_removes_the_entries() {
        let index = TestIndex::new();
        index.index(&["indexed/file.txt", "indexed/dir/", "indexed/dir/a.txt"]);

        let report = index.updater().apply(&[
            PathChange::moved_file(index.path("indexed/file.txt"), index.path("outside/file.txt")),
            PathChange::moved_directory(index.path("indexed/dir"), index.path("outside/dir")),
        ]);

        assert_eq!(report.removed, 2);
        assert_eq!(index.indexed(), Vec::<String>::new());
    }

    #[test]
    fn already_applied_and_unindexed_changes_are_not_indexed() {
        let index = TestIndex::new();
        index.index(&["indexed/new.txt"]);

        let report = index.updater().apply(&[
            PathChange::moved_file(index.path("indexed/old.txt"), index.path("indexed/new.txt")),
            PathChange::moved_file(index.path("outside/x.txt"), index.path("indexed/x.txt")),
            PathChange::removed_file(index.path("indexed/never.txt")),
            PathChange::removed_directory(index.path("indexed/never")),
        ]);

        assert_eq!(report.not_indexed, 4);
        assert_eq!(report.moved + report.removed, 0);
        assert_eq!(
            index.indexed(),
            vec!["indexed/new.txt"],
            "unindexed files are not added"
        );
    }

    #[test]
    fn applying_the_same_changes_twice_changes_nothing_the_second_time() {
        let index = TestIndex::new();
        index.index(&["indexed/a.txt", "indexed/dir/", "indexed/dir/b.txt", "indexed/gone.txt"]);
        let changes = [
            PathChange::moved_file(index.path("indexed/a.txt"), index.path("indexed/A renamed.txt")),
            PathChange::moved_directory(index.path("indexed/dir"), index.path("indexed/directory")),
            PathChange::removed_file(index.path("indexed/gone.txt")),
        ];
        let updater = index.updater();

        let first = updater.apply(&changes);
        let after_first = index.indexed();
        let second = updater.apply(&changes);

        assert_eq!((first.moved, first.removed), (2, 1));
        assert_eq!(second.not_indexed, 3);
        assert_eq!(index.indexed(), after_first);
        assert_eq!(
            after_first,
            vec!["indexed/A renamed.txt", "indexed/directory", "indexed/directory/b.txt"]
        );
    }

    #[test]
    fn empty_index_roots_accept_any_indexed_volume() {
        let mut index = TestIndex::new();
        index.config.daemon.paths.clear();
        index.index(&["indexed/file.txt"]);

        let report = index.updater().apply(&[PathChange::moved_file(
            index.path("indexed/file.txt"),
            index.path("outside/file.txt"),
        )]);

        assert_eq!(report.moved, 1);
        assert_eq!(index.indexed(), vec!["outside/file.txt"]);
    }
}

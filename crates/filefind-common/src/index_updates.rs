//! Updating the filefind index after other tools move, rename, or remove files.
//!
//! cli-tools tools record what they did as [`PathChange`] values and apply them with an [`IndexUpdater`],
//! so the index follows their changes immediately instead of waiting for the daemon,
//! which only notices them through the USN journal on NTFS drives or the next rescan elsewhere.
//!
//! The updater only moves and removes entries that are already indexed.
//! It never adds new entries, since the daemon's exclude rules decide what belongs in the index.
//! Every change is idempotent, so a change the daemon already applied is reported as not indexed.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use cli_tools_common::PathChange;

use crate::config::UserConfig;
use crate::database::Database;
use crate::scope::{IndexScope, is_path_within_root, path_to_utf8};

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

/// Candidate mapping of an incoming path onto a stored index root.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RootMatch {
    /// Mapped path preserving the stored root spelling.
    mapped_path: String,
    /// Number of relative path components after the matched root.
    relative_depth: usize,
    /// Whether the incoming path already matched the stored root textually.
    exact_textual: bool,
}

/// Applies completed file system changes to the filefind index database.
pub struct IndexUpdater {
    /// The opened index database.
    database: Database,
    /// Shared root and exclusion policy from `[daemon]`.
    scope: IndexScope,
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
            scope: IndexScope::from_daemon_config(&config.daemon),
        }))
    }

    /// Apply `changes` in order, continuing past failed changes.
    ///
    /// Relative paths are resolved against the current directory,
    /// since the index stores absolute paths.
    #[must_use]
    pub fn apply(&self, changes: &[PathChange]) -> IndexUpdateReport {
        let mut report = IndexUpdateReport::default();
        let stored_roots = match self.collect_stored_roots() {
            Ok(roots) => roots,
            Err(error) => {
                report.errors.push(format!("{error:#}"));
                return report;
            }
        };
        for change in changes {
            match self.apply_change(change, &stored_roots) {
                Ok(ChangeOutcome::Moved) => report.moved += 1,
                Ok(ChangeOutcome::Removed) => report.removed += 1,
                Ok(ChangeOutcome::NotIndexed) => report.not_indexed += 1,
                Err(error) => report.errors.push(format!("{}: {error:#}", change.source().display())),
            }
        }
        report
    }

    /// Collect deduplicated volume mount points and configured roots in stored spelling.
    fn collect_stored_roots(&self) -> Result<Vec<String>> {
        let mut seen = HashSet::new();
        let mut roots = Vec::new();
        for root in &self.scope.roots {
            if !root.is_empty() && seen.insert(root.clone()) {
                roots.push(root.clone());
            }
        }
        for volume in self.database.get_all_volumes()? {
            if !volume.mount_point.is_empty() && seen.insert(volume.mount_point.clone()) {
                roots.push(volume.mount_point);
            }
        }
        Ok(roots)
    }

    /// Apply one change to the index.
    fn apply_change(&self, change: &PathChange, stored_roots: &[String]) -> Result<ChangeOutcome> {
        match change {
            PathChange::Moved { from, to, is_directory } => self.apply_move(
                &absolute_path_string(from)?,
                &absolute_path_string(to)?,
                *is_directory,
                stored_roots,
            ),
            PathChange::Removed { path, is_directory } => {
                self.apply_removal(&absolute_path_string(path)?, *is_directory, stored_roots)
            }
        }
    }

    /// Move the indexed entry at `from`, and everything under it for a directory, to `to`.
    fn apply_move(
        &self,
        from: &str,
        to: &str,
        is_directory_hint: bool,
        stored_roots: &[String],
    ) -> Result<ChangeOutcome> {
        let (resolved_from, is_directory) = if let Some((path, entry)) = self.find_indexed_entry(from, stored_roots)? {
            (path, is_directory_hint || entry.is_directory)
        } else if is_directory_hint && let Some(path) = self.find_indexed_subtree(from, stored_roots)? {
            (path, true)
        } else {
            return Ok(ChangeOutcome::NotIndexed);
        };

        let resolved_to = resolve_destination_path(to, stored_roots);
        if !self.is_indexed_location(&resolved_to)? {
            if is_directory {
                self.database.delete_files_under_path(&resolved_from)?;
            } else {
                self.database.delete_file_by_path(&resolved_from)?;
            }
            return Ok(ChangeOutcome::Removed);
        }

        let new_name = crate::scope::extract_entry_name(&resolved_to).to_string();
        if is_directory {
            let moved = self
                .database
                .reconcile_moved_directory(&resolved_from, &resolved_to, &new_name)?;
            return Ok(if moved > 0 {
                ChangeOutcome::Moved
            } else {
                ChangeOutcome::NotIndexed
            });
        }
        if self
            .database
            .reconcile_moved_file(&resolved_from, &resolved_to, &new_name, true)?
        {
            Ok(ChangeOutcome::Moved)
        } else {
            Ok(ChangeOutcome::NotIndexed)
        }
    }

    /// Remove the indexed entry at `path`, and everything under it for a directory.
    fn apply_removal(&self, path: &str, is_directory: bool, stored_roots: &[String]) -> Result<ChangeOutcome> {
        if let Some((resolved_path, entry)) = self.find_indexed_entry(path, stored_roots)? {
            let removed = if is_directory || entry.is_directory {
                self.database.delete_files_under_path(&resolved_path)? > 0
            } else {
                self.database.delete_file_by_path(&resolved_path)?
            };
            return Ok(if removed {
                ChangeOutcome::Removed
            } else {
                ChangeOutcome::NotIndexed
            });
        }

        if is_directory
            && let Some(resolved_path) = self.find_indexed_subtree(path, stored_roots)?
            && self.database.delete_files_under_path(&resolved_path)? > 0
        {
            return Ok(ChangeOutcome::Removed);
        }

        Ok(ChangeOutcome::NotIndexed)
    }

    /// Look up `path` directly in the index, falling back to aliased stored roots when absent.
    fn find_indexed_entry(
        &self,
        path: &str,
        stored_roots: &[String],
    ) -> Result<Option<(String, crate::types::FileEntry)>> {
        if let Some(entry) = self.database.get_file_by_path(path)? {
            return Ok(Some((path.to_string(), entry)));
        }

        let candidates = alias_candidates(path, stored_roots);
        let mut cursor = 0;
        while let Some(first) = candidates.get(cursor) {
            let depth = first.relative_depth;
            let mut depth_matches = Vec::new();
            while let Some(candidate) = candidates.get(cursor)
                && candidate.relative_depth == depth
            {
                if let Some(entry) = self.database.get_file_by_path(&candidate.mapped_path)? {
                    depth_matches.push((candidate.mapped_path.clone(), entry));
                }
                cursor += 1;
            }
            if depth_matches.len() == 1 {
                return Ok(depth_matches.into_iter().next());
            }
            if depth_matches.len() > 1 {
                return Ok(None);
            }
        }

        Ok(None)
    }

    /// Find `path` or an unambiguous aliased root spelling that has indexed descendants.
    fn find_indexed_subtree(&self, path: &str, stored_roots: &[String]) -> Result<Option<String>> {
        if self.database.has_entries_under_path(path)? {
            return Ok(Some(path.to_string()));
        }

        let candidates = alias_candidates(path, stored_roots);
        let mut cursor = 0;
        while let Some(first) = candidates.get(cursor) {
            let depth = first.relative_depth;
            let mut depth_matches = Vec::new();
            while let Some(candidate) = candidates.get(cursor)
                && candidate.relative_depth == depth
            {
                if self.database.has_entries_under_path(&candidate.mapped_path)? {
                    depth_matches.push(candidate.mapped_path.clone());
                }
                cursor += 1;
            }
            if depth_matches.len() == 1 {
                return Ok(depth_matches.into_iter().next());
            }
            if depth_matches.len() > 1 {
                return Ok(None);
            }
        }

        Ok(None)
    }

    /// Check whether `path` is on an indexed volume, inside configured roots, and not excluded.
    fn is_indexed_location(&self, path: &str) -> Result<bool> {
        let Some((_, volume_mount)) = self.database.get_volume_for_path(path)? else {
            return Ok(false);
        };
        Ok(self.scope.is_included(path, Some(&volume_mount)))
    }
}

/// Apply `changes` to the user's filefind index, when there is one, and return the warnings to show.
///
/// Tools call this once after a run.
/// It returns no warnings when there are no changes or no index,
/// and index problems never fail the tool, they only come back as warnings.
#[must_use]
pub fn update_default_index(changes: &[PathChange]) -> Vec<String> {
    if changes.is_empty() {
        return Vec::new();
    }
    update_index_with_config(&UserConfig::load(), changes)
}

/// Apply `changes` to the index named by `config` and return the warnings to show.
fn update_index_with_config(config: &UserConfig, changes: &[PathChange]) -> Vec<String> {
    match IndexUpdater::open_with_config(config) {
        Ok(Some(updater)) => updater.apply(changes).errors,
        Ok(None) => Vec::new(),
        Err(error) => vec![format!("{error:#}")],
    }
}

/// Resolve `path` against the current directory and return it as a valid UTF-8 index key.
fn absolute_path_string(path: &Path) -> Result<String> {
    let absolute: PathBuf = std::path::absolute(path)
        .with_context(|| format!("Failed to resolve the absolute path of {}", path.display()))?;
    Ok(path_to_utf8(&absolute)?.to_string())
}

/// Translate `destination` into the most specific unambiguous stored root spelling.
fn resolve_destination_path(destination: &str, stored_roots: &[String]) -> String {
    let mut matches: Vec<RootMatch> = stored_roots
        .iter()
        .filter_map(|root| match_path_against_root(destination, root))
        .collect();
    if matches.is_empty() {
        return destination.to_string();
    }

    matches.sort_by_key(|candidate| (candidate.relative_depth, !candidate.exact_textual));
    let Some(best_depth) = matches.first().map(|candidate| candidate.relative_depth) else {
        return destination.to_string();
    };
    let best_tier: Vec<&RootMatch> = matches
        .iter()
        .take_while(|candidate| candidate.relative_depth == best_depth)
        .collect();

    if let Some(exact) = best_tier.iter().find(|candidate| candidate.exact_textual) {
        return exact.mapped_path.clone();
    }
    if let [single] = best_tier.as_slice() {
        return single.mapped_path.clone();
    }
    destination.to_string()
}

/// Return non-identical aliased candidates for `path` ordered from most specific to least specific.
fn alias_candidates(path: &str, stored_roots: &[String]) -> Vec<RootMatch> {
    let mut seen = HashSet::new();
    let mut candidates: Vec<RootMatch> = stored_roots
        .iter()
        .filter_map(|root| match_path_against_root(path, root))
        .filter(|candidate| !candidate.exact_textual && candidate.mapped_path != path)
        .filter(|candidate| seen.insert(candidate.mapped_path.clone()))
        .collect();
    candidates.sort_by_key(|candidate| candidate.relative_depth);
    candidates
}

/// Match `path_str` against `stored_root`, preferring a textual match before directory identity.
///
/// Only existing directory ancestors are compared by filesystem identity.
/// A final path component that is a symbolic link is never followed into `stored_root`.
fn match_path_against_root(path_str: &str, stored_root: &str) -> Option<RootMatch> {
    let path = Path::new(path_str);
    let root = Path::new(stored_root);

    if is_within_root(path_str, stored_root) {
        let relative_depth = textual_relative_depth(path_str, stored_root);
        return Some(RootMatch {
            mapped_path: path_str.to_string(),
            relative_depth,
            exact_textual: true,
        });
    }

    if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir())
        && cli_tools_common::paths_refer_to_same_file(path, root)
    {
        return Some(RootMatch {
            mapped_path: stored_root.to_string(),
            relative_depth: 0,
            exact_textual: false,
        });
    }

    let matching_ancestor = path
        .ancestors()
        .skip(1)
        .find(|ancestor| cli_tools_common::paths_refer_to_same_file(ancestor, root))?;
    let relative = path.strip_prefix(matching_ancestor).ok()?;
    Some(RootMatch {
        mapped_path: join_stored_root(stored_root, relative),
        relative_depth: relative.components().count(),
        exact_textual: false,
    })
}

/// Count the path components in `path` after a textual `root` prefix.
fn textual_relative_depth(path: &str, root: &str) -> usize {
    let separator = if cfg!(windows) { '\\' } else { '/' };
    let normalize = |value: &str| {
        if cfg!(windows) {
            value.replace('/', "\\").to_lowercase()
        } else {
            value.to_string()
        }
    };
    let normalized_path = normalize(path);
    let normalized_root = normalize(root);
    let trimmed_root = normalized_root.trim_end_matches(separator);
    let remainder = normalized_path
        .strip_prefix(trimmed_root)
        .unwrap_or_default()
        .trim_start_matches(separator);
    if remainder.is_empty() {
        0
    } else {
        remainder.split(separator).filter(|part| !part.is_empty()).count()
    }
}

/// Join `relative` onto `stored_root`, expanding a bare Windows drive root before joining.
fn join_stored_root(stored_root: &str, relative: &Path) -> String {
    if relative.as_os_str().is_empty() {
        return stored_root.to_string();
    }
    let base = crate::normalize_drive_root(Path::new(stored_root));
    base.join(relative).to_string_lossy().into_owned()
}

/// Check whether `path` is `root` itself or inside it.
///
/// Windows paths compare case-insensitively and accept either separator.
fn is_within_root(path: &str, root: &str) -> bool {
    is_path_within_root(path, root)
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
    fn update_with_config_applies_the_changes_without_warnings() {
        let index = TestIndex::new();
        index.index(&["indexed/a.txt"]);

        let warnings = update_index_with_config(
            &index.config,
            &[PathChange::moved_file(
                index.path("indexed/a.txt"),
                index.path("indexed/b.txt"),
            )],
        );

        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(index.indexed(), vec!["indexed/b.txt"]);
    }

    #[test]
    fn update_without_an_index_has_no_warnings() {
        let temp = TempDir::new().unwrap();
        let mut config = UserConfig::default();
        config.daemon.database_path = Some(temp.path().join("missing.db"));

        let warnings = update_index_with_config(&config, &[PathChange::removed_file(temp.path().join("a.txt"))]);

        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(!temp.path().join("missing.db").exists());
    }

    #[test]
    fn update_with_an_unreadable_index_returns_a_warning() {
        let temp = TempDir::new().unwrap();
        let database_path = temp.path().join("filefind.db");
        fs::write(&database_path, "not a database").unwrap();
        let mut config = UserConfig::default();
        config.daemon.database_path = Some(database_path);

        let warnings = update_index_with_config(&config, &[PathChange::removed_file(temp.path().join("a.txt"))]);

        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("filefind index"), "{warnings:?}");
    }

    #[test]
    fn update_default_index_with_no_changes_does_nothing() {
        assert_eq!(update_default_index(&[]), Vec::<String>::new());
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

#[cfg(test)]
#[cfg(unix)]
mod test_aliased_index_roots {
    use std::fs;
    use std::os::unix::fs::symlink;

    use tempfile::TempDir;

    use super::*;
    use crate::types::{FileEntry, IndexedVolume, VolumeType};

    /// Temporary workspace with a physical directory and a symlink alias pointing to it.
    struct AliasedTestIndex {
        temp: TempDir,
        real_root: PathBuf,
        alias_root: PathBuf,
        outside_root: PathBuf,
        sibling_root: PathBuf,
        config: UserConfig,
    }

    impl AliasedTestIndex {
        fn new(store_alias_as_root: bool) -> Self {
            let temp = TempDir::new().unwrap();
            let base = fs::canonicalize(temp.path()).unwrap();
            let real_root = base.join("real");
            let alias_root = base.join("alias");
            let outside_root = base.join("outside");
            let sibling_root = base.join("real-sibling");
            fs::create_dir_all(&real_root).unwrap();
            fs::create_dir_all(&outside_root).unwrap();
            fs::create_dir_all(&sibling_root).unwrap();
            symlink(&real_root, &alias_root).unwrap();

            let stored_root = if store_alias_as_root {
                alias_root.clone()
            } else {
                real_root.clone()
            };

            let mut config = UserConfig::default();
            config.daemon.database_path = Some(base.join("filefind.db"));
            config.daemon.paths = vec![stored_root.to_string_lossy().to_string()];

            let database = Database::open(&config.database_path()).unwrap();
            database
                .upsert_volume(&IndexedVolume {
                    id: None,
                    serial_number: "ALIASED-ROOT".into(),
                    label: None,
                    mount_point: stored_root.to_string_lossy().to_string(),
                    volume_type: VolumeType::Local,
                    is_online: true,
                    last_scan_time: None,
                    last_usn: None,
                })
                .unwrap();

            Self {
                temp,
                real_root,
                alias_root,
                outside_root,
                sibling_root,
                config,
            }
        }

        fn stored_root(&self) -> PathBuf {
            PathBuf::from(self.config.daemon.paths.first().unwrap())
        }

        fn open_database(&self) -> Database {
            Database::open(&self.config.database_path()).unwrap()
        }

        fn index_under_stored_root(&self, entries: &[&str]) {
            let database = self.open_database();
            let root = self.stored_root();
            let volume_id = database
                .get_volume_id_for_path(&root.to_string_lossy())
                .unwrap()
                .unwrap();
            for entry in entries {
                let is_directory = entry.ends_with('/');
                let relative = entry.trim_end_matches('/');
                let full_path = root.join(relative);
                let name = full_path.file_name().unwrap().to_string_lossy().to_string();
                database
                    .insert_file(&FileEntry {
                        id: None,
                        volume_id,
                        parent_id: None,
                        name,
                        full_path: full_path.to_string_lossy().to_string(),
                        is_directory,
                        size: 10,
                        created_time: None,
                        modified_time: None,
                        mft_reference: None,
                    })
                    .unwrap();
            }
        }

        fn indexed_relative(&self) -> Vec<String> {
            let root = self.stored_root().to_string_lossy().to_string();
            let mut paths: Vec<String> = self
                .open_database()
                .search_by_path("", 1000)
                .unwrap()
                .into_iter()
                .map(|entry| {
                    entry
                        .full_path
                        .strip_prefix(&root)
                        .unwrap_or(&entry.full_path)
                        .trim_start_matches('/')
                        .to_string()
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
    fn resolves_physical_changes_against_an_aliased_stored_root() {
        let index = AliasedTestIndex::new(true);
        fs::create_dir_all(index.real_root.join("Show Alpha")).unwrap();
        fs::create_dir_all(index.real_root.join("Movie/subs/forced")).unwrap();
        fs::write(index.real_root.join("Show Alpha/ep1.mkv"), "ep1").unwrap();
        fs::write(index.real_root.join("Movie/english.srt"), "en").unwrap();
        fs::write(index.real_root.join("Movie/forced.srt"), "fr").unwrap();
        // Simulate subs and its child already removed from disk before the batch runs.
        fs::remove_dir_all(index.real_root.join("Movie/subs")).unwrap();

        index.index_under_stored_root(&[
            "Show Alpha/",
            "ep1.mkv",
            "Movie/",
            "Movie/subs/",
            "Movie/subs/english.srt",
            "Movie/subs/forced/",
            "Movie/subs/forced/forced.srt",
        ]);

        let changes = [
            PathChange::moved_file(
                index.real_root.join("ep1.mkv"),
                index.real_root.join("Show Alpha/ep1.mkv"),
            ),
            PathChange::moved_file(
                index.real_root.join("Movie/subs/english.srt"),
                index.real_root.join("Movie/english.srt"),
            ),
            PathChange::moved_directory(
                index.real_root.join("Movie/subs/forced"),
                index.real_root.join("Movie/forced"),
            ),
            PathChange::removed_directory(index.real_root.join("Movie/subs")),
        ];

        let updater = index.updater();
        let report = updater.apply(&changes);
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!((report.moved, report.removed), (3, 1));
        assert_eq!(
            index.indexed_relative(),
            vec![
                "Movie",
                "Movie/english.srt",
                "Movie/forced",
                "Movie/forced/forced.srt",
                "Show Alpha",
                "Show Alpha/ep1.mkv",
            ]
        );

        let replay = updater.apply(&changes);
        assert_eq!((replay.moved, replay.removed, replay.not_indexed), (0, 0, 4));
    }

    #[test]
    fn resolves_aliased_changes_against_a_physical_stored_root() {
        let index = AliasedTestIndex::new(false);
        fs::write(index.real_root.join("renamed.txt"), "data").unwrap();
        index.index_under_stored_root(&["original.txt"]);

        let report = index.updater().apply(&[PathChange::moved_file(
            index.alias_root.join("original.txt"),
            index.alias_root.join("renamed.txt"),
        )]);

        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(report.moved, 1);
        assert_eq!(index.indexed_relative(), vec!["renamed.txt"]);
    }

    #[test]
    fn handles_pruned_parent_out_of_root_sibling_and_final_symlink() {
        let index = AliasedTestIndex::new(true);
        let outside_link = index.outside_root.join("link-to-root");
        symlink(&index.real_root, &outside_link).unwrap();
        fs::write(index.outside_root.join("moved-out.txt"), "out").unwrap();
        fs::write(index.sibling_root.join("sibling.txt"), "sib").unwrap();

        index.index_under_stored_root(&["pruned/nested/gone.txt", "leaving.txt", "kept.txt"]);

        let report = index.updater().apply(&[
            // Both file and its parent directories are already gone on disk.
            PathChange::removed_file(index.real_root.join("pruned/nested/gone.txt")),
            // Truly moved outside the indexed root.
            PathChange::moved_file(
                index.real_root.join("leaving.txt"),
                index.outside_root.join("moved-out.txt"),
            ),
            // Similarly prefixed sibling directory must not match the stored root.
            PathChange::removed_file(index.sibling_root.join("sibling.txt")),
            // A final symlink outside the root pointing to the root must not be followed.
            PathChange::removed_directory(outside_link),
        ]);

        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!((report.moved, report.removed, report.not_indexed), (0, 2, 2));
        assert_eq!(index.indexed_relative(), vec!["kept.txt"]);
        let _keep_temp = &index.temp;
    }
}

#[cfg(test)]
mod test_shared_scope_and_volumes {
    use std::fs;

    use tempfile::TempDir;

    use super::*;
    use crate::types::{FileEntry, IndexedVolume, VolumeType};

    #[test]
    fn cross_volume_directory_move_updates_volume_id_and_handles_missing_parent() {
        let temp = TempDir::new().unwrap();
        let root_a = temp.path().join("root_a");
        let root_b = temp.path().join("root_b");
        fs::create_dir_all(&root_a).unwrap();
        fs::create_dir_all(&root_b).unwrap();

        let mut config = UserConfig::default();
        config.daemon.database_path = Some(temp.path().join("filefind.db"));
        config.daemon.paths = vec![
            root_a.to_string_lossy().into_owned(),
            root_b.to_string_lossy().into_owned(),
        ];

        let database = Database::open(&config.database_path()).unwrap();
        let volume_a = database
            .upsert_volume(&IndexedVolume::new(
                "VOL-A".into(),
                root_a.to_string_lossy().into_owned(),
                VolumeType::Local,
            ))
            .unwrap();
        let volume_b = database
            .upsert_volume(&IndexedVolume::new(
                "VOL-B".into(),
                root_b.to_string_lossy().into_owned(),
                VolumeType::Local,
            ))
            .unwrap();

        // Only the child is indexed.
        // The moved parent directory row itself is absent.
        let child_old = root_a.join("pack").join("episode.mkv");
        let child_new = root_b.join("pack").join("episode.mkv");
        database
            .insert_file(&FileEntry {
                id: None,
                volume_id: volume_a,
                parent_id: None,
                name: "episode.mkv".into(),
                full_path: child_old.to_string_lossy().into_owned(),
                is_directory: false,
                size: 12,
                created_time: None,
                modified_time: None,
                mft_reference: Some(99),
            })
            .unwrap();

        let updater = IndexUpdater::open_with_config(&config).unwrap().unwrap();
        let changes = [PathChange::moved_directory(root_a.join("pack"), root_b.join("pack"))];
        let report = updater.apply(&changes);
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(report.moved, 1);

        let moved = database
            .get_file_by_path(&child_new.to_string_lossy())
            .unwrap()
            .unwrap();
        assert_eq!(moved.volume_id, volume_b);
        assert_eq!(moved.mft_reference, None);
        assert!(
            database
                .get_file_by_path(&child_old.to_string_lossy())
                .unwrap()
                .is_none()
        );

        // Replay after daemon or prior run already applied the move is a no-op.
        let replay = updater.apply(&changes);
        assert_eq!((replay.moved, replay.removed, replay.not_indexed), (0, 0, 1));
    }

    #[test]
    fn move_into_excluded_pattern_or_hidden_file_removes_entry_from_index() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("library");
        fs::create_dir_all(&root).unwrap();

        let mut config = UserConfig::default();
        config.daemon.database_path = Some(temp.path().join("filefind.db"));
        config.daemon.paths = vec![root.to_string_lossy().into_owned()];
        config.daemon.exclude = vec!["ignored_dir".into()];
        config.daemon.exclude_patterns = vec!["*.tmp".into()];

        let database = Database::open(&config.database_path()).unwrap();
        let volume_id = database
            .upsert_volume(&IndexedVolume::new(
                "EXCL-VOL".into(),
                root.to_string_lossy().into_owned(),
                VolumeType::Local,
            ))
            .unwrap();

        for name in ["a.txt", "b.txt", "c.txt"] {
            let full_path = root.join(name);
            database
                .insert_file(&FileEntry::new(
                    volume_id,
                    name.into(),
                    full_path.to_string_lossy().into_owned(),
                    false,
                ))
                .unwrap();
        }

        let updater = IndexUpdater::open_with_config(&config).unwrap().unwrap();
        let report = updater.apply(&[
            PathChange::moved_file(root.join("a.txt"), root.join("a.tmp")),
            PathChange::moved_file(root.join("b.txt"), root.join(".hidden.txt")),
            PathChange::moved_file(root.join("c.txt"), root.join("ignored_dir").join("c.txt")),
        ]);

        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!((report.moved, report.removed), (0, 3));
        assert!(database.search_by_path("", 100).unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn skips_and_reports_non_utf8_paths_without_lossy_keys() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let temp = TempDir::new().unwrap();
        let root = temp.path().join("root");
        fs::create_dir_all(&root).unwrap();

        let mut config = UserConfig::default();
        config.daemon.database_path = Some(temp.path().join("filefind.db"));
        let database = Database::open(&config.database_path()).unwrap();
        database
            .upsert_volume(&IndexedVolume::new(
                "UTF8-VOL".into(),
                root.to_string_lossy().into_owned(),
                VolumeType::Local,
            ))
            .unwrap();

        let bad_path = root.join(OsStr::from_bytes(b"bad-\xff.txt"));
        let updater = IndexUpdater::open_with_config(&config).unwrap().unwrap();
        let report = updater.apply(&[PathChange::removed_file(bad_path)]);

        assert_eq!(report.errors.len(), 1);
        assert!(report.errors[0].contains("UTF-8"));
    }
}

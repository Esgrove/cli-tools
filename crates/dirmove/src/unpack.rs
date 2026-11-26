//! Unpacking configured directory names and deleting unwanted directories.
//!
//! `Unpacker::unpack` deletes the unwanted directories under the input roots,
//! moves the contents of every directory with an unpack name up into its parent,
//! and prunes the directories the moves left empty, reporting each step to an `UnpackObserver`.
//! The steps are also public, so callers can collect candidates and plan moves without moving anything.

use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::Result;
use walkdir::WalkDir;

use cli_tools_common::{TargetReservations, path_to_filename_string, unique_in_order};

use crate::name_filter;

/// Outcome of moving one entry out of an unpacked directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryMove {
    /// The entry was moved.
    Moved,
    /// The target already exists and overwriting is disabled.
    SkippedExisting,
}

/// One step of unpacking directories.
#[derive(Debug)]
pub enum UnpackEvent<'a> {
    /// An unwanted directory was deleted, or would be deleted in a dry run.
    UnwantedDeleted(&'a Path),
    /// An unwanted directory could not be deleted.
    UnwantedDeleteFailed {
        /// Directory that could not be deleted.
        directory: &'a Path,
        /// Why the deletion failed.
        error: &'a io::Error,
    },
    /// The moves that unpack a directory into its parent were planned.
    /// In a dry run nothing more happens for the directory.
    Planned {
        /// Directory receiving the contents.
        parent: &'a Path,
        /// Planned file and directory moves.
        plan: &'a UnpackPlan,
    },
    /// A directory was left in place because the target exists and overwriting is disabled.
    SkippedExistingDirectory(&'a Path),
    /// A file was left in place because the target exists and overwriting is disabled.
    SkippedExistingFile(&'a Path),
    /// An entry was left in place because another entry of the same directory moves to the same target,
    /// such as two names that only differ by case or by invalid UTF-8.
    DuplicateTarget(&'a Path),
    /// Moving an entry failed. The remaining entries are still moved.
    MoveFailed {
        /// Entry that could not be moved.
        source: &'a Path,
        /// Why the move failed.
        error: &'a anyhow::Error,
    },
    /// An empty directory left behind by the moves was removed.
    Pruned(&'a Path),
}

/// Unpacks directories with configured names into their parents.
#[derive(Debug, Clone, Default)]
pub struct Unpacker {
    /// Lowercase directory names to unpack.
    unpack_directory_names: Vec<String>,
    /// Whether existing targets are overwritten.
    overwrite: bool,
    /// Whether to only report what would happen, without changing anything.
    dry_run: bool,
}

/// Unwanted directories to delete and directories to unpack, found under the input roots.
#[derive(Debug, Default)]
pub struct UnpackCandidates {
    /// Unwanted directories, deepest first so nested ones are deleted before their parents.
    pub unwanted: Vec<PathBuf>,
    /// Outermost directories to unpack, deepest first.
    pub unpack: Vec<PathBuf>,
}

/// One planned move of a file or directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedMove {
    /// Current path of the file or directory.
    pub source: PathBuf,
    /// Path the file or directory moves to.
    pub target: PathBuf,
}

/// The moves that unpack one directory into its parent.
#[derive(Debug, Default)]
pub struct UnpackPlan {
    /// Files and symlinks to move.
    pub file_moves: Vec<PlannedMove>,
    /// Directories to move as a whole.
    pub directory_moves: Vec<PlannedMove>,
}

/// Receives the steps of unpacking directories.
///
/// The method does nothing by default, and `()` reports nothing.
pub trait UnpackObserver {
    /// One step of unpacking happened.
    fn on_unpack_event(&mut self, _event: UnpackEvent<'_>) {}
}

impl UnpackObserver for () {}

impl Unpacker {
    /// Create an unpacker for the given directory names, matched case-insensitively.
    ///
    /// Names are trimmed and lowercased, and duplicates are removed.
    /// With `overwrite`, existing targets are replaced instead of skipped.
    /// With `dry_run`, nothing is deleted or moved and the observer is told what would happen.
    #[must_use]
    pub fn new<N>(unpack_directory_names: N, overwrite: bool, dry_run: bool) -> Self
    where
        N: IntoIterator,
        N::Item: AsRef<str>,
    {
        let unpack_directory_names = unique_in_order(
            unpack_directory_names
                .into_iter()
                .map(|name| name.as_ref().trim().to_lowercase())
                .filter(|name| !name.is_empty()),
        );
        Self {
            unpack_directory_names,
            overwrite,
            dry_run,
        }
    }

    /// Delete unwanted directories and unpack configured directory names under every input root.
    ///
    /// For each directory to unpack, its entire contents move to its parent directory,
    /// preserving the structure below it.
    /// For example, `Example/Videos/Name/file2.txt` moves to `Example/Name/file2.txt` when unpacking "videos".
    /// A failed deletion or move is reported and the remaining entries are still handled.
    /// Empty directories left behind by the moves are pruned.
    /// Without `recurse`, only direct children of each input root are inspected.
    pub fn unpack(&self, input_roots: &[PathBuf], recurse: bool, observer: &mut impl UnpackObserver) {
        let candidates = self.collect_candidates(input_roots, recurse);

        for directory in &candidates.unwanted {
            if self.dry_run {
                observer.on_unpack_event(UnpackEvent::UnwantedDeleted(directory));
                continue;
            }
            match std::fs::remove_dir_all(directory) {
                Ok(()) => observer.on_unpack_event(UnpackEvent::UnwantedDeleted(directory)),
                Err(error) => observer.on_unpack_event(UnpackEvent::UnwantedDeleteFailed {
                    directory,
                    error: &error,
                }),
            }
        }

        let mut touched_directories: HashSet<PathBuf> = HashSet::new();
        let mut reservations = TargetReservations::new();
        for directory in candidates.unpack {
            if !directory.exists() {
                continue;
            }
            let Some(parent) = directory.parent().map(Path::to_path_buf) else {
                continue;
            };

            let plan = self.plan(&directory, &parent);
            observer.on_unpack_event(UnpackEvent::Planned {
                parent: &parent,
                plan: &plan,
            });
            if self.dry_run {
                continue;
            }

            for PlannedMove { source, target } in &plan.directory_moves {
                if !reservations.reserve(target) {
                    observer.on_unpack_event(UnpackEvent::DuplicateTarget(source));
                    continue;
                }
                match self.move_directory(source, target, &mut touched_directories) {
                    Ok(EntryMove::Moved) => {}
                    Ok(EntryMove::SkippedExisting) => {
                        observer.on_unpack_event(UnpackEvent::SkippedExistingDirectory(target));
                    }
                    Err(error) => observer.on_unpack_event(UnpackEvent::MoveFailed { source, error: &error }),
                }
            }
            for PlannedMove { source, target } in &plan.file_moves {
                if !reservations.reserve(target) {
                    observer.on_unpack_event(UnpackEvent::DuplicateTarget(source));
                    continue;
                }
                match self.move_file(source, target, &mut touched_directories) {
                    Ok(EntryMove::Moved) => {}
                    Ok(EntryMove::SkippedExisting) => {
                        observer.on_unpack_event(UnpackEvent::SkippedExistingFile(target));
                    }
                    Err(error) => observer.on_unpack_event(UnpackEvent::MoveFailed { source, error: &error }),
                }
            }

            touched_directories.insert(directory.clone());
            touched_directories.insert(parent);

            for pruned in prune_empty_directories(&directory, &mut touched_directories) {
                observer.on_unpack_event(UnpackEvent::Pruned(&pruned));
            }
        }
    }

    /// Collect unwanted directories to delete and unpack candidates from every input directory root.
    ///
    /// The input roots themselves are never candidates.
    /// Without `recurse`, only direct children of each input root are inspected.
    /// Nested candidates inside another candidate are left out,
    /// since unpacking the outer directory handles them.
    #[must_use]
    pub fn collect_candidates(&self, input_roots: &[PathBuf], recurse: bool) -> UnpackCandidates {
        let mut unwanted_directories = Vec::new();
        let mut unpack_candidates = Vec::new();

        for input_root in input_roots {
            if !input_root.is_dir() {
                continue;
            }

            let walker = if recurse {
                WalkDir::new(input_root)
            } else {
                WalkDir::new(input_root).max_depth(1)
            };

            for entry in walker
                .into_iter()
                .filter_entry(|entry| {
                    entry
                        .file_name()
                        .to_str()
                        .is_some_and(name_filter::is_unwanted_directory)
                        || !cli_tools_common::should_skip_entry(entry)
                })
                .filter_map(Result::ok)
            {
                if !entry.file_type().is_dir() || entry.path() == input_root {
                    continue;
                }

                let Some(name) = entry.path().file_name().and_then(|entry_name| entry_name.to_str()) else {
                    continue;
                };

                if name_filter::is_unwanted_directory(name) {
                    unwanted_directories.push(entry.path().to_path_buf());
                } else if self.is_unpack_name(name) {
                    unpack_candidates.push(entry.path().to_path_buf());
                }
            }
        }

        unwanted_directories.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
        unpack_candidates.sort_by_key(|path| path.components().count());

        let mut root_candidates: Vec<PathBuf> = Vec::new();
        for candidate in unpack_candidates {
            if !root_candidates
                .iter()
                .any(|root_candidate| candidate.starts_with(root_candidate))
            {
                root_candidates.push(candidate);
            }
        }
        root_candidates.sort_by_key(|path| std::cmp::Reverse(path.components().count()));

        UnpackCandidates {
            unwanted: unwanted_directories,
            unpack: root_candidates,
        }
    }

    /// Plan the moves that unpack `directory` into `parent`.
    ///
    /// Target names have invalid UTF-8 removed, and an entry whose name has nothing left is not moved.
    /// Files and symlinks move as they are, so unpacking never pulls files out of a directory a link points to.
    /// Nested directories with an unpack name are unpacked into the same parent.
    /// A directory holding a nested unpack directory is walked with itself as the new parent,
    /// and any other directory moves as a whole.
    #[must_use]
    pub fn plan(&self, directory: &Path, parent: &Path) -> UnpackPlan {
        let mut info = UnpackPlan::default();

        let Ok(entries) = std::fs::read_dir(directory) else {
            return info;
        };

        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            // Invalid UTF-8 is removed from the target name.
            // The moves reserve their targets, so two names that end up the same never overwrite each other.
            let name = path_to_filename_string(&path);
            if name.is_empty() {
                continue;
            }

            let target = parent.join(&name);
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_dir() {
                info.file_moves.push(PlannedMove { source: path, target });
            } else if self.is_unpack_name(&name) {
                let nested_info = self.plan(&path, parent);
                info.file_moves.extend(nested_info.file_moves);
                info.directory_moves.extend(nested_info.directory_moves);
            } else if self.contains_unpack_directory(&path) {
                let nested_info = self.plan(&path, &target);
                info.file_moves.extend(nested_info.file_moves);
                info.directory_moves.extend(nested_info.directory_moves);
            } else {
                info.directory_moves.push(PlannedMove { source: path, target });
            }
        }

        info
    }

    /// Check if a directory contains any subdirectories matching unpack names, recursively.
    ///
    /// Symlinks are not followed, matching how `plan` moves them as links.
    #[must_use]
    fn contains_unpack_directory(&self, directory: &Path) -> bool {
        let Ok(entries) = std::fs::read_dir(directory) else {
            return false;
        };

        entries.filter_map(Result::ok).any(|entry| {
            let path = entry.path();
            entry.file_type().is_ok_and(|file_type| file_type.is_dir())
                && (path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| self.is_unpack_name(name))
                    || self.contains_unpack_directory(&path))
        })
    }

    /// Move an entire directory to a new location.
    ///
    /// Tries a rename first, and falls back to a recursive copy and removal for cross-device moves.
    /// The parent of the source is recorded in `touched_directories`.
    fn move_directory(
        &self,
        source: &Path,
        target: &Path,
        touched_directories: &mut HashSet<PathBuf>,
    ) -> Result<EntryMove> {
        if std::fs::symlink_metadata(target).is_ok() && !self.overwrite {
            return Ok(EntryMove::SkippedExisting);
        }

        if let Some(parent) = source.parent() {
            touched_directories.insert(parent.to_path_buf());
        }

        crate::merge::move_directory_into_place(source, target, self.overwrite)?;

        Ok(EntryMove::Moved)
    }

    /// Move a single file, creating the target directory if needed.
    ///
    /// Tries a rename first, and falls back to a copy and removal for cross-device moves.
    /// The source and target parents are recorded in `touched_directories`.
    fn move_file(&self, source: &Path, target: &Path, touched_directories: &mut HashSet<PathBuf>) -> Result<EntryMove> {
        if std::fs::symlink_metadata(target).is_ok() && !self.overwrite {
            return Ok(EntryMove::SkippedExisting);
        }

        if let Some(target_parent) = target.parent() {
            if !target_parent.exists() {
                std::fs::create_dir_all(target_parent)?;
            }
            touched_directories.insert(target_parent.to_path_buf());
        }

        if let Some(source_parent) = source.parent() {
            touched_directories.insert(source_parent.to_path_buf());
        }

        if std::fs::rename(source, target).is_err() {
            cli_tools_common::copy_file_with_times(source, target)?;
            std::fs::remove_file(source)?;
        }

        Ok(EntryMove::Moved)
    }

    /// Return whether a directory name is one of the configured unpack names, ignoring case.
    fn is_unpack_name(&self, name: &str) -> bool {
        self.unpack_directory_names.contains(&name.to_lowercase())
    }
}

/// Prune empty directories under `root_directory` that the unpack moves touched.
///
/// Directories are visited deepest first, so children are removed before their parents.
/// Only `root_directory` itself and directories in `touched_directories` are removed,
/// and the parent of every removed directory is marked as touched.
/// Returns the removed directories in removal order.
fn prune_empty_directories(root_directory: &Path, touched_directories: &mut HashSet<PathBuf>) -> Vec<PathBuf> {
    let mut pruned = Vec::new();
    if !root_directory.exists() {
        return pruned;
    }

    let mut directories: Vec<PathBuf> = WalkDir::new(root_directory)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_dir())
        .map(|entry| entry.path().to_path_buf())
        .collect();
    directories.sort_by_key(|path| std::cmp::Reverse(path.components().count()));

    for directory in directories {
        if directory != root_directory && !touched_directories.contains(&directory) {
            continue;
        }

        let is_empty = std::fs::read_dir(&directory).is_ok_and(|mut entries| entries.next().is_none());
        if !is_empty {
            continue;
        }

        if std::fs::remove_dir(&directory).is_ok() {
            if let Some(parent) = directory.parent() {
                touched_directories.insert(parent.to_path_buf());
            }
            pruned.push(directory);
        }
    }

    pruned
}

#[cfg(test)]
mod test_unpacker {
    use super::*;
    use crate::test_helpers::write_file;
    #[cfg(unix)]
    use cli_tools_common::test_support::TempTree;
    use tempfile::TempDir;

    #[test]
    fn plan_counts_files_and_directories() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Example");
        std::fs::create_dir_all(&root)?;

        let videos = root.join("Videos");
        write_file(&videos.join("file1.txt"), "1")?;
        write_file(&videos.join("file2.txt"), "2")?;
        write_file(&videos.join("file3.txt"), "3")?;
        write_file(&videos.join("Dir1").join("nested.txt"), "nested")?;
        write_file(&videos.join("Dir2").join("nested.txt"), "nested")?;

        let info = Unpacker::new(["videos"], false, false).plan(&videos, &root);

        assert_eq!(info.file_moves.len(), 3);
        assert_eq!(info.directory_moves.len(), 2);
        Ok(())
    }

    #[test]
    fn collect_candidates_filters_nested() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Test");
        std::fs::create_dir_all(&root)?;

        let path = root.join("videos").join("updates").join("downloads");
        std::fs::create_dir_all(&path)?;
        write_file(&path.join("file.txt"), "x")?;

        let candidates = Unpacker::new(["videos", "updates", "downloads"], false, false)
            .collect_candidates(std::slice::from_ref(&root), true);

        assert_eq!(candidates.unpack, vec![root.join("videos")]);
        Ok(())
    }

    #[test]
    fn contains_unpack_directory_searches_nested_directories() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Root");
        std::fs::create_dir_all(&root)?;

        let videos = root.join("A").join("B").join("videos");
        write_file(&videos.join("file.txt"), "x")?;
        write_file(&root.join("C").join("nothing_special").join("file.txt"), "y")?;

        let unpacker = Unpacker::new(["videos"], false, false);

        assert!(unpacker.contains_unpack_directory(&root.join("A")));
        assert!(unpacker.contains_unpack_directory(&root.join("A").join("B")));
        assert!(!unpacker.contains_unpack_directory(&root.join("C")));
        Ok(())
    }

    #[test]
    fn collect_candidates_finds_unwanted_and_unpack_directories() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Example");
        std::fs::create_dir_all(&root)?;
        let unwanted = root.join(".unwanted");
        std::fs::create_dir(&unwanted)?;
        let videos = root.join("videos");
        std::fs::create_dir(&videos)?;
        write_file(&videos.join("file.txt"), "x")?;

        let candidates = Unpacker::new(["videos"], false, false).collect_candidates(&[root], false);

        assert_eq!(candidates.unwanted, vec![unwanted]);
        assert_eq!(candidates.unpack, vec![videos]);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn prune_does_not_follow_a_symlink_to_an_empty_directory() {
        let tree = TempTree::new();
        tree.dir("outside/empty target");
        tree.dir("Library/Movie/subs/touched");
        tree.symlink("outside/empty target", "Library/Movie/subs/touched/link");
        let mut touched_directories: HashSet<PathBuf> = HashSet::from([
            tree.join("Library/Movie/subs/touched"),
            tree.join("Library/Movie/subs/touched/link"),
            tree.join("outside/empty target"),
        ]);

        let _ = prune_empty_directories(&tree.join("Library/Movie/subs"), &mut touched_directories);

        assert!(tree.exists("outside/empty target"));
        assert!(tree.exists("Library/Movie/subs/touched/link"));
    }
}

#[cfg(test)]
mod test_unpack {
    use super::*;
    use crate::test_helpers::{assert_exists, assert_not_exists, unpack, write_file};
    use tempfile::TempDir;

    #[test]
    fn a_failed_move_is_reported_and_the_rest_are_still_moved() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Example");
        // The file named like its unpack directory cannot replace that directory, so its move fails.
        write_file(&root.join("Videos").join("Videos"), "same name as the parent")?;
        write_file(&root.join("Videos").join("a.txt"), "a")?;
        let root_for_asserts = root.clone();

        unpack(&root, &["videos"], false, false, true);

        assert_eq!(std::fs::read_to_string(root_for_asserts.join("a.txt"))?, "a");
        Ok(())
    }

    #[test]
    fn basic_preserves_structure_and_removes_matched_dir() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Example");
        std::fs::create_dir_all(&root)?;

        let videos = root.join("Videos");
        write_file(&videos.join("Name").join("file2.txt"), "file2")?;
        write_file(&videos.join("file1.txt"), "file1")?;

        let root_for_asserts = root.clone();

        unpack(&root, &["videos"], true, false, false);

        assert_exists(&root_for_asserts.join("Name").join("file2.txt"));
        assert_exists(&root_for_asserts.join("file1.txt"));
        assert_not_exists(&root_for_asserts.join("Videos"));

        Ok(())
    }

    #[test]
    fn case_insensitive_dirname_match() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Example");
        std::fs::create_dir_all(&root)?;

        let videos = root.join("ViDeOs");
        write_file(&videos.join("Nested").join("file.txt"), "x")?;

        let root_for_asserts = root.clone();

        unpack(&root, &["videos"], true, false, false);

        assert_exists(&root_for_asserts.join("Nested").join("file.txt"));
        assert_not_exists(&root_for_asserts.join("ViDeOs"));

        Ok(())
    }

    #[test]
    fn does_not_prune_unrelated_empty_dirs() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Example");
        std::fs::create_dir_all(&root)?;

        let unrelated_empty = root.join("EmptyUnrelated");
        std::fs::create_dir_all(&unrelated_empty)?;

        let matched_empty = root.join("Videos");
        std::fs::create_dir_all(&matched_empty)?;

        let root_for_asserts = root.clone();

        unpack(&root, &["videos"], false, false, false);

        assert_exists(&root_for_asserts.join("EmptyUnrelated"));
        assert_not_exists(&root_for_asserts.join("Videos"));

        Ok(())
    }

    #[test]
    fn moves_non_matching_dirs_directly() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Example");
        std::fs::create_dir_all(&root)?;

        let videos = root.join("Videos");
        let subdir = videos.join("SubDir");
        write_file(&subdir.join("nested").join("deep.txt"), "deep content")?;
        write_file(&subdir.join("file.txt"), "file content")?;

        let root_for_asserts = root.clone();

        unpack(&root, &["videos"], true, false, false);

        assert_exists(&root_for_asserts.join("SubDir").join("nested").join("deep.txt"));
        assert_exists(&root_for_asserts.join("SubDir").join("file.txt"));
        assert_not_exists(&root_for_asserts.join("Videos"));

        Ok(())
    }

    #[test]
    fn dry_run_does_not_modify_files() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Example");
        std::fs::create_dir_all(&root)?;

        let videos = root.join("Videos");
        write_file(&videos.join("file1.txt"), "content1")?;
        write_file(&videos.join("SubDir").join("file2.txt"), "content2")?;

        let root_for_asserts = root.clone();

        unpack(&root, &["videos"], true, true, false);

        assert_exists(&root_for_asserts.join("Videos").join("file1.txt"));
        assert_exists(&root_for_asserts.join("Videos").join("SubDir").join("file2.txt"));
        assert_not_exists(&root_for_asserts.join("file1.txt"));
        assert_not_exists(&root_for_asserts.join("SubDir"));

        Ok(())
    }

    #[test]
    fn nested_matching_dirs_are_flattened() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Example");
        std::fs::create_dir_all(&root)?;

        let outer_videos = root.join("Videos");
        let inner_videos = outer_videos.join("Videos");
        write_file(&inner_videos.join("file.txt"), "nested content")?;

        let root_for_asserts = root.clone();

        unpack(&root, &["videos"], true, false, false);

        assert_exists(&root_for_asserts.join("file.txt"));
        assert_not_exists(&root_for_asserts.join("Videos"));

        Ok(())
    }

    #[test]
    fn multiple_unpack_names() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Example");
        std::fs::create_dir_all(&root)?;

        let videos = root.join("Videos");
        let extras = root.join("Extras");
        write_file(&videos.join("video.mp4"), "video")?;
        write_file(&extras.join("extra.txt"), "extra")?;

        let root_for_asserts = root.clone();

        unpack(&root, &["videos", "extras"], true, false, false);

        assert_exists(&root_for_asserts.join("video.mp4"));
        assert_exists(&root_for_asserts.join("extra.txt"));
        assert_not_exists(&root_for_asserts.join("Videos"));
        assert_not_exists(&root_for_asserts.join("Extras"));

        Ok(())
    }

    #[test]
    fn skips_existing_file_without_overwrite() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Example");
        std::fs::create_dir_all(&root)?;

        let videos = root.join("Videos");
        write_file(&videos.join("conflict.txt"), "from videos")?;
        write_file(&root.join("conflict.txt"), "original")?;

        let root_for_asserts = root.clone();

        unpack(&root, &["videos"], true, false, false);

        let content = std::fs::read_to_string(root_for_asserts.join("conflict.txt"))?;
        assert_eq!(content, "original");

        Ok(())
    }

    #[test]
    fn overwrites_existing_file_with_overwrite() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Example");
        std::fs::create_dir_all(&root)?;

        let videos = root.join("Videos");
        write_file(&videos.join("conflict.txt"), "from videos")?;
        write_file(&root.join("conflict.txt"), "original")?;

        let root_for_asserts = root.clone();

        unpack(&root, &["videos"], true, false, true);

        let content = std::fs::read_to_string(root_for_asserts.join("conflict.txt"))?;
        assert_eq!(content, "from videos");

        Ok(())
    }

    #[test]
    fn skips_existing_directory_without_overwrite() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Example");
        std::fs::create_dir_all(&root)?;

        let videos = root.join("Videos");
        write_file(&videos.join("SubDir").join("new.txt"), "new content")?;
        write_file(&root.join("SubDir").join("existing.txt"), "existing content")?;

        let root_for_asserts = root.clone();

        unpack(&root, &["videos"], true, false, false);

        assert_exists(&root_for_asserts.join("SubDir").join("existing.txt"));
        assert_not_exists(&root_for_asserts.join("SubDir").join("new.txt"));

        Ok(())
    }

    #[test]
    fn non_recursive_only_checks_root_level() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Example");
        std::fs::create_dir_all(&root)?;

        let nested = root.join("Parent").join("Videos");
        write_file(&nested.join("file.txt"), "nested")?;

        let root_videos = root.join("Videos");
        write_file(&root_videos.join("root_file.txt"), "root")?;

        let root_for_asserts = root.clone();

        unpack(&root, &["videos"], false, false, false);

        assert_exists(&root_for_asserts.join("root_file.txt"));
        assert_not_exists(&root_for_asserts.join("Videos"));
        assert_exists(&root_for_asserts.join("Parent").join("Videos").join("file.txt"));

        Ok(())
    }

    #[test]
    fn deeply_nested_structure() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Example");
        std::fs::create_dir_all(&root)?;

        let videos = root.join("Videos");
        write_file(&videos.join("A").join("B").join("C").join("deep.txt"), "deep")?;
        write_file(&videos.join("A").join("shallow.txt"), "shallow")?;

        let root_for_asserts = root.clone();

        unpack(&root, &["videos"], true, false, false);

        assert_exists(&root_for_asserts.join("A").join("B").join("C").join("deep.txt"));
        assert_exists(&root_for_asserts.join("A").join("shallow.txt"));
        assert_not_exists(&root_for_asserts.join("Videos"));

        Ok(())
    }

    #[test]
    fn mixed_files_and_directories() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Example");
        std::fs::create_dir_all(&root)?;

        let videos = root.join("Videos");
        write_file(&videos.join("file1.txt"), "file1")?;
        write_file(&videos.join("file2.txt"), "file2")?;
        write_file(&videos.join("Dir1").join("nested1.txt"), "nested1")?;
        write_file(&videos.join("Dir2").join("nested2.txt"), "nested2")?;

        let root_for_asserts = root.clone();

        unpack(&root, &["videos"], true, false, false);

        assert_exists(&root_for_asserts.join("file1.txt"));
        assert_exists(&root_for_asserts.join("file2.txt"));
        assert_exists(&root_for_asserts.join("Dir1").join("nested1.txt"));
        assert_exists(&root_for_asserts.join("Dir2").join("nested2.txt"));
        assert_not_exists(&root_for_asserts.join("Videos"));

        Ok(())
    }

    #[test]
    fn empty_unpack_names_does_nothing() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Example");
        std::fs::create_dir_all(&root)?;

        let videos = root.join("Videos");
        write_file(&videos.join("file.txt"), "content")?;

        let root_for_asserts = root.clone();

        unpack(&root, &[], true, false, false);

        assert_exists(&root_for_asserts.join("Videos").join("file.txt"));
        assert_not_exists(&root_for_asserts.join("file.txt"));

        Ok(())
    }

    #[test]
    fn no_matching_directories() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Example");
        std::fs::create_dir_all(&root)?;

        let other = root.join("Other");
        write_file(&other.join("file.txt"), "content")?;

        let root_for_asserts = root.clone();

        unpack(&root, &["videos"], true, false, false);

        assert_exists(&root_for_asserts.join("Other").join("file.txt"));

        Ok(())
    }

    #[test]
    fn multiple_matching_dirs_at_different_levels() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Example");
        std::fs::create_dir_all(&root)?;

        let root_videos = root.join("Videos");
        write_file(&root_videos.join("file1.txt"), "file1")?;

        let nested_videos = root.join("Parent").join("Videos");
        write_file(&nested_videos.join("file2.txt"), "file2")?;

        let root_for_asserts = root.clone();

        unpack(&root, &["videos"], true, false, false);

        assert_exists(&root_for_asserts.join("file1.txt"));
        assert_exists(&root_for_asserts.join("Parent").join("file2.txt"));
        assert_not_exists(&root_for_asserts.join("Videos"));
        assert_not_exists(&root_for_asserts.join("Parent").join("Videos"));

        Ok(())
    }

    #[test]
    fn preserves_file_content() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Example");
        std::fs::create_dir_all(&root)?;

        let videos = root.join("Videos");
        let original_content = "This is the original file content with special chars: äöü 日本語";
        write_file(&videos.join("content.txt"), original_content)?;

        let root_for_asserts = root.clone();

        unpack(&root, &["videos"], true, false, false);

        let moved_content = std::fs::read_to_string(root_for_asserts.join("content.txt"))?;
        assert_eq!(moved_content, original_content);

        Ok(())
    }

    #[test]
    fn handles_special_characters_in_names() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Example");
        std::fs::create_dir_all(&root)?;

        let videos = root.join("Videos");
        write_file(&videos.join("file with spaces.txt"), "spaces")?;
        write_file(&videos.join("file-with-dashes.txt"), "dashes")?;
        write_file(&videos.join("file_with_underscores.txt"), "underscores")?;
        write_file(&videos.join("Dir With Spaces").join("nested.txt"), "nested")?;

        let root_for_asserts = root.clone();

        unpack(&root, &["videos"], true, false, false);

        assert_exists(&root_for_asserts.join("file with spaces.txt"));
        assert_exists(&root_for_asserts.join("file-with-dashes.txt"));
        assert_exists(&root_for_asserts.join("file_with_underscores.txt"));
        assert_exists(&root_for_asserts.join("Dir With Spaces").join("nested.txt"));
        assert_not_exists(&root_for_asserts.join("Videos"));

        Ok(())
    }

    #[test]
    fn alternating_match_non_match_dirs() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Example");
        std::fs::create_dir_all(&root)?;

        let path = root.join("Videos").join("Other").join("Videos");
        write_file(&path.join("file.txt"), "deep")?;

        let root_for_asserts = root.clone();

        unpack(&root, &["videos"], true, false, false);

        assert_exists(&root_for_asserts.join("Other").join("file.txt"));
        assert_not_exists(&root_for_asserts.join("Videos"));
        assert_not_exists(&root_for_asserts.join("Other").join("Videos"));

        Ok(())
    }

    #[test]
    fn dry_run_preserves_empty_matched_directory() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Example");
        std::fs::create_dir_all(&root)?;

        let videos = root.join("Videos");
        std::fs::create_dir_all(&videos)?;

        let root_for_asserts = root.clone();

        unpack(&root, &["videos"], true, true, false);

        assert_exists(&root_for_asserts.join("Videos"));

        Ok(())
    }

    #[test]
    fn nested_chain_single_summary() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Project");
        std::fs::create_dir_all(&root)?;

        let deep_path = root.join("updates").join("1").join("videos");
        write_file(&deep_path.join("file.txt"), "content")?;
        write_file(&deep_path.join("another.txt"), "more content")?;

        let root_for_asserts = root.clone();

        unpack(&root, &["updates", "1", "videos"], true, false, false);

        assert_exists(&root_for_asserts.join("file.txt"));
        assert_exists(&root_for_asserts.join("another.txt"));
        assert_not_exists(&root_for_asserts.join("updates"));

        Ok(())
    }

    #[test]
    fn nested_chain_with_non_matching_dir_between() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Project");
        std::fs::create_dir_all(&root)?;

        let path = root.join("updates").join("KeepThis").join("videos");
        write_file(&path.join("file.txt"), "content")?;

        let root_for_asserts = root.clone();

        unpack(&root, &["updates", "videos"], true, false, false);

        assert_exists(&root_for_asserts.join("KeepThis").join("file.txt"));
        assert_not_exists(&root_for_asserts.join("updates"));
        assert_not_exists(&root_for_asserts.join("KeepThis").join("videos"));

        Ok(())
    }

    #[test]
    fn nested_chain_multiple_non_matching_dirs() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Root");
        std::fs::create_dir_all(&root)?;

        let path = root
            .join("updates")
            .join("A")
            .join("videos")
            .join("B")
            .join("downloads");
        write_file(&path.join("file.txt"), "content")?;

        let root_for_asserts = root.clone();

        unpack(&root, &["updates", "videos", "downloads"], true, false, false);

        assert_exists(&root_for_asserts.join("A").join("B").join("file.txt"));
        assert_not_exists(&root_for_asserts.join("updates"));

        Ok(())
    }

    #[test]
    fn separate_trees_get_separate_processing() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Root");
        std::fs::create_dir_all(&root)?;

        write_file(&root.join("DirA").join("videos").join("file1.txt"), "1")?;
        write_file(&root.join("DirB").join("videos").join("file2.txt"), "2")?;

        let root_for_asserts = root.clone();

        unpack(&root, &["videos"], true, false, false);

        assert_exists(&root_for_asserts.join("DirA").join("file1.txt"));
        assert_exists(&root_for_asserts.join("DirB").join("file2.txt"));
        assert_not_exists(&root_for_asserts.join("DirA").join("videos"));
        assert_not_exists(&root_for_asserts.join("DirB").join("videos"));

        Ok(())
    }

    #[test]
    fn chain_with_files_at_multiple_levels() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Root");
        std::fs::create_dir_all(&root)?;

        let updates = root.join("updates");
        write_file(&updates.join("file1.txt"), "1")?;
        write_file(&updates.join("videos").join("file2.txt"), "2")?;
        write_file(&updates.join("videos").join("downloads").join("file3.txt"), "3")?;

        let root_for_asserts = root.clone();

        unpack(&root, &["updates", "videos", "downloads"], true, false, false);

        assert_exists(&root_for_asserts.join("file1.txt"));
        assert_exists(&root_for_asserts.join("file2.txt"));
        assert_exists(&root_for_asserts.join("file3.txt"));
        assert_not_exists(&root_for_asserts.join("updates"));

        Ok(())
    }

    #[test]
    fn chain_with_non_matching_subdirs_moved_directly() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Root");
        std::fs::create_dir_all(&root)?;

        let keep_me = root.join("updates").join("KeepMe");
        write_file(&keep_me.join("deep").join("file.txt"), "content")?;

        let root_for_asserts = root.clone();

        unpack(&root, &["updates"], true, false, false);

        assert_exists(&root_for_asserts.join("KeepMe").join("deep").join("file.txt"));
        assert_not_exists(&root_for_asserts.join("updates"));

        Ok(())
    }

    #[test]
    fn unpack_checks_all_input_roots() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let input_root_a = tmp.path().join("input_a");
        let input_root_b = tmp.path().join("input_b");
        let unpack_directory_a = input_root_a.join("ExampleA").join("Videos");
        let unpack_directory_b = input_root_b.join("ExampleB").join("Videos");
        std::fs::create_dir_all(&unpack_directory_a)?;
        std::fs::create_dir_all(&unpack_directory_b)?;
        std::fs::write(unpack_directory_a.join("a.mp4"), "a")?;
        std::fs::write(unpack_directory_b.join("b.mp4"), "b")?;

        Unpacker::new(["Videos"], false, false).unpack(&[input_root_a.clone(), input_root_b.clone()], true, &mut ());

        assert_not_exists(&unpack_directory_a);
        assert_not_exists(&unpack_directory_b);
        assert_exists(&input_root_a.join("ExampleA").join("a.mp4"));
        assert_exists(&input_root_b.join("ExampleB").join("b.mp4"));
        Ok(())
    }
}

#[cfg(test)]
mod test_unwanted_directories {
    use crate::test_helpers::{assert_exists, assert_not_exists, unpack, write_file};
    use tempfile::TempDir;

    #[test]
    fn deleted() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Example");
        std::fs::create_dir_all(&root)?;
        let unwanted = root.join(".unwanted");
        std::fs::create_dir(&unwanted)?;
        write_file(&unwanted.join("junk.txt"), "junk")?;

        unpack(&root, &[], false, false, false);

        assert_not_exists(&unwanted);
        Ok(())
    }

    #[test]
    fn deleted_recursive() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Example");
        std::fs::create_dir_all(&root)?;
        let subdir = root.join("subdir");
        std::fs::create_dir(&subdir)?;
        let unwanted = subdir.join(".unwanted");
        std::fs::create_dir(&unwanted)?;
        write_file(&unwanted.join("junk.txt"), "junk")?;

        unpack(&root, &[], true, false, false);

        assert_not_exists(&unwanted);
        assert_exists(&subdir);
        Ok(())
    }

    #[test]
    fn dry_run_preserves() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Example");
        std::fs::create_dir_all(&root)?;
        let unwanted = root.join(".unwanted");
        std::fs::create_dir(&unwanted)?;
        write_file(&unwanted.join("junk.txt"), "junk")?;

        unpack(&root, &[], false, true, false);

        assert_exists(&unwanted);
        Ok(())
    }

    #[test]
    fn case_insensitive() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Example");
        std::fs::create_dir_all(&root)?;
        let unwanted = root.join(".Unwanted");
        std::fs::create_dir(&unwanted)?;
        write_file(&unwanted.join("junk.txt"), "junk")?;

        unpack(&root, &[], false, false, false);

        assert_not_exists(&unwanted);
        Ok(())
    }
}

/// Nested tree and filesystem edge case tests for unpacking.
///
/// Every test builds a tree several levels deep with `TempTree`
/// and asserts the complete resulting tree, so a lost, duplicated, or misplaced entry fails the test.
#[cfg(test)]
mod test_unpack_trees {
    use cli_tools_common::test_support::TempTree;

    /// Run the unpack pass for "subs" and "sample" on the "Library" directory of the tree.
    fn unpack(tree: &TempTree, recurse: bool, dry_run: bool) {
        crate::test_helpers::unpack(&tree.join("Library"), &["subs", "sample"], recurse, dry_run, false);
    }

    /// Tree with unpack directories at several depths,
    /// hidden files inside them, and an unrelated empty directory.
    fn unpack_tree() -> TempTree {
        let tree = TempTree::new();
        tree.file_with("Library/Show A/Season 1/episode.mkv", "episode");
        tree.file_with("Library/Show A/Season 1/subs/english.srt", "english");
        tree.file_with("Library/Show A/Season 1/subs/.hidden.nfo", "hidden");
        tree.file_with("Library/Show A/Season 1/subs/languages/.deep hidden.srt", "deep hidden");
        tree.file_with("Library/Show A/Season 1/subs/languages/finnish.srt", "finnish");
        tree.file_with("Library/Show B/extras/sample/clip.mkv", "clip");
        tree.file_with("Library/Show B/extras/sample/sample/inner clip.mkv", "inner clip");
        tree.dir("Library/Show B/unrelated empty");
        tree
    }

    #[test]
    fn recursive_unpack_moves_every_file_including_hidden_ones() {
        let tree = unpack_tree();

        unpack(&tree, true, false);

        assert_eq!(
            tree.snapshot(),
            vec![
                "Library/",
                "Library/Show A/",
                "Library/Show A/Season 1/",
                "Library/Show A/Season 1/.hidden.nfo: hidden",
                "Library/Show A/Season 1/english.srt: english",
                "Library/Show A/Season 1/episode.mkv: episode",
                "Library/Show A/Season 1/languages/",
                "Library/Show A/Season 1/languages/.deep hidden.srt: deep hidden",
                "Library/Show A/Season 1/languages/finnish.srt: finnish",
                "Library/Show B/",
                "Library/Show B/extras/",
                "Library/Show B/extras/clip.mkv: clip",
                "Library/Show B/extras/inner clip.mkv: inner clip",
                "Library/Show B/unrelated empty/",
            ]
        );
    }

    #[test]
    fn unpack_is_idempotent() {
        let tree = unpack_tree();
        unpack(&tree, true, false);
        let after_first_run = tree.snapshot();

        unpack(&tree, true, false);

        assert_eq!(tree.snapshot(), after_first_run);
    }

    #[test]
    fn non_recursive_unpack_leaves_nested_unpack_directories_alone() {
        let tree = unpack_tree();
        let before = tree.snapshot();

        unpack(&tree, false, false);

        assert_eq!(tree.snapshot(), before);
    }

    #[test]
    fn dry_run_unpack_on_a_deep_tree_changes_nothing() {
        let tree = unpack_tree();
        let before = tree.snapshot();

        unpack(&tree, true, true);

        assert_eq!(tree.snapshot(), before);
    }

    #[cfg(unix)]
    #[test]
    fn unpack_does_not_pull_files_out_of_a_linked_unpack_directory() {
        let tree = TempTree::new();
        tree.file_with("outside/real/data.txt", "outside data");
        tree.file_with("Library/Movie/subs/english.srt", "english");
        tree.symlink("outside/real", "Library/Movie/subs/sample");

        unpack(&tree, true, false);

        assert_eq!(tree.read("outside/real/data.txt"), "outside data");
        assert!(!tree.exists("Library/Movie/data.txt"));
        assert_eq!(tree.read("Library/Movie/english.srt"), "english");
    }

    #[cfg(unix)]
    #[test]
    fn unpack_moves_a_linked_directory_as_a_link() {
        let tree = TempTree::new();
        tree.file_with("outside/real/data.txt", "outside data");
        tree.file_with("Library/Movie/subs/english.srt", "english");
        tree.symlink("outside/real", "Library/Movie/subs/linked");

        unpack(&tree, true, false);

        assert_eq!(tree.read("outside/real/data.txt"), "outside data");
        let link = tree
            .join("Library/Movie/linked")
            .symlink_metadata()
            .expect("the link should be moved next to the unpacked files");
        assert!(link.file_type().is_symlink());
        assert!(!tree.exists("Library/Movie/subs"));
    }

    #[cfg(unix)]
    #[test]
    fn pruning_keeps_a_directory_that_only_holds_a_symlink() {
        let tree = TempTree::new();
        tree.file("outside/target.txt");
        tree.file_with("Library/Movie/subs/english.srt", "english");
        tree.symlink("outside/target.txt", "Library/Movie/subs/keeper/link.txt");

        unpack(&tree, true, false);

        assert!(tree.exists("outside/target.txt"));
        assert!(
            tree.join("Library/Movie/keeper/link.txt")
                .symlink_metadata()
                .expect("the link should survive")
                .file_type()
                .is_symlink()
        );
    }

    #[cfg(unix)]
    #[test]
    fn unpack_does_not_follow_destination_symlink_or_overwrite_broken_symlink() {
        let tree = TempTree::new();
        tree.dir("outside/real");
        tree.file_with("Library/Movie/subs/Sub/a.txt", "from unpack");
        tree.symlink("outside/real", "Library/Movie/Sub");

        // With overwrite = true, unpacking Sub must replace the symlink, not copy into outside/real.
        let names = vec!["subs".to_string()];
        super::Unpacker::new(&names, true, false).unpack(&[tree.join("Library")], true, &mut ());
        assert!(
            !tree.exists("outside/real/a.txt"),
            "unpack must not follow a destination symlink into outside/real"
        );

        // With overwrite = false, a broken symlink at target must not be overwritten.
        let tree2 = TempTree::new();
        tree2.file_with("Library/Movie/subs/broken.srt", "new sub");
        tree2.symlink("outside/missing.srt", "Library/Movie/broken.srt");
        super::Unpacker::new(&names, false, false).unpack(&[tree2.join("Library")], true, &mut ());
        assert!(
            tree2
                .join("Library/Movie/broken.srt")
                .symlink_metadata()
                .expect("broken symlink")
                .file_type()
                .is_symlink(),
            "broken symlink at unpack target must be preserved when overwrite is false"
        );
        assert!(tree2.exists("Library/Movie/subs/broken.srt"));
    }

    #[test]
    fn sibling_unpack_directories_do_not_overwrite_each_other_under_force() {
        let tree = TempTree::new();
        tree.file_with("Example/Videos/clip.mp4", "from videos");
        tree.file_with("Example/Extras/clip.mp4", "from extras");

        let names = vec!["videos".to_string(), "extras".to_string()];
        super::Unpacker::new(&names, true, false).unpack(&[tree.join("Example")], true, &mut ());

        assert!(
            tree.exists("Example/clip.mp4"),
            "first unpacked file should land at Example/clip.mp4"
        );
        let remaining_in_sources =
            usize::from(tree.exists("Example/Videos/clip.mp4")) + usize::from(tree.exists("Example/Extras/clip.mp4"));
        assert_eq!(
            remaining_in_sources,
            1,
            "the colliding second file must stay in its source directory instead of overwriting the first: {:?}",
            tree.snapshot()
        );
    }
}

#[cfg(test)]
mod test_unpack_events {
    use super::*;
    use cli_tools_common::test_support::TempTree;

    /// Observer that records each unpack event as a short description.
    #[derive(Default)]
    struct EventRecorder {
        /// Event descriptions in the order they were reported.
        events: Vec<String>,
    }

    impl UnpackObserver for EventRecorder {
        fn on_unpack_event(&mut self, event: UnpackEvent<'_>) {
            let description = match event {
                UnpackEvent::UnwantedDeleted(path) => format!("unwanted {}", path_to_filename_string(path)),
                UnpackEvent::UnwantedDeleteFailed { directory, .. } => {
                    format!("unwanted failed {}", path_to_filename_string(directory))
                }
                UnpackEvent::Planned { parent, plan } => format!(
                    "planned {} files {} directories into {}",
                    plan.file_moves.len(),
                    plan.directory_moves.len(),
                    path_to_filename_string(parent)
                ),
                UnpackEvent::SkippedExistingDirectory(path) => {
                    format!("skipped directory {}", path_to_filename_string(path))
                }
                UnpackEvent::SkippedExistingFile(path) => format!("skipped file {}", path_to_filename_string(path)),
                UnpackEvent::DuplicateTarget(path) => format!("duplicate {}", path_to_filename_string(path)),
                UnpackEvent::MoveFailed { source, .. } => format!("failed {}", path_to_filename_string(source)),
                UnpackEvent::Pruned(path) => format!("pruned {}", path_to_filename_string(path)),
            };
            self.events.push(description);
        }
    }

    /// Unpack "subs" under the "Library" directory of the tree and return the recorded events.
    fn unpack_recorded(tree: &TempTree, overwrite: bool, dry_run: bool) -> Vec<String> {
        let mut recorder = EventRecorder::default();
        Unpacker::new(["subs"], overwrite, dry_run).unpack(&[tree.join("Library")], true, &mut recorder);
        recorder.events
    }

    #[test]
    fn reports_unwanted_deletion_plan_and_pruning() {
        let tree = TempTree::new();
        tree.file_with("Library/.unwanted/junk.txt", "junk");
        tree.file_with("Library/Movie/subs/english.srt", "english");
        tree.file_with("Library/Movie/subs/extra/finnish.srt", "finnish");

        let events = unpack_recorded(&tree, false, false);

        assert_eq!(
            events,
            [
                "unwanted .unwanted",
                "planned 1 files 1 directories into Movie",
                "pruned subs"
            ]
        );
        assert_eq!(
            tree.snapshot(),
            [
                "Library/",
                "Library/Movie/",
                "Library/Movie/english.srt: english",
                "Library/Movie/extra/",
                "Library/Movie/extra/finnish.srt: finnish",
            ]
        );
    }

    #[test]
    fn dry_run_reports_without_changing_anything() {
        let tree = TempTree::new();
        tree.file_with("Library/.unwanted/junk.txt", "junk");
        tree.file_with("Library/Movie/subs/english.srt", "english");
        let before = tree.snapshot();

        let events = unpack_recorded(&tree, false, true);

        assert_eq!(
            events,
            ["unwanted .unwanted", "planned 1 files 0 directories into Movie"]
        );
        assert_eq!(tree.snapshot(), before);
    }

    #[test]
    fn existing_targets_are_skipped_and_the_source_is_kept() {
        let tree = TempTree::new();
        tree.file_with("Library/Movie/english.srt", "existing");
        tree.file_with("Library/Movie/extra/old.txt", "old");
        tree.file_with("Library/Movie/subs/english.srt", "new");
        tree.file_with("Library/Movie/subs/extra/new.txt", "new");

        let events = unpack_recorded(&tree, false, false);

        assert_eq!(
            events,
            [
                "planned 1 files 1 directories into Movie",
                "skipped directory extra",
                "skipped file english.srt"
            ]
        );
        assert_eq!(tree.read("Library/Movie/english.srt"), "existing");
        assert_eq!(tree.read("Library/Movie/subs/english.srt"), "new");
        assert_eq!(tree.read("Library/Movie/subs/extra/new.txt"), "new");
        assert!(!tree.exists("Library/Movie/extra/new.txt"));
    }

    #[test]
    fn overwrite_replaces_an_existing_file() {
        let tree = TempTree::new();
        tree.file_with("Library/Movie/english.srt", "existing");
        tree.file_with("Library/Movie/subs/english.srt", "new");

        let events = unpack_recorded(&tree, true, false);

        assert_eq!(events, ["planned 1 files 0 directories into Movie", "pruned subs"]);
        assert_eq!(
            tree.snapshot(),
            ["Library/", "Library/Movie/", "Library/Movie/english.srt: new"]
        );
    }

    #[test]
    fn names_differing_only_by_case_are_not_both_moved() {
        let tree = TempTree::new();
        tree.file_with("Library/Movie/subs/Notes.txt", "outer");
        tree.file_with("Library/Movie/subs/subs/notes.txt", "inner");

        let events = unpack_recorded(&tree, true, false);

        let duplicates = events.iter().filter(|event| event.starts_with("duplicate")).count();
        assert_eq!(
            duplicates, 1,
            "one of the two files should be left in place: {events:?}"
        );
        let moved_contents = std::fs::read_dir(tree.join("Library/Movie"))
            .expect("parent should be readable")
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_ok_and(|file_type| file_type.is_file()))
            .map(|entry| std::fs::read_to_string(entry.path()).expect("moved file should be readable"))
            .collect::<Vec<_>>();
        assert_eq!(moved_contents.len(), 1);
        let left_in_place = ["Library/Movie/subs/Notes.txt", "Library/Movie/subs/subs/notes.txt"]
            .iter()
            .filter(|path| tree.exists(path))
            .count();
        assert_eq!(left_in_place, 1, "both versions should survive");
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_names_move_with_the_invalid_bytes_removed() {
        let tree = TempTree::new();
        tree.file_with("Library/Movie/subs/english.srt", "english");
        // APFS rejects non-UTF-8 names, so there is nothing to test there.
        let Some(_) = tree.non_utf8_file("Library/Movie/subs", "track", "srt") else {
            return;
        };

        let events = unpack_recorded(&tree, false, false);

        assert_eq!(events, ["planned 2 files 0 directories into Movie", "pruned subs"]);
        assert!(tree.exists("Library/Movie/track.srt"));
        assert!(tree.exists("Library/Movie/english.srt"));
        assert!(!tree.exists("Library/Movie/subs"));
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_names_that_collide_after_sanitizing_are_not_overwritten() {
        let tree = TempTree::new();
        tree.file_with("Library/Movie/subs/track.srt", "valid");
        let Some(invalid) = tree.non_utf8_file("Library/Movie/subs", "track", "srt") else {
            return;
        };

        let events = unpack_recorded(&tree, true, false);

        // Directory order decides which of the two moves, the other is left in place.
        let duplicates = events.iter().filter(|event| event.starts_with("duplicate")).count();
        assert_eq!(duplicates, 1, "{events:?}");
        assert!(tree.exists("Library/Movie/track.srt"));
        assert!(
            invalid.exists() != tree.exists("Library/Movie/subs/track.srt"),
            "exactly one colliding source should be left in place"
        );
    }
}

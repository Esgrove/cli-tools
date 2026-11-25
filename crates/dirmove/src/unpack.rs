//! Unpacking configured directory names and finding unwanted directories.
//!
//! `Unpacker` finds the unwanted directories to delete and the directories whose names mark them for unpacking,
//! plans how the contents of an unpacked directory move up into its parent,
//! moves the planned entries, and prunes the directories the moves left empty.

use std::collections::HashSet;
use std::hash::BuildHasher;
use std::path::{Path, PathBuf};

use anyhow::Result;
use walkdir::WalkDir;

use crate::types::{MoveInfo, UnpackInfo};
use crate::utils;

/// Unpacks directories with configured names into their parents.
#[derive(Debug, Clone, Copy)]
pub struct Unpacker<'a> {
    /// Lowercase directory names to unpack.
    unpack_directory_names: &'a [String],
    /// Whether existing targets are overwritten.
    overwrite: bool,
}

/// Unwanted directories to delete and directories to unpack, found under the input roots.
#[derive(Debug, Default)]
pub struct UnpackCandidates {
    /// Unwanted directories, deepest first so nested ones are deleted before their parents.
    pub unwanted: Vec<PathBuf>,
    /// Outermost directories to unpack, deepest first.
    pub unpack: Vec<PathBuf>,
}

/// Outcome of moving one entry out of an unpacked directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryMove {
    /// The entry was moved.
    Moved,
    /// The target already exists and overwriting is disabled.
    SkippedExisting,
}

impl<'a> Unpacker<'a> {
    /// Create an unpacker for the given lowercase directory names.
    #[must_use]
    pub const fn new(unpack_directory_names: &'a [String], overwrite: bool) -> Self {
        Self {
            unpack_directory_names,
            overwrite,
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
                    entry.file_name().to_str().is_some_and(utils::is_unwanted_directory)
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

                if utils::is_unwanted_directory(name) {
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
    /// Files and symlinks move as they are, so unpacking never pulls files out of a directory a link points to.
    /// Nested directories with an unpack name are unpacked into the same parent.
    /// A directory holding a nested unpack directory is walked with itself as the new parent,
    /// and any other directory moves as a whole.
    #[must_use]
    pub fn plan(&self, directory: &Path, parent: &Path) -> UnpackInfo {
        let mut info = UnpackInfo::default();

        let Ok(entries) = std::fs::read_dir(directory) else {
            return info;
        };

        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };

            let target = parent.join(name);
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_dir() {
                info.file_moves.push(MoveInfo::new(path, target));
            } else if self.is_unpack_name(name) {
                let nested_info = self.plan(&path, parent);
                info.file_moves.extend(nested_info.file_moves);
                info.directory_moves.extend(nested_info.directory_moves);
            } else if self.contains_unpack_directory(&path) {
                let nested_info = self.plan(&path, &target);
                info.file_moves.extend(nested_info.file_moves);
                info.directory_moves.extend(nested_info.directory_moves);
            } else {
                info.directory_moves.push(MoveInfo::new(path, target));
            }
        }

        info
    }

    /// Check if a directory contains any subdirectories matching unpack names, recursively.
    ///
    /// Symlinks are not followed, matching how `plan` moves them as links.
    #[must_use]
    pub fn contains_unpack_directory(&self, directory: &Path) -> bool {
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
    ///
    /// # Errors
    /// Returns an error if the directory cannot be moved.
    pub fn move_directory<S: BuildHasher>(
        &self,
        source: &Path,
        target: &Path,
        touched_directories: &mut HashSet<PathBuf, S>,
    ) -> Result<EntryMove> {
        if target.exists() && !self.overwrite {
            return Ok(EntryMove::SkippedExisting);
        }

        if let Some(parent) = source.parent() {
            touched_directories.insert(parent.to_path_buf());
        }

        if std::fs::rename(source, target).is_err() {
            cli_tools_common::copy_dir_recursive(source, target)?;
            std::fs::remove_dir_all(source)?;
        }

        Ok(EntryMove::Moved)
    }

    /// Move a single file, creating the target directory if needed.
    ///
    /// Tries a rename first, and falls back to a copy and removal for cross-device moves.
    /// The source and target parents are recorded in `touched_directories`.
    ///
    /// # Errors
    /// Returns an error if the file cannot be moved.
    pub fn move_file<S: BuildHasher>(
        &self,
        source: &Path,
        target: &Path,
        touched_directories: &mut HashSet<PathBuf, S>,
    ) -> Result<EntryMove> {
        if target.exists() && !self.overwrite {
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
/// With `dryrun`, nothing is removed and the directories that would be removed are returned.
/// Returns the removed directories in removal order.
#[must_use]
pub fn prune_empty_directories<S: BuildHasher>(
    root_directory: &Path,
    touched_directories: &mut HashSet<PathBuf, S>,
    dryrun: bool,
) -> Vec<PathBuf> {
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

        if dryrun {
            pruned.push(directory);
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
    #[cfg(unix)]
    use cli_tools_common::test_support::TempTree;
    use tempfile::TempDir;

    /// Write a file, creating its parent directories.
    fn write_file(path: &Path, contents: &str) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, contents)?;
        Ok(())
    }

    /// Lowercase unpack names from string slices.
    fn names(unpack_names: &[&str]) -> Vec<String> {
        unpack_names.iter().map(|name| name.to_lowercase()).collect()
    }

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

        let unpack_names = names(&["videos"]);
        let info = Unpacker::new(&unpack_names, false).plan(&videos, &root);

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

        let unpack_names = names(&["videos", "updates", "downloads"]);
        let candidates = Unpacker::new(&unpack_names, false).collect_candidates(std::slice::from_ref(&root), true);

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

        let unpack_names = names(&["videos"]);
        let unpacker = Unpacker::new(&unpack_names, false);

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

        let unpack_names = names(&["videos"]);
        let candidates = Unpacker::new(&unpack_names, false).collect_candidates(&[root], false);

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

        let _ = prune_empty_directories(&tree.join("Library/Movie/subs"), &mut touched_directories, false);

        assert!(tree.exists("outside/empty target"));
        assert!(tree.exists("Library/Movie/subs/touched/link"));
    }
}

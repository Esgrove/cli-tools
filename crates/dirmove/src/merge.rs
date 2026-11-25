//! Merging directory contents into another directory.
//!
//! `find_prefix_merge_pairs` finds sibling directories where one name is another with an ignored prefix,
//! and `DirectoryMerger` moves the contents of a source directory into a target directory,
//! merging nested directories entry by entry and reporting each step to an observer.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::Result;
use walkdir::WalkDir;

use cli_tools_common::path_to_filename_string;

use crate::mover::{self, MoveObserver};
use crate::name_filter::NameFilter;
use crate::types::MergePair;

/// Moves the contents of one directory into another.
#[derive(Debug, Clone, Copy, Default)]
pub struct DirectoryMerger {
    /// Whether existing target entries are overwritten.
    overwrite: bool,
    /// Whether to only report what would be moved, without moving anything.
    dryrun: bool,
}

/// One step of merging a source directory into a target directory.
#[derive(Debug)]
pub enum MergeEvent<'a> {
    /// Merging `source` into `target` started, including for each nested directory.
    Started {
        /// Directory whose contents are moved.
        source: &'a Path,
        /// Directory receiving the contents.
        target: &'a Path,
    },
    /// An existing target entry with this name is overwritten.
    Overwriting(&'a str),
    /// An entry with this name would be moved in a dry run.
    WouldMove(&'a str),
    /// Entries with these names were left in the source because the target already has them.
    Skipped(&'a [String]),
    /// Number of entries moved, or that would be moved in a dry run.
    Moved(usize),
    /// The emptied source directory was removed.
    SourceRemoved(&'a Path),
    /// The emptied source directory could not be removed.
    SourceRemoveFailed(&'a Path, &'a std::io::Error),
    /// The source directory would be removed in a dry run.
    WouldRemoveSource(&'a Path),
}

/// Receives the steps of merging directories, and the file moves they make.
///
/// Every method does nothing by default, and `()` reports nothing.
pub trait MergeObserver: MoveObserver {
    /// One step of the merge happened.
    fn on_merge_event(&mut self, _event: MergeEvent<'_>) {}
}

impl MergeObserver for () {}

impl DirectoryMerger {
    /// Create a merger.
    ///
    /// With `overwrite`, existing target entries are replaced instead of skipped.
    /// With `dryrun`, nothing is moved and the observer is told what would be moved.
    #[must_use]
    pub const fn new(overwrite: bool, dryrun: bool) -> Self {
        Self { overwrite, dryrun }
    }

    /// Merge contents from the source directory into the target directory.
    ///
    /// Existing target entries are skipped unless overwriting is enabled.
    /// An existing target subdirectory is merged entry by entry,
    /// so overwriting only replaces files with the same name and never deletes files only in the target.
    /// When the target lies inside the source, the entry holding the target stays where it is.
    /// The source directory is removed when it ends up empty.
    ///
    /// # Errors
    /// Returns an error if the source cannot be read or an entry cannot be moved.
    pub fn merge(&self, source: &Path, target: &Path, observer: &mut impl MergeObserver) -> Result<()> {
        observer.on_merge_event(MergeEvent::Started { source, target });

        let mut skipped_files = Vec::new();
        let mut moved_count = 0;
        let mut file_entries = Vec::new();
        let mut directory_entries = Vec::new();

        let entries: Vec<_> = std::fs::read_dir(source)?.filter_map(Result::ok).collect();
        for entry in entries {
            let entry_path = entry.path();
            let Some(entry_name) = entry_path.file_name() else {
                continue;
            };

            // Moving the entry holding the target would move a directory into itself.
            if target.starts_with(&entry_path) || cli_tools_common::paths_refer_to_same_file(&entry_path, target) {
                continue;
            }

            let target_path = target.join(entry_name);
            let entry_name = entry_name.to_string_lossy();

            if target_path.exists() {
                if self.overwrite {
                    observer.on_merge_event(MergeEvent::Overwriting(&entry_name));
                } else {
                    skipped_files.push(entry_name.into_owned());
                    continue;
                }
            }

            if self.dryrun {
                observer.on_merge_event(MergeEvent::WouldMove(&entry_name));
                moved_count += 1;
                continue;
            }

            // The entry's own type is used so a symlink to a directory moves as a link,
            // instead of merging the files out of the directory it points to.
            if entry.file_type().is_ok_and(|file_type| file_type.is_dir()) {
                directory_entries.push((entry_path, target_path));
            } else {
                file_entries.push(entry_path);
            }
        }

        // Moved files are reported through `MoveEvent::Finished` and are not counted in `moved_count`,
        // which counts the directories and dry run entries.
        if !file_entries.is_empty() {
            let report = mover::move_files_to_target_dir(target, &file_entries, self.overwrite, observer)?;
            skipped_files.extend(report.skipped_files.iter().map(|path| path_to_filename_string(path)));
        }

        for (entry_path, target_path) in directory_entries {
            if target_path.is_dir() {
                self.merge(&entry_path, &target_path, observer)?;
                moved_count += 1;
                continue;
            }
            if target_path.exists() && self.overwrite {
                std::fs::remove_file(&target_path)?;
            }
            if std::fs::rename(&entry_path, &target_path).is_err() {
                cli_tools_common::copy_dir_recursive(&entry_path, &target_path)?;
                std::fs::remove_dir_all(&entry_path)?;
            }
            moved_count += 1;
        }

        if !skipped_files.is_empty() {
            observer.on_merge_event(MergeEvent::Skipped(&skipped_files));
        }
        if moved_count > 0 || !skipped_files.is_empty() {
            observer.on_merge_event(MergeEvent::Moved(moved_count));
        }

        if self.dryrun {
            observer.on_merge_event(MergeEvent::WouldRemoveSource(source));
        } else if std::fs::read_dir(source).is_ok_and(|mut entries| entries.next().is_none()) {
            match std::fs::remove_dir(source) {
                Ok(()) => observer.on_merge_event(MergeEvent::SourceRemoved(source)),
                Err(error) => observer.on_merge_event(MergeEvent::SourceRemoveFailed(source, &error)),
            }
        }

        Ok(())
    }
}

/// Find output directory merge pairs where one sibling is another with an ignored prefix.
///
/// Each output root is checked for sibling directories.
/// With `recurse`, every nested output directory is checked as a sibling parent too.
/// Pairs are returned with the deepest source first.
///
/// # Errors
/// Returns an error if a parent directory cannot be read.
pub fn find_prefix_merge_pairs(
    output_roots: &[PathBuf],
    recurse: bool,
    name_filter: &NameFilter,
) -> Result<Vec<MergePair>> {
    let mut all_pairs = Vec::new();
    for parent_directory in collect_merge_parent_directories(output_roots, recurse) {
        all_pairs.extend(find_prefix_merge_pairs_in_directory(&parent_directory, name_filter)?);
    }
    all_pairs.sort_by_key(|pair| std::cmp::Reverse(pair.source.components().count()));
    Ok(all_pairs)
}

/// Count regular files under a directory recursively.
#[must_use]
pub fn count_files(directory: &Path) -> usize {
    WalkDir::new(directory)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .count()
}

/// Collect output parent directories that should be checked for prefix merge candidates.
///
/// Every output root is included.
/// With `recurse`, every directory that holds a directory under each output root is included too.
fn collect_merge_parent_directories(output_roots: &[PathBuf], recurse: bool) -> Vec<PathBuf> {
    let mut parent_directories: HashSet<PathBuf> = output_roots
        .iter()
        .filter(|output_root| output_root.is_dir())
        .cloned()
        .collect();

    if recurse {
        for output_root in output_roots.iter().filter(|output_root| output_root.is_dir()) {
            for entry in WalkDir::new(output_root)
                .into_iter()
                .filter_entry(|entry| !cli_tools_common::should_skip_entry(entry))
                .filter_map(Result::ok)
            {
                if entry.file_type().is_dir()
                    && let Some(parent) = entry.path().parent()
                {
                    parent_directories.insert(parent.to_path_buf());
                }
            }
        }
    }

    parent_directories.into_iter().collect()
}

/// Find merge pairs within a single directory.
///
/// Returns a pair for each sibling directory with an ignored prefix whose name
/// with the prefix stripped matches another directory's name, ignoring case.
fn find_prefix_merge_pairs_in_directory(parent: &Path, name_filter: &NameFilter) -> Result<Vec<MergePair>> {
    let directories: Vec<PathBuf> = std::fs::read_dir(parent)?
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|file_type| file_type.is_dir()))
        .map(|entry| entry.path())
        .collect();

    let name_to_path: HashMap<String, &PathBuf> = directories
        .iter()
        .filter_map(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .map(|name| (name.to_lowercase(), path))
        })
        .collect();

    let mut pairs = Vec::new();
    for directory in &directories {
        let Some(directory_name) = directory.file_name().and_then(|name| name.to_str()) else {
            continue;
        };

        let stripped = name_filter.strip_ignored_prefixes(directory_name);
        if stripped == directory_name {
            continue;
        }

        if let Some(&target_path) = name_to_path.get(&stripped.to_lowercase())
            && target_path != directory
        {
            pairs.push(MergePair::new(directory.clone(), target_path.clone()));
        }
    }

    Ok(pairs)
}

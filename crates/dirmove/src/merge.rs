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

/// Moves the contents of one directory into another.
#[derive(Debug, Clone, Copy, Default)]
pub struct DirectoryMerger {
    /// Whether existing target entries are overwritten.
    overwrite: bool,
    /// Whether to only report what would be moved, without moving anything.
    dry_run: bool,
}

/// A directory with an ignored prefix and the sibling directory without it, which receives its contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergePair {
    /// Directory whose name starts with an ignored prefix.
    pub source: PathBuf,
    /// Sibling directory named like the source without the prefix.
    pub target: PathBuf,
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
    /// With `dry_run`, nothing is moved and the observer is told what would be moved.
    #[must_use]
    pub const fn new(overwrite: bool, dry_run: bool) -> Self {
        Self { overwrite, dry_run }
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

            if self.dry_run {
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

        if self.dry_run {
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

    // Several directories can share a name that only differs by case.
    // A directory named exactly like the stripped name is preferred, otherwise the first one in name order.
    let mut name_to_paths: HashMap<String, Vec<&PathBuf>> = HashMap::new();
    for path in &directories {
        if let Some(name) = path.file_name().and_then(|name| name.to_str()) {
            name_to_paths.entry(name.to_lowercase()).or_default().push(path);
        }
    }
    for paths in name_to_paths.values_mut() {
        paths.sort();
    }

    let mut pairs = Vec::new();
    for directory in &directories {
        let Some(directory_name) = directory.file_name().and_then(|name| name.to_str()) else {
            continue;
        };

        let stripped = name_filter.strip_ignored_prefixes(directory_name);
        if stripped == directory_name {
            continue;
        }

        let target = name_to_paths.get(&stripped.to_lowercase()).and_then(|paths| {
            paths
                .iter()
                .find(|path| path.file_name().is_some_and(|name| name == stripped.as_ref()))
                .or_else(|| paths.first())
                .copied()
        });
        if let Some(target_path) = target
            && target_path != directory
        {
            pairs.push(MergePair {
                source: directory.clone(),
                target: target_path.clone(),
            });
        }
    }

    Ok(pairs)
}

#[cfg(test)]
mod test_merge_prefixed_directories {
    use super::*;
    use crate::test_helpers::{assert_exists, assert_not_exists, merge_prefixed, write_file};
    use tempfile::TempDir;

    #[test]
    fn merges_prefixed_directory_into_non_prefixed() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Root");
        std::fs::create_dir_all(&root)?;

        // Create "studio alpha beta" with files
        let prefixed_dir = root.join("studio alpha beta");
        write_file(&prefixed_dir.join("file1.txt"), "content1")?;
        write_file(&prefixed_dir.join("file2.txt"), "content2")?;

        // Create "alpha beta" with different files
        let target_dir = root.join("alpha beta");
        write_file(&target_dir.join("existing.txt"), "existing")?;

        let root_for_asserts = root.clone();

        merge_prefixed(&root, &["studio"], false, false, false)?;

        // Files should be merged into "alpha beta"
        assert_exists(&root_for_asserts.join("alpha beta").join("file1.txt"));
        assert_exists(&root_for_asserts.join("alpha beta").join("file2.txt"));
        assert_exists(&root_for_asserts.join("alpha beta").join("existing.txt"));

        // "studio alpha beta" should be deleted (empty after merge)
        assert_not_exists(&root_for_asserts.join("studio alpha beta"));

        Ok(())
    }

    #[test]
    fn skips_existing_files_without_overwrite() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Root");
        std::fs::create_dir_all(&root)?;

        let prefixed_dir = root.join("studio alpha");
        write_file(&prefixed_dir.join("conflict.txt"), "new content")?;

        let target_dir = root.join("alpha");
        write_file(&target_dir.join("conflict.txt"), "original content")?;

        let root_for_asserts = root.clone();

        merge_prefixed(&root, &["studio"], false, false, false)?;

        // Original file should be preserved
        let content = std::fs::read_to_string(root_for_asserts.join("alpha").join("conflict.txt"))?;
        assert_eq!(content, "original content");

        // Source directory still exists because file wasn't moved
        assert_exists(&root_for_asserts.join("studio alpha"));

        Ok(())
    }

    #[test]
    fn overwrites_existing_files_with_force_flag() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Root");
        std::fs::create_dir_all(&root)?;

        let prefixed_dir = root.join("studio alpha");
        write_file(&prefixed_dir.join("conflict.txt"), "new content")?;

        let target_dir = root.join("alpha");
        write_file(&target_dir.join("conflict.txt"), "original content")?;

        let root_for_asserts = root.clone();

        merge_prefixed(&root, &["studio"], false, false, true)?;

        // File should be overwritten
        let content = std::fs::read_to_string(root_for_asserts.join("alpha").join("conflict.txt"))?;
        assert_eq!(content, "new content");

        // Source directory should be deleted (empty after merge)
        assert_not_exists(&root_for_asserts.join("studio alpha"));

        Ok(())
    }

    #[test]
    fn merging_a_directory_into_its_own_child_skips_the_child() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Root");
        write_file(&root.join("Show").join("episode.mkv"), "episode")?;
        write_file(&root.join("Show").join("Show").join("other.mkv"), "other")?;
        let source = root.join("Show");
        let target = source.join("Show");

        DirectoryMerger::new(false, false).merge(&source, &target, &mut ())?;

        let file_count = walkdir::WalkDir::new(&source)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_file())
            .count();
        assert_eq!(file_count, 2);
        assert_eq!(std::fs::read_to_string(target.join("episode.mkv"))?, "episode");
        assert_eq!(std::fs::read_to_string(target.join("other.mkv"))?, "other");
        assert_not_exists(&target.join("Show"));
        Ok(())
    }

    #[test]
    fn overwrite_merges_existing_subdirectories_without_deleting_their_files() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Root");
        std::fs::create_dir_all(&root)?;

        write_file(&root.join("studio alpha").join("Season 1").join("E03.mkv"), "three")?;
        write_file(&root.join("alpha").join("Season 1").join("E01.mkv"), "one")?;
        write_file(&root.join("alpha").join("Season 1").join("E02.mkv"), "two")?;

        let root_for_asserts = root.clone();

        merge_prefixed(&root, &["studio"], false, false, true)?;

        let season = root_for_asserts.join("alpha").join("Season 1");
        assert_eq!(std::fs::read_to_string(season.join("E01.mkv"))?, "one");
        assert_eq!(std::fs::read_to_string(season.join("E02.mkv"))?, "two");
        assert_eq!(std::fs::read_to_string(season.join("E03.mkv"))?, "three");
        assert_not_exists(&root_for_asserts.join("studio alpha"));

        Ok(())
    }

    #[test]
    fn dry_run_does_not_modify_files() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Root");
        std::fs::create_dir_all(&root)?;

        let prefixed_dir = root.join("studio alpha");
        write_file(&prefixed_dir.join("file.txt"), "content")?;

        let target_dir = root.join("alpha");
        std::fs::create_dir_all(&target_dir)?;

        let root_for_asserts = root.clone();

        merge_prefixed(&root, &["studio"], false, true, false)?;

        // File should still be in original location
        assert_exists(&root_for_asserts.join("studio alpha").join("file.txt"));
        assert_not_exists(&root_for_asserts.join("alpha").join("file.txt"));

        Ok(())
    }

    #[test]
    fn case_insensitive_prefix_matching() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Root");
        std::fs::create_dir_all(&root)?;

        let prefixed_dir = root.join("STUDIO alpha");
        write_file(&prefixed_dir.join("file.txt"), "content")?;

        let target_dir = root.join("alpha");
        std::fs::create_dir_all(&target_dir)?;

        let root_for_asserts = root.clone();

        merge_prefixed(&root, &["studio"], false, false, false)?;

        // File should be merged
        assert_exists(&root_for_asserts.join("alpha").join("file.txt"));
        assert_not_exists(&root_for_asserts.join("STUDIO alpha"));

        Ok(())
    }

    #[test]
    fn merges_subdirectories() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Root");
        std::fs::create_dir_all(&root)?;

        let prefixed_dir = root.join("studio alpha");
        write_file(&prefixed_dir.join("subdir").join("nested.txt"), "nested")?;

        let target_dir = root.join("alpha");
        std::fs::create_dir_all(&target_dir)?;

        let root_for_asserts = root.clone();

        merge_prefixed(&root, &["studio"], false, false, false)?;

        // Subdirectory should be merged
        assert_exists(&root_for_asserts.join("alpha").join("subdir").join("nested.txt"));
        assert_not_exists(&root_for_asserts.join("studio alpha"));

        Ok(())
    }

    #[test]
    fn multiple_prefix_ignores() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Root");
        std::fs::create_dir_all(&root)?;

        let prefixed_dir1 = root.join("studio alpha");
        write_file(&prefixed_dir1.join("file1.txt"), "content1")?;

        let prefixed_dir2 = root.join("team alpha");
        write_file(&prefixed_dir2.join("file2.txt"), "content2")?;

        let target_dir = root.join("alpha");
        std::fs::create_dir_all(&target_dir)?;

        let root_for_asserts = root.clone();

        merge_prefixed(&root, &["studio", "team"], false, false, false)?;

        // Both prefixed directories should be merged
        assert_exists(&root_for_asserts.join("alpha").join("file1.txt"));
        assert_exists(&root_for_asserts.join("alpha").join("file2.txt"));
        assert_not_exists(&root_for_asserts.join("studio alpha"));
        assert_not_exists(&root_for_asserts.join("team alpha"));

        Ok(())
    }

    #[test]
    fn no_merge_without_matching_target() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Root");
        std::fs::create_dir_all(&root)?;

        // Only prefixed directory exists, no matching target
        let prefixed_dir = root.join("studio alpha");
        write_file(&prefixed_dir.join("file.txt"), "content")?;

        let root_for_asserts = root.clone();

        merge_prefixed(&root, &["studio"], false, false, false)?;

        // Directory should remain unchanged
        assert_exists(&root_for_asserts.join("studio alpha").join("file.txt"));

        Ok(())
    }

    #[test]
    fn empty_prefix_ignores_does_nothing() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Root");
        std::fs::create_dir_all(&root)?;

        let dir1 = root.join("studio alpha");
        write_file(&dir1.join("file.txt"), "content")?;

        let dir2 = root.join("alpha");
        std::fs::create_dir_all(&dir2)?;

        let root_for_asserts = root.clone();

        merge_prefixed(&root, &[], false, false, false)?;

        // Nothing should change
        assert_exists(&root_for_asserts.join("studio alpha").join("file.txt"));
        assert_not_exists(&root_for_asserts.join("alpha").join("file.txt"));

        Ok(())
    }

    #[test]
    fn chained_prefix_stripping() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Root");
        std::fs::create_dir_all(&root)?;

        // Directory with multiple prefixes that can be stripped
        let prefixed_dir = root.join("studio team alpha");
        write_file(&prefixed_dir.join("file.txt"), "content")?;

        let target_dir = root.join("alpha");
        std::fs::create_dir_all(&target_dir)?;

        let root_for_asserts = root.clone();

        merge_prefixed(&root, &["studio", "team"], false, false, false)?;

        // Should strip both prefixes and find "alpha"
        assert_exists(&root_for_asserts.join("alpha").join("file.txt"));
        assert_not_exists(&root_for_asserts.join("studio team alpha"));

        Ok(())
    }
    #[test]
    fn recursive_merges_in_subdirectories() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Root");
        std::fs::create_dir_all(&root)?;

        // Create nested structure:
        // Root/subdir/studio alpha/file.txt
        // Root/subdir/alpha/
        let subdir = root.join("subdir");
        let prefixed_dir = subdir.join("studio alpha");
        write_file(&prefixed_dir.join("file.txt"), "content")?;

        let target_dir = subdir.join("alpha");
        std::fs::create_dir_all(&target_dir)?;

        let root_for_asserts = root.clone();

        // With recurse=true, should find and merge in subdirectory
        merge_prefixed(&root, &["studio"], true, false, false)?;

        assert_exists(&root_for_asserts.join("subdir").join("alpha").join("file.txt"));
        assert_not_exists(&root_for_asserts.join("subdir").join("studio alpha"));

        Ok(())
    }

    #[test]
    fn non_recursive_ignores_subdirectories() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Root");
        std::fs::create_dir_all(&root)?;

        // Create nested structure that would match if recursive
        let subdir = root.join("subdir");
        let prefixed_dir = subdir.join("studio alpha");
        write_file(&prefixed_dir.join("file.txt"), "content")?;

        let target_dir = subdir.join("alpha");
        std::fs::create_dir_all(&target_dir)?;

        let root_for_asserts = root.clone();

        // With recurse=false, should NOT merge in subdirectory
        merge_prefixed(&root, &["studio"], false, false, false)?;

        // File should still be in original location
        assert_exists(&root_for_asserts.join("subdir").join("studio alpha").join("file.txt"));
        assert_not_exists(&root_for_asserts.join("subdir").join("alpha").join("file.txt"));

        Ok(())
    }

    #[test]
    fn recursive_merges_at_multiple_levels() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Root");
        std::fs::create_dir_all(&root)?;

        // Create merge candidates at root level
        let root_prefixed = root.join("studio beta");
        write_file(&root_prefixed.join("root_file.txt"), "root")?;
        let root_target = root.join("beta");
        std::fs::create_dir_all(&root_target)?;

        // Create merge candidates in subdirectory
        let subdir = root.join("subdir");
        let sub_prefixed = subdir.join("studio alpha");
        write_file(&sub_prefixed.join("sub_file.txt"), "sub")?;
        let sub_target = subdir.join("alpha");
        std::fs::create_dir_all(&sub_target)?;

        let root_for_asserts = root.clone();

        merge_prefixed(&root, &["studio"], true, false, false)?;

        // Both levels should be merged
        assert_exists(&root_for_asserts.join("beta").join("root_file.txt"));
        assert_not_exists(&root_for_asserts.join("studio beta"));
        assert_exists(&root_for_asserts.join("subdir").join("alpha").join("sub_file.txt"));
        assert_not_exists(&root_for_asserts.join("subdir").join("studio alpha"));

        Ok(())
    }

    #[test]
    fn recursive_deeply_nested() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Root");
        std::fs::create_dir_all(&root)?;

        // Create deeply nested structure
        let deep = root.join("a").join("b").join("c");
        let prefixed_dir = deep.join("studio gamma");
        write_file(&prefixed_dir.join("deep_file.txt"), "deep")?;
        let target_dir = deep.join("gamma");
        std::fs::create_dir_all(&target_dir)?;

        let root_for_asserts = root.clone();

        merge_prefixed(&root, &["studio"], true, false, false)?;

        assert_exists(
            &root_for_asserts
                .join("a")
                .join("b")
                .join("c")
                .join("gamma")
                .join("deep_file.txt"),
        );
        assert_not_exists(&root_for_asserts.join("a").join("b").join("c").join("studio gamma"));

        Ok(())
    }

    #[test]
    fn only_merges_siblings_not_across_levels() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Root");
        std::fs::create_dir_all(&root)?;

        // Create structure where prefixed and non-prefixed dirs are at different levels:
        // Root/studio dirname/file1.txt       <- prefixed at root
        // Root/subdir/dirname/file2.txt       <- non-prefixed in subdir (NOT a sibling)
        // Root/subdir/subsubdir/studio dirname/file3.txt  <- prefixed deeply nested

        let prefixed_root = root.join("studio dirname");
        write_file(&prefixed_root.join("file1.txt"), "root level")?;

        let non_prefixed_subdir = root.join("subdir").join("dirname");
        write_file(&non_prefixed_subdir.join("file2.txt"), "subdir level")?;

        let prefixed_deep = root.join("subdir").join("subsubdir").join("studio dirname");
        write_file(&prefixed_deep.join("file3.txt"), "deep level")?;

        let root_for_asserts = root.clone();

        merge_prefixed(&root, &["studio"], true, false, false)?;

        // "studio dirname" at root should NOT merge with "subdir/dirname" (different parents)
        assert_exists(&root_for_asserts.join("studio dirname").join("file1.txt"));
        assert_exists(&root_for_asserts.join("subdir").join("dirname").join("file2.txt"));

        // "subdir/subsubdir/studio dirname" should NOT merge (no sibling "dirname" exists there)
        assert_exists(
            &root_for_asserts
                .join("subdir")
                .join("subsubdir")
                .join("studio dirname")
                .join("file3.txt"),
        );

        Ok(())
    }

    #[test]
    fn merges_only_when_sibling_exists() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Root");
        std::fs::create_dir_all(&root)?;

        // Create structure:
        // Root/studio alpha/file1.txt         <- NO sibling "alpha" at root
        // Root/subdir/studio beta/file2.txt   <- HAS sibling "beta" in same subdir
        // Root/subdir/beta/existing.txt

        let prefixed_no_sibling = root.join("studio alpha");
        write_file(&prefixed_no_sibling.join("file1.txt"), "no sibling")?;

        let subdir = root.join("subdir");
        let prefixed_with_sibling = subdir.join("studio beta");
        write_file(&prefixed_with_sibling.join("file2.txt"), "has sibling")?;

        let sibling_target = subdir.join("beta");
        write_file(&sibling_target.join("existing.txt"), "existing")?;

        let root_for_asserts = root.clone();

        merge_prefixed(&root, &["studio"], true, false, false)?;

        // "studio alpha" should remain (no sibling "alpha" at root level)
        assert_exists(&root_for_asserts.join("studio alpha").join("file1.txt"));

        // "subdir/studio beta" should merge into "subdir/beta"
        assert_exists(&root_for_asserts.join("subdir").join("beta").join("file2.txt"));
        assert_exists(&root_for_asserts.join("subdir").join("beta").join("existing.txt"));
        assert_not_exists(&root_for_asserts.join("subdir").join("studio beta"));

        Ok(())
    }

    #[test]
    fn prefix_merge_checks_all_output_roots() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let output_root_a = tmp.path().join("output_a");
        let output_root_b = tmp.path().join("output_b");
        let source_directory = output_root_b.join("studio alpha");
        let target_directory = output_root_b.join("alpha");
        std::fs::create_dir(&output_root_a)?;
        std::fs::create_dir_all(&source_directory)?;
        std::fs::create_dir_all(&target_directory)?;
        std::fs::write(source_directory.join("episode.mp4"), "video")?;

        let name_filter = NameFilter::new(["studio"], Vec::<String>::new());
        for pair in find_prefix_merge_pairs(&[output_root_a, output_root_b], false, &name_filter)? {
            DirectoryMerger::new(false, false).merge(&pair.source, &pair.target, &mut ())?;
        }

        assert_not_exists(&source_directory);
        assert_exists(&target_directory.join("episode.mp4"));
        Ok(())
    }
}

/// Nested tree and filesystem edge case tests for merging prefixed directories.
///
/// Every test builds a tree several levels deep with `TempTree`
/// and asserts the complete resulting tree, so a lost, duplicated, or misplaced entry fails the test.
/// The trees cover hidden files, dry runs, and symlinks that point outside the walked directories.
#[cfg(test)]
mod test_merge_trees {
    use cli_tools_common::test_support::TempTree;

    /// Run the prefixed directory merge for "studio" on the "Root" directory of the tree.
    fn merge(tree: &TempTree, recurse: bool, dry_run: bool, overwrite: bool) {
        crate::test_helpers::merge_prefixed(&tree.join("Root"), &["studio"], recurse, dry_run, overwrite)
            .expect("merging should succeed");
    }

    /// Tree with a prefixed directory holding hidden files
    /// and nested directories to merge into its unprefixed match.
    fn merge_tree() -> TempTree {
        let tree = TempTree::new();
        tree.file_with("Root/studio alpha beta/file one.mkv", "one");
        tree.file_with("Root/studio alpha beta/.hidden", "hidden");
        tree.file_with("Root/studio alpha beta/extras/deep/.deep hidden", "deep hidden");
        tree.file_with("Root/studio alpha beta/extras/deep/clip.mkv", "clip");
        tree.file_with("Root/alpha beta/existing.mkv", "existing");
        tree.file_with("Root/alpha beta/extras/existing extra.mkv", "existing extra");
        tree
    }

    #[test]
    fn merge_moves_hidden_files_and_keeps_an_existing_subdirectory_without_overwrite() {
        let tree = merge_tree();

        merge(&tree, false, false, false);

        assert_eq!(
            tree.snapshot(),
            vec![
                "Root/",
                "Root/alpha beta/",
                "Root/alpha beta/.hidden: hidden",
                "Root/alpha beta/existing.mkv: existing",
                "Root/alpha beta/extras/",
                "Root/alpha beta/extras/existing extra.mkv: existing extra",
                "Root/alpha beta/file one.mkv: one",
                "Root/studio alpha beta/",
                "Root/studio alpha beta/extras/",
                "Root/studio alpha beta/extras/deep/",
                "Root/studio alpha beta/extras/deep/.deep hidden: deep hidden",
                "Root/studio alpha beta/extras/deep/clip.mkv: clip",
            ]
        );
    }

    #[test]
    fn overwrite_merges_nested_directories_and_hidden_files_at_every_depth() {
        let tree = merge_tree();

        merge(&tree, false, false, true);

        assert_eq!(
            tree.snapshot(),
            vec![
                "Root/",
                "Root/alpha beta/",
                "Root/alpha beta/.hidden: hidden",
                "Root/alpha beta/existing.mkv: existing",
                "Root/alpha beta/extras/",
                "Root/alpha beta/extras/deep/",
                "Root/alpha beta/extras/deep/.deep hidden: deep hidden",
                "Root/alpha beta/extras/deep/clip.mkv: clip",
                "Root/alpha beta/extras/existing extra.mkv: existing extra",
                "Root/alpha beta/file one.mkv: one",
            ]
        );
    }

    #[test]
    fn dry_run_merge_on_a_deep_tree_changes_nothing() {
        let tree = merge_tree();
        let before = tree.snapshot();

        merge(&tree, true, true, false);

        assert_eq!(tree.snapshot(), before);
    }

    #[test]
    fn merge_conflicts_keep_both_versions() {
        let tree = merge_tree();
        tree.file_with("Root/studio alpha beta/existing.mkv", "incoming");

        merge(&tree, false, false, false);

        assert_eq!(tree.read("Root/alpha beta/existing.mkv"), "existing");
        assert_eq!(tree.read("Root/studio alpha beta/existing.mkv"), "incoming");
    }

    #[cfg(unix)]
    #[test]
    fn merge_does_not_pull_files_out_of_a_linked_subdirectory() {
        let tree = TempTree::new();
        tree.file_with("outside/real/data.txt", "outside data");
        tree.file_with("Root/studio alpha beta/file one.mkv", "one");
        tree.symlink("outside/real", "Root/studio alpha beta/extras");
        tree.file_with("Root/alpha beta/extras/existing extra.mkv", "existing extra");

        merge(&tree, false, false, false);

        assert_eq!(tree.read("outside/real/data.txt"), "outside data");
        assert!(!tree.exists("Root/alpha beta/extras/data.txt"));
        assert_eq!(tree.read("Root/alpha beta/file one.mkv"), "one");
    }

    #[cfg(unix)]
    #[test]
    fn overwrite_merge_does_not_pull_files_out_of_a_linked_subdirectory() {
        let tree = TempTree::new();
        tree.file_with("outside/real/data.txt", "outside data");
        tree.file_with("Root/studio alpha beta/file one.mkv", "one");
        tree.symlink("outside/real", "Root/studio alpha beta/extras");
        tree.file_with("Root/alpha beta/extras/existing extra.mkv", "existing extra");

        merge(&tree, false, false, true);

        assert_eq!(tree.read("outside/real/data.txt"), "outside data");
        assert!(!tree.exists("Root/alpha beta/extras/data.txt"));
        assert_eq!(tree.read("Root/alpha beta/extras/existing extra.mkv"), "existing extra");
        assert_eq!(tree.read("Root/alpha beta/file one.mkv"), "one");
    }

    #[cfg(unix)]
    #[test]
    fn merge_moves_a_linked_subdirectory_as_a_link() {
        let tree = TempTree::new();
        tree.file_with("outside/real/data.txt", "outside data");
        tree.file_with("Root/studio alpha beta/file one.mkv", "one");
        tree.symlink("outside/real", "Root/studio alpha beta/extras");
        tree.dir("Root/alpha beta");

        merge(&tree, false, false, false);

        assert_eq!(tree.read("outside/real/data.txt"), "outside data");
        let link = tree
            .join("Root/alpha beta/extras")
            .symlink_metadata()
            .expect("the link should be moved into the merge target");
        assert!(link.file_type().is_symlink());
        assert!(!tree.exists("Root/studio alpha beta"));
    }
}

#[cfg(test)]
mod test_merge_events {
    use super::*;
    use crate::mover::MoveEvent;
    use cli_tools_common::test_support::TempTree;

    /// Observer that records merge events and finished file batches as short descriptions.
    #[derive(Default)]
    struct EventRecorder {
        /// Event descriptions in the order they were reported.
        events: Vec<String>,
    }

    impl MoveObserver for EventRecorder {
        fn on_event(&mut self, event: MoveEvent<'_>) {
            if let MoveEvent::Finished(report) = event {
                self.events.push(format!("moved {} files", report.moved_count()));
            }
        }
    }

    impl MergeObserver for EventRecorder {
        fn on_merge_event(&mut self, event: MergeEvent<'_>) {
            let description = match event {
                MergeEvent::Started { source, target } => format!(
                    "started {} -> {}",
                    path_to_filename_string(source),
                    path_to_filename_string(target)
                ),
                MergeEvent::Overwriting(name) => format!("overwriting {name}"),
                MergeEvent::WouldMove(name) => format!("would move {name}"),
                MergeEvent::Skipped(names) => format!("skipped {}", names.join(", ")),
                MergeEvent::Moved(count) => format!("moved {count} items"),
                MergeEvent::SourceRemoved(source) => format!("removed {}", path_to_filename_string(source)),
                MergeEvent::SourceRemoveFailed(source, _) => {
                    format!("remove failed {}", path_to_filename_string(source))
                }
                MergeEvent::WouldRemoveSource(source) => format!("would remove {}", path_to_filename_string(source)),
            };
            self.events.push(description);
        }
    }

    /// Merge "source" into "target" in the tree and return the recorded events.
    fn merge_recorded(tree: &TempTree, overwrite: bool, dry_run: bool) -> Vec<String> {
        let mut recorder = EventRecorder::default();
        DirectoryMerger::new(overwrite, dry_run)
            .merge(&tree.join("source"), &tree.join("target"), &mut recorder)
            .expect("merging should succeed");
        recorder.events
    }

    #[test]
    fn overwrite_reports_nested_merges_file_batches_and_source_removal() {
        let tree = TempTree::new();
        tree.file_with("source/one.txt", "one");
        tree.file_with("source/nested/two.txt", "two");
        tree.dir("target/nested");

        let events = merge_recorded(&tree, true, false);

        assert_eq!(
            events,
            [
                "started source -> target",
                "overwriting nested",
                "moved 1 files",
                "started nested -> nested",
                "moved 1 files",
                "removed nested",
                "moved 1 items",
                "removed source"
            ]
        );
        assert_eq!(
            tree.snapshot(),
            [
                "target/",
                "target/nested/",
                "target/nested/two.txt: two",
                "target/one.txt: one"
            ]
        );
    }

    #[test]
    fn skipped_entries_keep_the_source_directory() {
        let tree = TempTree::new();
        tree.file_with("source/one.txt", "new");
        tree.file_with("target/one.txt", "old");

        let events = merge_recorded(&tree, false, false);

        assert_eq!(events, ["started source -> target", "skipped one.txt", "moved 0 items"]);
        assert_eq!(tree.read("source/one.txt"), "new");
        assert_eq!(tree.read("target/one.txt"), "old");
    }

    #[test]
    fn dry_run_reports_what_would_move_and_changes_nothing() {
        let tree = TempTree::new();
        tree.file_with("source/one.txt", "one");
        tree.dir("target");
        let before = tree.snapshot();

        let events = merge_recorded(&tree, false, true);

        assert_eq!(
            events,
            [
                "started source -> target",
                "would move one.txt",
                "moved 1 items",
                "would remove source"
            ]
        );
        assert_eq!(tree.snapshot(), before);
    }

    #[test]
    fn overwrite_replaces_a_target_file_with_a_source_directory() {
        let tree = TempTree::new();
        tree.file_with("source/extras/clip.mkv", "clip");
        tree.file_with("target/extras", "a file named like the directory");

        let events = merge_recorded(&tree, true, false);

        assert!(events.contains(&"overwriting extras".to_string()), "{events:?}");
        assert_eq!(
            tree.snapshot(),
            ["target/", "target/extras/", "target/extras/clip.mkv: clip"]
        );
    }

    #[test]
    fn a_target_file_blocks_a_source_directory_without_overwrite() {
        let tree = TempTree::new();
        tree.file_with("source/extras/clip.mkv", "clip");
        tree.file_with("target/extras", "file");

        let events = merge_recorded(&tree, false, false);

        assert!(events.contains(&"skipped extras".to_string()), "{events:?}");
        assert_eq!(tree.read("source/extras/clip.mkv"), "clip");
        assert_eq!(tree.read("target/extras"), "file");
    }
}

#[cfg(test)]
mod test_find_prefix_merge_pairs {
    use super::*;
    use cli_tools_common::test_support::TempTree;

    /// Find the merge pairs for the "studio" prefix under the tree root, as relative paths.
    fn pairs(tree: &TempTree, recurse: bool) -> Vec<(String, String)> {
        let name_filter = NameFilter::new(["studio"], Vec::<String>::new());
        find_prefix_merge_pairs(&[tree.path().to_path_buf()], recurse, &name_filter)
            .expect("pairs should be found")
            .into_iter()
            .map(|pair| {
                let relative = |path: &Path| {
                    path.strip_prefix(tree.path())
                        .expect("pair should be inside the tree")
                        .to_string_lossy()
                        .replace('\\', "/")
                };
                (relative(&pair.source), relative(&pair.target))
            })
            .collect()
    }

    #[test]
    fn pairs_a_prefixed_directory_with_its_unprefixed_sibling() {
        let tree = TempTree::with_entries(&["studio alpha/", "alpha/", "studio beta/"]);

        assert_eq!(pairs(&tree, false), [("studio alpha".to_string(), "alpha".to_string())]);
    }

    #[test]
    fn matches_the_sibling_ignoring_case() {
        let tree = TempTree::with_entries(&["Studio Alpha/", "ALPHA/"]);

        assert_eq!(pairs(&tree, false), [("Studio Alpha".to_string(), "ALPHA".to_string())]);
    }

    #[test]
    fn prefers_the_sibling_with_the_exact_stripped_name() {
        let tree = TempTree::new();
        tree.dir("studio Alpha");
        tree.dir("Alpha");
        // A case-insensitive file system cannot hold both names, so there is only one candidate there.
        if !cli_tools_common::directory_is_case_sensitive(tree.path()).unwrap_or(false) {
            return;
        }
        tree.dir("ALPHA");
        tree.dir("alpha");

        assert_eq!(pairs(&tree, false), [("studio Alpha".to_string(), "Alpha".to_string())]);
    }

    #[test]
    fn without_recurse_only_the_root_is_searched() {
        let tree = TempTree::with_entries(&["shows/studio alpha/", "shows/alpha/"]);

        assert_eq!(
            pairs(&tree, false),
            [] as [(std::string::String, std::string::String); 0]
        );
    }

    #[test]
    fn with_recurse_nested_pairs_come_deepest_first() {
        let tree = TempTree::with_entries(&[
            "studio alpha/",
            "alpha/",
            "shows/drama/studio beta/",
            "shows/drama/beta/",
        ]);

        assert_eq!(
            pairs(&tree, true),
            [
                ("shows/drama/studio beta".to_string(), "shows/drama/beta".to_string()),
                ("studio alpha".to_string(), "alpha".to_string()),
            ]
        );
    }
}

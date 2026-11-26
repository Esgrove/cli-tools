//! File and directory renaming for flipdate.
//!
//! Finds dates in file or directory names and renames them to the `yyyy.mm.dd` format.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use colored::Colorize;
use walkdir::WalkDir;

use cli_tools_common::date::Date;
use cli_tools_common::{OverwritePolicy, RenameOutcome, TargetReservations};

pub use crate::config::Config;

pub static FILE_EXTENSIONS: [&str; 20] = [
    // Audio
    "m4a", "mp3", // Video
    "mp4", "mkv", "mov", "avi", "wmv", "flv", "m4v", // Image
    "jpg", "jpeg", "png", "gif", "bmp", "tiff", "webp", "heic", // Other
    "txt", "rtf", "csv",
];

/// A file or directory scheduled to be renamed to its reordered date name.
#[derive(Debug)]
struct RenameItem {
    path: PathBuf,
    filename: String,
    new_name: String,
}

/// Flip date to start with year for all matching files from the given path.
pub fn date_flip_files(path: &PathBuf, config: &Config) -> Result<()> {
    let (files, _root) = files_to_rename(path, &config.file_extensions, config.recurse)?;
    if files.is_empty() {
        if config.verbose {
            println!("No files to process");
        }
        return Ok(());
    }

    let mut files_to_rename: Vec<RenameItem> = Vec::new();
    for file in files {
        let Some(filename) = file.file_name().and_then(std::ffi::OsStr::to_str) else {
            if config.verbose {
                eprintln!(
                    "{}",
                    format!("Skipping non-UTF-8 filename: {}", file.display()).yellow()
                );
            }
            continue;
        };

        if let Some(new_name) =
            Date::reorder_filename_date(filename, config.year_first, config.swap_year, config.verbose)
            && new_name.to_lowercase() != filename.to_lowercase()
        {
            let filename = filename.to_string();
            files_to_rename.push(RenameItem {
                path: file,
                filename,
                new_name,
            });
        }
    }

    // Case-insensitive sort by filename
    files_to_rename.sort_by_key(|rename_item| rename_item.filename.to_lowercase());

    let heading = if config.dryrun {
        "Dryrun:".cyan().bold()
    } else {
        "Rename:".magenta().bold()
    };

    let policy = if config.overwrite {
        OverwritePolicy::Overwrite
    } else {
        OverwritePolicy::Skip
    };
    // Overwriting only replaces files that existed before the run,
    // never a file that another source in this run was renamed to.
    let mut reservations = TargetReservations::new();
    let mut planned: Vec<(RenameItem, PathBuf)> = Vec::new();
    for item in files_to_rename {
        // Recursive runs find files in subdirectories, so the new name stays in the file's own directory.
        let new_path = item.path.with_file_name(&item.new_name);
        if new_path == item.path {
            continue;
        }
        if !reservations.reserve(&new_path) {
            eprintln!(
                "{}",
                format!(
                    "Skipping {}, another file in this run is renamed to {}",
                    item.path.display(),
                    item.new_name
                )
                .yellow()
            );
            continue;
        }
        planned.push((item, new_path));
    }

    if policy == OverwritePolicy::Skip {
        skip_blocked_renames(&mut planned);
    }
    for (item, _) in &planned {
        println!("{heading}");
        cli_tools_common::show_diff(&item.filename, &item.new_name);
    }
    if !config.dryrun {
        execute_planned_renames(&planned, policy)?;
    }

    Ok(())
}

/// Remove renames blocked by entries that will remain in place.
/// Repeat until skipping a rename no longer blocks another rename in the batch.
fn skip_blocked_renames(planned: &mut Vec<(RenameItem, PathBuf)>) {
    use std::collections::HashSet;

    loop {
        let active_sources: HashSet<_> = planned.iter().map(|(item, _)| item.path.clone()).collect();
        planned.retain(|(_, target)| {
            if std::fs::symlink_metadata(target).is_err()
                || active_sources.contains(target)
                || active_sources.iter().any(|source| {
                    source.parent() == target.parent() && cli_tools_common::paths_refer_to_same_entry(source, target)
                })
            {
                return true;
            }
            eprintln!("{}", format!("File already exists: {}", target.display()).yellow());
            false
        });
        if planned.len() == active_sources.len() {
            break;
        }
    }
}

/// Execute planned file renames without clobbering any pending source in cycles or chains.
///
/// Stage sources that are also destinations in the batch under unique temporary sibling paths.
/// On failure, restore pending sources whose original names are free.
fn execute_planned_renames(planned: &[(RenameItem, PathBuf)], policy: OverwritePolicy) -> Result<()> {
    let mut active_targets = TargetReservations::new();
    for (_, target) in planned {
        active_targets.reserve(target);
    }

    // Phase 1: Stage any active source whose path is also targeted by another active rename.
    let mut staged_sources = Vec::with_capacity(planned.len());
    for (item, _) in planned {
        if active_targets.contains(&item.path) {
            let temporary = cli_tools_common::unique_sibling_path(&item.path, "flipdate-tmp");
            if let Err(error) = cli_tools_common::safe_rename(&item.path, &temporary, OverwritePolicy::Error)
                .with_context(|| format!("Failed to stage {}", item.path.display()))
            {
                return Err(recover_staged_sources(planned, &staged_sources, error));
            }
            staged_sources.push(Some(temporary));
        } else {
            staged_sources.push(None);
        }
    }

    // Phase 2: Move each active item from its original or staged path to its final destination.
    for ((item, new_path), staged) in planned.iter().zip(&mut staged_sources) {
        let current_source = staged.as_deref().unwrap_or(&item.path);
        let outcome = cli_tools_common::safe_rename(current_source, new_path, policy)
            .with_context(|| format!("Failed to rename {}", item.path.display()));
        match outcome {
            Ok(RenameOutcome::SkippedExisting) => {
                if let Some(temporary) = staged.take()
                    && let Err(error) = restore_staged_source(&temporary, &item.path)
                {
                    return Err(recover_staged_sources(planned, &staged_sources, error));
                }
                eprintln!("{}", "File already exists".yellow());
            }
            Ok(_) => *staged = None,
            Err(error) => return Err(recover_staged_sources(planned, &staged_sources, error)),
        }
    }

    Ok(())
}

/// Restore pending inputs after a failure without undoing completed renames.
/// Attach a recovery path to the error for every input that cannot be restored.
fn recover_staged_sources(
    planned: &[(RenameItem, PathBuf)],
    staged_sources: &[Option<PathBuf>],
    mut error: anyhow::Error,
) -> anyhow::Error {
    for ((item, _), staged) in planned.iter().zip(staged_sources) {
        if let Some(temporary) = staged
            && let Err(recovery_error) = restore_staged_source(temporary, &item.path)
        {
            error = error.context(format!("{recovery_error:#}"));
        }
    }
    error
}

/// Restore an input without overwriting anything that now occupies its original name.
fn restore_staged_source(temporary: &Path, original: &Path) -> Result<()> {
    cli_tools_common::safe_rename(temporary, original, OverwritePolicy::Error).with_context(|| {
        format!(
            "Could not restore {} from recovery path {}",
            original.display(),
            temporary.display()
        )
    })?;
    Ok(())
}

/// Flip date to start with year for all matching directories from the given path.
pub fn date_flip_directories(path: PathBuf, config: &Config) -> Result<()> {
    let mut directories = directories_to_rename(path, config.recurse)?;
    // Renaming a parent first would leave the stored paths of its subdirectories pointing nowhere,
    // so the deepest directories are renamed first.
    directories.sort_by_key(|directory| std::cmp::Reverse(directory.path.components().count()));
    if directories.is_empty() {
        if config.verbose {
            println!("No directories to rename");
        }
        return Ok(());
    }

    let max_chars: usize = directories
        .iter()
        .map(|r| r.filename.chars().count())
        .max()
        .context("Failed to get max path length")?;

    for directory in directories {
        let new_path = directory.path.with_file_name(directory.new_name.clone());
        println!(
            "{:<width$}  ==>  {}",
            directory.filename,
            directory.new_name,
            width = max_chars
        );
        if !config.dryrun {
            // An existing directory is never replaced, so a name taken by another directory is skipped.
            match cli_tools_common::safe_rename(&directory.path, &new_path, OverwritePolicy::Skip) {
                Ok(RenameOutcome::SkippedExisting) => {
                    eprintln!(
                        "{}",
                        format!("Directory already exists: {}", new_path.display()).yellow()
                    );
                }
                Ok(_) => {}
                Err(error) => {
                    cli_tools_common::print_error!(
                        "Failed to rename {} to {}: {error}",
                        directory.path.display(),
                        new_path.display()
                    );
                }
            }
        }
    }

    Ok(())
}

/// Get list of files to process
fn files_to_rename(path: &PathBuf, file_extensions: &[String], recurse: bool) -> Result<(Vec<PathBuf>, PathBuf)> {
    let (mut files, root) = if path.is_file() {
        (
            vec![path.clone()],
            path.parent().context("Failed to get file parent")?.to_path_buf(),
        )
    } else {
        let list: Vec<PathBuf> = WalkDir::new(path)
            .min_depth(1)
            .max_depth(if recurse { usize::MAX } else { 1 })
            .into_iter()
            .filter_entry(|e| !cli_tools_common::should_skip_entry(e))
            .filter_map(std::result::Result::ok)
            .map(walkdir::DirEntry::into_path)
            .filter(|path| {
                path.is_file()
                    && path.extension().is_some_and(|ext| {
                        file_extensions
                            .iter()
                            .any(|e| ext.to_str().is_some_and(|ext_str| e.eq_ignore_ascii_case(ext_str)))
                    })
            })
            .collect();
        (list, path.clone())
    };

    files.sort_unstable();
    Ok((files, root))
}

/// Get list of directories to process
fn directories_to_rename(path: PathBuf, recurse: bool) -> Result<Vec<RenameItem>> {
    let mut directories_to_rename = Vec::new();

    let walker = WalkDir::new(path).min_depth(1).max_depth(if recurse { 100 } else { 1 });

    for entry in walker
        .into_iter()
        .filter_entry(|e| !cli_tools_common::should_skip_entry(e))
    {
        let entry = entry.context("Failed to read directory entry")?;
        if entry.path().is_dir() {
            let Some(filename) = entry.file_name().to_str() else {
                continue;
            };
            if let Some(new_name) = Date::reorder_directory_date(filename) {
                directories_to_rename.push(RenameItem {
                    path: entry.path().to_path_buf(),
                    filename: filename.to_string(),
                    new_name,
                });
            }
        }
    }

    // Case-insensitive sort by filename
    directories_to_rename.sort_by_key(|rename_item| rename_item.filename.to_lowercase());

    Ok(directories_to_rename)
}

#[cfg(test)]
mod test_rename_discovery {
    use super::*;
    use std::fs::{self, File};
    use tempfile::TempDir;

    /// Helper to create a temporary directory structure for testing.
    fn create_test_dir() -> TempDir {
        tempfile::tempdir().expect("Failed to create temp dir")
    }

    /// Helper to create an empty file.
    fn create_file(dir: &std::path::Path, name: &str) {
        File::create(dir.join(name)).expect("Failed to create file");
    }

    /// Helper to create a subdirectory.
    fn create_subdir(dir: &std::path::Path, name: &str) -> PathBuf {
        let subdir = dir.join(name);
        fs::create_dir(&subdir).expect("Failed to create subdir");
        subdir
    }

    #[test]
    fn test_files_to_rename_filters_by_extension() {
        let temp_dir = create_test_dir();
        let dir_path = temp_dir.path();

        create_file(dir_path, "test.mp3");
        create_file(dir_path, "test.mp4");
        create_file(dir_path, "test.txt");
        create_file(dir_path, "test.jpg");
        create_file(dir_path, "test.png");

        let extensions = vec!["mp3".to_string(), "mp4".to_string()];
        let (files, _root) = files_to_rename(&dir_path.to_path_buf(), &extensions, false).unwrap();

        assert_eq!(files.len(), 2);
        assert!(files.iter().any(|f| f.file_name().unwrap() == "test.mp3"));
        assert!(files.iter().any(|f| f.file_name().unwrap() == "test.mp4"));
    }

    #[test]
    fn test_files_to_rename_no_matching_extensions() {
        let temp_dir = create_test_dir();
        let dir_path = temp_dir.path();

        create_file(dir_path, "test.jpg");
        create_file(dir_path, "test.png");

        let extensions = vec!["mp3".to_string(), "mp4".to_string()];
        let (files, _root) = files_to_rename(&dir_path.to_path_buf(), &extensions, false).unwrap();

        assert_eq!(files, [] as [PathBuf; 0]);
    }

    #[test]
    fn test_files_to_rename_non_recursive() {
        let temp_dir = create_test_dir();
        let dir_path = temp_dir.path();

        create_file(dir_path, "root.mp3");
        let subdir = create_subdir(dir_path, "subdir");
        create_file(&subdir, "nested.mp3");

        let extensions = vec!["mp3".to_string()];
        let (files, _root) = files_to_rename(&dir_path.to_path_buf(), &extensions, false).unwrap();

        assert_eq!(files.len(), 1);
        assert_eq!(files[0].file_name().unwrap(), "root.mp3");
    }

    #[test]
    fn test_files_to_rename_recursive() {
        let temp_dir = create_test_dir();
        let dir_path = temp_dir.path();

        create_file(dir_path, "root.mp3");
        let subdir = create_subdir(dir_path, "subdir");
        create_file(&subdir, "nested.mp3");
        let nested_subdir = create_subdir(&subdir, "deep");
        create_file(&nested_subdir, "deep.mp3");

        let extensions = vec!["mp3".to_string()];
        let (files, _root) = files_to_rename(&dir_path.to_path_buf(), &extensions, true).unwrap();

        assert_eq!(files.len(), 3);
    }

    #[test]
    fn test_files_to_rename_single_file() {
        let temp_dir = create_test_dir();
        let dir_path = temp_dir.path();

        let file_path = dir_path.join("single.mp3");
        File::create(&file_path).expect("Failed to create file");

        let extensions = vec!["mp3".to_string()];
        let (files, root) = files_to_rename(&file_path, &extensions, false).unwrap();

        assert_eq!(files.len(), 1);
        assert_eq!(root, dir_path);
    }

    #[test]
    fn test_files_to_rename_returns_sorted() {
        let temp_dir = create_test_dir();
        let dir_path = temp_dir.path();

        create_file(dir_path, "zebra.mp3");
        create_file(dir_path, "apple.mp3");
        create_file(dir_path, "mango.mp3");

        let extensions = vec!["mp3".to_string()];
        let (files, _root) = files_to_rename(&dir_path.to_path_buf(), &extensions, false).unwrap();

        assert_eq!(files.len(), 3);
        assert_eq!(files[0].file_name().unwrap(), "apple.mp3");
        assert_eq!(files[1].file_name().unwrap(), "mango.mp3");
        assert_eq!(files[2].file_name().unwrap(), "zebra.mp3");
    }

    #[test]
    fn test_files_to_rename_empty_directory() {
        let temp_dir = create_test_dir();
        let dir_path = temp_dir.path();

        let extensions = vec!["mp3".to_string()];
        let (files, _root) = files_to_rename(&dir_path.to_path_buf(), &extensions, false).unwrap();

        assert_eq!(files, [] as [PathBuf; 0]);
    }

    #[test]
    fn test_directories_to_rename_with_date() {
        let temp_dir = create_test_dir();
        let dir_path = temp_dir.path();

        // Create directories with dates that need reordering (DD.MM.YYYY format with dots)
        create_subdir(dir_path, "25.12.2023 Christmas");
        create_subdir(dir_path, "01.01.2024 New Year");

        let result = directories_to_rename(dir_path.to_path_buf(), false).unwrap();

        assert_eq!(result.len(), 2);
        // Should be sorted case-insensitively
        assert!(result.iter().any(|r| r.new_name.starts_with("2023")));
        assert!(result.iter().any(|r| r.new_name.starts_with("2024")));
    }

    #[test]
    fn test_directories_to_rename_no_dates() {
        let temp_dir = create_test_dir();
        let dir_path = temp_dir.path();

        create_subdir(dir_path, "no_date_here");
        create_subdir(dir_path, "another_folder");

        let result = directories_to_rename(dir_path.to_path_buf(), false).unwrap();

        assert!(result.is_empty());
    }

    #[test]
    fn test_directories_to_rename_already_correct_format() {
        let temp_dir = create_test_dir();
        let dir_path = temp_dir.path();

        // Create directory with correct YYYY-MM-DD format
        create_subdir(dir_path, "2023-12-25 Christmas");

        let result = directories_to_rename(dir_path.to_path_buf(), false).unwrap();

        // Should not include directories already in correct format
        assert!(result.is_empty());
    }

    #[test]
    fn test_directories_to_rename_non_recursive() {
        let temp_dir = create_test_dir();
        let dir_path = temp_dir.path();

        let subdir = create_subdir(dir_path, "25.12.2023 Parent");
        create_subdir(&subdir, "01.01.2024 Child");

        let result = directories_to_rename(dir_path.to_path_buf(), false).unwrap();

        // Should only find the parent directory
        assert_eq!(result.len(), 1);
        assert!(result[0].filename.contains("Parent"));
    }

    #[test]
    fn test_directories_to_rename_recursive() {
        let temp_dir = create_test_dir();
        let dir_path = temp_dir.path();

        let subdir = create_subdir(dir_path, "25.12.2023 Parent");
        create_subdir(&subdir, "01.01.2024 Child");

        let result = directories_to_rename(dir_path.to_path_buf(), true).unwrap();

        // Should find both directories
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn test_directories_to_rename_sorted_case_insensitive() {
        let temp_dir = create_test_dir();
        let dir_path = temp_dir.path();

        create_subdir(dir_path, "25.12.2023 Zebra");
        create_subdir(dir_path, "01.01.2024 apple");
        create_subdir(dir_path, "15.06.2023 Mango");

        let result = directories_to_rename(dir_path.to_path_buf(), false).unwrap();

        assert_eq!(result.len(), 3);
        // Should be sorted case-insensitively: apple, Mango, Zebra
        assert!(result[0].filename.to_lowercase().contains("apple"));
        assert!(result[1].filename.to_lowercase().contains("mango"));
        assert!(result[2].filename.to_lowercase().contains("zebra"));
    }

    #[test]
    fn test_rename_item_creation() {
        let item = RenameItem {
            path: PathBuf::from("/test/path"),
            filename: "old_name.txt".to_string(),
            new_name: "new_name.txt".to_string(),
        };

        assert_eq!(item.path, PathBuf::from("/test/path"));
        assert_eq!(item.filename, "old_name.txt");
        assert_eq!(item.new_name, "new_name.txt");
    }

    // ==================== Config tests ====================

    #[test]
    fn test_default_file_extensions() {
        // Verify the default extensions are set correctly
        assert!(FILE_EXTENSIONS.contains(&"mp3"));
        assert!(FILE_EXTENSIONS.contains(&"mp4"));
        assert!(FILE_EXTENSIONS.contains(&"txt"));
        assert!(FILE_EXTENSIONS.contains(&"mkv"));
        assert_eq!(FILE_EXTENSIONS.len(), 20);
    }
}

#[cfg(test)]
mod test_date_flip_files {
    use super::*;
    use std::fs;
    use std::path::Path;
    use tempfile::TempDir;

    fn config(overwrite: bool) -> Config {
        Config {
            directory_mode: false,
            dryrun: false,
            file_extensions: vec!["txt".to_string()],
            overwrite,
            recurse: true,
            swap_year: false,
            verbose: false,
            year_first: false,
        }
    }

    /// Names of the regular files directly inside a directory.
    fn file_names(directory: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(directory)
            .expect("directory should be readable")
            .filter_map(Result::ok)
            .filter(|entry| entry.path().is_file())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn recursive_rename_keeps_files_in_their_own_directory() {
        let root = TempDir::new().expect("temp dir");
        let sub = root.path().join("sub");
        fs::create_dir(&sub).expect("create sub");
        fs::write(sub.join("Report 31.12.2020.txt"), "report").expect("write");

        date_flip_files(&root.path().to_path_buf(), &config(false)).expect("flip should succeed");

        assert!(file_names(root.path()).is_empty(), "{:?}", file_names(root.path()));
        let renamed = file_names(&sub);
        assert_eq!(renamed.len(), 1);
        assert_ne!(renamed[0], "Report 31.12.2020.txt");
    }

    #[test]
    fn same_names_in_different_directories_both_survive_with_overwrite() {
        let root = TempDir::new().expect("temp dir");
        for (directory, content) in [("a", "A"), ("b", "B")] {
            let path = root.path().join(directory);
            fs::create_dir(&path).expect("create directory");
            fs::write(path.join("x 31.12.2020.txt"), content).expect("write");
        }

        date_flip_files(&root.path().to_path_buf(), &config(true)).expect("flip should succeed");

        for (directory, content) in [("a", "A"), ("b", "B")] {
            let names = file_names(&root.path().join(directory));
            assert_eq!(names.len(), 1, "{directory}: {names:?}");
            let path = root.path().join(directory).join(&names[0]);
            assert_eq!(fs::read_to_string(path).expect("read"), content);
        }
    }

    #[test]
    fn an_existing_target_is_kept_without_overwrite() {
        let root = TempDir::new().expect("temp dir");
        fs::write(root.path().join("x 31.12.2020.txt"), "source").expect("write");
        let target_name = Date::reorder_filename_date("x 31.12.2020.txt", false, false, false).expect("has a date");
        fs::write(root.path().join(&target_name), "existing").expect("write");

        date_flip_files(&root.path().to_path_buf(), &config(false)).expect("flip should succeed");

        assert_eq!(
            fs::read_to_string(root.path().join(&target_name)).expect("read"),
            "existing"
        );
        assert_eq!(
            fs::read_to_string(root.path().join("x 31.12.2020.txt")).expect("read"),
            "source"
        );
    }
}

#[cfg(test)]
mod test_date_flip_directories {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn config(overwrite: bool) -> Config {
        Config {
            directory_mode: true,
            dryrun: false,
            file_extensions: Vec::new(),
            overwrite,
            recurse: true,
            swap_year: false,
            verbose: false,
            year_first: false,
        }
    }

    /// Paths of every directory under the root, relative to it and sorted.
    fn directories(root: &std::path::Path) -> Vec<String> {
        let mut names: Vec<String> = WalkDir::new(root)
            .min_depth(1)
            .into_iter()
            .filter_map(std::result::Result::ok)
            .filter(|entry| entry.file_type().is_dir())
            .map(|entry| {
                entry
                    .path()
                    .strip_prefix(root)
                    .expect("entry should be under the root")
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        names
    }

    #[test]
    fn nested_dated_directories_are_both_renamed() {
        let root = TempDir::new().expect("temp dir");
        // The parent sorts before the child by name, so a name ordered run renames the parent first.
        fs::create_dir_all(root.path().join("A Trip 01.01.2020").join("B Day 31.12.2021")).expect("create");

        date_flip_directories(root.path().to_path_buf(), &config(false)).expect("flip should succeed");

        let names = directories(root.path());
        assert_eq!(names.len(), 2, "{names:?}");
        assert!(
            names
                .iter()
                .all(|name| !name.contains("01.01.2020") && !name.contains("31.12.2021")),
            "{names:?}"
        );
    }

    #[test]
    fn an_existing_target_directory_is_kept() {
        let root = TempDir::new().expect("temp dir");
        fs::create_dir(root.path().join("Trip 01.01.2020")).expect("create");
        let target = Date::reorder_directory_date("Trip 01.01.2020").expect("has a date");
        fs::create_dir(root.path().join(&target)).expect("create target");
        fs::write(root.path().join(&target).join("keep.txt"), "keep").expect("write");

        date_flip_directories(root.path().to_path_buf(), &config(false)).expect("flip should succeed");

        assert!(root.path().join("Trip 01.01.2020").is_dir());
        assert_eq!(
            fs::read_to_string(root.path().join(&target).join("keep.txt")).expect("read"),
            "keep"
        );
    }
}

#[cfg(test)]
mod test_nested_trees {
    use std::collections::BTreeSet;

    use cli_tools_common::test_support::TempTree;

    use super::*;

    /// Config for file mode with every option off except the ones given.
    fn file_config(recurse: bool, dryrun: bool, overwrite: bool) -> Config {
        Config {
            dryrun,
            file_extensions: FILE_EXTENSIONS.iter().map(ToString::to_string).collect(),
            overwrite,
            recurse,
            ..Config::default()
        }
    }

    /// Config for directory mode with every option off except the ones given.
    fn directory_config(recurse: bool, dryrun: bool) -> Config {
        Config {
            directory_mode: true,
            dryrun,
            recurse,
            ..Config::default()
        }
    }

    /// Tree with a dated file at every depth, an undated file, and a correctly dated file.
    fn nested_files() -> TempTree {
        let tree = TempTree::new();
        tree.file_with("Report 31.12.2020.txt", "report");
        tree.file_with("a/Song 01.05.2021.mp3", "song");
        tree.file_with("a/b/Clip 15.06.2022.mp4", "clip");
        tree.file_with("a/b/c/Photo 2.3.2023.jpg", "photo");
        tree.file_with("a/b/c/plain.txt", "plain");
        tree.file_with("a/b/ok 2020.01.02.txt", "ok");
        tree.file_with("a/b/skip 01.02.2020.doc", "other extension");
        tree
    }

    /// Run file mode on the whole tree.
    fn flip_files(tree: &TempTree, config: &Config) {
        date_flip_files(&tree.path().to_path_buf(), config).expect("flip should succeed");
    }

    /// Run directory mode on the whole tree.
    fn flip_directories(tree: &TempTree, config: &Config) {
        date_flip_directories(tree.path().to_path_buf(), config).expect("flip should succeed");
    }

    /// Contents of every regular file in the tree, so a test can check that nothing was lost.
    fn contents(tree: &TempTree) -> BTreeSet<String> {
        tree.files()
            .iter()
            .filter(|relative| tree.join(relative).is_file())
            .map(|relative| tree.read(relative))
            .collect()
    }

    #[test]
    fn recursive_run_renames_files_at_every_depth() {
        let tree = nested_files();

        flip_files(&tree, &file_config(true, false, false));

        assert_eq!(
            tree.files(),
            vec![
                "Report 2020.12.31.txt",
                "a/Song 2021.05.01.mp3",
                "a/b/Clip 2022.06.15.mp4",
                "a/b/c/Photo 2023.03.02.jpg",
                "a/b/c/plain.txt",
                "a/b/ok 2020.01.02.txt",
                "a/b/skip 01.02.2020.doc",
            ]
        );
        assert_eq!(tree.read("a/b/c/Photo 2023.03.02.jpg"), "photo");
    }

    #[test]
    fn non_recursive_run_only_renames_the_top_level() {
        let tree = nested_files();

        flip_files(&tree, &file_config(false, false, false));

        assert!(tree.exists("Report 2020.12.31.txt"));
        assert!(tree.exists("a/Song 01.05.2021.mp3"));
        assert!(tree.exists("a/b/Clip 15.06.2022.mp4"));
        assert!(tree.exists("a/b/c/Photo 2.3.2023.jpg"));
    }

    #[test]
    fn second_run_changes_nothing() {
        let tree = nested_files();
        flip_files(&tree, &file_config(true, false, false));
        let after_first_run = tree.snapshot();

        flip_files(&tree, &file_config(true, false, false));

        assert_eq!(tree.snapshot(), after_first_run);
    }

    #[test]
    fn dryrun_leaves_the_tree_unchanged() {
        let tree = nested_files();
        tree.dir("Trip 24.12.2019/Day 01.02.2020");
        let before = tree.snapshot();

        flip_files(&tree, &file_config(true, true, true));
        flip_directories(&tree, &directory_config(true, true));

        assert_eq!(tree.snapshot(), before);
    }

    #[test]
    fn a_file_given_as_the_path_is_renamed_alone() {
        let tree = nested_files();

        date_flip_files(&tree.join("a/Song 01.05.2021.mp3"), &file_config(false, false, false))
            .expect("flip should succeed");

        assert!(tree.exists("a/Song 2021.05.01.mp3"));
        assert!(tree.exists("Report 31.12.2020.txt"));
        assert!(tree.exists("a/b/Clip 15.06.2022.mp4"));
    }

    #[test]
    fn empty_directory_is_a_no_op() {
        let tree = TempTree::with_entries(&["empty/"]);

        flip_files(&tree, &file_config(true, false, false));
        flip_directories(&tree, &directory_config(true, false));

        assert_eq!(tree.snapshot(), vec!["empty/"]);
    }

    #[test]
    fn hidden_files_and_directories_are_skipped() {
        let tree = TempTree::new();
        tree.file(".hidden 01.02.2020.txt");
        tree.file(".secret/x 01.02.2020.txt");
        tree.dir(".private/Trip 01.02.2020");
        tree.file("visible/y 01.02.2020.txt");
        let hidden_before: Vec<String> = tree
            .snapshot()
            .into_iter()
            .filter(|entry| entry.starts_with('.'))
            .collect();

        flip_files(&tree, &file_config(true, false, false));
        flip_directories(&tree, &directory_config(true, false));

        let hidden_after: Vec<String> = tree
            .snapshot()
            .into_iter()
            .filter(|entry| entry.starts_with('.'))
            .collect();
        assert_eq!(hidden_after, hidden_before);
        assert!(tree.exists("visible/y 2020.02.01.txt"));
    }

    #[test]
    fn same_target_in_one_directory_keeps_both_files_without_overwrite() {
        let tree = TempTree::new();
        tree.file_with("x 01.02.2020.txt", "first");
        tree.file_with("x 1.2.2020.txt", "second");

        flip_files(&tree, &file_config(false, false, false));

        assert_eq!(tree.files().len(), 2, "{:?}", tree.files());
        assert!(tree.exists("x 2020.02.01.txt"));
        assert_eq!(
            contents(&tree),
            BTreeSet::from(["first".to_string(), "second".to_string()])
        );
    }

    #[test]
    fn overwrite_never_replaces_a_file_renamed_earlier_in_the_same_run() {
        let tree = TempTree::new();
        tree.file_with("x 01.02.2020.txt", "first");
        tree.file_with("x 1.2.2020.txt", "second");

        flip_files(&tree, &file_config(false, false, true));

        assert_eq!(
            contents(&tree),
            BTreeSet::from(["first".to_string(), "second".to_string()])
        );
    }

    #[test]
    fn overwrite_replaces_a_target_that_existed_before_the_run() {
        let tree = TempTree::new();
        tree.file_with("x 01.02.2020.txt", "source");
        tree.file_with("x 2020.02.01.txt", "existing");

        flip_files(&tree, &file_config(false, false, true));

        assert_eq!(tree.files(), vec!["x 2020.02.01.txt"]);
        assert_eq!(tree.read("x 2020.02.01.txt"), "source");
    }

    #[test]
    fn same_name_in_different_directories_is_renamed_in_each() {
        let tree = TempTree::new();
        tree.file_with("one/x 01.02.2020.txt", "one");
        tree.file_with("one/two/x 01.02.2020.txt", "two");
        tree.file_with("one/two/three/x 01.02.2020.txt", "three");

        flip_files(&tree, &file_config(true, false, false));

        assert_eq!(
            tree.files(),
            vec![
                "one/two/three/x 2020.02.01.txt",
                "one/two/x 2020.02.01.txt",
                "one/x 2020.02.01.txt",
            ]
        );
        assert_eq!(tree.read("one/two/three/x 2020.02.01.txt"), "three");
    }

    #[test]
    fn target_differing_only_by_case_keeps_both_files() {
        let tree = TempTree::new();
        tree.file_with("Clip 01.02.2020.txt", "source");
        tree.file_with("clip 2020.02.01.txt", "existing");

        flip_files(&tree, &file_config(false, false, false));

        assert_eq!(tree.files().len(), 2, "{:?}", tree.files());
        assert_eq!(
            contents(&tree),
            BTreeSet::from(["existing".to_string(), "source".to_string()])
        );
    }

    #[test]
    fn deeply_nested_dated_directories_are_all_renamed() {
        let tree = TempTree::new();
        tree.file_with("Trip 24.12.2019/Day 01.02.2020/Evening 03.04.2021/notes.txt", "notes");
        tree.dir("Trip 24.12.2019/plain");

        flip_directories(&tree, &directory_config(true, false));

        assert_eq!(
            tree.dirs(),
            vec![
                "2019-12-24 Trip",
                "2019-12-24 Trip/2020-02-01 Day",
                "2019-12-24 Trip/2020-02-01 Day/2021-04-03 Evening",
                "2019-12-24 Trip/plain",
            ]
        );
        assert_eq!(
            tree.read("2019-12-24 Trip/2020-02-01 Day/2021-04-03 Evening/notes.txt"),
            "notes"
        );
    }

    #[test]
    fn non_recursive_directory_run_only_renames_the_top_level() {
        let tree = TempTree::with_entries(&["Trip 24.12.2019/Day 01.02.2020/"]);

        flip_directories(&tree, &directory_config(false, false));

        assert_eq!(tree.dirs(), vec!["2019-12-24 Trip", "2019-12-24 Trip/Day 01.02.2020"]);
    }

    #[test]
    fn directories_with_the_same_target_are_both_kept() {
        let tree = TempTree::new();
        tree.file_with("Trip 01.02.2020/a.txt", "a");
        tree.file_with("Trip 1.2.2020/b.txt", "b");

        flip_directories(&tree, &directory_config(false, false));

        assert_eq!(tree.dirs().len(), 2, "{:?}", tree.dirs());
        assert!(tree.exists("2020-02-01 Trip"));
        assert_eq!(contents(&tree), BTreeSet::from(["a".to_string(), "b".to_string()]));
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_not_followed_into_their_targets() {
        let tree = TempTree::new();
        tree.file_with("outside/real 01.02.2020.txt", "real");
        tree.file_with("outside/folder/inner 01.02.2020.txt", "inner");
        tree.dir("outside/Linked 01.02.2020");
        tree.symlink("outside/folder", "walked/folder link");
        tree.symlink("outside/Linked 01.02.2020", "walked/Link 05.06.2020");
        tree.symlink("outside/real 01.02.2020.txt", "walked/file link 01.02.2020.txt");

        date_flip_files(&tree.join("walked"), &file_config(true, false, false)).expect("flip should succeed");
        date_flip_directories(tree.join("walked"), &directory_config(true, false)).expect("flip should succeed");

        assert_eq!(tree.read("outside/real 01.02.2020.txt"), "real");
        assert_eq!(tree.read("outside/folder/inner 01.02.2020.txt"), "inner");
        assert!(tree.join("outside/Linked 01.02.2020").is_dir());
        for link in tree.files().iter().filter(|relative| relative.starts_with("walked/")) {
            assert!(tree.join(link).exists(), "link should still resolve: {link}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn name_that_is_not_utf8_keeps_its_contents() {
        let tree = TempTree::new();
        let Some(path) = tree.non_utf8_file("sub", "x 01.02.2020 ", "txt") else {
            return;
        };
        std::fs::write(&path, "data").expect("file should be written");

        flip_files(&tree, &file_config(true, false, false));

        assert!(path.exists(), "non-UTF-8 file must not be corrupted into U+FFFD name");
        assert_eq!(tree.files().len(), 1, "{:?}", tree.files());
        assert_eq!(contents(&tree), BTreeSet::from(["data".to_string()]));
    }

    #[test]
    fn uppercase_extensions_are_renamed_in_directory_scans() {
        let tree = TempTree::new();
        tree.file_with("Photo 01.02.2020.JPG", "img");
        tree.file_with("Clip 31.12.2023.MP4", "vid");

        flip_files(&tree, &file_config(false, false, false));

        assert!(tree.exists("Photo 2020.02.01.JPG"), "{:?}", tree.snapshot());
        assert!(tree.exists("Clip 2023.12.31.MP4"), "{:?}", tree.snapshot());
    }

    /// Simulate an input disappearing after planning but before staging.
    #[test]
    fn a_failed_staging_rename_restores_previously_staged_input() {
        let tree = TempTree::new();
        tree.file_with("first.txt", "first input");
        let before = tree.snapshot();
        let planned = [("first.txt", "missing.txt"), ("missing.txt", "first.txt")]
            .into_iter()
            .map(|(source, target)| {
                let item = RenameItem {
                    path: tree.join(source),
                    filename: source.to_string(),
                    new_name: target.to_string(),
                };
                (item, tree.join(target))
            })
            .collect::<Vec<_>>();

        let error = execute_planned_renames(&planned, OverwritePolicy::Overwrite).unwrap_err();

        assert!(error.to_string().contains("Failed to stage"), "{error:#}");
        assert_eq!(tree.snapshot(), before);
    }

    #[test]
    fn swap_with_overwrite_preserves_both_files_in_a_rename_cycle() {
        let tree = TempTree::new();
        tree.file_with("photo 2005.12.23.jpg", "A_content");
        tree.file_with("photo 2023.12.05.jpg", "B_content");

        let mut config = file_config(false, false, true);
        config.swap_year = true;
        flip_files(&tree, &config);

        assert_eq!(tree.read("photo 2023.12.05.jpg"), "A_content");
        assert_eq!(tree.read("photo 2005.12.23.jpg"), "B_content");
    }

    #[test]
    fn swap_with_overwrite_preserves_all_files_in_a_rename_chain() {
        let tree = TempTree::new();
        tree.file_with("05.12.2023.jpg", "A_content");
        tree.file_with("2023.12.05.jpg", "B_content");

        let mut config = file_config(false, false, true);
        config.swap_year = true;
        flip_files(&tree, &config);

        assert_eq!(tree.read("2023.12.05.jpg"), "A_content");
        assert_eq!(tree.read("2005.12.23.jpg"), "B_content");
    }
}

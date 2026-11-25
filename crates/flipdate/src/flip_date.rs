//! File and directory renaming for flipdate.
//!
//! Finds dates in file or directory names and renames them to the `yyyy.mm.dd` format.

use std::path::PathBuf;

use anyhow::{Context, Result};
use colored::Colorize;
use walkdir::WalkDir;

use cli_tools_common::date::Date;
use cli_tools_common::{OverwritePolicy, RenameOutcome};

pub use crate::config::Config;

pub static FILE_EXTENSIONS: [&str; 20] = [
    "m4a", "mp3", // Audio
    "mp4", "mkv", "mov", "avi", "wmv", "flv", "m4v", // Video
    "jpg", "jpeg", "png", "gif", "bmp", "tiff", "webp", "heic", // Image
    "txt", "rtf", "csv", // Other
];

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
        let filename = file
            .file_name()
            .context("Failed to get filename")?
            .to_string_lossy()
            .into_owned();

        if let Some(new_name) =
            Date::reorder_filename_date(&filename, config.year_first, config.swap_year, config.verbose)
            && new_name.to_lowercase() != filename.to_lowercase()
        {
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
    for item in files_to_rename {
        // Recursive runs find files in subdirectories, so the new name stays in the file's own directory.
        let new_path = item.path.with_file_name(&item.new_name);
        if new_path == item.path {
            continue;
        }
        println!("{heading}");
        cli_tools_common::show_diff(&item.filename, &item.new_name);
        if !config.dryrun {
            let outcome = cli_tools_common::safe_rename(&item.path, &new_path, policy)
                .with_context(|| format!("Failed to rename {}", item.path.display()))?;
            if outcome == RenameOutcome::SkippedExisting {
                eprintln!("{}", "File already exists".yellow());
            }
        }
    }

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
                            .any(|e| ext.to_str().is_some_and(|ext_str| e == ext_str))
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
            let filename = entry.file_name().to_string_lossy().into_owned();
            if let Some(new_name) = Date::reorder_directory_date(&filename) {
                directories_to_rename.push(RenameItem {
                    path: entry.path().to_path_buf(),
                    filename,
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

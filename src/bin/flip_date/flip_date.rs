use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use colored::Colorize;
use walkdir::WalkDir;

use cli_tools::date::Date;

pub use crate::config::Config;

pub static FILE_EXTENSIONS: [&str; 9] = ["m4a", "mp3", "txt", "rtf", "csv", "mp4", "mkv", "mov", "avi"];

#[derive(Debug)]
struct RenameItem {
    path: PathBuf,
    filename: String,
    new_name: String,
}

/// Flip date to start with year for all matching files from the given path.
pub fn date_flip_files(path: &PathBuf, config: &Config) -> Result<()> {
    let (files, root) = files_to_rename(path, &config.file_extensions, config.recurse)?;
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
    files_to_rename.sort_by(|a, b| a.filename.to_lowercase().cmp(&b.filename.to_lowercase()));

    let heading = if config.dryrun {
        "Dryrun:".cyan().bold()
    } else {
        "Rename:".magenta().bold()
    };

    for item in files_to_rename {
        let new_path = root.join(&item.new_name);
        if new_path == item.path {
            continue;
        }
        println!("{heading}");
        cli_tools::show_diff(&item.filename, &item.new_name);
        if !config.dryrun {
            if new_path.exists() && !config.overwrite {
                eprintln!("{}", "File already exists".yellow());
            } else {
                fs::rename(item.path, new_path).context("Failed to rename file")?;
            }
        }
    }

    Ok(())
}

/// Flip date to start with year for all matching directories from the given path.
pub fn date_flip_directories(path: PathBuf, config: &Config) -> Result<()> {
    let directories = directories_to_rename(path, config.recurse)?;
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
            fs::rename(&directory.path, &new_path).with_context(|| {
                format!(
                    "Failed to rename {} to {}",
                    directory.path.display(),
                    new_path.display()
                )
            })?;
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
            .filter_entry(|e| !cli_tools::should_skip_entry(e))
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

    for entry in walker.into_iter().filter_entry(|e| !cli_tools::should_skip_entry(e)) {
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
    directories_to_rename.sort_by(|a, b| a.filename.to_lowercase().cmp(&b.filename.to_lowercase()));

    Ok(directories_to_rename)
}

#[cfg(test)]
mod tests {
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

        assert!(files.is_empty());
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

        assert!(files.is_empty());
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
        assert_eq!(FILE_EXTENSIONS.len(), 9);
    }
}

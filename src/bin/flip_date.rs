use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use colored::Colorize;
use serde::Deserialize;
use walkdir::WalkDir;

use cli_tools::date::Date;

static FILE_EXTENSIONS: [&str; 9] = ["m4a", "mp3", "txt", "rtf", "csv", "mp4", "mkv", "mov", "avi"];

#[derive(Parser)]
#[command(
    author,
    version,
    name = env!("CARGO_BIN_NAME"),
    about = "Flip dates in file and directory names to start with year"
)]
struct Args {
    /// Optional input directory or file
    #[arg(value_hint = clap::ValueHint::AnyPath)]
    path: Option<PathBuf>,

    /// Use directory rename mode
    #[arg(short, long)]
    dir: bool,

    /// Overwrite existing
    #[arg(short, long)]
    force: bool,

    /// Specify file extension(s)
    #[arg(short, long, num_args = 1, action = clap::ArgAction::Append, value_name = "EXTENSION", conflicts_with = "dir")]
    extensions: Option<Vec<String>>,

    /// Assume year is first in short dates
    #[arg(short, long)]
    year: bool,

    /// Only print changes without renaming
    #[arg(short, long)]
    print: bool,

    /// Recurse into subdirectories
    #[arg(short, long)]
    recurse: bool,

    /// Swap year and day around
    #[arg(short, long)]
    swap: bool,

    /// Print verbose output
    #[arg(short, long)]
    verbose: bool,
}

/// Config from a config file
#[derive(Debug, Default, Deserialize)]
struct DateConfig {
    #[serde(default)]
    directory: bool,
    #[serde(default)]
    dryrun: bool,
    #[serde(default)]
    file_extensions: Vec<String>,
    #[serde(default)]
    overwrite: bool,
    #[serde(default)]
    recurse: bool,
    #[serde(default)]
    swap_year: bool,
    #[serde(default)]
    verbose: bool,
    #[serde(default)]
    year_first: bool,
}

/// Wrapper needed for parsing the config file section.
#[derive(Debug, Default, Deserialize)]
struct UserConfig {
    #[serde(default)]
    flip_date: DateConfig,
}

/// Final config created from CLI arguments and user config file.
#[derive(Debug, Default)]
struct Config {
    directory_mode: bool,
    dryrun: bool,
    file_extensions: Vec<String>,
    overwrite: bool,
    recurse: bool,
    swap_year: bool,
    verbose: bool,
    year_first: bool,
}

#[derive(Debug)]
struct RenameItem {
    path: PathBuf,
    filename: String,
    new_name: String,
}

impl DateConfig {
    /// Try to read user config from the file if it exists.
    /// Otherwise, fall back to default config.
    ///
    /// # Errors
    /// Returns an error if config file exists but cannot be read or parsed.
    fn get_user_config() -> Result<Self> {
        let Some(path) = cli_tools::config::CONFIG_PATH.as_deref() else {
            return Ok(Self::default());
        };

        match fs::read_to_string(path) {
            Ok(content) => Self::from_toml_str(&content)
                .map_err(|e| anyhow::anyhow!("Failed to parse config file {}:\n{e}", path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(anyhow::anyhow!(
                "Failed to read config file {}: {error}",
                path.display()
            )),
        }
    }

    /// Parse config from a TOML string.
    ///
    /// # Errors
    /// Returns an error if the TOML string is invalid.
    fn from_toml_str(toml_str: &str) -> Result<Self> {
        toml::from_str::<UserConfig>(toml_str)
            .map(|config| config.flip_date)
            .context("Failed to parse flip_date config TOML")
    }
}

impl Config {
    /// Create config from given command line args and user config file.
    ///
    /// # Errors
    /// Returns an error if the config file cannot be read or parsed.
    pub fn from_args(args: Args) -> Result<Self> {
        let user_config = DateConfig::get_user_config()?;

        // Determine which extensions to use (args > config > default)
        let file_extensions = args
            .extensions
            .filter(|extensions| !extensions.is_empty())
            .or({
                if user_config.file_extensions.is_empty() {
                    None
                } else {
                    Some(user_config.file_extensions)
                }
            })
            .unwrap_or_else(|| FILE_EXTENSIONS.iter().map(std::string::ToString::to_string).collect());

        Ok(Self {
            directory_mode: args.dir || user_config.directory,
            dryrun: args.print || user_config.dryrun,
            file_extensions,
            overwrite: args.force || user_config.overwrite,
            recurse: args.recurse || user_config.recurse,
            swap_year: args.swap || user_config.swap_year,
            verbose: args.verbose || user_config.verbose,
            year_first: args.year || user_config.year_first,
        })
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    let path = cli_tools::resolve_input_path(args.path.as_deref())?;
    let config = Config::from_args(args)?;
    if config.directory_mode {
        date_flip_directories(path, &config)
    } else {
        date_flip_files(&path, &config)
    }
}

/// Flip date to start with year for all matching files from the given path.
fn date_flip_files(path: &PathBuf, config: &Config) -> Result<()> {
    let (files, root) = files_to_rename(path, &config.file_extensions, config.recurse)?;
    if files.is_empty() {
        anyhow::bail!("No files to process");
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
fn date_flip_directories(path: PathBuf, config: &Config) -> Result<()> {
    let directories = directories_to_rename(path, config.recurse)?;
    if directories.is_empty() {
        anyhow::bail!("No directories to rename")
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
        assert!(files[0].file_name().unwrap() == "root.mp3");
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
        assert!(files[0].file_name().unwrap() == "apple.mp3");
        assert!(files[1].file_name().unwrap() == "mango.mp3");
        assert!(files[2].file_name().unwrap() == "zebra.mp3");
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

#[cfg(test)]
mod date_config_tests {
    use super::*;

    #[test]
    fn from_toml_str_parses_empty_config() {
        let toml = "";
        let config = DateConfig::from_toml_str(toml).unwrap();
        assert!(!config.directory);
        assert!(!config.dryrun);
        assert!(!config.verbose);
    }

    #[test]
    fn from_toml_str_parses_flip_date_section() {
        let toml = r"
[flip_date]
directory = true
dryrun = true
verbose = true
recurse = true
";
        let config = DateConfig::from_toml_str(toml).unwrap();
        assert!(config.directory);
        assert!(config.dryrun);
        assert!(config.verbose);
        assert!(config.recurse);
    }

    #[test]
    fn from_toml_str_parses_file_extensions() {
        let toml = r#"
[flip_date]
file_extensions = ["mp4", "mkv", "avi"]
"#;
        let config = DateConfig::from_toml_str(toml).unwrap();
        assert_eq!(config.file_extensions, vec!["mp4", "mkv", "avi"]);
    }

    #[test]
    fn from_toml_str_parses_overwrite_and_swap() {
        let toml = r"
[flip_date]
overwrite = true
swap_year = true
year_first = true
";
        let config = DateConfig::from_toml_str(toml).unwrap();
        assert!(config.overwrite);
        assert!(config.swap_year);
        assert!(config.year_first);
    }

    #[test]
    fn from_toml_str_invalid_toml_returns_error() {
        let toml = "this is not valid toml {{{";
        let result = DateConfig::from_toml_str(toml);
        assert!(result.is_err());
    }

    #[test]
    fn from_toml_str_ignores_other_sections() {
        let toml = r"
[other_section]
some_value = true

[flip_date]
verbose = true
";
        let config = DateConfig::from_toml_str(toml).unwrap();
        assert!(config.verbose);
        assert!(!config.directory);
    }

    #[test]
    fn default_values_are_correct() {
        let config = DateConfig::default();
        assert!(!config.directory);
        assert!(!config.dryrun);
        assert!(!config.overwrite);
        assert!(!config.recurse);
        assert!(!config.swap_year);
        assert!(!config.verbose);
        assert!(!config.year_first);
        assert!(config.file_extensions.is_empty());
    }
}

#[cfg(test)]
mod config_from_args_tests {
    use super::*;

    fn default_args() -> Args {
        Args {
            path: None,
            dir: false,
            force: false,
            extensions: None,
            year: false,
            print: false,
            recurse: false,
            swap: false,
            verbose: false,
        }
    }

    #[test]
    fn from_args_uses_default_extensions() {
        let args = default_args();
        let config = Config::from_args(args).expect("config should parse");
        assert_eq!(config.file_extensions.len(), FILE_EXTENSIONS.len());
    }

    #[test]
    fn from_args_cli_overrides_defaults() {
        let mut args = default_args();
        args.dir = true;
        args.force = true;
        args.print = true;
        args.recurse = true;
        args.swap = true;
        args.verbose = true;
        args.year = true;

        let config = Config::from_args(args).expect("config should parse");
        assert!(config.directory_mode);
        assert!(config.overwrite);
        assert!(config.dryrun);
        assert!(config.recurse);
        assert!(config.swap_year);
        assert!(config.verbose);
        assert!(config.year_first);
    }

    #[test]
    fn from_args_uses_cli_extensions() {
        let mut args = default_args();
        args.extensions = Some(vec!["mp4".to_string(), "mkv".to_string()]);

        let config = Config::from_args(args).expect("config should parse");
        assert_eq!(config.file_extensions, vec!["mp4", "mkv"]);
    }
}

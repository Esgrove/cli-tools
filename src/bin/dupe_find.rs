use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

use clap::{CommandFactory, Parser};
use clap_complete::Shell;
use colored::Colorize;
use itertools::Itertools;
use rayon::prelude::*;
use regex::Regex;
use serde::Deserialize;
use walkdir::WalkDir;

use cli_tools::{print_error, print_warning};

/// Common video resolution patterns
const RESOLUTION_PATTERNS: &[&str] = &["720p", "1080p", "1440p", "2160p"];

/// Common codec patterns to remove when normalizing
const CODEC_PATTERNS: &[&str] = &["x264", "x265", "h264", "h265"];

/// All video extensions
const FILE_EXTENSIONS: &[&str] = &["mp4", "mkv", "wmv", "flv", "m4v", "ts", "mpg", "avi", "mov", "webm"];

/// Regex to match resolution patterns like 720p, 1080p, or 1234x5678
static RE_RESOLUTION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(\d{3,4}p|\d{3,4}x\d{3,4})\b").expect("Invalid resolution regex"));

/// Regex to match codec patterns
static RE_CODEC: LazyLock<Regex> = LazyLock::new(|| {
    let pattern = format!(r"(?i)\b({})\b", CODEC_PATTERNS.join("|"));
    Regex::new(&pattern).expect("Invalid codec regex")
});

#[derive(Parser)]
#[command(author, version, name = env!("CARGO_BIN_NAME"), about = "Find duplicate video files based on identifier patterns")]
struct Args {
    /// Input directories to search
    #[arg(value_hint = clap::ValueHint::DirPath)]
    paths: Vec<PathBuf>,

    /// Identifier patterns to search for (regex)
    #[arg(short = 'g', long, num_args = 1, action = clap::ArgAction::Append, name = "PATTERN")]
    pattern: Vec<String>,

    /// File extensions to include
    #[arg(short = 'e', long, num_args = 1, action = clap::ArgAction::Append, name = "EXTENSION")]
    extension: Vec<String>,

    /// Move duplicates to a "Duplicates" directory
    #[arg(short, long = "move")]
    move_files: bool,

    /// Only print changes without moving files
    #[arg(short, long)]
    print: bool,

    /// Recurse into subdirectories
    #[arg(short, long)]
    recurse: bool,

    /// Use default paths from config file
    #[arg(short, long)]
    default: bool,

    /// Generate shell completion
    #[arg(short = 'l', long, name = "SHELL")]
    completion: Option<Shell>,

    /// Print verbose output
    #[arg(short, long)]
    verbose: bool,
}

/// Config from the user config file.
#[derive(Debug, Default, Deserialize)]
struct DupeConfig {
    #[serde(default)]
    default_paths: Vec<PathBuf>,
    #[serde(default)]
    dryrun: bool,
    #[serde(default)]
    extensions: Vec<String>,
    #[serde(default)]
    move_files: bool,
    #[serde(default)]
    paths: Vec<PathBuf>,
    #[serde(default)]
    patterns: Vec<String>,
    #[serde(default)]
    recurse: bool,
    #[serde(default)]
    verbose: bool,
}

/// Wrapper needed for parsing the config file section.
#[derive(Debug, Default, Deserialize)]
struct UserConfig {
    #[serde(default)]
    dupefind: DupeConfig,
}

/// Final config created from CLI arguments and user config file.
#[derive(Debug, Clone)]
struct Config {
    dryrun: bool,
    extensions: Vec<String>,
    move_files: bool,
    patterns: Vec<Regex>,
    recurse: bool,
    verbose: bool,
}

/// Information about a found file
#[derive(Debug, Clone)]
struct FileInfo {
    path: PathBuf,
    filename: String,
}

struct DupeFind {
    roots: Vec<PathBuf>,
    config: Config,
}

impl DupeConfig {
    /// Try to read user config from the file if it exists.
    fn get_user_config() -> Self {
        cli_tools::config::CONFIG_PATH
            .as_deref()
            .and_then(|path| {
                fs::read_to_string(path)
                    .map_err(|e| {
                        print_error!("Error reading config file {}: {e}", path.display());
                    })
                    .ok()
            })
            .and_then(|config_string| {
                toml::from_str::<UserConfig>(&config_string)
                    .map_err(|e| {
                        print_error!("Error reading config file: {e}");
                    })
                    .ok()
            })
            .map(|config| config.dupefind)
            .unwrap_or_default()
    }
}

impl Config {
    /// Create config from given command line args and user config file.
    pub fn from_args(args: &Args) -> anyhow::Result<Self> {
        let user_config = DupeConfig::get_user_config();

        // Combine patterns from config and CLI
        let pattern_strings: Vec<String> = user_config
            .patterns
            .into_iter()
            .chain(args.pattern.clone())
            .unique()
            .collect();

        // Compile regex patterns
        let patterns: Vec<Regex> = pattern_strings
            .iter()
            .map(|p| Regex::new(p))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| anyhow::anyhow!("Invalid regex pattern: {e}"))?;

        // Combine extensions from config and CLI, with defaults if none specified
        let mut extensions: Vec<String> = user_config
            .extensions
            .into_iter()
            .chain(args.extension.clone())
            .unique()
            .collect();

        if extensions.is_empty() {
            extensions = FILE_EXTENSIONS.iter().map(|&s| s.to_string()).collect();
        }

        // Normalize extensions to lowercase without leading dot
        let extensions: Vec<String> = extensions
            .into_iter()
            .map(|e| e.trim_start_matches('.').to_lowercase())
            .collect();

        Ok(Self {
            dryrun: args.print || user_config.dryrun,
            extensions,
            move_files: args.move_files || user_config.move_files,
            patterns,
            recurse: args.recurse || user_config.recurse,
            verbose: args.verbose || user_config.verbose,
        })
    }
}

impl FileInfo {
    fn new(path: PathBuf) -> Self {
        let filename = path.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
        Self { path, filename }
    }
}

impl DupeFind {
    pub fn new(args: &Args) -> anyhow::Result<Self> {
        let user_config = DupeConfig::get_user_config();

        // Resolve all input paths:
        // - If default flag is set, use default_paths from config
        // - CLI args take priority, then config file, then current directory
        let roots = if args.default && !user_config.default_paths.is_empty() {
            user_config
                .default_paths
                .iter()
                .map(|p| cli_tools::resolve_required_input_path(p))
                .collect::<anyhow::Result<Vec<_>>>()?
        } else if !args.paths.is_empty() {
            args.paths
                .iter()
                .map(|p| cli_tools::resolve_required_input_path(p))
                .collect::<anyhow::Result<Vec<_>>>()?
        } else if !user_config.paths.is_empty() {
            user_config
                .paths
                .iter()
                .map(|p| cli_tools::resolve_required_input_path(p))
                .collect::<anyhow::Result<Vec<_>>>()?
        } else {
            vec![cli_tools::resolve_input_path(None)?]
        };

        let config = Config::from_args(args)?;

        Ok(Self { roots, config })
    }

    pub fn run(&self) -> anyhow::Result<()> {
        let paths_display = self
            .roots
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        println!("Scanning {} for video files...", paths_display.cyan());

        if self.config.verbose {
            println!("Extensions: {:?}", self.config.extensions);
            println!("Recursive: {}", self.config.recurse);
            if !self.config.patterns.is_empty() {
                println!(
                    "Patterns: {:?}",
                    self.config
                        .patterns
                        .iter()
                        .map(regex::Regex::as_str)
                        .collect::<Vec<_>>()
                );
            }
        }

        // Collect all video files in parallel
        let files = self.collect_video_files_parallel();
        println!("Found {} video files", files.len());

        // Check for exact filename duplicates in different paths
        self.find_path_duplicates(&files);

        // Check for pattern-based duplicates
        if !self.config.patterns.is_empty() {
            self.find_pattern_duplicates(&files)?;
        }

        // Check for resolution/codec/extension variants
        self.find_variants(&files);

        Ok(())
    }

    /// Collect all video files from all root directories in parallel
    fn collect_video_files_parallel(&self) -> Vec<FileInfo> {
        let files: Mutex<Vec<FileInfo>> = Mutex::new(Vec::new());

        // Process each root directory in parallel
        self.roots.par_iter().for_each(|root| {
            let root_files = self.collect_video_files_from_root(root);
            if let Ok(mut all_files) = files.lock() {
                all_files.extend(root_files);
            }
        });

        files.into_inner().unwrap_or_default()
    }

    /// Collect video files from a single root directory
    fn collect_video_files_from_root(&self, root: &Path) -> Vec<FileInfo> {
        let mut files = Vec::new();

        let walker = if self.config.recurse {
            WalkDir::new(root)
        } else {
            WalkDir::new(root).max_depth(1)
        };

        for entry in walker.into_iter().filter_map(Result::ok) {
            if !entry.file_type().is_file() {
                continue;
            }

            let path = entry.path();

            // Check if it's a video file
            if let Some(ext) = path.extension().and_then(|e| e.to_str())
                && self.config.extensions.contains(&ext.to_lowercase())
            {
                // Skip files already in Duplicates directory
                if !path.components().any(|c| c.as_os_str() == "Duplicates") {
                    files.push(FileInfo::new(path.to_path_buf()));
                }
            }
        }

        files
    }

    /// Find files with identical filenames in different directories
    #[allow(clippy::unused_self)]
    fn find_path_duplicates(&self, files: &[FileInfo]) {
        // Group files by filename
        let mut filename_groups: HashMap<String, Vec<&FileInfo>> = HashMap::new();

        for file in files {
            filename_groups
                .entry(file.filename.to_lowercase())
                .or_default()
                .push(file);
        }

        // Filter to filenames that appear in multiple paths
        let duplicates: Vec<_> = filename_groups
            .into_iter()
            .filter(|(_, files)| files.len() > 1)
            .sorted_by(|a, b| a.0.cmp(&b.0))
            .collect();

        if duplicates.is_empty() {
            println!("{}", "\nNo identical files in different paths found.".green());
            return;
        }

        println!(
            "\n{}",
            format!(
                "Found {} files with identical names in different paths:",
                duplicates.len()
            )
            .yellow()
        );

        for (filename, files) in &duplicates {
            println!("\n{}:", filename.cyan());
            for file in files.iter().sorted_by_key(|f| &f.path) {
                println!("  {}", file.path.display());
            }
        }
    }

    /// Find duplicates based on identifier patterns
    fn find_pattern_duplicates(&self, files: &[FileInfo]) -> anyhow::Result<()> {
        // Map identifier -> list of files
        let mut matches: HashMap<String, Vec<&FileInfo>> = HashMap::new();

        for file in files {
            for pattern in &self.config.patterns {
                if let Some(m) = pattern.find(&file.filename) {
                    let identifier = m.as_str().to_string();
                    matches.entry(identifier).or_default().push(file);
                    break; // Only match first pattern
                }
            }
        }

        // Filter to only identifiers with multiple files
        let duplicates: Vec<_> = matches
            .into_iter()
            .filter(|(_, paths)| paths.len() > 1)
            .sorted_by(|a, b| a.0.cmp(&b.0))
            .collect();

        if duplicates.is_empty() {
            println!("{}", "\nNo pattern duplicates found.".green());
            return Ok(());
        }

        println!(
            "\n{}",
            format!("Found {} identifiers with duplicates:", duplicates.len()).yellow()
        );

        for (identifier, paths) in &duplicates {
            println!("\n{}:", identifier.cyan());
            for file in paths.iter().sorted_by_key(|f| &f.path) {
                println!("  {}", file.path.display());
            }
        }

        if self.config.move_files {
            self.move_duplicates(&duplicates)?;
        }

        Ok(())
    }

    /// Find files with the same base name but different resolution, codec, or extension
    #[allow(clippy::unused_self)]
    fn find_variants(&self, files: &[FileInfo]) {
        // Create a normalized name (without resolution, codec, and extension) -> list of files
        let mut normalized_groups: HashMap<String, Vec<&FileInfo>> = HashMap::new();

        for file in files {
            let normalized = Self::normalize_filename(&file.filename);
            if !normalized.is_empty() {
                normalized_groups.entry(normalized).or_default().push(file);
            }
        }

        // Filter to groups with multiple files (different resolutions/codecs/extensions)
        let variants: Vec<_> = normalized_groups
            .into_iter()
            .filter(|(_, files)| files.len() > 1)
            .sorted_by(|a, b| a.0.cmp(&b.0))
            .collect();

        if variants.is_empty() {
            println!("{}", "\nNo resolution/codec/extension variants found.".green());
            return;
        }

        println!(
            "\n{}",
            format!(
                "Found {} files with resolution/codec/extension variants:",
                variants.len()
            )
            .yellow()
        );

        for (base_name, files) in &variants {
            println!("\n{}:", base_name.cyan());
            for file in files.iter().sorted_by_key(|f| &f.filename) {
                // Show resolution and codec info
                let resolution = Self::extract_resolution(&file.filename).unwrap_or_else(|| "?".to_string());
                let codec = Self::extract_codec(&file.filename).unwrap_or_else(|| "?".to_string());
                let ext = file.path.extension().and_then(|e| e.to_str()).unwrap_or("?");
                println!("  {} [{}] [{}] [.{}]", file.path.display(), resolution, codec, ext);
            }
        }
    }

    /// Normalize a filename by removing resolution, codec, and extension
    fn normalize_filename(filename: &str) -> String {
        // Remove extension
        let name = Path::new(filename)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(filename);

        let mut normalized = name.to_lowercase();

        // Remove common resolution patterns like 720p, 1080p, etc.
        for pattern in RESOLUTION_PATTERNS {
            // Remove with surrounding separators
            normalized = normalized.replace(&format!(".{}", pattern.to_lowercase()), "");
            normalized = normalized.replace(&format!("-{}", pattern.to_lowercase()), "");
            normalized = normalized.replace(&format!("_{}", pattern.to_lowercase()), "");
            normalized = normalized.replace(&pattern.to_lowercase(), "");
        }

        // Remove WxH resolution patterns
        normalized = RE_RESOLUTION.replace_all(&normalized, "").to_string();

        // Remove codec patterns
        for codec in CODEC_PATTERNS {
            normalized = normalized.replace(&format!(".{}", codec.to_lowercase()), "");
            normalized = normalized.replace(&format!("-{}", codec.to_lowercase()), "");
            normalized = normalized.replace(&format!("_{}", codec.to_lowercase()), "");
            normalized = normalized.replace(&codec.to_lowercase(), "");
        }

        // Also use regex to catch any remaining codec patterns
        normalized = RE_CODEC.replace_all(&normalized, "").to_string();

        // Clean up multiple dots/spaces/separators
        loop {
            let before = normalized.clone();
            normalized = normalized.replace("..", ".").replace("  ", " ");
            if before == normalized {
                break;
            }
        }

        normalized
            .trim_matches(|c| c == '.' || c == ' ' || c == '_' || c == '-')
            .to_string()
    }

    /// Extract resolution from a filename
    fn extract_resolution(filename: &str) -> Option<String> {
        // First check for common patterns
        let lower = filename.to_lowercase();
        for pattern in RESOLUTION_PATTERNS {
            if lower.contains(&pattern.to_lowercase()) {
                return Some((*pattern).to_string());
            }
        }

        // Then check for WxH format
        RE_RESOLUTION.find(filename).map(|m| {
            m.as_str()
                .trim_matches(|c| c == '.' || c == '-' || c == '_')
                .to_string()
        })
    }

    /// Extract codec from a filename
    fn extract_codec(filename: &str) -> Option<String> {
        let lower = filename.to_lowercase();
        for codec in CODEC_PATTERNS {
            if lower.contains(&codec.to_lowercase()) {
                return Some((*codec).to_string());
            }
        }
        None
    }

    /// Move duplicate files to a Duplicates directory
    fn move_duplicates(&self, duplicates: &[(String, Vec<&FileInfo>)]) -> anyhow::Result<()> {
        // Use the first root as the duplicates directory location
        let duplicates_dir = self.roots.first().map_or_else(
            || {
                std::env::current_dir()
                    .unwrap_or_else(|_| PathBuf::from("."))
                    .join("Duplicates")
            },
            |r| r.join("Duplicates"),
        );

        if self.config.dryrun {
            println!("\n{}", "Dry run - no files will be moved.".yellow());
        }

        for (identifier, files) in duplicates {
            for file in files {
                let target_dir = duplicates_dir.join(identifier);
                let target_path = get_unique_path(&target_dir, &file.filename);

                println!(
                    "\n{}: {} -> {}",
                    "Move".magenta(),
                    file.path.display(),
                    target_path.display()
                );

                if self.config.dryrun {
                    continue;
                }

                print!("{}", "Move file? (y/n): ".magenta());
                io::stdout().flush()?;

                let mut input = String::new();
                io::stdin().read_line(&mut input)?;

                if input.trim().eq_ignore_ascii_case("y") {
                    // Create directory if needed
                    if let Err(e) = fs::create_dir_all(&target_dir) {
                        print_warning!("Failed to create directory {}: {e}", target_dir.display());
                        continue;
                    }

                    match fs::rename(&file.path, &target_path) {
                        Ok(()) => println!("{}", "Moved".green()),
                        Err(e) => print_error!("Failed to move file: {e}"),
                    }
                } else {
                    println!("Skipped");
                }
            }
        }

        Ok(())
    }
}

/// Get a unique file path, adding a counter if the file already exists
fn get_unique_path(dir: &Path, filename: &str) -> PathBuf {
    let mut path = dir.join(filename);

    if !path.exists() {
        return path;
    }

    let stem = Path::new(filename)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(filename);
    let ext = Path::new(filename).extension().and_then(|s| s.to_str()).unwrap_or("");

    let mut counter = 1;
    while path.exists() {
        let new_name = if ext.is_empty() {
            format!("{stem}.{counter}")
        } else {
            format!("{stem}.{counter}.{ext}")
        };
        path = dir.join(new_name);
        counter += 1;
    }

    path
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    if let Some(ref shell) = args.completion {
        cli_tools::generate_shell_completion(*shell, Args::command(), true, env!("CARGO_BIN_NAME"))
    } else {
        DupeFind::new(&args)?.run()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_resolution_common_patterns() {
        assert_eq!(DupeFind::extract_resolution("video.720p.mp4"), Some("720p".to_string()));
        assert_eq!(
            DupeFind::extract_resolution("video.1080p.mp4"),
            Some("1080p".to_string())
        );
        assert_eq!(
            DupeFind::extract_resolution("video.1440p.mp4"),
            Some("1440p".to_string())
        );
        assert_eq!(
            DupeFind::extract_resolution("video.2160p.mp4"),
            Some("2160p".to_string())
        );
    }

    #[test]
    fn test_extract_resolution_wxh_format() {
        assert_eq!(
            DupeFind::extract_resolution("video.1920x1080.mp4"),
            Some("1920x1080".to_string())
        );
        assert_eq!(
            DupeFind::extract_resolution("video.1280x720.mp4"),
            Some("1280x720".to_string())
        );
        assert_eq!(
            DupeFind::extract_resolution("video.3840x2160.mp4"),
            Some("3840x2160".to_string())
        );
        assert_eq!(
            DupeFind::extract_resolution("video.640x480.mp4"),
            Some("640x480".to_string())
        );
    }

    #[test]
    fn test_extract_resolution_none() {
        assert_eq!(DupeFind::extract_resolution("video.mp4"), None);
        assert_eq!(DupeFind::extract_resolution("video.x265.mp4"), None);
    }

    #[test]
    fn test_extract_codec() {
        assert_eq!(DupeFind::extract_codec("video.x265.mp4"), Some("x265".to_string()));
        assert_eq!(DupeFind::extract_codec("video.x264.mp4"), Some("x264".to_string()));
        assert_eq!(DupeFind::extract_codec("video.h265.mp4"), Some("h265".to_string()));
        assert_eq!(DupeFind::extract_codec("video.h264.mp4"), Some("h264".to_string()));
        assert_eq!(DupeFind::extract_codec("video.mp4"), None);
    }

    #[test]
    fn test_normalize_filename_removes_resolution() {
        assert_eq!(DupeFind::normalize_filename("video.1080p.mp4"), "video");
        assert_eq!(DupeFind::normalize_filename("video.720p.mkv"), "video");
        assert_eq!(DupeFind::normalize_filename("video.1920x1080.mp4"), "video");
    }

    #[test]
    fn test_normalize_filename_removes_codec() {
        assert_eq!(DupeFind::normalize_filename("video.x265.mp4"), "video");
        assert_eq!(DupeFind::normalize_filename("video.x264.mkv"), "video");
        assert_eq!(DupeFind::normalize_filename("video.h265.mp4"), "video");
    }

    #[test]
    fn test_normalize_filename_removes_both() {
        assert_eq!(DupeFind::normalize_filename("video.1080p.x265.mp4"), "video");
        assert_eq!(DupeFind::normalize_filename("video.720p.x264.mkv"), "video");
        assert_eq!(
            DupeFind::normalize_filename("Movie.Title.2024.1080p.x265.mp4"),
            "movie.title.2024"
        );
    }

    #[test]
    fn test_normalize_filename_same_base() {
        // These should all normalize to the same base name
        let name1 = DupeFind::normalize_filename("Movie.Title.1080p.mp4");
        let name2 = DupeFind::normalize_filename("Movie.Title.720p.mp4");
        let name3 = DupeFind::normalize_filename("Movie.Title.1920x1080.mkv");
        let name4 = DupeFind::normalize_filename("Movie.Title.1080p.x265.mp4");
        let name5 = DupeFind::normalize_filename("Movie.Title.720p.x264.mkv");

        assert_eq!(name1, name2);
        assert_eq!(name2, name3);
        assert_eq!(name3, name4);
        assert_eq!(name4, name5);
    }

    #[test]
    fn test_normalize_filename_preserves_content() {
        let normalized = DupeFind::normalize_filename("Some.Movie.2024.1080p.x265.mp4");
        assert!(normalized.contains("some"));
        assert!(normalized.contains("movie"));
        assert!(normalized.contains("2024"));
        assert!(!normalized.contains("1080p"));
        assert!(!normalized.contains("x265"));
    }

    #[test]
    fn test_get_unique_path_no_conflict() {
        let dir = PathBuf::from("/nonexistent/path");
        let result = get_unique_path(&dir, "video.mp4");
        assert_eq!(result, dir.join("video.mp4"));
    }
}

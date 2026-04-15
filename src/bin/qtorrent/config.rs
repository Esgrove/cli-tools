//! Configuration module for qtorrent.
//!
//! Handles reading configuration from CLI arguments and the user config file.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;
use walkdir::WalkDir;

use crate::torrent::FileFilter;

use crate::QtorrentArgs;

/// Default qBittorrent `WebUI` host.
const DEFAULT_HOST: &str = "localhost";

/// Default qBittorrent `WebUI` port.
const DEFAULT_PORT: u16 = 8080;

/// User configuration from the config file.
#[derive(Debug, Default, Deserialize)]
pub struct QtorrentConfig {
    /// qBittorrent `WebUI` host.
    #[serde(default)]
    host: Option<String>,
    /// qBittorrent `WebUI` port.
    #[serde(default)]
    port: Option<u16>,
    /// qBittorrent `WebUI` username.
    #[serde(default)]
    username: Option<String>,
    /// qBittorrent `WebUI` password.
    #[serde(default)]
    password: Option<String>,
    /// Default save path for torrents.
    #[serde(default)]
    save_path: Option<String>,
    /// Default category for torrents.
    #[serde(default)]
    category: Option<String>,
    /// Default tags for torrents.
    #[serde(default)]
    tags: Option<String>,
    /// Add torrents in paused state by default.
    #[serde(default)]
    paused: bool,
    /// Enable verbose output by default.
    #[serde(default)]
    verbose: bool,
    /// Enable dry-run mode by default.
    #[serde(default)]
    dryrun: bool,
    /// Enable offline mode by default (implies dryrun).
    #[serde(default)]
    offline: bool,
    /// Skip confirmation prompts by default.
    #[serde(default)]
    yes: bool,
    /// Skip rename prompts for existing/duplicate torrents.
    #[serde(default)]
    skip_existing: bool,
    /// Recurse into subdirectories when searching for torrent files.
    #[serde(default)]
    recurse: bool,
    /// File extensions to skip (without dot, e.g., "nfo", "txt", "jpg").
    #[serde(default)]
    skip_extensions: Vec<String>,
    /// Directory names to skip (case-insensitive full name match).
    #[serde(default)]
    skip_directories: Vec<String>,
    /// Minimum file size in MB. Files smaller than this will be skipped.
    #[serde(default)]
    min_file_size_mb: Option<f64>,
    /// Include image files (.jpg, .jpeg, .png) in multi-file torrents.
    #[serde(default)]
    include_images: bool,
    /// Minimum image file size in KB. Files smaller than this will be skipped.
    #[serde(default)]
    min_image_size_kb: Option<u64>,
    /// Substrings to remove from torrent filename when generating suggested name.
    #[serde(default)]
    remove_from_name: Vec<String>,
    /// Apply dots formatting to suggested name (uses dots config from config file).
    #[serde(default)]
    use_dots_formatting: bool,
    /// If torrent filename contains any of these strings, ignore it and use internal name instead.
    #[serde(default)]
    ignore_torrent_names: Vec<String>,
    /// Prefix-to-tag pairs for overwriting tags based on torrent filename.
    /// Each entry is `[prefix, tag]`.
    /// If a torrent filename starts with the prefix (case-insensitive),
    /// the corresponding tag is used instead of the default `tags` value.
    /// Longer prefixes are checked first to allow more specific matches.
    #[serde(default, deserialize_with = "deserialize_tag_overwrite_prefixes")]
    tag_overwrite_prefixes: Vec<TagOverwrite>,
}

/// A tag overwrite rule:
/// if a torrent filename starts with the prefix,
/// use the associated tag value.
#[derive(Debug, Clone)]
pub struct TagOverwrite {
    /// Tag value to use when the prefix matches.
    pub tag: String,
    /// Lowercase prefix for case-insensitive matching.
    lowercase_prefix: String,
}

/// Final config combined from CLI arguments and user config file.
#[derive(Debug)]
pub struct Config {
    /// qBittorrent `WebUI` host.
    pub host: String,
    /// qBittorrent `WebUI` port.
    pub port: u16,
    /// qBittorrent `WebUI` username.
    pub username: String,
    /// qBittorrent `WebUI` password.
    pub password: String,
    /// Save path for torrents.
    pub save_path: Option<String>,
    /// Category for torrents.
    pub category: Option<String>,
    /// Tags for torrents.
    pub tags: Option<String>,
    /// Add torrent in paused state.
    pub paused: bool,
    /// Verbose output.
    pub verbose: bool,
    /// Dry-run mode (don't actually add torrents).
    pub dryrun: bool,
    /// Offline mode (skip qBittorrent connection, implies dryrun).
    pub offline: bool,
    /// Skip confirmation prompts.
    pub yes: bool,
    /// Skip rename prompts for existing/duplicate torrents.
    pub skip_existing: bool,
    /// Recurse into subdirectories when searching for torrent files.
    pub recurse: bool,
    /// Input paths from command line arguments.
    pub input_paths: Vec<PathBuf>,
    /// File filter configuration for skipping files by extension, directory, or size.
    pub file_filter: FileFilter,
    /// Substrings to remove from torrent filename when generating suggested name.
    pub remove_from_name: Vec<String>,
    /// Apply dots formatting to suggested name (uses dots config from config file).
    pub use_dots_formatting: bool,
    /// If torrent filename contains any of these strings, ignore it and use internal name instead.
    pub ignore_torrent_names: Vec<String>,
    /// Prefixes to match against torrent filenames for tag overwriting.
    /// If a torrent filename starts with one of these prefixes (case-insensitive),
    /// use the prefix as the tag string instead of the default `tags` value.
    pub tag_overwrite_prefixes: Vec<TagOverwrite>,
}

/// Wrapper needed for parsing the config file section.
#[derive(Debug, Default, Deserialize)]
struct UserConfig {
    #[serde(default)]
    qtorrent: QtorrentConfig,
}

impl QtorrentConfig {
    /// Try to read user config from the file if it exists.
    /// Otherwise, fall back to default config.
    ///
    /// # Errors
    /// Returns an error if config file exists but cannot be read or parsed.
    pub fn get_user_config() -> Result<Self> {
        let Some(path) = cli_tools::config_path() else {
            return Ok(Self::default());
        };

        match fs::read_to_string(path) {
            Ok(content) => Self::from_toml_str(&content)
                .map_err(|e| anyhow!("Failed to parse config file {}:\n{e}", path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(anyhow!("Failed to read config file {}: {error}", path.display())),
        }
    }

    /// Parse configuration from a TOML string.
    ///
    /// # Errors
    /// Returns an error if the TOML string is invalid.
    pub fn from_toml_str(toml_str: &str) -> Result<Self> {
        toml::from_str::<UserConfig>(toml_str)
            .map(|config| config.qtorrent)
            .map_err(|e| anyhow!("Failed to parse config: {e}"))
    }
}

impl Config {
    /// Create config from given command line args and user config file.
    ///
    /// # Errors
    /// Returns an error if the config file cannot be read or parsed.
    #[allow(clippy::too_many_lines)]
    pub fn from_args(args: QtorrentArgs) -> Result<Self> {
        let user_config = QtorrentConfig::get_user_config()?;
        Ok(Self::from_args_with_user_config(args, user_config))
    }

    /// Create config by merging CLI arguments with a pre-loaded user config.
    #[allow(clippy::too_many_lines)]
    fn from_args_with_user_config(args: QtorrentArgs, user_config: QtorrentConfig) -> Self {
        // Get credentials from args or config, with args taking priority
        let host = args
            .host
            .or(user_config.host)
            .unwrap_or_else(|| DEFAULT_HOST.to_string());

        let port = args.port.or(user_config.port).unwrap_or(DEFAULT_PORT);

        let username = args.username.or(user_config.username).unwrap_or_default();

        let password = args.password.or(user_config.password).unwrap_or_default();

        // Get other options from args or config
        let save_path = args.save_path.or(user_config.save_path);
        let category = args.category.or(user_config.category);
        let tags = args.tags.or(user_config.tags);
        let paused = args.paused || user_config.paused;
        let verbose = args.verbose || user_config.verbose;
        let offline = args.offline || user_config.offline;
        // Offline implies dryrun
        let dryrun = args.dryrun || user_config.dryrun || offline;
        let yes = args.yes || user_config.yes;
        let skip_existing = args.skip_existing || user_config.skip_existing;
        let recurse = args.recurse || user_config.recurse;

        // Resolve input paths
        let input_paths = args.path;

        // File filtering options - merge CLI args with config, CLI takes priority
        let skip_extensions: Vec<String> = if args.skip_extensions.is_empty() {
            user_config
                .skip_extensions
                .into_iter()
                .map(|extension| extension.to_lowercase().trim_start_matches('.').to_string())
                .collect()
        } else {
            args.skip_extensions
                .into_iter()
                .map(|extension| extension.to_lowercase().trim_start_matches('.').to_string())
                .collect()
        };

        let skip_directories: Vec<String> = if args.skip_directories.is_empty() {
            user_config
                .skip_directories
                .into_iter()
                .map(|name| name.to_lowercase())
                .collect()
        } else {
            args.skip_directories
                .into_iter()
                .map(|name| name.to_lowercase())
                .collect()
        };

        let include_images = args.include_images || user_config.include_images;

        // Convert MB to bytes for easier comparison
        let min_file_size_bytes = args
            .min_file_size_mb
            .or(user_config.min_file_size_mb)
            .and_then(mb_to_bytes);

        let min_image_size_kb = args
            .min_image_size_kb
            .or_else(|| user_config.min_image_size_kb.map(|kb| kb as f64));

        let min_image_size_bytes = min_image_size_kb.and_then(kb_to_bytes);

        let file_filter = FileFilter::new(
            skip_extensions,
            skip_directories,
            min_file_size_bytes,
            include_images,
            min_image_size_bytes,
        );

        // Substrings to remove from suggested name
        let remove_from_name = user_config.remove_from_name;

        // Dots formatting option
        let use_dots_formatting = user_config.use_dots_formatting;

        // Filename ignore patterns
        let ignore_torrent_names = user_config.ignore_torrent_names;

        let tag_overwrite_prefixes = user_config.tag_overwrite_prefixes;

        Self {
            host,
            port,
            username,
            password,
            save_path,
            category,
            tags,
            paused,
            verbose,
            dryrun,
            offline,
            yes,
            skip_existing,
            recurse,
            input_paths,
            file_filter,
            remove_from_name,
            use_dots_formatting,
            ignore_torrent_names,
            tag_overwrite_prefixes,
        }
    }

    /// Check if credentials are provided.
    #[must_use]
    pub const fn has_credentials(&self) -> bool {
        !self.username.is_empty() && !self.password.is_empty()
    }

    /// Resolve tags for a given torrent file path.
    ///
    /// If the torrent filename (without extension) starts with one of the configured
    /// `tag_overwrite_prefixes` (case-insensitive), returns the associated tag value.
    /// Otherwise, returns the default `tags` value from config.
    #[must_use]
    pub fn resolve_tags(&self, torrent_path: &std::path::Path) -> Option<String> {
        if !self.tag_overwrite_prefixes.is_empty() {
            let filename = torrent_path
                .file_stem()
                .map(|stem| stem.to_string_lossy().to_lowercase())
                .unwrap_or_default();

            for entry in &self.tag_overwrite_prefixes {
                if filename.starts_with(&entry.lowercase_prefix) {
                    return Some(entry.tag.clone());
                }
            }
        }

        self.tags.clone()
    }

    /// Collect torrent file paths from the configured input paths.
    ///
    /// If no paths are provided, the current working directory is used.
    /// If a path is a directory, it is searched for `.torrent` files.
    /// If a path is a `.torrent` file, it is used directly.
    /// If `recurse` is true, directories are searched recursively.
    ///
    /// # Errors
    /// Returns an error if paths cannot be resolved or directories cannot be read.
    pub fn collect_torrent_paths(&self) -> Result<Vec<PathBuf>> {
        let mut torrent_paths = Vec::new();

        if self.input_paths.is_empty() {
            // No paths provided, use current working directory
            let current_directory = cli_tools::resolve_input_path(None)?;
            Self::collect_torrents_from_directory(&current_directory, &mut torrent_paths, self.recurse)?;
        } else {
            for path in &self.input_paths {
                let resolved = cli_tools::resolve_required_input_path(path)?;
                if resolved.is_dir() {
                    Self::collect_torrents_from_directory(&resolved, &mut torrent_paths, self.recurse)?;
                } else if Self::is_torrent_file(&resolved) {
                    torrent_paths.push(resolved);
                }
            }
        }

        torrent_paths.sort_unstable();
        torrent_paths.dedup();

        Ok(torrent_paths)
    }

    /// Collect all `.torrent` files from the given directory.
    ///
    /// If `recurse` is true, subdirectories are searched recursively.
    fn collect_torrents_from_directory(
        directory: &Path,
        torrent_paths: &mut Vec<PathBuf>,
        recurse: bool,
    ) -> Result<()> {
        if recurse {
            for entry in WalkDir::new(directory)
                .follow_links(false)
                .into_iter()
                .filter_map(Result::ok)
            {
                let path = entry.path();
                if path.is_file() && Self::is_torrent_file(path) {
                    torrent_paths.push(path.to_path_buf());
                }
            }
        } else {
            for entry in
                fs::read_dir(directory).with_context(|| format!("Failed to read directory: {}", directory.display()))?
            {
                let entry = entry?;
                let path = entry.path();
                if path.is_file() && Self::is_torrent_file(&path) {
                    torrent_paths.push(path);
                }
            }
        }
        Ok(())
    }

    /// Check if the given path is a `.torrent` file.
    fn is_torrent_file(path: &Path) -> bool {
        path.extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("torrent"))
    }
}

fn mb_to_bytes(mb: f64) -> Option<u64> {
    (mb > 0.0).then_some((mb * 1024.0 * 1024.0) as u64)
}

fn kb_to_bytes(kb: f64) -> Option<u64> {
    (kb > 0.0).then_some((kb * 1024.0) as u64)
}

/// Deserialize `[[prefix, tag], ...]` pairs into sorted `TagOverwrite` rules.
///
/// Lowercases prefixes for case-insensitive matching and sorts by prefix length
/// descending for longest-match-first semantics.
fn deserialize_tag_overwrite_prefixes<'de, D>(deserializer: D) -> std::result::Result<Vec<TagOverwrite>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let pairs: Vec<[String; 2]> = Vec::deserialize(deserializer)?;
    let mut prefixes: Vec<TagOverwrite> = pairs
        .into_iter()
        .map(|[prefix, tag]| TagOverwrite {
            lowercase_prefix: prefix.to_lowercase(),
            tag,
        })
        .collect();

    prefixes.sort_unstable_by_key(|entry| std::cmp::Reverse(entry.lowercase_prefix.chars().count()));
    Ok(prefixes)
}

#[cfg(test)]
mod qtorrent_config_tests {
    use std::borrow::Cow;

    use clap::Parser;

    use crate::torrent::FileInfo;

    use super::*;

    fn make_image_file_info(path: &str, size: u64) -> FileInfo<'static> {
        FileInfo {
            index: 0,
            path: Cow::Owned(path.to_string()),
            size,
            exclusion_reason: None,
        }
    }

    fn make_config_with_image_size_limit(min_image_size_kb: u64) -> Config {
        let user_config = QtorrentConfig::from_toml_str(&format!(
            "\n[qtorrent]\ninclude_images = true\nmin_image_size_kb = {min_image_size_kb}\n"
        ))
        .expect("should parse config");
        let args = crate::QtorrentArgs::try_parse_from(["test"]).expect("should parse args");
        Config::from_args_with_user_config(args, user_config)
    }

    fn make_config_with_filters(
        skip_extensions: &[&str],
        min_file_size_mb: Option<f64>,
        include_images: bool,
        min_image_size_kb: Option<u64>,
    ) -> Config {
        let skip_ext_toml: Vec<String> = skip_extensions.iter().map(|ext| format!("\"{ext}\" ")).collect();
        let toml = format!(
            "[qtorrent]\nskip_extensions = [{}]\ninclude_images = {}\n{}{}\n",
            skip_ext_toml.join(", "),
            include_images,
            min_file_size_mb.map_or(String::new(), |mb| format!("min_file_size_mb = {mb}\n")),
            min_image_size_kb.map_or(String::new(), |kb| format!("min_image_size_kb = {kb}\n")),
        );
        let user_config = QtorrentConfig::from_toml_str(&toml).expect("should parse config");
        let args = crate::QtorrentArgs::try_parse_from(["test"]).expect("should parse args");
        Config::from_args_with_user_config(args, user_config)
    }

    #[test]
    fn from_toml_str_parses_empty_config() {
        let toml = "";
        let config = QtorrentConfig::from_toml_str(toml).expect("should parse empty config");
        assert!(config.host.is_none());
        assert!(config.port.is_none());
        assert!(config.username.is_none());
        assert!(config.password.is_none());
        assert!(!config.paused);
        assert!(!config.verbose);
        assert!(!config.dryrun);
        assert!(!config.offline);
    }

    #[test]
    fn from_toml_str_parses_qtorrent_section() {
        let toml = r#"
[qtorrent]
host = "192.168.1.100"
port = 9090
username = "admin"
password = "secret"
paused = true
verbose = true
dryrun = true
yes = true
"#;
        let config = QtorrentConfig::from_toml_str(toml).expect("should parse config");
        assert_eq!(config.host, Some("192.168.1.100".to_string()));
        assert_eq!(config.port, Some(9090));
        assert_eq!(config.username, Some("admin".to_string()));
        assert_eq!(config.password, Some("secret".to_string()));
        assert!(config.paused);
        assert!(config.verbose);
        assert!(config.dryrun);
        assert!(config.yes);
    }

    #[test]
    fn from_toml_str_parses_save_path_and_category() {
        let toml = r#"
[qtorrent]
save_path = "/downloads/torrents"
category = "movies"
tags = "hd,new"
"#;
        let config = QtorrentConfig::from_toml_str(toml).expect("should parse config");
        assert_eq!(config.save_path, Some("/downloads/torrents".to_string()));
        assert_eq!(config.category, Some("movies".to_string()));
        assert_eq!(config.tags, Some("hd,new".to_string()));
    }

    #[test]
    fn from_toml_str_parses_file_filtering_options() {
        let toml = r#"
[qtorrent]
skip_extensions = ["nfo", "txt", "jpg"]
skip_directories = ["sample", "subs"]
min_file_size_mb = 50.0
include_images = true
min_image_size_kb = 500
"#;
        let config = QtorrentConfig::from_toml_str(toml).expect("should parse config");
        assert_eq!(config.skip_extensions, vec!["nfo", "txt", "jpg"]);
        assert_eq!(config.skip_directories, vec!["sample", "subs"]);
        assert_eq!(config.min_file_size_mb, Some(50.0));
        assert!(config.include_images);
        assert_eq!(config.min_image_size_kb, Some(500));
    }

    #[test]
    fn from_toml_str_parses_integer_file_filtering_options() {
        let toml = r"
[qtorrent]
include_images = true
min_image_size_kb = 500
    ";
        let config = QtorrentConfig::from_toml_str(toml).expect("should parse config");
        assert!(config.include_images);
        assert_eq!(config.min_image_size_kb, Some(500));
    }

    #[test]
    fn excludes_undersized_images_with_lowercase_extensions() {
        let config = make_config_with_image_size_limit(500);
        let files = [
            make_image_file_info("movie/snapshot.jpg", 226 * 1024),
            make_image_file_info("movie/snapshot.jpeg", 159 * 1024),
            make_image_file_info("movie/snapshot.png", 200 * 1024),
        ];

        assert_eq!(config.file_filter.min_image_size_bytes, Some(500 * 1024));
        let excluded_count = files
            .iter()
            .filter(|f| config.file_filter.should_exclude(f).is_some())
            .count();
        assert_eq!(excluded_count, 3);
    }

    #[test]
    fn excludes_undersized_images_with_uppercase_extensions() {
        let config = make_config_with_image_size_limit(500);
        let files = [
            make_image_file_info("movie/snapshot.JPG", 100 * 1024),
            make_image_file_info("movie/snapshot.JPEG", 200 * 1024),
            make_image_file_info("movie/snapshot.PNG", 300 * 1024),
        ];

        let excluded_count = files
            .iter()
            .filter(|f| config.file_filter.should_exclude(f).is_some())
            .count();
        assert_eq!(excluded_count, 3);
    }

    #[test]
    fn excludes_undersized_images_with_titlecase_extensions() {
        let config = make_config_with_image_size_limit(500);
        let files = [
            make_image_file_info("movie/snapshot.Jpg", 100 * 1024),
            make_image_file_info("movie/snapshot.Jpeg", 200 * 1024),
            make_image_file_info("movie/snapshot.Png", 300 * 1024),
        ];

        let excluded_count = files
            .iter()
            .filter(|f| config.file_filter.should_exclude(f).is_some())
            .count();
        assert_eq!(excluded_count, 3);
    }

    #[test]
    fn keeps_images_at_or_above_size_limit() {
        let config = make_config_with_image_size_limit(500);

        let exact_limit = make_image_file_info("movie/exact.jpg", 500 * 1024);
        let above_limit = make_image_file_info("movie/large.JPEG", 600 * 1024);
        let well_above = make_image_file_info("movie/poster.Png", 2000 * 1024);

        assert!(config.file_filter.should_exclude(&exact_limit).is_none());
        assert!(config.file_filter.should_exclude(&above_limit).is_none());
        assert!(config.file_filter.should_exclude(&well_above).is_none());
    }

    #[test]
    fn image_size_limit_does_not_apply_to_non_image_files() {
        let config = make_config_with_image_size_limit(500);
        let files = [
            make_image_file_info("movie/video.mp4", 10 * 1024),
            make_image_file_info("movie/readme.txt", 2 * 1024),
            make_image_file_info("movie/info.nfo", 1024),
            make_image_file_info("movie/subs.srt", 50 * 1024),
        ];

        let included_count = files
            .iter()
            .filter(|f| config.file_filter.should_exclude(f).is_none())
            .count();
        assert_eq!(included_count, 4);
    }

    #[test]
    fn mixed_files_with_image_size_and_skip_extensions() {
        let config = make_config_with_filters(&["nfo", "txt"], None, true, Some(500));

        let files = [
            // Included: large image above limit
            make_image_file_info("movie/poster.jpg", 600 * 1024),
            // Included: video file (no size limit configured)
            make_image_file_info("movie/video.mkv", 700 * 1024 * 1024),
            // Excluded: small image below limit
            make_image_file_info("movie/thumb.PNG", 100 * 1024),
            // Excluded: skipped extension
            make_image_file_info("movie/info.nfo", 512),
            // Excluded: skipped extension
            make_image_file_info("movie/readme.txt", 2 * 1024),
            // Included: non-filtered extension
            make_image_file_info("movie/subs.srt", 50 * 1024),
            // Excluded: small image below limit
            make_image_file_info("movie/screen.Jpeg", 200 * 1024),
        ];

        let included_count = files
            .iter()
            .filter(|f| config.file_filter.should_exclude(f).is_none())
            .count();
        let excluded_count = files
            .iter()
            .filter(|f| config.file_filter.should_exclude(f).is_some())
            .count();

        assert_eq!(included_count, 3);
        assert_eq!(excluded_count, 4);
    }

    #[test]
    fn skip_extensions_exclude_regardless_of_case() {
        let config = make_config_with_filters(&["nfo", "txt"], None, true, None);

        let files = [
            make_image_file_info("movie/info.NFO", 512),
            make_image_file_info("movie/readme.Txt", 1024),
            make_image_file_info("movie/notes.txt", 2048),
        ];

        let excluded_count = files
            .iter()
            .filter(|f| config.file_filter.should_exclude(f).is_some())
            .count();
        assert_eq!(excluded_count, 3);
    }

    #[test]
    fn images_excluded_entirely_when_include_images_disabled() {
        let config = make_config_with_filters(&[], None, false, None);

        let files = [
            make_image_file_info("movie/poster.jpg", 5 * 1024 * 1024),
            make_image_file_info("movie/cover.PNG", 2 * 1024 * 1024),
            make_image_file_info("movie/video.mkv", 700 * 1024 * 1024),
        ];

        let excluded_count = files
            .iter()
            .filter(|f| config.file_filter.should_exclude(f).is_some())
            .count();
        let included_count = files
            .iter()
            .filter(|f| config.file_filter.should_exclude(f).is_none())
            .count();

        assert_eq!(excluded_count, 2);
        assert_eq!(included_count, 1);
    }

    #[test]
    fn from_toml_str_parses_name_processing_options() {
        let toml = r#"
[qtorrent]
remove_from_name = ["-RELEASE", ".WEB."]
use_dots_formatting = true
ignore_torrent_names = ["unknown", "noname"]
"#;
        let config = QtorrentConfig::from_toml_str(toml).expect("should parse config");
        assert_eq!(config.remove_from_name, vec!["-RELEASE", ".WEB."]);
        assert!(config.use_dots_formatting);
        assert_eq!(config.ignore_torrent_names, vec!["unknown", "noname"]);
    }

    #[test]
    fn from_toml_str_parses_tag_overwrite_prefixes() {
        let toml = r#"
[qtorrent]
tag_overwrite_prefixes = [["LongPrefix", "longtag"], ["Short", "shorttag"]]
"#;
        let config = QtorrentConfig::from_toml_str(toml).expect("should parse config");
        assert_eq!(config.tag_overwrite_prefixes.len(), 2);
        // Sorted by prefix length descending
        assert_eq!(config.tag_overwrite_prefixes[0].lowercase_prefix, "longprefix");
        assert_eq!(config.tag_overwrite_prefixes[0].tag, "longtag");
        assert_eq!(config.tag_overwrite_prefixes[1].lowercase_prefix, "short");
        assert_eq!(config.tag_overwrite_prefixes[1].tag, "shorttag");
    }

    #[test]
    fn from_toml_str_parses_empty_tag_overwrite_prefixes() {
        let toml = r"
[qtorrent]
tag_overwrite_prefixes = []
";
        let config = QtorrentConfig::from_toml_str(toml).expect("should parse config");
        assert!(config.tag_overwrite_prefixes.is_empty());
    }

    #[test]
    fn from_toml_str_defaults_tag_overwrite_prefixes_to_empty() {
        let toml = r"
[qtorrent]
verbose = true
";
        let config = QtorrentConfig::from_toml_str(toml).expect("should parse config");
        assert!(config.tag_overwrite_prefixes.is_empty());
    }

    #[test]
    fn from_toml_str_parses_recurse_and_skip_existing() {
        let toml = r"
[qtorrent]
recurse = true
skip_existing = true
";
        let config = QtorrentConfig::from_toml_str(toml).expect("should parse config");
        assert!(config.recurse);
        assert!(config.skip_existing);
    }

    #[test]
    fn from_toml_str_parses_offline() {
        let toml = r"
[qtorrent]
offline = true
";
        let config = QtorrentConfig::from_toml_str(toml).expect("should parse config");
        assert!(config.offline);
    }

    #[test]
    fn from_toml_str_invalid_toml_returns_error() {
        let toml = "this is not valid toml {{{";
        let result = QtorrentConfig::from_toml_str(toml);
        assert!(result.is_err());
    }

    #[test]
    fn from_toml_str_parses_tags_and_tag_overwrite_prefixes_together() {
        let toml = r#"
[qtorrent]
tags = "default-tag"
tag_overwrite_prefixes = [["SpecialPrefix", "special"], ["Other", "othertag"]]
"#;
        let config = QtorrentConfig::from_toml_str(toml).expect("should parse config");
        assert_eq!(config.tags, Some("default-tag".to_string()));
        assert_eq!(config.tag_overwrite_prefixes.len(), 2);
        assert_eq!(config.tag_overwrite_prefixes[0].lowercase_prefix, "specialprefix");
        assert_eq!(config.tag_overwrite_prefixes[0].tag, "special");
    }

    #[test]
    fn from_toml_str_ignores_other_sections() {
        let toml = r"
[other_section]
some_value = true

[qtorrent]
verbose = true
";
        let config = QtorrentConfig::from_toml_str(toml).expect("should parse config");
        assert!(config.verbose);
        assert!(!config.dryrun);
    }
}

#[cfg(test)]
mod test_resolve_tags {
    use std::path::Path;

    use super::*;

    /// Helper to create a minimal `Config` with given tags and prefix-tag pairs.
    fn make_config(tags: Option<&str>, prefixes: Vec<(&str, &str)>) -> Config {
        let mut tag_overwrite_prefixes: Vec<TagOverwrite> = prefixes
            .into_iter()
            .map(|(prefix, tag)| TagOverwrite {
                lowercase_prefix: prefix.to_lowercase(),
                tag: tag.to_string(),
            })
            .collect();
        tag_overwrite_prefixes.sort_by_key(|entry| std::cmp::Reverse(entry.lowercase_prefix.len()));

        Config {
            host: String::new(),
            port: 8080,
            username: String::new(),
            password: String::new(),
            save_path: None,
            category: None,
            tags: tags.map(String::from),
            paused: false,
            verbose: false,
            dryrun: false,
            offline: false,
            yes: false,
            skip_existing: false,
            recurse: false,
            input_paths: Vec::new(),
            file_filter: FileFilter::default(),
            remove_from_name: Vec::new(),
            use_dots_formatting: false,
            ignore_torrent_names: Vec::new(),
            tag_overwrite_prefixes,
        }
    }

    #[test]
    fn returns_default_tags_when_no_prefixes_configured() {
        let config = make_config(Some("default-tag"), vec![]);
        let result = config.resolve_tags(Path::new("something.torrent"));
        assert_eq!(result, Some("default-tag".to_string()));
    }

    #[test]
    fn returns_none_when_no_tags_and_no_prefix_match() {
        let config = make_config(None, vec![("prefix", "mytag")]);
        let result = config.resolve_tags(Path::new("other.torrent"));
        assert_eq!(result, None);
    }

    #[test]
    fn returns_default_tags_when_no_prefix_matches() {
        let config = make_config(Some("default-tag"), vec![("prefix", "mytag")]);
        let result = config.resolve_tags(Path::new("other.torrent"));
        assert_eq!(result, Some("default-tag".to_string()));
    }

    #[test]
    fn returns_associated_tag_on_prefix_match() {
        let config = make_config(Some("default-tag"), vec![("MyPrefix", "custom-tag")]);
        let result = config.resolve_tags(Path::new("MyPrefix.Something.2024.torrent"));
        assert_eq!(result, Some("custom-tag".to_string()));
    }

    #[test]
    fn matches_case_insensitively() {
        let config = make_config(Some("default-tag"), vec![("MyPrefix", "custom-tag")]);
        let result = config.resolve_tags(Path::new("myprefix.something.torrent"));
        assert_eq!(result, Some("custom-tag".to_string()));
    }

    #[test]
    fn longer_prefix_matches_first() {
        let config = make_config(
            Some("default-tag"),
            vec![("name", "short-tag"), ("nameprefix", "long-tag")],
        );
        let result = config.resolve_tags(Path::new("nameprefix.something.torrent"));
        assert_eq!(result, Some("long-tag".to_string()));
    }

    #[test]
    fn shorter_prefix_matches_when_longer_does_not() {
        let config = make_config(
            Some("default-tag"),
            vec![("name", "short-tag"), ("nameprefix", "long-tag")],
        );
        let result = config.resolve_tags(Path::new("name.other.torrent"));
        assert_eq!(result, Some("short-tag".to_string()));
    }

    #[test]
    fn uses_file_stem_without_torrent_extension() {
        let config = make_config(Some("default-tag"), vec![("test", "testtag")]);
        let result = config.resolve_tags(Path::new("/some/path/test.file.torrent"));
        assert_eq!(result, Some("testtag".to_string()));
    }

    #[test]
    fn handles_path_with_directories() {
        let config = make_config(Some("default-tag"), vec![("MyPrefix", "custom-tag")]);
        let result = config.resolve_tags(Path::new("/downloads/torrents/MyPrefix.Show.torrent"));
        assert_eq!(result, Some("custom-tag".to_string()));
    }

    #[test]
    fn returns_none_when_no_tags_and_no_prefixes() {
        let config = make_config(None, vec![]);
        let result = config.resolve_tags(Path::new("something.torrent"));
        assert_eq!(result, None);
    }

    #[test]
    fn prefixes_sorted_by_length_descending_in_config() {
        let config = make_config(None, vec![("a", "tag-a"), ("ccc", "tag-c"), ("bb", "tag-b")]);
        let prefixes: Vec<&str> = config
            .tag_overwrite_prefixes
            .iter()
            .map(|entry| entry.lowercase_prefix.as_str())
            .collect();
        assert_eq!(prefixes, vec!["ccc", "bb", "a"]);
    }
}

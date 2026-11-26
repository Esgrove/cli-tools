//! Configuration for the qtorrent binary.
//!
//! Reads the `[qtorrent]` section of the user config file
//! and merges it with the command line arguments into the library option structs.

use anyhow::Result;
use serde::Deserialize;

use cli_tools_qtorrent::{AddOptions, ConnectionSettings, FileFilter, NamingOptions, TagRules, TorrentPathCollector};

use crate::QtorrentArgs;

/// Default minimum number of image files required before including images.
const DEFAULT_MIN_IMAGE_FILE_COUNT: usize = 10;

/// User configuration from the `[qtorrent]` section of the config file.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
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
    /// Automatically apply suggested renames when `--yes` is set.
    #[serde(default)]
    auto_rename: bool,
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
    /// Minimum number of image files required before images are included.
    /// If fewer images are present, images are skipped as if `include_images` were false.
    #[serde(default)]
    min_image_file_count: Option<usize>,
    /// Minimum image file size in KB. Files smaller than this will be skipped.
    #[serde(default)]
    min_image_size_kb: Option<f64>,
    /// Substrings to remove from torrent filename when generating suggested name.
    #[serde(default)]
    remove_from_name: Vec<String>,
    /// Apply dots formatting to suggested name (uses dots config from config file).
    #[serde(default)]
    use_dots_formatting: bool,
    /// Apply dots formatting to custom names entered by the user in rename prompts.
    #[serde(default)]
    format_custom_name: bool,
    /// If torrent filename contains any of these strings, ignore it and use internal name instead.
    #[serde(default)]
    ignore_torrent_names: Vec<String>,
    /// Prefix-to-tag pairs for overwriting tags based on torrent filename.
    /// Each entry is `[prefix, tag]`.
    /// If a torrent filename starts with the prefix (case-insensitive),
    /// the corresponding tag is used instead of the default `tags` value.
    /// Longer prefixes are checked first to allow more specific matches.
    #[serde(default)]
    tag_overwrite_prefixes: Vec<[String; 2]>,
    /// Path-to-tag pairs for overwriting tags based on the torrent file location.
    /// Each entry is `[path, tag]`.
    /// Paths are matched case-insensitively against directory components.
    /// More specific paths are checked first.
    #[serde(default)]
    tag_overwrite_paths: Vec<[String; 2]>,
}

/// Final config combined from CLI arguments and user config file.
#[derive(Debug)]
pub struct Config {
    /// qBittorrent `WebUI` connection.
    pub connection: ConnectionSettings,
    /// Save path, category, and paused state for added torrents.
    pub add_options: AddOptions,
    /// Default tags and the tag overwrite rules.
    pub tag_rules: TagRules,
    /// Input paths and the torrent file name filters.
    pub collector: TorrentPathCollector,
    /// File filter configuration for skipping files by extension, directory, or size.
    pub file_filter: FileFilter,
    /// Options for building output names.
    pub naming: NamingOptions,
    /// Apply dots formatting to suggested names, using the dots config from the config file.
    pub use_dots_formatting: bool,
    /// Verbose output.
    pub verbose: bool,
    /// Dry-run mode (don't actually add torrents).
    pub dryrun: bool,
    /// Offline mode (skip qBittorrent connection, implies dryrun).
    pub offline: bool,
    /// Skip confirmation prompts.
    pub yes: bool,
    /// Automatically apply suggested renames when `--yes` is set.
    pub auto_rename: bool,
    /// Skip rename prompts for existing/duplicate torrents.
    pub skip_existing: bool,
}

impl QtorrentConfig {
    /// Try to read user config from the file if it exists.
    /// Otherwise, fall back to default config.
    ///
    /// # Errors
    /// Returns an error if config file exists but cannot be read or parsed.
    pub fn get_user_config() -> Result<Self> {
        cli_tools_common::load_user_config_section("qtorrent")
    }

    /// Parse configuration from a TOML string.
    ///
    /// # Errors
    /// Returns an error if the TOML string is invalid.
    #[cfg(test)]
    pub fn from_toml_str(toml_str: &str) -> Result<Self> {
        cli_tools_common::parse_user_config_section(toml_str, "qtorrent")
    }
}

impl Config {
    /// Create config from given command line args and user config file.
    ///
    /// # Errors
    /// Returns an error if the config file cannot be read or parsed.
    pub fn from_args(args: QtorrentArgs) -> Result<Self> {
        let user_config = QtorrentConfig::get_user_config()?;
        Ok(Self::from_args_with_user_config(args, user_config))
    }

    /// Create config by merging CLI arguments with a pre-loaded user config.
    ///
    /// Command line values take priority over the config file.
    pub fn from_args_with_user_config(args: QtorrentArgs, user_config: QtorrentConfig) -> Self {
        let defaults = ConnectionSettings::default();
        let connection = ConnectionSettings::new(
            args.host.or(user_config.host).unwrap_or(defaults.host),
            args.port.or(user_config.port).unwrap_or(defaults.port),
            args.username.or(user_config.username).unwrap_or_default(),
            args.password.or(user_config.password).unwrap_or_default(),
        );

        let add_options = AddOptions {
            save_path: args.save_path.or(user_config.save_path),
            category: args.category.or(user_config.category),
            paused: args.paused || user_config.paused,
        };

        let tag_rules = TagRules::from_pairs(
            args.tags.or(user_config.tags),
            user_config.tag_overwrite_prefixes,
            user_config.tag_overwrite_paths,
        );

        let collector = TorrentPathCollector::new(
            args.path,
            args.recurse || user_config.recurse,
            &args.include,
            &args.exclude,
        );

        let offline = args.offline || user_config.offline;

        let skip_extensions: Vec<String> = if args.skip_extensions.is_empty() {
            user_config.skip_extensions
        } else {
            args.skip_extensions
        }
        .into_iter()
        .map(|extension| extension.to_lowercase().trim_start_matches('.').to_string())
        .collect();

        let skip_directories: Vec<String> = if args.skip_directories.is_empty() {
            user_config.skip_directories
        } else {
            args.skip_directories
        }
        .into_iter()
        .map(|name| name.to_lowercase())
        .collect();

        let include_images = args.include_images || user_config.include_images;

        let min_file_size_bytes = args
            .min_file_size_mb
            .or(user_config.min_file_size_mb)
            .and_then(mb_to_bytes);

        let min_image_size_kb = args.min_image_size_kb.or(user_config.min_image_size_kb);

        let min_image_size_bytes = min_image_size_kb.and_then(kb_to_bytes);
        let min_image_file_count =
            include_images.then_some(user_config.min_image_file_count.unwrap_or(DEFAULT_MIN_IMAGE_FILE_COUNT));

        let file_filter = FileFilter::new(
            skip_extensions,
            skip_directories,
            min_file_size_bytes,
            include_images,
            min_image_size_bytes,
            min_image_file_count,
        );

        let naming = NamingOptions {
            remove_from_name: user_config.remove_from_name,
            format_custom_name: user_config.format_custom_name,
            ignore_torrent_names: user_config.ignore_torrent_names,
        };

        Self {
            connection,
            add_options,
            tag_rules,
            collector,
            file_filter,
            naming,
            use_dots_formatting: user_config.use_dots_formatting,
            verbose: args.verbose || user_config.verbose,
            // Offline implies dryrun
            dryrun: args.dryrun || user_config.dryrun || offline,
            offline,
            yes: args.yes || user_config.yes,
            auto_rename: args.auto_rename || user_config.auto_rename,
            skip_existing: args.skip_existing || user_config.skip_existing,
        }
    }
}

/// Convert megabytes to bytes, with zero or less meaning no limit.
fn mb_to_bytes(mb: f64) -> Option<u64> {
    (mb > 0.0).then_some((mb * 1024.0 * 1024.0) as u64)
}

/// Convert kilobytes to bytes, with zero or less meaning no limit.
fn kb_to_bytes(kb: f64) -> Option<u64> {
    (kb > 0.0).then_some((kb * 1024.0) as u64)
}

#[cfg(test)]
mod qtorrent_config_tests {
    use std::borrow::Cow;

    use clap::Parser;
    use cli_tools_qtorrent::torrent::FileInfo;

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
            "\n[qtorrent]\ninclude_images = true\nmin_image_size_kb = {min_image_size_kb}\nmin_image_file_count = 0\n"
        ))
        .expect("should parse config");
        let args = QtorrentArgs::try_parse_from(["test"]).expect("should parse args");
        Config::from_args_with_user_config(args, user_config)
    }

    fn make_config_with_filters(
        skip_extensions: &[&str],
        min_file_size_mb: Option<f64>,
        include_images: bool,
        min_image_size_kb: Option<u64>,
        min_image_file_count: Option<usize>,
    ) -> Config {
        let skip_ext_toml: Vec<String> = skip_extensions.iter().map(|ext| format!("\"{ext}\" ")).collect();
        let toml = format!(
            "[qtorrent]\nskip_extensions = [{}]\ninclude_images = {}\n{}{}{}\n",
            skip_ext_toml.join(", "),
            include_images,
            min_file_size_mb.map_or(String::new(), |mb| format!("min_file_size_mb = {mb}\n")),
            min_image_size_kb.map_or(String::new(), |kb| format!("min_image_size_kb = {kb}\n")),
            min_image_file_count.map_or(String::new(), |count| format!("min_image_file_count = {count}\n")),
        );
        let user_config = QtorrentConfig::from_toml_str(&toml).expect("should parse config");
        let args = QtorrentArgs::try_parse_from(["test"]).expect("should parse args");
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
min_image_file_count = 12
min_image_size_kb = 500
"#;
        let config = QtorrentConfig::from_toml_str(toml).expect("should parse config");
        assert_eq!(config.skip_extensions, vec!["nfo", "txt", "jpg"]);
        assert_eq!(config.skip_directories, vec!["sample", "subs"]);
        assert_eq!(config.min_file_size_mb, Some(50.0));
        assert!(config.include_images);
        assert_eq!(config.min_image_file_count, Some(12));
        assert_eq!(config.min_image_size_kb, Some(500.0));
    }

    #[test]
    fn from_toml_str_parses_integer_file_filtering_options() {
        let toml = r"
[qtorrent]
include_images = true
min_image_file_count = 3
min_image_size_kb = 500
    ";
        let config = QtorrentConfig::from_toml_str(toml).expect("should parse config");
        assert!(config.include_images);
        assert_eq!(config.min_image_file_count, Some(3));
        assert_eq!(config.min_image_size_kb, Some(500.0));
    }

    #[test]
    fn include_images_defaults_min_image_file_count_to_ten() {
        let user_config =
            QtorrentConfig::from_toml_str("[qtorrent]\ninclude_images = true\n").expect("should parse config");
        let args = QtorrentArgs::try_parse_from(["test"]).expect("should parse args");
        let config = Config::from_args_with_user_config(args, user_config);

        assert_eq!(
            config.file_filter.min_image_file_count,
            Some(DEFAULT_MIN_IMAGE_FILE_COUNT)
        );
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
        let config = make_config_with_filters(&["nfo", "txt"], None, true, Some(500), Some(0));

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
        let config = make_config_with_filters(&["nfo", "txt"], None, true, None, Some(0));

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
        let config = make_config_with_filters(&[], None, false, None, None);

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
        let rules = TagRules::from_pairs(None, config.tag_overwrite_prefixes, Vec::new());
        // Sorted by prefix length descending
        assert_eq!(rules.prefixes()[0].lowercase_prefix(), "longprefix");
        assert_eq!(rules.prefixes()[0].tag, "longtag");
        assert_eq!(rules.prefixes()[1].lowercase_prefix(), "short");
        assert_eq!(rules.prefixes()[1].tag, "shorttag");
    }

    #[test]
    fn from_toml_str_parses_empty_tag_overwrite_prefixes() {
        let toml = r"
[qtorrent]
tag_overwrite_prefixes = []
";
        let config = QtorrentConfig::from_toml_str(toml).expect("should parse config");
        assert_eq!(config.tag_overwrite_prefixes, [] as [[std::string::String; 2]; 0]);
    }

    #[test]
    fn from_toml_str_defaults_tag_overwrite_prefixes_to_empty() {
        let toml = r"
[qtorrent]
verbose = true
";
        let config = QtorrentConfig::from_toml_str(toml).expect("should parse config");
        assert_eq!(config.tag_overwrite_prefixes, [] as [[std::string::String; 2]; 0]);
    }

    #[test]
    fn from_toml_str_parses_tag_overwrite_paths() {
        let toml = r"
[qtorrent]
tag_overwrite_paths = [['Media\Movies\priority', 'priority-tag'], ['C:\Archive\Media\Movies', 'movie-tag'], ['Series', 'series-tag']]
";
        let config = QtorrentConfig::from_toml_str(toml).expect("should parse config");
        assert_eq!(config.tag_overwrite_paths.len(), 3);
        let rules = TagRules::from_pairs(None, Vec::new(), config.tag_overwrite_paths);
        assert_eq!(rules.paths()[0].lowercase_path(), "c:/archive/media/movies");
        assert_eq!(rules.paths()[0].tag, "movie-tag");
        assert_eq!(rules.paths()[1].lowercase_path(), "media/movies/priority");
        assert_eq!(rules.paths()[1].tag, "priority-tag");
        assert_eq!(rules.paths()[2].lowercase_path(), "series");
        assert_eq!(rules.paths()[2].tag, "series-tag");
    }

    #[test]
    fn from_toml_str_defaults_tag_overwrite_paths_to_empty() {
        let toml = r"
[qtorrent]
verbose = true
";
        let config = QtorrentConfig::from_toml_str(toml).expect("should parse config");
        assert_eq!(config.tag_overwrite_paths, [] as [[std::string::String; 2]; 0]);
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
        let rules = TagRules::from_pairs(config.tags, config.tag_overwrite_prefixes, Vec::new());
        assert_eq!(rules.default_tags.as_deref(), Some("default-tag"));
        assert_eq!(rules.prefixes().len(), 2);
        assert_eq!(rules.prefixes()[0].lowercase_prefix(), "specialprefix");
        assert_eq!(rules.prefixes()[0].tag, "special");
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

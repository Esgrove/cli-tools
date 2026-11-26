//! Configuration loading and resolution for video conversion.
//!
//! Defines the user config file settings,
//! and combines command-line arguments with the user configuration into the run options.

use std::fs;

use anyhow::{Context, Result};
use cli_tools_vconvert::options::{ALL_EXTENSIONS, DEFAULT_EXTENSIONS, OTHER_EXTENSIONS};
use cli_tools_vconvert::{BitrateTiersConfig, ConvertOptions, MinimumBitrate, SortOrder};
use itertools::Itertools;
use serde::Deserialize;

use crate::VideoConvertArgs;
use crate::cli::DatabaseMode;

/// Default display limit for showing pending files.
const DEFAULT_DISPLAY_LIMIT: usize = 100;

/// User configuration from the config file.
#[derive(Debug, Default, Deserialize)]
pub struct VideoConvertConfig {
    #[serde(default)]
    convert_all_types: bool,
    #[serde(default)]
    count: Option<usize>,
    #[serde(default)]
    bitrate: Option<u64>,
    #[serde(default)]
    bitrate_tiers: BitrateTiersConfig,
    #[serde(default)]
    max_bitrate: Option<u64>,
    #[serde(default)]
    min_duration: Option<f64>,
    #[serde(default)]
    max_duration: Option<f64>,
    #[serde(default)]
    min_resolution: Option<u32>,
    #[serde(default)]
    delete: bool,
    #[serde(default)]
    delete_duplicates: bool,
    #[serde(default)]
    display_limit: Option<usize>,
    #[serde(default)]
    exclude: Vec<String>,
    #[serde(default)]
    extensions: Vec<String>,
    #[serde(default)]
    include: Vec<String>,
    #[serde(default)]
    convert_other_types: bool,
    #[serde(default)]
    overwrite: bool,
    #[serde(default)]
    recurse: bool,
    #[serde(default)]
    sort: Option<SortOrder>,
    #[serde(default)]
    verbose: bool,
}

/// Final config combined from CLI arguments and user config file.
#[derive(Debug, Default)]
pub struct Config {
    /// Options passed to the converter.
    pub(crate) options: ConvertOptions,
    pub(crate) convert_all: bool,
    pub(crate) convert_other: bool,
    pub(crate) database_mode: Option<DatabaseMode>,
    pub(crate) display_limit: Option<usize>,
    pub(crate) verbose: bool,
}

/// Wrapper needed for parsing the config file section.
#[derive(Debug, Default, Deserialize)]
struct UserConfig {
    #[serde(default)]
    video_convert: VideoConvertConfig,
}

impl VideoConvertConfig {
    /// Try to read user config from the file if it exists.
    /// Otherwise, fall back to default config.
    ///
    /// # Errors
    /// Returns an error if config file exists but cannot be read or parsed.
    pub fn get_user_config() -> Result<Self> {
        let Some(path) = cli_tools_common::config_path() else {
            return Ok(Self::default());
        };

        match fs::read_to_string(&path) {
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
    pub fn from_toml_str(toml_str: &str) -> Result<Self> {
        toml::from_str::<UserConfig>(toml_str)
            .map(|config| config.video_convert)
            .with_context(|| "Failed to parse video_convert config TOML")
    }
}

impl Config {
    /// Create config from given command line args and user config file.
    pub(crate) fn try_from_args(args: VideoConvertArgs, user_config: VideoConvertConfig) -> Result<Self> {
        // Get database_mode before moving args fields
        let database_mode = args.database_mode();

        let include: Vec<String> = args.include.into_iter().chain(user_config.include).unique().collect();
        let exclude: Vec<String> = args.exclude.into_iter().chain(user_config.exclude).unique().collect();

        let paths = if args.paths.is_empty() {
            vec![cli_tools_common::resolve_input_path(None)?]
        } else {
            args.paths
                .iter()
                .map(|path| cli_tools_common::resolve_input_path(Some(path)))
                .collect::<Result<Vec<_>>>()?
                .into_iter()
                .unique()
                .collect()
        };

        let convert_all = args.all || (!args.other && args.extension.is_empty() && user_config.convert_all_types);
        let convert_other = args.other || (!args.all && args.extension.is_empty() && user_config.convert_other_types);

        let extensions = if !args.extension.is_empty() {
            Self::normalize_extensions(&args.extension)
        } else if args.all {
            Self::lowercase_vec(ALL_EXTENSIONS)
        } else if args.other {
            Self::lowercase_vec(OTHER_EXTENSIONS)
        } else if !user_config.extensions.is_empty() {
            Self::normalize_extensions(&user_config.extensions)
        } else if user_config.convert_all_types {
            Self::lowercase_vec(ALL_EXTENSIONS)
        } else if user_config.convert_other_types {
            Self::lowercase_vec(OTHER_EXTENSIONS)
        } else {
            Self::lowercase_vec(DEFAULT_EXTENSIONS)
        };

        // Merge filter values. CLI args take priority when they are optional.
        let min_bitrate = args.bitrate.or(user_config.bitrate).map_or_else(
            || MinimumBitrate::Tiered(user_config.bitrate_tiers.resolve()),
            MinimumBitrate::Fixed,
        );
        let max_bitrate = args.max_bitrate.or(user_config.max_bitrate);
        let min_duration = args.min_duration.or(user_config.min_duration);
        let max_duration = args.max_duration.or(user_config.max_duration);
        let min_resolution = args.min_resolution.or(user_config.min_resolution);
        let count = args.count.or(user_config.count);
        let sort = args.sort.map(SortOrder::from).or(user_config.sort).unwrap_or_default();

        // Display limit: CLI overrides config, default is 100, 0 means no limit
        let display_limit = match args.display_limit.or(user_config.display_limit) {
            Some(0) => None,
            Some(limit) => Some(limit),
            None => Some(DEFAULT_DISPLAY_LIMIT),
        };

        Ok(Self {
            options: ConvertOptions {
                paths,
                extensions,
                include,
                exclude,
                min_bitrate,
                max_bitrate,
                min_duration,
                max_duration,
                min_resolution,
                movie_mode: args.movie,
                overwrite: args.force || user_config.overwrite,
                delete: args.delete || user_config.delete,
                delete_duplicates: args.delete_duplicates || user_config.delete_duplicates,
                dry_run: args.print,
                recurse: args.recurse || user_config.recurse,
                skip_convert: args.skip_convert,
                skip_remux: args.skip_remux,
                count,
                sort,
            },
            convert_all,
            convert_other,
            database_mode,
            display_limit,
            verbose: args.verbose || user_config.verbose,
        })
    }

    /// Whether this run processes video files, as opposed to only reading or clearing the database.
    pub(crate) fn processes_files(&self) -> bool {
        self.database_mode.is_none_or(|mode| mode == DatabaseMode::Process)
    }

    /// Convert a slice of strings to lowercase.
    fn lowercase_vec(slice: &[impl AsRef<str>]) -> Vec<String> {
        slice.iter().map(|s| s.as_ref().to_lowercase()).collect()
    }

    /// Normalize file extensions to lowercase without leading dots.
    fn normalize_extensions(slice: &[impl AsRef<str>]) -> Vec<String> {
        slice
            .iter()
            .map(|extension| extension.as_ref().trim().trim_start_matches('.').to_lowercase())
            .filter(|extension| !extension.is_empty())
            .collect()
    }
}

#[cfg(test)]
mod video_convert_config_tests {
    use super::*;

    #[test]
    fn from_toml_str_parses_empty_config() {
        let toml = "";
        let config = VideoConvertConfig::from_toml_str(toml).expect("empty config should parse");
        assert!(!config.convert_all_types);
        assert!(!config.delete);
        assert!(config.bitrate.is_none());
    }

    #[test]
    fn from_toml_str_parses_video_convert_section() {
        let toml = r"
[video_convert]
convert_all_types = true
delete = true
verbose = true
recurse = true
";
        let config = VideoConvertConfig::from_toml_str(toml).expect("video_convert section should parse");
        assert!(config.convert_all_types);
        assert!(config.delete);
        assert!(config.verbose);
        assert!(config.recurse);
    }

    #[test]
    fn from_toml_str_parses_bitrate_settings() {
        let toml = r"
[video_convert]
bitrate = 8000
max_bitrate = 50000
";
        let config = VideoConvertConfig::from_toml_str(toml).expect("bitrate settings should parse");
        assert_eq!(config.bitrate, Some(8000));
        assert_eq!(config.max_bitrate, Some(50000));
    }

    #[test]
    fn from_toml_str_parses_duration_settings() {
        let toml = r"
[video_convert]
min_duration = 60.0
max_duration = 7200.0
";
        let config = VideoConvertConfig::from_toml_str(toml).expect("duration settings should parse");
        assert_eq!(config.min_duration, Some(60.0));
        assert_eq!(config.max_duration, Some(7200.0));
    }

    #[test]
    fn from_toml_str_parses_count_and_display_limit() {
        let toml = r"
[video_convert]
count = 10
display_limit = 50
";
        let config = VideoConvertConfig::from_toml_str(toml).expect("count settings should parse");
        assert_eq!(config.count, Some(10));
        assert_eq!(config.display_limit, Some(50));
    }

    #[test]
    fn from_toml_str_parses_include_exclude_lists() {
        let toml = r#"
[video_convert]
include = ["pattern1", "pattern2"]
exclude = ["skip1", "skip2"]
"#;
        let config = VideoConvertConfig::from_toml_str(toml).expect("include and exclude lists should parse");
        assert_eq!(config.include, vec!["pattern1", "pattern2"]);
        assert_eq!(config.exclude, vec!["skip1", "skip2"]);
    }

    #[test]
    fn from_toml_str_parses_extensions() {
        let toml = r#"
[video_convert]
extensions = ["mp4", "mkv", "avi"]
"#;
        let config = VideoConvertConfig::from_toml_str(toml).expect("extensions should parse");
        assert_eq!(config.extensions, vec!["mp4", "mkv", "avi"]);
    }

    #[test]
    fn from_toml_str_parses_all_sort_orders() {
        let cases = [
            ("bitrate", SortOrder::Bitrate),
            ("size", SortOrder::Size),
            ("size_asc", SortOrder::SizeAsc),
            ("duration", SortOrder::Duration),
            ("duration_asc", SortOrder::DurationAsc),
            ("resolution", SortOrder::Resolution),
            ("resolution_asc", SortOrder::ResolutionAsc),
            ("impact", SortOrder::Impact),
            ("name", SortOrder::Name),
        ];

        for (name, expected) in cases {
            let toml = format!("[video_convert]\nsort = \"{name}\"\n");
            let config = VideoConvertConfig::from_toml_str(&toml).expect("sort order should parse");
            assert_eq!(config.sort, Some(expected), "failed to parse {name}");
        }
    }

    #[test]
    fn from_toml_str_parses_remaining_settings() {
        let toml = r"
[video_convert]
convert_other_types = true
delete_duplicates = true
min_resolution = 720
overwrite = true
";
        let config = VideoConvertConfig::from_toml_str(toml).expect("remaining settings should parse");

        assert!(config.convert_other_types);
        assert!(config.delete_duplicates);
        assert_eq!(config.min_resolution, Some(720));
        assert!(config.overwrite);
    }

    #[test]
    fn from_toml_str_invalid_toml_returns_contextual_error() {
        let error =
            VideoConvertConfig::from_toml_str("this is not valid toml {{{").expect_err("invalid TOML should fail");

        assert!(error.to_string().contains("Failed to parse video_convert config TOML"));
    }

    #[test]
    fn from_toml_str_ignores_other_sections() {
        let toml = r"
[other_section]
some_value = true

[video_convert]
verbose = true
";
        let config = VideoConvertConfig::from_toml_str(toml).expect("config with unrelated section should parse");
        assert!(config.verbose);
    }

    #[test]
    fn default_values_are_correct() {
        let config = VideoConvertConfig::default();
        assert!(!config.convert_all_types);
        assert!(!config.convert_other_types);
        assert!(!config.delete);
        assert!(!config.delete_duplicates);
        assert!(!config.overwrite);
        assert!(!config.recurse);
        assert!(!config.verbose);
        assert!(config.bitrate.is_none());
        assert!(config.max_bitrate.is_none());
        assert!(config.min_duration.is_none());
        assert!(config.max_duration.is_none());
        assert!(config.count.is_none());
        assert!(config.display_limit.is_none());
        assert!(config.sort.is_none());
        assert_eq!(config.include, [] as [String; 0]);
        assert_eq!(config.exclude, [] as [String; 0]);
        assert_eq!(config.extensions, [] as [String; 0]);
    }
}

#[cfg(test)]
mod config_lowercase_vec_tests {
    use super::*;

    #[test]
    fn converts_to_lowercase() {
        let input = ["MP4", "MKV", "AVI"];
        let result = Config::lowercase_vec(&input);
        assert_eq!(result, vec!["mp4", "mkv", "avi"]);
    }

    #[test]
    fn handles_mixed_case() {
        let input = ["Mp4", "mKv", "AVI"];
        let result = Config::lowercase_vec(&input);
        assert_eq!(result, vec!["mp4", "mkv", "avi"]);
    }

    #[test]
    fn handles_empty_slice() {
        let input: [&str; 0] = [];
        let result = Config::lowercase_vec(&input);
        assert_eq!(result, [] as [String; 0]);
    }

    #[test]
    fn handles_string_slice() {
        let input = vec!["MP4".to_string(), "MKV".to_string()];
        let result = Config::lowercase_vec(&input);
        assert_eq!(result, vec!["mp4", "mkv"]);
    }
}

#[cfg(test)]
mod config_test_helpers {
    use clap::Parser;

    use super::*;

    pub(super) fn parse_args(arguments: &[&str]) -> VideoConvertArgs {
        VideoConvertArgs::try_parse_from(std::iter::once("vconvert").chain(arguments.iter().copied()))
            .expect("test arguments should parse")
    }

    pub(super) fn resolve_config(arguments: &[&str], user_config: VideoConvertConfig) -> Config {
        Config::try_from_args(parse_args(arguments), user_config).expect("test config should resolve")
    }
}

#[cfg(test)]
mod config_default_resolution_tests {
    use super::config_test_helpers::*;
    use super::*;

    #[test]
    fn resolves_defaults_and_builds_matching_database_filter() {
        let config = resolve_config(&[], VideoConvertConfig::default());
        let expected_path = cli_tools_common::resolve_input_path(None).expect("current directory should resolve");

        assert_eq!(config.options.min_bitrate, MinimumBitrate::default());
        assert!(!config.convert_all);
        assert!(!config.convert_other);
        assert_eq!(config.options.count, None);
        assert_eq!(config.database_mode, None);
        assert!(!config.options.delete);
        assert!(!config.options.delete_duplicates);
        assert_eq!(config.display_limit, Some(DEFAULT_DISPLAY_LIMIT));
        assert!(!config.options.dry_run);
        assert_eq!(config.options.exclude, [] as [String; 0]);
        assert_eq!(config.options.extensions, DEFAULT_EXTENSIONS);
        assert_eq!(config.options.include, [] as [String; 0]);
        assert_eq!(config.options.max_bitrate, None);
        assert_eq!(config.options.max_duration, None);
        assert_eq!(config.options.min_duration, None);
        assert_eq!(config.options.min_resolution, None);
        assert!(!config.options.movie_mode);
        assert!(!config.options.overwrite);
        assert_eq!(config.options.paths, vec![expected_path]);
        assert!(!config.options.recurse);
        assert!(!config.options.skip_convert);
        assert!(!config.options.skip_remux);
        assert_eq!(config.options.sort, SortOrder::Bitrate);
        assert!(!config.verbose);

        let filter = config.options.pending_file_filter();
        assert!(filter.action.is_none());
        assert_eq!(filter.extensions, config.options.extensions);
        assert_eq!(filter.min_bitrate, Some(config.options.min_bitrate));
        assert_eq!(filter.max_bitrate, config.options.max_bitrate);
        assert_eq!(filter.min_duration, config.options.min_duration);
        assert_eq!(filter.max_duration, config.options.max_duration);
        assert_eq!(filter.limit, config.options.count);
        assert_eq!(filter.sort, Some(config.options.sort));
    }

    #[test]
    fn resolves_multiple_paths_and_removes_duplicate_roots() {
        let directory = tempfile::tempdir().expect("Failed to create temporary directory");
        let video = directory.path().join("clip.mkv");
        std::fs::write(&video, b"video").expect("Failed to write video fixture");
        let root_text = directory.path().to_string_lossy();
        let video_text = video.to_string_lossy();

        let config = resolve_config(
            &[root_text.as_ref(), video_text.as_ref(), root_text.as_ref()],
            VideoConvertConfig::default(),
        );

        assert_eq!(
            config.options.paths,
            vec![
                cli_tools_common::resolve_input_path(Some(directory.path())).expect("Failed to resolve directory"),
                cli_tools_common::resolve_input_path(Some(&video)).expect("Failed to resolve video"),
            ]
        );
    }

    #[test]
    fn returns_error_for_missing_input_path() {
        let temporary_directory = tempfile::tempdir().expect("temporary directory should be created");
        let missing_path = temporary_directory.path().join("missing");
        let missing_path_text = missing_path.to_string_lossy();
        let args = parse_args(&[missing_path_text.as_ref()]);

        let error =
            Config::try_from_args(args, VideoConvertConfig::default()).expect_err("missing input path should fail");

        assert!(
            error
                .to_string()
                .contains("Input path does not exist or is not accessible")
        );
    }
}

#[cfg(test)]
mod config_cli_resolution_tests {
    use super::config_test_helpers::*;
    use super::*;

    #[test]
    fn resolves_all_cli_values_and_database_filter() {
        let config = resolve_config(
            &[
                "--bitrate",
                "9000",
                "--max-bitrate",
                "50000",
                "--min-duration",
                "60",
                "--max-duration",
                "7200",
                "--min-resolution",
                "720",
                "--count",
                "12",
                "--display-limit",
                "25",
                "--delete",
                "--delete-duplicates",
                "--print",
                "--force",
                "--include",
                "Alpha",
                "--include",
                "Alpha",
                "--include",
                "Beta",
                "--exclude",
                "Skip",
                "--extension",
                "MKV",
                "--extension",
                "AVI",
                "--recurse",
                "--skip-convert",
                "--movie",
                "--skip-remux",
                "--sort",
                "duration-asc",
                "--verbose",
                "--from-db",
            ],
            VideoConvertConfig::default(),
        );

        assert_eq!(config.options.min_bitrate, MinimumBitrate::Fixed(9000));
        assert_eq!(config.options.max_bitrate, Some(50000));
        assert_eq!(config.options.min_duration, Some(60.0));
        assert_eq!(config.options.max_duration, Some(7200.0));
        assert_eq!(config.options.min_resolution, Some(720));
        assert_eq!(config.options.count, Some(12));
        assert_eq!(config.display_limit, Some(25));
        assert!(config.options.delete);
        assert!(config.options.delete_duplicates);
        assert!(config.options.dry_run);
        assert!(config.options.overwrite);
        assert_eq!(config.options.include, ["Alpha", "Beta"]);
        assert_eq!(config.options.exclude, ["Skip"]);
        assert_eq!(config.options.extensions, ["mkv", "avi"]);
        assert!(config.options.recurse);
        assert!(config.options.skip_convert);
        assert!(config.options.movie_mode);
        assert!(config.options.skip_remux);
        assert_eq!(config.options.sort, SortOrder::DurationAsc);
        assert!(config.verbose);
        assert_eq!(config.database_mode, Some(DatabaseMode::Process));

        let filter = config.options.pending_file_filter();
        assert_eq!(filter.extensions, config.options.extensions);
        assert_eq!(filter.min_bitrate, Some(MinimumBitrate::Fixed(9000)));
        assert_eq!(filter.max_bitrate, Some(50000));
        assert_eq!(filter.min_duration, Some(60.0));
        assert_eq!(filter.max_duration, Some(7200.0));
        assert_eq!(filter.limit, Some(12));
        assert_eq!(filter.sort, Some(SortOrder::DurationAsc));
    }

    #[test]
    fn cli_options_override_user_options_and_merge_patterns() {
        let user_config = VideoConvertConfig {
            bitrate: Some(7000),
            count: Some(2),
            display_limit: Some(50),
            exclude: vec!["Shared".to_string(), "ConfigExclude".to_string()],
            extensions: vec!["WEBM".to_string()],
            include: vec!["Shared".to_string(), "ConfigInclude".to_string()],
            max_bitrate: Some(40000),
            max_duration: Some(5000.0),
            min_duration: Some(30.0),
            min_resolution: Some(480),
            sort: Some(SortOrder::Size),
            ..VideoConvertConfig::default()
        };
        let config = resolve_config(
            &[
                "--count",
                "4",
                "--display-limit",
                "0",
                "--exclude",
                "CliExclude",
                "--exclude",
                "Shared",
                "--extension",
                "MKV",
                "--include",
                "CliInclude",
                "--include",
                "Shared",
                "--max-bitrate",
                "60000",
                "--max-duration",
                "9000",
                "--min-duration",
                "90",
                "--min-resolution",
                "1080",
                "--sort",
                "impact",
            ],
            user_config,
        );

        assert_eq!(config.options.min_bitrate, MinimumBitrate::Fixed(7000));
        assert_eq!(config.options.count, Some(4));
        assert_eq!(config.display_limit, None);
        assert_eq!(config.options.exclude, ["CliExclude", "Shared", "ConfigExclude"]);
        assert_eq!(config.options.extensions, ["mkv"]);
        assert_eq!(config.options.include, ["CliInclude", "Shared", "ConfigInclude"]);
        assert_eq!(config.options.max_bitrate, Some(60000));
        assert_eq!(config.options.max_duration, Some(9000.0));
        assert_eq!(config.options.min_duration, Some(90.0));
        assert_eq!(config.options.min_resolution, Some(1080));
        assert_eq!(config.options.sort, SortOrder::Impact);
    }
}

#[cfg(test)]
mod config_user_resolution_tests {
    use super::config_test_helpers::*;
    use super::*;

    #[test]
    fn uses_user_values_when_cli_values_are_absent() {
        let user_config = VideoConvertConfig {
            bitrate: Some(6500),
            convert_all_types: true,
            convert_other_types: true,
            count: Some(3),
            delete: true,
            delete_duplicates: true,
            display_limit: Some(15),
            exclude: vec!["ConfigExclude".to_string()],
            extensions: vec!["WEBM".to_string()],
            include: vec!["ConfigInclude".to_string()],
            max_bitrate: Some(45000),
            max_duration: Some(8000.0),
            min_duration: Some(45.0),
            min_resolution: Some(576),
            overwrite: true,
            recurse: true,
            sort: Some(SortOrder::ResolutionAsc),
            verbose: true,
            ..VideoConvertConfig::default()
        };
        let config = resolve_config(&[], user_config);

        assert_eq!(config.options.min_bitrate, MinimumBitrate::Fixed(6500));
        assert!(config.convert_all);
        assert!(config.convert_other);
        assert_eq!(config.options.count, Some(3));
        assert!(config.options.delete);
        assert!(config.options.delete_duplicates);
        assert_eq!(config.display_limit, Some(15));
        assert_eq!(config.options.exclude, ["ConfigExclude"]);
        assert_eq!(config.options.extensions, ["webm"]);
        assert_eq!(config.options.include, ["ConfigInclude"]);
        assert_eq!(config.options.max_bitrate, Some(45000));
        assert_eq!(config.options.max_duration, Some(8000.0));
        assert_eq!(config.options.min_duration, Some(45.0));
        assert_eq!(config.options.min_resolution, Some(576));
        assert!(config.options.overwrite);
        assert!(config.options.recurse);
        assert_eq!(config.options.sort, SortOrder::ResolutionAsc);
        assert!(config.verbose);
    }
}

#[cfg(test)]
mod config_minimum_bitrate_resolution_tests {
    use super::config_test_helpers::*;
    use super::*;

    #[test]
    fn cli_bitrate_overrides_user_bitrate() {
        let user_config = VideoConvertConfig {
            bitrate: Some(7000),
            ..VideoConvertConfig::default()
        };
        let config = resolve_config(&["--bitrate", "5000"], user_config);

        assert_eq!(config.options.min_bitrate, MinimumBitrate::Fixed(5000));
    }

    #[test]
    fn user_bitrate_overrides_tiers() {
        let user_config = VideoConvertConfig::from_toml_str(
            r#"
[video_convert]
bitrate = 7000

[video_convert.bitrate_tiers]
"720p" = 3000
"#,
        )
        .expect("bitrate settings should parse");
        let config = resolve_config(&[], user_config);

        assert_eq!(config.options.min_bitrate, MinimumBitrate::Fixed(7000));
    }

    #[test]
    fn user_tiers_override_default_tiers() {
        let user_config = VideoConvertConfig::from_toml_str(
            r#"
[video_convert.bitrate_tiers]
"720p" = 3000
1440p_high_fps = 30000
"#,
        )
        .expect("bitrate tiers should parse");
        let config = resolve_config(&[], user_config);

        assert_eq!(config.options.min_bitrate.threshold(1280, 720, 25.0), 3000);
        assert_eq!(config.options.min_bitrate.threshold(1280, 720, 60.0), 6000);
        assert_eq!(config.options.min_bitrate.threshold(1920, 1080, 24.0), 8000);
        assert_eq!(config.options.min_bitrate.threshold(3840, 2160, 60.0), 30000);
        let filter = config.options.pending_file_filter();
        assert_eq!(filter.min_bitrate, Some(config.options.min_bitrate));
    }
}

#[cfg(test)]
mod config_extension_resolution_tests {
    use super::config_test_helpers::*;
    use super::*;

    #[test]
    fn all_flag_selects_every_supported_extension() {
        let config = resolve_config(&["--all"], VideoConvertConfig::default());

        assert!(config.convert_all);
        assert_eq!(config.options.extensions, ALL_EXTENSIONS);
    }

    #[test]
    fn other_flag_excludes_mp4() {
        let config = resolve_config(&["--other"], VideoConvertConfig::default());

        assert!(config.convert_other);
        assert_eq!(config.options.extensions, OTHER_EXTENSIONS);
        assert!(!config.options.extensions.iter().any(|extension| extension == "mp4"));
    }

    #[test]
    fn cli_all_and_other_override_user_config_extensions_and_modes() {
        let with_all = resolve_config(
            &["--all"],
            VideoConvertConfig {
                extensions: vec!["avi".to_string()],
                convert_other_types: true,
                ..VideoConvertConfig::default()
            },
        );
        assert_eq!(with_all.options.extensions, ALL_EXTENSIONS);

        let with_other = resolve_config(
            &["--other"],
            VideoConvertConfig {
                extensions: vec!["avi".to_string()],
                convert_all_types: true,
                ..VideoConvertConfig::default()
            },
        );
        assert_eq!(with_other.options.extensions, OTHER_EXTENSIONS);
        assert!(!with_other.options.extensions.iter().any(|extension| extension == "mp4"));
    }

    #[test]
    fn leading_dots_are_stripped_from_extensions() {
        let config = resolve_config(&["-t", ".MP4", "-t", ".mkv"], VideoConvertConfig::default());
        assert_eq!(config.options.extensions, vec!["mp4", "mkv"]);
    }
}

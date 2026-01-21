use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use itertools::Itertools;
use serde::Deserialize;

use crate::DatabaseMode;
use crate::SortOrder;
use crate::VideoConvertArgs;
use crate::database::PendingFileFilter;

/// Default video extensions
const DEFAULT_EXTENSIONS: &[&str] = &["mp4", "mkv"];

/// Other video extensions excluding mp4
const OTHER_EXTENSIONS: &[&str] = &["mkv", "wmv", "flv", "m4v", "ts", "mpg", "avi", "mov", "webm"];

/// All video extensions
const ALL_EXTENSIONS: &[&str] = &["mp4", "mkv", "wmv", "flv", "m4v", "ts", "mpg", "avi", "mov", "webm"];

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
    max_bitrate: Option<u64>,
    #[serde(default)]
    min_duration: Option<f64>,
    #[serde(default)]
    max_duration: Option<f64>,
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

/// Default display limit for showing pending files.
const DEFAULT_DISPLAY_LIMIT: usize = 100;

/// Final config combined from CLI arguments and user config file.
#[derive(Debug, Default)]
pub struct Config {
    pub(crate) bitrate_limit: u64,
    pub(crate) convert_all: bool,
    pub(crate) convert_other: bool,
    pub(crate) count: Option<usize>,
    pub(crate) database_mode: Option<DatabaseMode>,
    pub(crate) db_filter: PendingFileFilter,
    pub(crate) delete: bool,
    pub(crate) delete_duplicates: bool,
    pub(crate) display_limit: Option<usize>,
    pub(crate) dryrun: bool,
    pub(crate) exclude: Vec<String>,
    pub(crate) extensions: Vec<String>,
    pub(crate) include: Vec<String>,
    pub(crate) max_bitrate: Option<u64>,
    pub(crate) max_duration: Option<f64>,
    pub(crate) min_duration: Option<f64>,
    pub(crate) overwrite: bool,
    pub(crate) path: PathBuf,
    pub(crate) recurse: bool,
    pub(crate) skip_convert: bool,
    pub(crate) skip_remux: bool,
    pub(crate) sort: SortOrder,
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

        let path = cli_tools::resolve_input_path(args.path.as_deref())?;

        let convert_all = args.all || user_config.convert_all_types;
        let convert_other = args.other || user_config.convert_other_types;

        let extensions = if !args.extension.is_empty() {
            Self::lowercase_vec(&args.extension)
        } else if !user_config.extensions.is_empty() {
            Self::lowercase_vec(&user_config.extensions)
        } else if convert_all {
            Self::lowercase_vec(ALL_EXTENSIONS)
        } else if convert_other {
            Self::lowercase_vec(OTHER_EXTENSIONS)
        } else {
            Self::lowercase_vec(DEFAULT_EXTENSIONS)
        };

        // Merge filter values: CLI args take priority over user config
        let bitrate_limit = user_config.bitrate.unwrap_or(args.bitrate);
        let max_bitrate = args.max_bitrate.or(user_config.max_bitrate);
        let min_duration = args.min_duration.or(user_config.min_duration);
        let max_duration = args.max_duration.or(user_config.max_duration);
        let count = args.count.or(user_config.count);
        let sort = args.sort.or(user_config.sort).unwrap_or(SortOrder::Name);

        // Display limit: CLI overrides config, default is 100, 0 means no limit
        let display_limit = match args.display_limit.or(user_config.display_limit) {
            Some(0) => None,
            Some(limit) => Some(limit),
            None => Some(DEFAULT_DISPLAY_LIMIT),
        };

        // Build db_filter with merged values
        let db_filter = PendingFileFilter {
            action: None,
            extensions: extensions.clone(),
            min_bitrate: Some(bitrate_limit),
            max_bitrate,
            min_duration,
            max_duration,
            limit: count,
            sort: Some(sort),
        };

        Ok(Self {
            bitrate_limit,
            convert_all,
            convert_other,
            count,
            database_mode,
            db_filter,
            delete: args.delete || user_config.delete,
            delete_duplicates: args.delete_duplicates || user_config.delete_duplicates,
            display_limit,
            dryrun: args.print,
            exclude,
            extensions,
            include,
            max_bitrate,
            max_duration,
            min_duration,
            overwrite: args.force || user_config.overwrite,
            path,
            recurse: args.recurse || user_config.recurse,
            skip_convert: args.skip_convert,
            skip_remux: args.skip_remux,
            sort,
            verbose: args.verbose || user_config.verbose,
        })
    }

    /// Convert a slice of strings to lowercase.
    fn lowercase_vec(slice: &[impl AsRef<str>]) -> Vec<String> {
        slice.iter().map(|s| s.as_ref().to_lowercase()).collect()
    }
}

#[cfg(test)]
mod video_convert_config_tests {
    use super::*;

    #[test]
    fn from_toml_str_parses_empty_config() {
        let toml = "";
        let config = VideoConvertConfig::from_toml_str(toml).unwrap();
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
        let config = VideoConvertConfig::from_toml_str(toml).unwrap();
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
        let config = VideoConvertConfig::from_toml_str(toml).unwrap();
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
        let config = VideoConvertConfig::from_toml_str(toml).unwrap();
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
        let config = VideoConvertConfig::from_toml_str(toml).unwrap();
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
        let config = VideoConvertConfig::from_toml_str(toml).unwrap();
        assert_eq!(config.include, vec!["pattern1", "pattern2"]);
        assert_eq!(config.exclude, vec!["skip1", "skip2"]);
    }

    #[test]
    fn from_toml_str_parses_extensions() {
        let toml = r#"
[video_convert]
extensions = ["mp4", "mkv", "avi"]
"#;
        let config = VideoConvertConfig::from_toml_str(toml).unwrap();
        assert_eq!(config.extensions, vec!["mp4", "mkv", "avi"]);
    }

    #[test]
    fn from_toml_str_parses_sort_order() {
        let toml = r#"
[video_convert]
sort = "bitrate"
"#;
        let config = VideoConvertConfig::from_toml_str(toml).unwrap();
        assert_eq!(config.sort, Some(SortOrder::Bitrate));
    }

    #[test]
    fn from_toml_str_parses_sort_order_size() {
        let toml = r#"
[video_convert]
sort = "size"
"#;
        let config = VideoConvertConfig::from_toml_str(toml).unwrap();
        assert_eq!(config.sort, Some(SortOrder::Size));
    }

    #[test]
    fn from_toml_str_invalid_toml_returns_error() {
        let toml = "this is not valid toml {{{";
        let result = VideoConvertConfig::from_toml_str(toml);
        assert!(result.is_err());
    }

    #[test]
    fn from_toml_str_ignores_other_sections() {
        let toml = r"
[other_section]
some_value = true

[video_convert]
verbose = true
";
        let config = VideoConvertConfig::from_toml_str(toml).unwrap();
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
        assert!(config.include.is_empty());
        assert!(config.exclude.is_empty());
        assert!(config.extensions.is_empty());
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
        assert!(result.is_empty());
    }

    #[test]
    fn handles_string_slice() {
        let input = vec!["MP4".to_string(), "MKV".to_string()];
        let result = Config::lowercase_vec(&input);
        assert_eq!(result, vec!["mp4", "mkv"]);
    }
}

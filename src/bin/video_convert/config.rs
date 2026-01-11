use std::fs;
use std::path::PathBuf;

use anyhow::Result;
use cli_tools::print_error;
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
    pub fn get_user_config() -> Self {
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
            .map(|config| config.video_convert)
            .unwrap_or_default()
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

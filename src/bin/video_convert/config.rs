use std::fs;
use std::path::PathBuf;

use anyhow::Result;
use cli_tools::print_error;
use serde::Deserialize;

use crate::VideoConvertArgs;

/// Default video extensions
const DEFAULT_EXTENSIONS: &[&str] = &["mp4", "mkv"];

/// Other video extensions excluding mp4
const OTHER_EXTENSIONS: &[&str] = &["mkv", "wmv", "flv", "m4v", "ts", "mpg", "avi", "mov", "webm"];

/// All video extensions
const ALL_EXTENSIONS: &[&str] = &["mp4", "mkv", "wmv", "flv", "m4v", "ts", "mpg", "avi", "mov", "webm"];

/// Config from a config file
#[derive(Debug, Default, Deserialize)]
pub struct VideoConvertConfig {
    #[serde(default)]
    convert_all_types: bool,
    #[serde(default)]
    bitrate: Option<u64>,
    #[serde(default)]
    delete: bool,
    #[serde(default)]
    exclude: Vec<String>,
    /// Custom list of file extensions to process (overrides all/other flags)
    #[serde(default)]
    extensions: Vec<String>,
    #[serde(default)]
    include: Vec<String>,
    #[serde(default)]
    number: Option<usize>,
    #[serde(default)]
    convert_other_types: bool,
    #[serde(default)]
    overwrite: bool,
    #[serde(default)]
    recursive: bool,
    #[serde(default)]
    verbose: bool,
}

/// Final config created from CLI arguments and user config file.
#[derive(Debug, Default)]
pub struct Config {
    pub(crate) bitrate_limit: u64,
    pub(crate) convert_all: bool,
    pub(crate) convert_other: bool,
    pub(crate) delete: bool,
    pub(crate) dryrun: bool,
    pub(crate) exclude: Vec<String>,
    pub(crate) include: Vec<String>,
    /// File extensions to convert (lowercase, without leading dot)
    pub(crate) extensions: Vec<String>,
    pub(crate) number: usize,
    pub(crate) overwrite: bool,
    pub(crate) path: PathBuf,
    pub(crate) recursive: bool,
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
    pub(crate) fn get_user_config() -> Self {
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
        let mut include = args.include;
        include.extend(user_config.include);

        let mut exclude = args.exclude;
        exclude.extend(user_config.exclude);

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

        Ok(Self {
            bitrate_limit: user_config.bitrate.unwrap_or(args.bitrate),
            convert_all,
            convert_other,
            delete: args.delete || user_config.delete,
            dryrun: args.print,
            exclude,
            extensions,
            include,
            number: user_config.number.unwrap_or(args.number),
            overwrite: args.force || user_config.overwrite,
            path,
            recursive: args.recurse || user_config.recursive,
            verbose: args.verbose || user_config.verbose,
        })
    }

    fn lowercase_vec(slice: &[impl AsRef<str>]) -> Vec<String> {
        slice.iter().map(|s| s.as_ref().to_lowercase()).collect()
    }
}

use std::fs;

use serde::Deserialize;

use cli_tools::print_error;

use crate::ThumbnailArgs;

/// Default number of columns for landscape videos.
const DEFAULT_COLS_LANDSCAPE: u32 = 3;

/// Default number of rows for landscape videos.
const DEFAULT_ROWS_LANDSCAPE: u32 = 4;

/// Default number of columns for portrait videos.
const DEFAULT_COLS_PORTRAIT: u32 = 4;

/// Default number of rows for portrait videos.
const DEFAULT_ROWS_PORTRAIT: u32 = 3;

/// Default padding between tiles in pixels for landscape videos.
const DEFAULT_PADDING_LANDSCAPE: u32 = 8;

/// Default padding between tiles in pixels for portrait videos.
const DEFAULT_PADDING_PORTRAIT: u32 = 16;

/// Default thumbnail width in pixels.
const DEFAULT_SCALE_WIDTH: u32 = 480;

/// Default font size for timestamp overlay.
const DEFAULT_FONT_SIZE: u32 = 20;

/// Default JPEG quality (lower is better).
const DEFAULT_QUALITY: u32 = 2;

/// User configuration from the config file.
#[derive(Debug, Default, Deserialize)]
pub struct ThumbnailConfig {
    #[serde(default)]
    cols_landscape: Option<u32>,
    #[serde(default)]
    cols_portrait: Option<u32>,
    #[serde(default)]
    dryrun: bool,
    #[serde(default)]
    font_size: Option<u32>,
    #[serde(default)]
    overwrite: bool,
    #[serde(default)]
    padding_landscape: Option<u32>,
    #[serde(default)]
    padding_portrait: Option<u32>,
    #[serde(default)]
    quality: Option<u32>,
    #[serde(default)]
    recurse: bool,
    #[serde(default)]
    rows_landscape: Option<u32>,
    #[serde(default)]
    rows_portrait: Option<u32>,
    #[serde(default)]
    scale_width: Option<u32>,
    #[serde(default)]
    verbose: bool,
}

/// Final config combined from CLI arguments and user config file.
#[derive(Debug)]
pub struct Config {
    pub(crate) cols_landscape: u32,
    pub(crate) cols_portrait: u32,
    pub(crate) dryrun: bool,
    pub(crate) font_size: u32,
    pub(crate) overwrite: bool,
    pub(crate) padding_landscape: u32,
    pub(crate) padding_portrait: u32,
    pub(crate) quality: u32,
    pub(crate) recurse: bool,
    pub(crate) rows_landscape: u32,
    pub(crate) rows_portrait: u32,
    pub(crate) scale_width: u32,
    pub(crate) verbose: bool,
}

/// Wrapper needed for parsing the user config file section.
#[derive(Debug, Default, Deserialize)]
struct UserConfig {
    #[serde(default)]
    thumbnail: ThumbnailConfig,
}

impl ThumbnailConfig {
    /// Try to read user config from the file if it exists.
    /// Otherwise, fall back to default config.
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
            .map(|config| config.thumbnail)
            .unwrap_or_default()
    }
}

impl Config {
    /// Create config from given command line args and user config file.
    pub fn from_args(args: &ThumbnailArgs) -> Self {
        let user_config = ThumbnailConfig::get_user_config();

        let cols_landscape = args
            .cols
            .or(user_config.cols_landscape)
            .unwrap_or(DEFAULT_COLS_LANDSCAPE);

        let cols_portrait = args.cols.or(user_config.cols_portrait).unwrap_or(DEFAULT_COLS_PORTRAIT);
        let font_size = args.fontsize.or(user_config.font_size).unwrap_or(DEFAULT_FONT_SIZE);

        let padding_landscape = args
            .padding
            .or(user_config.padding_landscape)
            .unwrap_or(DEFAULT_PADDING_LANDSCAPE);

        let padding_portrait = args
            .padding
            .or(user_config.padding_portrait)
            .unwrap_or(DEFAULT_PADDING_PORTRAIT);

        let quality = args.quality.or(user_config.quality).unwrap_or(DEFAULT_QUALITY);
        let rows_landscape = args
            .rows
            .or(user_config.rows_landscape)
            .unwrap_or(DEFAULT_ROWS_LANDSCAPE);

        let rows_portrait = args.rows.or(user_config.rows_portrait).unwrap_or(DEFAULT_ROWS_PORTRAIT);
        let scale_width = args.scale.or(user_config.scale_width).unwrap_or(DEFAULT_SCALE_WIDTH);

        Self {
            cols_landscape,
            cols_portrait,
            dryrun: args.print || user_config.dryrun,
            font_size,
            overwrite: args.force || user_config.overwrite,
            padding_landscape,
            padding_portrait,
            quality,
            recurse: args.recurse || user_config.recurse,
            rows_landscape,
            rows_portrait,
            scale_width,
            verbose: args.verbose || user_config.verbose,
        }
    }
}

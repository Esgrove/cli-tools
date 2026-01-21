use std::fs;

use anyhow::Result;
use serde::Deserialize;

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

    /// Parse configuration from a TOML string.
    ///
    /// # Errors
    /// Returns an error if the TOML string is invalid.
    pub fn from_toml_str(toml_str: &str) -> Result<Self> {
        toml::from_str::<UserConfig>(toml_str)
            .map(|config| config.thumbnail)
            .map_err(|e| anyhow::anyhow!("Failed to parse config: {e}"))
    }
}

impl Config {
    /// Create config from given command line args and user config file.
    ///
    /// # Errors
    /// Returns an error if the config file cannot be read or parsed.
    pub fn from_args(args: &ThumbnailArgs) -> Result<Self> {
        let user_config = ThumbnailConfig::get_user_config()?;

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

        Ok(Self {
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
        })
    }
}

#[cfg(test)]
mod thumbnail_config_tests {
    use super::*;

    #[test]
    fn from_toml_str_parses_empty_config() {
        let toml = "";
        let config = ThumbnailConfig::from_toml_str(toml).expect("should parse empty config");
        assert!(!config.dryrun);
        assert!(!config.overwrite);
        assert!(!config.recurse);
        assert!(!config.verbose);
        assert!(config.cols_landscape.is_none());
        assert!(config.rows_landscape.is_none());
    }

    #[test]
    fn from_toml_str_parses_thumbnail_section() {
        let toml = r"
[thumbnail]
dryrun = true
overwrite = true
recurse = true
verbose = true
";
        let config = ThumbnailConfig::from_toml_str(toml).expect("should parse config");
        assert!(config.dryrun);
        assert!(config.overwrite);
        assert!(config.recurse);
        assert!(config.verbose);
    }

    #[test]
    fn from_toml_str_parses_grid_settings() {
        let toml = r"
[thumbnail]
cols_landscape = 4
rows_landscape = 5
cols_portrait = 5
rows_portrait = 4
";
        let config = ThumbnailConfig::from_toml_str(toml).expect("should parse config");
        assert_eq!(config.cols_landscape, Some(4));
        assert_eq!(config.rows_landscape, Some(5));
        assert_eq!(config.cols_portrait, Some(5));
        assert_eq!(config.rows_portrait, Some(4));
    }

    #[test]
    fn from_toml_str_parses_padding_settings() {
        let toml = r"
[thumbnail]
padding_landscape = 10
padding_portrait = 20
";
        let config = ThumbnailConfig::from_toml_str(toml).expect("should parse config");
        assert_eq!(config.padding_landscape, Some(10));
        assert_eq!(config.padding_portrait, Some(20));
    }

    #[test]
    fn from_toml_str_parses_display_settings() {
        let toml = r"
[thumbnail]
font_size = 24
quality = 3
scale_width = 640
";
        let config = ThumbnailConfig::from_toml_str(toml).expect("should parse config");
        assert_eq!(config.font_size, Some(24));
        assert_eq!(config.quality, Some(3));
        assert_eq!(config.scale_width, Some(640));
    }

    #[test]
    fn from_toml_str_invalid_toml_returns_error() {
        let toml = "this is not valid toml {{{";
        let result = ThumbnailConfig::from_toml_str(toml);
        assert!(result.is_err());
    }

    #[test]
    fn from_toml_str_ignores_other_sections() {
        let toml = r"
[other_section]
some_value = true

[thumbnail]
verbose = true
";
        let config = ThumbnailConfig::from_toml_str(toml).expect("should parse config");
        assert!(config.verbose);
        assert!(!config.dryrun);
    }
}

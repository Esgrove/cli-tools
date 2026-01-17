//! Configuration module for the resolution binary.
//!
//! This module handles reading configuration from both CLI arguments and the user
//! config file (`~/.config/cli-tools.toml`). CLI arguments take priority over
//! config file settings.
//!
//! # Example config file section
//!
//! ```toml
//! [resolution]
//! debug = false
//! delete_limit = 500
//! dryrun = false
//! overwrite = false
//! recurse = false
//! verbose = false
//! ```

use std::fs;
use std::path::PathBuf;

use anyhow::Result;
use serde::Deserialize;

use cli_tools::print_error;

use crate::Args;

/// Default resolution limit for delete mode (in pixels).
/// Files with width or height smaller than this value will be deleted.
const DEFAULT_DELETE_LIMIT: u32 = 500;

/// User configuration from the config file.
///
/// These settings can be overridden by CLI arguments.
#[derive(Debug, Default, Deserialize)]
struct ResolutionConfig {
    /// Enable debug output including fuzzy resolution range information.
    #[serde(default)]
    debug: bool,

    /// Default resolution limit for delete mode.
    /// Files with width or height smaller than this value will be deleted.
    #[serde(default)]
    delete_limit: Option<u32>,

    /// Only print operations without actually performing them.
    #[serde(default)]
    dryrun: bool,

    /// Overwrite existing files when renaming.
    #[serde(default)]
    overwrite: bool,

    /// Process files recursively in subdirectories.
    #[serde(default)]
    recurse: bool,

    /// Print verbose output during processing.
    #[serde(default)]
    verbose: bool,
}

/// Final configuration combined from CLI arguments and user config file.
///
/// This struct is used throughout the application after merging
/// CLI arguments with user configuration. CLI arguments take priority.
#[derive(Debug)]
pub struct Config {
    /// Enable debug output including fuzzy resolution range information.
    pub debug: bool,

    /// Resolution limit for delete mode (in pixels).
    /// When set, files with width or height smaller than this value will be deleted.
    /// `None` means delete mode is disabled.
    pub delete_limit: Option<u32>,

    /// Only print operations without actually performing them.
    pub dryrun: bool,

    /// Overwrite existing files when renaming.
    pub overwrite: bool,

    /// Input path to process (file or directory).
    pub path: PathBuf,

    /// Process files recursively in subdirectories.
    pub recurse: bool,

    /// Print verbose output during processing.
    pub verbose: bool,
}

/// Wrapper struct for parsing the `[resolution]` section from the config file.
#[derive(Debug, Default, Deserialize)]
struct UserConfig {
    #[serde(default)]
    resolution: ResolutionConfig,
}

impl ResolutionConfig {
    /// Read user configuration from the config file.
    ///
    /// Attempts to read from `~/.config/cli-tools.toml`. If the file doesn't exist
    /// or cannot be parsed, returns default configuration.
    fn get_user_config() -> Self {
        cli_tools::config::CONFIG_PATH
            .as_deref()
            .and_then(|path| {
                fs::read_to_string(path)
                    .map_err(|error| {
                        print_error!("Error reading config file {}: {error}", path.display());
                    })
                    .ok()
            })
            .and_then(|config_string| {
                toml::from_str::<UserConfig>(&config_string)
                    .map_err(|error| {
                        print_error!("Error parsing config file: {error}");
                    })
                    .ok()
            })
            .map(|config| config.resolution)
            .unwrap_or_default()
    }
}

impl Config {
    /// Create configuration from CLI arguments and user config file.
    ///
    /// CLI arguments take priority over config file settings.
    /// Boolean flags are combined with OR (enabled if either source enables them).
    ///
    /// # Errors
    ///
    /// Returns an error if the input path cannot be resolved.
    pub fn try_from_args(args: &Args) -> Result<Self> {
        let user_config = ResolutionConfig::get_user_config();
        let path = cli_tools::resolve_input_path(args.path.as_deref())?;

        // Handle delete limit: CLI can specify --delete with optional value
        // If --delete is passed without value, use config or default
        // If --delete is passed with value, use that value
        let delete_limit = args
            .delete
            .map(|cli_limit| cli_limit.unwrap_or_else(|| user_config.delete_limit.unwrap_or(DEFAULT_DELETE_LIMIT)));

        Ok(Self {
            debug: args.debug || user_config.debug,
            delete_limit,
            dryrun: args.print || user_config.dryrun,
            overwrite: args.force || user_config.overwrite,
            path,
            recurse: args.recurse || user_config.recurse,
            verbose: args.verbose || user_config.verbose,
        })
    }
}

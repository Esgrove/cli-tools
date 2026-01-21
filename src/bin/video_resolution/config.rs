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

use anyhow::{Result, anyhow};
use serde::Deserialize;

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
    /// Attempts to read from `~/.config/cli-tools.toml`. If the file doesn't exist,
    /// returns default configuration.
    ///
    /// # Errors
    /// Returns an error if config file exists but cannot be read or parsed.
    fn get_user_config() -> Result<Self> {
        let Some(path) = cli_tools::config::CONFIG_PATH.as_deref() else {
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
            .map(|config| config.resolution)
            .map_err(|e| anyhow!("Failed to parse config: {e}"))
    }
}

impl Config {
    /// Create configuration from CLI arguments and user config file.
    ///
    /// CLI arguments take priority over config file settings.
    /// Boolean flags are combined with OR (enabled if either source enables them).
    ///
    /// # Errors
    /// Returns an error if the path cannot be resolved or the config file cannot be read or parsed.
    pub fn try_from_args(args: &Args) -> Result<Self> {
        let user_config = ResolutionConfig::get_user_config()?;
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

#[cfg(test)]
mod resolution_config_tests {
    use super::*;

    #[test]
    fn from_toml_str_parses_empty_config() {
        let toml = "";
        let config = ResolutionConfig::from_toml_str(toml).expect("should parse empty config");
        assert!(!config.debug);
        assert!(!config.dryrun);
        assert!(!config.overwrite);
        assert!(!config.recurse);
        assert!(!config.verbose);
        assert!(config.delete_limit.is_none());
    }

    #[test]
    fn from_toml_str_parses_resolution_section() {
        let toml = r"
[resolution]
debug = true
dryrun = true
overwrite = true
recurse = true
verbose = true
";
        let config = ResolutionConfig::from_toml_str(toml).expect("should parse config");
        assert!(config.debug);
        assert!(config.dryrun);
        assert!(config.overwrite);
        assert!(config.recurse);
        assert!(config.verbose);
    }

    #[test]
    fn from_toml_str_parses_delete_limit() {
        let toml = r"
[resolution]
delete_limit = 720
";
        let config = ResolutionConfig::from_toml_str(toml).expect("should parse config");
        assert_eq!(config.delete_limit, Some(720));
    }

    #[test]
    fn from_toml_str_invalid_toml_returns_error() {
        let toml = "this is not valid toml {{{";
        let result = ResolutionConfig::from_toml_str(toml);
        assert!(result.is_err());
    }

    #[test]
    fn from_toml_str_ignores_other_sections() {
        let toml = r"
[other_section]
some_value = true

[resolution]
verbose = true
";
        let config = ResolutionConfig::from_toml_str(toml).expect("should parse config");
        assert!(config.verbose);
        assert!(!config.debug);
    }
}

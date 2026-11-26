//! Configuration for vstats.
//!
//! Combines the CLI arguments with the `[video_stats]` section of the user config file.

use std::path::PathBuf;

use anyhow::Result;
use serde::Deserialize;

use crate::VideoStatsArgs;

/// Config from the `[video_stats]` section of the user config file.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct VideoStatsConfig {
    #[serde(default)]
    recurse: bool,
    #[serde(default)]
    verbose: bool,
}

/// Final config combined from CLI arguments and user config file.
#[derive(Debug)]
pub struct Config {
    /// Resolved input directory or file.
    pub(crate) root: PathBuf,
    /// Recurse into subdirectories.
    pub(crate) recurse: bool,
    /// Skip reading or writing the shared scan cache.
    pub(crate) no_cache: bool,
    /// Print verbose per-file output.
    pub(crate) verbose: bool,
}

impl VideoStatsConfig {
    /// Try to read user config from the file if it exists.
    /// Otherwise, fall back to default config.
    ///
    /// # Errors
    /// Returns an error if config file exists but cannot be read or parsed.
    fn get_user_config() -> Result<Self> {
        cli_tools_common::load_user_config_section("video_stats")
    }

    /// Parse config from a TOML string.
    ///
    /// # Errors
    /// Returns an error if the TOML string is invalid.
    #[cfg(test)]
    fn from_toml_str(toml_str: &str) -> Result<Self> {
        cli_tools_common::parse_user_config_section(toml_str, "video_stats")
    }
}

impl Config {
    /// Create config from given command line args and user config file.
    ///
    /// # Errors
    /// Returns an error if the config file cannot be read or parsed, or the input path cannot be resolved.
    pub fn from_args(args: &VideoStatsArgs) -> Result<Self> {
        let user_config = VideoStatsConfig::get_user_config()?;
        Self::from_args_and_user_config(args, &user_config)
    }

    /// Combine the CLI arguments with an already loaded user config.
    fn from_args_and_user_config(args: &VideoStatsArgs, user_config: &VideoStatsConfig) -> Result<Self> {
        Ok(Self {
            root: cli_tools_common::resolve_input_path(args.path.as_deref())?,
            recurse: args.recurse || user_config.recurse,
            no_cache: args.no_cache,
            verbose: args.verbose || user_config.verbose,
        })
    }
}

#[cfg(test)]
mod test_video_stats_config {
    use super::*;

    #[test]
    fn empty_config_uses_defaults() {
        let config = VideoStatsConfig::from_toml_str("").expect("empty config should parse");
        assert!(!config.recurse);
        assert!(!config.verbose);
    }

    #[test]
    fn parses_video_stats_section_and_ignores_others() {
        let toml = r"
[video_convert]
verbose = false

[video_stats]
recurse = true
verbose = true
";
        let config = VideoStatsConfig::from_toml_str(toml).expect("config should parse");
        assert!(config.recurse);
        assert!(config.verbose);
    }

    #[test]
    fn invalid_value_type_and_unknown_field_return_error() {
        assert!(VideoStatsConfig::from_toml_str("[video_stats]\nrecurse = \"yes\"\n").is_err());
        assert!(VideoStatsConfig::from_toml_str("[video_stats]\nunknown_key = true\n").is_err());
    }
}

#[cfg(test)]
mod test_config_from_args {
    use clap::Parser;

    use super::*;

    fn parse_args(arguments: &[&str]) -> VideoStatsArgs {
        VideoStatsArgs::try_parse_from(arguments).expect("arguments should parse")
    }

    #[test]
    fn user_config_enables_flags_not_given_on_command_line() -> Result<()> {
        let temp_directory = tempfile::TempDir::new()?;
        let path = temp_directory.path().to_str().expect("temporary path should be UTF-8");
        let user_config = VideoStatsConfig {
            recurse: true,
            verbose: true,
        };

        let config = Config::from_args_and_user_config(&parse_args(&["vstats", path]), &user_config)?;

        assert_eq!(config.root, dunce::canonicalize(temp_directory.path())?);
        assert!(config.recurse);
        assert!(config.verbose);
        Ok(())
    }

    #[test]
    fn command_line_flags_apply_without_user_config() -> Result<()> {
        let temp_directory = tempfile::TempDir::new()?;
        let path = temp_directory.path().to_str().expect("temporary path should be UTF-8");

        let config = Config::from_args_and_user_config(
            &parse_args(&["vstats", path, "--recurse", "--verbose"]),
            &VideoStatsConfig::default(),
        )?;

        assert!(config.recurse);
        assert!(config.verbose);
        Ok(())
    }

    #[test]
    fn defaults_leave_flags_disabled() -> Result<()> {
        let temp_directory = tempfile::TempDir::new()?;
        let path = temp_directory.path().to_str().expect("temporary path should be UTF-8");

        let config = Config::from_args_and_user_config(&parse_args(&["vstats", path]), &VideoStatsConfig::default())?;

        assert!(!config.recurse);
        assert!(!config.verbose);
        Ok(())
    }
}

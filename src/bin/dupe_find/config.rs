//! Configuration for `DupeFind`.
//!
//! Handles reading configuration from CLI arguments and the user config file.

use std::fs;
use std::path::PathBuf;

use itertools::Itertools;
use regex::Regex;
use serde::Deserialize;

use cli_tools::print_error;

use crate::Args;
use crate::dupe_find::FILE_EXTENSIONS;

/// Config from the user config file.
#[derive(Debug, Default, Deserialize)]
pub struct DupeConfig {
    #[serde(default)]
    pub(crate) default_paths: Vec<PathBuf>,
    #[serde(default)]
    dryrun: bool,
    #[serde(default)]
    extensions: Vec<String>,
    #[serde(default)]
    move_files: bool,
    #[serde(default)]
    pub(crate) paths: Vec<PathBuf>,
    #[serde(default)]
    patterns: Vec<String>,
    #[serde(default)]
    recurse: bool,
    #[serde(default)]
    verbose: bool,
}

/// Wrapper needed for parsing the config file section.
#[derive(Debug, Default, Deserialize)]
struct UserConfig {
    #[serde(default)]
    dupefind: DupeConfig,
}

/// Final config created from CLI arguments and user config file.
#[derive(Debug, Clone)]
pub struct Config {
    pub(crate) dryrun: bool,
    pub(crate) extensions: Vec<String>,
    pub(crate) move_files: bool,
    pub(crate) patterns: Vec<Regex>,
    pub(crate) recurse: bool,
    pub(crate) verbose: bool,
}

impl DupeConfig {
    /// Try to read user config from the file if it exists.
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
            .map(|config| config.dupefind)
            .unwrap_or_default()
    }
}

impl Config {
    /// Create config from given command line args and user config file.
    pub fn from_args(args: Args) -> anyhow::Result<Self> {
        let user_config = DupeConfig::get_user_config();

        // Combine patterns from config and CLI
        let pattern_strings: Vec<String> = user_config.patterns.into_iter().chain(args.pattern).unique().collect();

        // Compile regex patterns
        let patterns: Vec<Regex> = pattern_strings
            .iter()
            .map(|p| Regex::new(p))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| anyhow::anyhow!("Invalid regex pattern: {e}"))?;

        // Combine extensions from config and CLI, with defaults if none specified
        let mut extensions: Vec<String> = user_config
            .extensions
            .into_iter()
            .chain(args.extension)
            .unique()
            .map(|e| e.trim_start_matches('.').to_lowercase())
            .collect();

        if extensions.is_empty() {
            extensions = FILE_EXTENSIONS.iter().map(|&s| s.to_string()).collect();
        }

        Ok(Self {
            dryrun: args.print || user_config.dryrun,
            extensions,
            move_files: args.move_files || user_config.move_files,
            patterns,
            recurse: args.recurse || user_config.recurse,
            verbose: args.verbose || user_config.verbose,
        })
    }
}

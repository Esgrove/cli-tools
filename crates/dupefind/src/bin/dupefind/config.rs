//! Configuration for `DupeFind`.
//!
//! Handles reading configuration from CLI arguments and the user config file.

use std::path::PathBuf;

use anyhow::Result;
use itertools::Itertools;
use regex::Regex;
use serde::Deserialize;

use crate::Args;
use cli_tools_dupefind::FILE_EXTENSIONS;

/// Config from the `[dupefind]` section of the user config file.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DupeConfig {
    #[serde(default)]
    pub(crate) default_paths: Vec<PathBuf>,
    #[serde(default)]
    dryrun: bool,
    #[serde(default)]
    extensions: Vec<String>,
    #[serde(default)]
    hash_compare: bool,
    #[serde(default)]
    ignore_matches: Vec<String>,
    #[serde(default)]
    move_files: bool,
    #[serde(default)]
    pub(crate) paths: Vec<PathBuf>,
    #[serde(default)]
    patterns: Vec<String>,
    #[serde(default)]
    prefix_ignores: Vec<String>,
    #[serde(default)]
    recurse: bool,
    #[serde(default)]
    verbose: bool,
}

/// Final config created from CLI arguments and user config file.
#[derive(Debug, Clone)]
pub struct Config {
    pub(crate) debug: bool,
    pub(crate) dryrun: bool,
    pub(crate) extensions: Vec<String>,
    pub(crate) hash_compare: bool,
    pub(crate) hash_only: bool,
    pub(crate) no_cache: bool,
    /// Pattern match texts to ignore (case-insensitive).
    /// Groups whose display name matches any of these strings will be skipped.
    pub(crate) ignore_matches: Vec<String>,
    pub(crate) move_files: bool,
    pub(crate) patterns: Vec<Regex>,
    pub(crate) prefix_ignores: Vec<String>,
    pub(crate) recurse: bool,
    pub(crate) verbose: bool,
}

impl DupeConfig {
    /// Try to read user config from the file if it exists.
    /// Otherwise, fall back to default config.
    ///
    /// # Errors
    /// Returns an error if config file exists but cannot be read or parsed.
    pub(crate) fn get_user_config() -> Result<Self> {
        cli_tools_common::load_user_config_section("dupefind")
    }

    /// Parse configuration from a TOML string.
    ///
    /// # Errors
    /// Returns an error if the TOML string is invalid.
    #[cfg(test)]
    pub fn from_toml_str(toml_str: &str) -> Result<Self> {
        cli_tools_common::parse_user_config_section(toml_str, "dupefind")
    }
}

impl Config {
    /// Create config from given command line args and user config file.
    ///
    /// # Errors
    /// Returns an error if the config file cannot be read or parsed.
    #[cfg(test)]
    pub fn from_args(args: Args) -> anyhow::Result<Self> {
        let user_config = DupeConfig::get_user_config()?;
        Self::from_args_and_user_config(args, user_config)
    }

    /// Combine CLI arguments with an already-loaded user config so `cli-tools.toml` is only read once.
    pub(crate) fn from_args_and_user_config(args: Args, user_config: DupeConfig) -> anyhow::Result<Self> {
        // Collect ignore matches from config (stored lowercase for case-insensitive comparison)
        let ignore_matches: Vec<String> = user_config
            .ignore_matches
            .into_iter()
            .map(|s| s.to_lowercase())
            .collect();

        // Combine patterns from config and CLI
        let pattern_strings: Vec<String> = user_config.patterns.into_iter().chain(args.pattern).unique().collect();

        let prefix_ignores = user_config
            .prefix_ignores
            .into_iter()
            .chain(args.prefix_ignore)
            .map(|prefix| prefix.trim().to_lowercase())
            .filter(|prefix| !prefix.is_empty())
            .unique()
            .collect();

        // Compile regex patterns
        let patterns: Vec<Regex> = pattern_strings
            .iter()
            .map(|p| Regex::new(p))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| anyhow::anyhow!("Invalid regex pattern: {e}"))?;

        // CLI extensions override config extensions when supplied.
        // Otherwise use config or defaults.
        let mut extensions = if !args.extension.is_empty() {
            cli_tools_common::normalize_extensions(args.extension)
        } else if !user_config.extensions.is_empty() {
            cli_tools_common::normalize_extensions(user_config.extensions)
        } else {
            FILE_EXTENSIONS.iter().map(|&s| s.to_string()).collect()
        };
        if extensions.is_empty() {
            extensions = FILE_EXTENSIONS.iter().map(|&s| s.to_string()).collect();
        }

        Ok(Self {
            debug: args.debug,
            dryrun: args.print || user_config.dryrun,
            extensions,
            hash_compare: args.hash_compare || args.hash_only || user_config.hash_compare,
            hash_only: args.hash_only,
            no_cache: args.no_cache,
            ignore_matches,
            move_files: args.move_files || user_config.move_files,
            patterns,
            prefix_ignores,
            recurse: args.recurse || user_config.recurse,
            verbose: args.verbose || user_config.verbose,
        })
    }
}

#[cfg(test)]
mod dupe_config_tests {
    use super::*;

    #[test]
    fn from_toml_str_parses_empty_config() {
        let toml = "";
        let config = DupeConfig::from_toml_str(toml).expect("should parse empty config");
        assert!(!config.dryrun);
        assert!(!config.hash_compare);
        assert!(!config.move_files);
        assert!(!config.recurse);
        assert!(!config.verbose);
        assert_eq!(config.extensions, [] as [String; 0]);
        assert_eq!(config.ignore_matches, [] as [String; 0]);
        assert_eq!(config.patterns, [] as [String; 0]);
        assert_eq!(config.prefix_ignores, [] as [String; 0]);
        assert_eq!(config.paths, [] as [PathBuf; 0]);
        assert_eq!(config.default_paths, [] as [PathBuf; 0]);
    }

    #[test]
    fn from_toml_str_parses_dupefind_section() {
        let toml = r"
[dupefind]
dryrun = true
move_files = true
recurse = true
verbose = true
";
        let config = DupeConfig::from_toml_str(toml).expect("should parse config");
        assert!(config.dryrun);
        assert!(config.move_files);
        assert!(config.recurse);
        assert!(config.verbose);
    }

    #[test]
    fn from_toml_str_parses_extensions() {
        let toml = r#"
[dupefind]
extensions = ["mp4", "mkv", "avi"]
"#;
        let config = DupeConfig::from_toml_str(toml).expect("should parse config");
        assert_eq!(config.extensions, vec!["mp4", "mkv", "avi"]);
    }

    #[test]
    fn from_toml_str_parses_hash_and_prefix_options() {
        let toml = r#"
[dupefind]
hash_compare = true
prefix_ignores = ["prefix", "Other"]
"#;
        let config = DupeConfig::from_toml_str(toml).expect("should parse config");
        assert!(config.hash_compare);
        assert_eq!(config.prefix_ignores, vec!["prefix", "Other"]);
    }

    #[test]
    fn from_toml_str_parses_patterns() {
        let toml = r#"
[dupefind]
patterns = ["pattern1", "pattern2"]
"#;
        let config = DupeConfig::from_toml_str(toml).expect("should parse config");
        assert_eq!(config.patterns, vec!["pattern1", "pattern2"]);
    }

    #[test]
    fn from_toml_str_parses_paths() {
        let toml = r#"
[dupefind]
paths = ["/path/one", "/path/two"]
default_paths = ["/default/path"]
"#;
        let config = DupeConfig::from_toml_str(toml).expect("should parse config");
        assert_eq!(config.paths.len(), 2);
        assert_eq!(config.default_paths.len(), 1);
    }

    #[test]
    fn from_toml_str_invalid_toml_returns_error() {
        let toml = "this is not valid toml {{{";
        let result = DupeConfig::from_toml_str(toml);
        assert!(result.is_err());
    }

    #[test]
    fn from_toml_str_parses_ignore_matches() {
        let toml = r#"
[dupefind]
ignore_matches = ["ABC-123", "Some.Show"]
"#;
        let config = DupeConfig::from_toml_str(toml).expect("should parse config");
        assert_eq!(config.ignore_matches, vec!["ABC-123", "Some.Show"]);
    }

    #[test]
    fn from_toml_str_ignores_other_sections() {
        let toml = r"
[other_section]
some_value = true

[dupefind]
verbose = true
";
        let config = DupeConfig::from_toml_str(toml).expect("should parse config");
        assert!(config.verbose);
        assert!(!config.dryrun);
    }
}

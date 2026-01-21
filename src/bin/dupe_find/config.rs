//! Configuration for `DupeFind`.
//!
//! Handles reading configuration from CLI arguments and the user config file.

use std::fs;
use std::path::PathBuf;

use anyhow::Result;
use itertools::Itertools;
use regex::Regex;
use serde::Deserialize;

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
    /// Otherwise, fall back to default config.
    ///
    /// # Errors
    /// Returns an error if config file exists but cannot be read or parsed.
    pub(crate) fn get_user_config() -> Result<Self> {
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
            .map(|config| config.dupefind)
            .map_err(|e| anyhow::anyhow!("Failed to parse config: {e}"))
    }
}

impl Config {
    /// Create config from given command line args and user config file.
    ///
    /// # Errors
    /// Returns an error if the config file cannot be read or parsed.
    pub fn from_args(args: Args) -> anyhow::Result<Self> {
        let user_config = DupeConfig::get_user_config()?;

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

#[cfg(test)]
mod dupe_config_tests {
    use super::*;

    #[test]
    fn from_toml_str_parses_empty_config() {
        let toml = "";
        let config = DupeConfig::from_toml_str(toml).expect("should parse empty config");
        assert!(!config.dryrun);
        assert!(!config.move_files);
        assert!(!config.recurse);
        assert!(!config.verbose);
        assert!(config.extensions.is_empty());
        assert!(config.patterns.is_empty());
        assert!(config.paths.is_empty());
        assert!(config.default_paths.is_empty());
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

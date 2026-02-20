use std::fs;

use anyhow::Context;
use serde::Deserialize;

use crate::Args;

/// Final config created from CLI arguments and user config file.
#[derive(Debug, Default)]
pub struct Config {
    pub(crate) directory_mode: bool,
    pub(crate) dryrun: bool,
    pub(crate) file_extensions: Vec<String>,
    pub(crate) overwrite: bool,
    pub(crate) recurse: bool,
    pub(crate) swap_year: bool,
    pub(crate) verbose: bool,
    pub(crate) year_first: bool,
}

/// Config from a config file
#[derive(Debug, Default, Deserialize)]
struct DateConfig {
    #[serde(default)]
    directory: bool,
    #[serde(default)]
    dryrun: bool,
    #[serde(default)]
    file_extensions: Vec<String>,
    #[serde(default)]
    overwrite: bool,
    #[serde(default)]
    recurse: bool,
    #[serde(default)]
    swap_year: bool,
    #[serde(default)]
    verbose: bool,
    #[serde(default)]
    year_first: bool,
}

/// Wrapper needed for parsing the config file section.
#[derive(Debug, Default, Deserialize)]
struct UserConfig {
    #[serde(default)]
    flip_date: DateConfig,
}

impl DateConfig {
    /// Try to read user config from the file if it exists.
    /// Otherwise, fall back to default config.
    ///
    /// # Errors
    /// Returns an error if config file exists but cannot be read or parsed.
    fn get_user_config() -> anyhow::Result<Self> {
        let Some(path) = cli_tools::config_path() else {
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

    /// Parse config from a TOML string.
    ///
    /// # Errors
    /// Returns an error if the TOML string is invalid.
    fn from_toml_str(toml_str: &str) -> anyhow::Result<Self> {
        toml::from_str::<UserConfig>(toml_str)
            .map(|config| config.flip_date)
            .context("Failed to parse flip_date config TOML")
    }
}

impl Config {
    /// Create config from given command line args and user config file.
    ///
    /// # Errors
    /// Returns an error if the config file cannot be read or parsed.
    pub fn from_args(args: Args) -> anyhow::Result<Self> {
        let user_config = DateConfig::get_user_config()?;

        // Determine which extensions to use (args > config > default)
        let file_extensions = args
            .extensions
            .filter(|extensions| !extensions.is_empty())
            .or({
                if user_config.file_extensions.is_empty() {
                    None
                } else {
                    Some(user_config.file_extensions)
                }
            })
            .unwrap_or_else(|| {
                crate::flip_date::FILE_EXTENSIONS
                    .iter()
                    .map(std::string::ToString::to_string)
                    .collect()
            });

        Ok(Self {
            directory_mode: args.dir || user_config.directory,
            dryrun: args.print || user_config.dryrun,
            file_extensions,
            overwrite: args.force || user_config.overwrite,
            recurse: args.recurse || user_config.recurse,
            swap_year: args.swap || user_config.swap_year,
            verbose: args.verbose || user_config.verbose,
            year_first: args.year || user_config.year_first,
        })
    }
}

#[cfg(test)]
mod date_config_tests {
    use super::*;

    #[test]
    fn from_toml_str_parses_empty_config() {
        let toml = "";
        let config = DateConfig::from_toml_str(toml).unwrap();
        assert!(!config.directory);
        assert!(!config.dryrun);
        assert!(!config.verbose);
    }

    #[test]
    fn from_toml_str_parses_flip_date_section() {
        let toml = r"
[flip_date]
directory = true
dryrun = true
verbose = true
recurse = true
";
        let config = DateConfig::from_toml_str(toml).unwrap();
        assert!(config.directory);
        assert!(config.dryrun);
        assert!(config.verbose);
        assert!(config.recurse);
    }

    #[test]
    fn from_toml_str_parses_file_extensions() {
        let toml = r#"
[flip_date]
file_extensions = ["mp4", "mkv", "avi"]
"#;
        let config = DateConfig::from_toml_str(toml).unwrap();
        assert_eq!(config.file_extensions, vec!["mp4", "mkv", "avi"]);
    }

    #[test]
    fn from_toml_str_parses_overwrite_and_swap() {
        let toml = r"
[flip_date]
overwrite = true
swap_year = true
year_first = true
";
        let config = DateConfig::from_toml_str(toml).unwrap();
        assert!(config.overwrite);
        assert!(config.swap_year);
        assert!(config.year_first);
    }

    #[test]
    fn from_toml_str_invalid_toml_returns_error() {
        let toml = "this is not valid toml {{{";
        let result = DateConfig::from_toml_str(toml);
        assert!(result.is_err());
    }

    #[test]
    fn from_toml_str_ignores_other_sections() {
        let toml = r"
[other_section]
some_value = true

[flip_date]
verbose = true
";
        let config = DateConfig::from_toml_str(toml).unwrap();
        assert!(config.verbose);
        assert!(!config.directory);
    }

    #[test]
    fn default_values_are_correct() {
        let config = DateConfig::default();
        assert!(!config.directory);
        assert!(!config.dryrun);
        assert!(!config.overwrite);
        assert!(!config.recurse);
        assert!(!config.swap_year);
        assert!(!config.verbose);
        assert!(!config.year_first);
        assert!(config.file_extensions.is_empty());
    }
}

#[cfg(test)]
mod config_from_args_tests {
    use super::*;
    use crate::flip_date::FILE_EXTENSIONS;

    fn default_args() -> Args {
        Args {
            path: None,
            dir: false,
            force: false,
            extensions: None,
            year: false,
            print: false,
            recurse: false,
            swap: false,
            verbose: false,
        }
    }

    #[test]
    fn from_args_uses_default_extensions() {
        let args = default_args();
        let config = Config::from_args(args).expect("config should parse");
        assert_eq!(config.file_extensions.len(), FILE_EXTENSIONS.len());
    }

    #[test]
    fn from_args_cli_overrides_defaults() {
        let mut args = default_args();
        args.dir = true;
        args.force = true;
        args.print = true;
        args.recurse = true;
        args.swap = true;
        args.verbose = true;
        args.year = true;

        let config = Config::from_args(args).expect("config should parse");
        assert!(config.directory_mode);
        assert!(config.overwrite);
        assert!(config.dryrun);
        assert!(config.recurse);
        assert!(config.swap_year);
        assert!(config.verbose);
        assert!(config.year_first);
    }

    #[test]
    fn from_args_uses_cli_extensions() {
        let mut args = default_args();
        args.extensions = Some(vec!["mp4".to_string(), "mkv".to_string()]);

        let config = Config::from_args(args).expect("config should parse");
        assert_eq!(config.file_extensions, vec!["mp4", "mkv"]);
    }
}

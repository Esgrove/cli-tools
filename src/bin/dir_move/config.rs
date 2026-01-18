use std::fs;

use anyhow::Result;
use itertools::Itertools;
use serde::Deserialize;

use cli_tools::print_error;

use crate::DirMoveArgs;

/// Final config combined from CLI arguments and user config file.
#[derive(Debug)]
pub struct Config {
    pub(crate) auto: bool,
    pub(crate) create: bool,
    pub(crate) debug: bool,
    pub(crate) dryrun: bool,
    pub(crate) include: Vec<String>,
    pub(crate) exclude: Vec<String>,
    pub(crate) min_group_size: usize,
    pub(crate) overwrite: bool,
    pub(crate) prefix_ignores: Vec<String>,
    pub(crate) prefix_overrides: Vec<String>,
    pub(crate) recurse: bool,
    pub(crate) verbose: bool,
    pub(crate) unpack_directory_names: Vec<String>,
}

/// Config from the user config file
#[derive(Debug, Default, Deserialize)]
struct DirMoveConfig {
    #[serde(default)]
    auto: bool,
    #[serde(default)]
    create: bool,
    #[serde(default)]
    debug: bool,
    #[serde(default)]
    dryrun: bool,
    #[serde(default)]
    include: Vec<String>,
    #[serde(default)]
    exclude: Vec<String>,
    #[serde(default)]
    min_group_size: Option<usize>,
    #[serde(default)]
    overwrite: bool,
    #[serde(default)]
    prefix_ignores: Vec<String>,
    #[serde(default)]
    prefix_overrides: Vec<String>,
    #[serde(default)]
    recurse: bool,
    #[serde(default)]
    verbose: bool,
    #[serde(default)]
    unpack_directories: Vec<String>,
}

/// Wrapper needed for parsing the user config file section.
#[derive(Debug, Default, Deserialize)]
struct UserConfig {
    #[serde(default)]
    dirmove: DirMoveConfig,
}

impl DirMoveConfig {
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
            .and_then(|config_string| Self::from_toml_str(&config_string).ok())
            .unwrap_or_default()
    }

    /// Parse configuration from a TOML string.
    ///
    /// # Errors
    /// Returns an error if the TOML string is invalid.
    pub fn from_toml_str(toml_str: &str) -> Result<Self> {
        toml::from_str::<UserConfig>(toml_str)
            .map(|config| config.dirmove)
            .map_err(|e| anyhow::anyhow!("Failed to parse config: {e}"))
    }
}

impl Config {
    /// Create config from given command line args and user config file.
    pub fn from_args(args: DirMoveArgs) -> Self {
        let user_config = DirMoveConfig::get_user_config();
        let include: Vec<String> = user_config.include.into_iter().chain(args.include).unique().collect();
        let exclude: Vec<String> = user_config.exclude.into_iter().chain(args.exclude).unique().collect();

        let prefix_ignores: Vec<String> = user_config
            .prefix_ignores
            .into_iter()
            .chain(args.prefix_ignore)
            .unique()
            .collect();

        let prefix_overrides: Vec<String> = user_config
            .prefix_overrides
            .into_iter()
            .chain(args.prefix_override)
            .unique()
            .collect();

        let unpack_directory_names: Vec<String> = user_config
            .unpack_directories
            .into_iter()
            .chain(args.unpack_directory)
            .map(|s| s.to_lowercase())
            .unique()
            .collect();

        Self {
            auto: args.auto || user_config.auto,
            create: args.create || user_config.create,
            debug: args.debug || user_config.debug,
            dryrun: args.print || user_config.dryrun,
            include,
            exclude,
            min_group_size: user_config.min_group_size.unwrap_or(args.group),
            overwrite: args.force || user_config.overwrite,
            prefix_ignores,
            prefix_overrides,
            recurse: args.recurse || user_config.recurse,
            verbose: args.verbose || user_config.verbose,
            unpack_directory_names,
        }
    }
}

#[cfg(test)]
mod dirmove_config_tests {
    use super::*;

    #[test]
    fn from_toml_str_parses_empty_config() {
        let toml = "";
        let config = DirMoveConfig::from_toml_str(toml).expect("should parse empty config");
        assert!(!config.auto);
        assert!(!config.create);
        assert!(!config.debug);
        assert!(!config.dryrun);
        assert!(!config.overwrite);
        assert!(!config.recurse);
        assert!(!config.verbose);
        assert!(config.include.is_empty());
        assert!(config.exclude.is_empty());
    }

    #[test]
    fn from_toml_str_parses_dirmove_section() {
        let toml = r"
[dirmove]
auto = true
create = true
debug = true
dryrun = true
overwrite = true
recurse = true
verbose = true
";
        let config = DirMoveConfig::from_toml_str(toml).expect("should parse config");
        assert!(config.auto);
        assert!(config.create);
        assert!(config.debug);
        assert!(config.dryrun);
        assert!(config.overwrite);
        assert!(config.recurse);
        assert!(config.verbose);
    }

    #[test]
    fn from_toml_str_parses_include_exclude() {
        let toml = r#"
[dirmove]
include = ["*.mp4", "*.mkv"]
exclude = ["*.txt", "*.nfo"]
"#;
        let config = DirMoveConfig::from_toml_str(toml).expect("should parse config");
        assert_eq!(config.include, vec!["*.mp4", "*.mkv"]);
        assert_eq!(config.exclude, vec!["*.txt", "*.nfo"]);
    }

    #[test]
    fn from_toml_str_parses_prefix_options() {
        let toml = r#"
[dirmove]
prefix_ignores = ["the", "a"]
prefix_overrides = ["special"]
"#;
        let config = DirMoveConfig::from_toml_str(toml).expect("should parse config");
        assert_eq!(config.prefix_ignores, vec!["the", "a"]);
        assert_eq!(config.prefix_overrides, vec!["special"]);
    }

    #[test]
    fn from_toml_str_parses_min_group_size() {
        let toml = r"
[dirmove]
min_group_size = 5
";
        let config = DirMoveConfig::from_toml_str(toml).expect("should parse config");
        assert_eq!(config.min_group_size, Some(5));
    }

    #[test]
    fn from_toml_str_parses_unpack_directories() {
        let toml = r#"
[dirmove]
unpack_directories = ["subs", "sample"]
"#;
        let config = DirMoveConfig::from_toml_str(toml).expect("should parse config");
        assert_eq!(config.unpack_directories, vec!["subs", "sample"]);
    }

    #[test]
    fn from_toml_str_invalid_toml_returns_error() {
        let toml = "this is not valid toml {{{";
        let result = DirMoveConfig::from_toml_str(toml);
        assert!(result.is_err());
    }

    #[test]
    fn from_toml_str_ignores_other_sections() {
        let toml = r"
[other_section]
some_value = true

[dirmove]
verbose = true
";
        let config = DirMoveConfig::from_toml_str(toml).expect("should parse config");
        assert!(config.verbose);
        assert!(!config.auto);
    }
}

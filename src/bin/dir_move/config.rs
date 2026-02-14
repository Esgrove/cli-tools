use std::fs;

use anyhow::Result;
use itertools::Itertools;
use serde::Deserialize;

use crate::DirMoveArgs;

/// A custom mapping pair that maps files containing a pattern to a specific directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomMapping {
    /// Pattern to match in filename (case-insensitive, normalized).
    pub(crate) pattern: String,
    /// Target directory name for matching files.
    pub(crate) directory: String,
}

impl CustomMapping {
    /// Create a new custom mapping with normalized pattern (lowercase, no dots/spaces).
    pub(crate) fn new(pattern: &str, directory: &str) -> Self {
        Self {
            pattern: pattern.to_lowercase().replace(['.', ' '], ""),
            directory: directory.to_string(),
        }
    }
}

/// Final config combined from CLI arguments and user config file.
#[derive(Debug, Default)]
pub struct Config {
    pub(crate) auto: bool,
    pub(crate) create: bool,
    pub(crate) custom_mappings: Vec<CustomMapping>,
    pub(crate) debug: bool,
    pub(crate) dryrun: bool,
    pub(crate) include: Vec<String>,
    pub(crate) exclude: Vec<String>,
    pub(crate) ignored_group_names: Vec<String>,
    pub(crate) ignored_group_parts: Vec<String>,
    pub(crate) min_group_size: usize,
    pub(crate) min_prefix_chars: usize,
    pub(crate) overwrite: bool,
    pub(crate) prefix_ignores: Vec<String>,
    pub(crate) prefix_overrides: Vec<String>,
    pub(crate) recurse: bool,
    pub(crate) verbose: bool,
    pub(crate) unpack_directory_names: Vec<String>,
    pub(crate) show_db: bool,
}

/// Config from the user config file
#[derive(Debug, Default, Deserialize)]
struct DirMoveConfig {
    #[serde(default)]
    auto: bool,
    #[serde(default)]
    create: bool,
    #[serde(default)]
    custom_mappings: Vec<[String; 2]>,
    #[serde(default)]
    debug: bool,
    #[serde(default)]
    dryrun: bool,
    #[serde(default)]
    include: Vec<String>,
    #[serde(default)]
    exclude: Vec<String>,
    #[serde(default)]
    ignored_group_names: Vec<String>,
    #[serde(default)]
    ignored_group_parts: Vec<String>,
    #[serde(default)]
    min_group_size: Option<usize>,
    #[serde(default)]
    min_prefix_chars: Option<usize>,
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
            .map(|config| config.dirmove)
            .map_err(|e| anyhow::anyhow!("Failed to parse config: {e}"))
    }
}

impl Config {
    /// Create config from given command line args and user config file.
    ///
    /// # Errors
    /// Returns an error if the config file cannot be read or parsed.
    pub fn from_args(args: DirMoveArgs) -> Result<Self> {
        let user_config = DirMoveConfig::get_user_config()?;
        let include: Vec<String> = user_config
            .include
            .into_iter()
            .chain(args.include)
            .map(|s| s.to_lowercase())
            .unique()
            .collect();
        let exclude: Vec<String> = user_config
            .exclude
            .into_iter()
            .chain(args.exclude)
            .map(|s| s.to_lowercase())
            .unique()
            .collect();

        // Parse CLI mappings from "pattern:dirname" format
        let cli_mappings: Vec<[String; 2]> = args
            .custom_mapping
            .into_iter()
            .filter_map(|s| {
                let parts: Vec<&str> = s.splitn(2, ':').collect();
                if parts.len() == 2 {
                    Some([parts[0].to_string(), parts[1].to_string()])
                } else {
                    eprintln!("Warning: Invalid custom mapping format '{s}', expected 'pattern:dirname'");
                    None
                }
            })
            .collect();

        let custom_mappings: Vec<CustomMapping> = user_config
            .custom_mappings
            .into_iter()
            .chain(cli_mappings)
            .map(|pair| CustomMapping::new(&pair[0], &pair[1]))
            .collect();

        let ignored_group_names: Vec<String> = user_config
            .ignored_group_names
            .into_iter()
            .chain(args.ignored_group_name)
            .map(|s| s.to_lowercase())
            .unique()
            .collect();

        let ignored_group_parts: Vec<String> = user_config
            .ignored_group_parts
            .into_iter()
            .chain(args.ignored_group_part)
            .map(|s| s.to_lowercase())
            .unique()
            .collect();

        let prefix_ignores: Vec<String> = user_config
            .prefix_ignores
            .into_iter()
            .chain(args.prefix_ignore)
            .map(|s| s.to_lowercase())
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

        Ok(Self {
            auto: args.auto || user_config.auto,
            create: args.create || user_config.create,
            custom_mappings,
            debug: args.debug || user_config.debug,
            dryrun: args.print || user_config.dryrun,
            include,
            exclude,
            ignored_group_names,
            ignored_group_parts,
            min_group_size: args.group.or(user_config.min_group_size).unwrap_or(3),
            min_prefix_chars: args.min_prefix_chars.or(user_config.min_prefix_chars).unwrap_or(5),
            overwrite: args.force || user_config.overwrite,
            prefix_ignores,
            prefix_overrides,
            recurse: args.recurse || user_config.recurse,
            verbose: args.verbose || user_config.verbose,
            unpack_directory_names,
            show_db: args.show_db,
        })
    }
}

#[cfg(test)]
impl Config {
    /// Create a test config with specified `min_group_size` and default values.
    pub fn test_with_group_size(min_group_size: usize) -> Self {
        Self {
            min_group_size,
            min_prefix_chars: 1,
            dryrun: true,
            ..Default::default()
        }
    }

    /// Create a test config with prefix ignores (automatically lowercased).
    pub fn test_with_prefix_ignores(prefix_ignores: Vec<&str>) -> Self {
        Self {
            prefix_ignores: prefix_ignores.into_iter().map(str::to_lowercase).collect(),
            min_group_size: 3,
            min_prefix_chars: 1,
            dryrun: true,
            ..Default::default()
        }
    }

    /// Create a test config with prefix overrides and ignores (ignores automatically lowercased).
    pub fn test_with_overrides_and_ignores(prefix_overrides: Vec<&str>, prefix_ignores: Vec<&str>) -> Self {
        Self {
            prefix_overrides: prefix_overrides.into_iter().map(String::from).collect(),
            prefix_ignores: prefix_ignores.into_iter().map(str::to_lowercase).collect(),
            min_group_size: 3,
            min_prefix_chars: 5,
            dryrun: true,
            ..Default::default()
        }
    }

    /// Create a test config for unpack operations.
    pub fn test_unpack(unpack_names: Vec<&str>, recurse: bool, dryrun: bool, overwrite: bool) -> Self {
        Self {
            auto: true,
            dryrun,
            overwrite,
            recurse,
            unpack_directory_names: unpack_names.into_iter().map(str::to_lowercase).collect(),
            min_group_size: 3,
            min_prefix_chars: 5,
            ..Default::default()
        }
    }

    /// Create a test config with ignored group names (automatically lowercased).
    pub fn test_with_ignored_group_names(ignored_group_names: Vec<&str>) -> Self {
        Self {
            ignored_group_names: ignored_group_names.into_iter().map(str::to_lowercase).collect(),
            min_group_size: 3,
            min_prefix_chars: 1,
            dryrun: true,
            create: true,
            ..Default::default()
        }
    }

    /// Create a test config with ignored group parts (automatically lowercased).
    pub fn test_with_ignored_group_parts(ignored_group_parts: Vec<&str>) -> Self {
        Self {
            ignored_group_parts: ignored_group_parts.into_iter().map(str::to_lowercase).collect(),
            min_group_size: 3,
            min_prefix_chars: 1,
            dryrun: true,
            create: true,
            ..Default::default()
        }
    }

    /// Create a test config with specified `min_group_size` and `min_prefix_chars`.
    pub fn test_with_group_size_and_min_chars(min_group_size: usize, min_prefix_chars: usize) -> Self {
        Self {
            min_group_size,
            min_prefix_chars,
            dryrun: true,
            ..Default::default()
        }
    }

    /// Create a test config with prefix ignores and specified `min_group_size` (ignores automatically lowercased).
    pub fn test_with_ignores_and_group_size(prefix_ignores: Vec<&str>, min_group_size: usize) -> Self {
        Self {
            prefix_ignores: prefix_ignores.into_iter().map(str::to_lowercase).collect(),
            min_group_size,
            min_prefix_chars: 1,
            dryrun: true,
            ..Default::default()
        }
    }

    /// Create a test config with both prefix ignores and ignored group names (both automatically lowercased).
    pub fn test_with_prefix_ignores_and_ignored_group_names(
        prefix_ignores: Vec<&str>,
        ignored_group_names: Vec<&str>,
    ) -> Self {
        Self {
            prefix_ignores: prefix_ignores.into_iter().map(str::to_lowercase).collect(),
            ignored_group_names: ignored_group_names.into_iter().map(str::to_lowercase).collect(),
            min_group_size: 3,
            min_prefix_chars: 1,
            dryrun: true,
            create: true,
            ..Default::default()
        }
    }

    /// Create a test config for merge directory operations with prefix ignores.
    pub fn test_merge(prefix_ignores: Vec<&str>, recurse: bool, dryrun: bool, overwrite: bool) -> Self {
        Self {
            auto: true,
            dryrun,
            overwrite,
            recurse,
            prefix_ignores: prefix_ignores.into_iter().map(str::to_lowercase).collect(),
            min_group_size: 3,
            min_prefix_chars: 5,
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod config_tests {
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
        assert!(config.ignored_group_names.is_empty());
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
    fn from_toml_str_parses_ignored_group_names() {
        let toml = r#"
[dirmove]
ignored_group_names = ["Episode", "Video", "Part"]
"#;
        let config = DirMoveConfig::from_toml_str(toml).expect("should parse config");
        assert_eq!(config.ignored_group_names, vec!["Episode", "Video", "Part"]);
    }

    #[test]
    fn from_toml_str_parses_ignored_group_parts() {
        let toml = r#"
[dirmove]
ignored_group_parts = ["x265", "x264", "HEVC", "TEST"]
"#;
        let config = DirMoveConfig::from_toml_str(toml).expect("should parse config");
        assert_eq!(config.ignored_group_parts, vec!["x265", "x264", "HEVC", "TEST"]);
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
    fn from_toml_str_parses_min_prefix_chars() {
        let toml = r"
[dirmove]
min_prefix_chars = 8
";
        let config = DirMoveConfig::from_toml_str(toml).expect("should parse config");
        assert_eq!(config.min_prefix_chars, Some(8));
    }

    #[test]
    fn from_toml_str_default_min_prefix_chars_is_none() {
        let toml = "";
        let config = DirMoveConfig::from_toml_str(toml).expect("should parse empty config");
        assert_eq!(config.min_prefix_chars, None);
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
    fn from_toml_str_parses_custom_mappings() {
        let toml = r#"
[dirmove]
custom_mappings = [["something", "Custom Dir"], ["other", "Another Dir"]]
"#;
        let config = DirMoveConfig::from_toml_str(toml).expect("should parse config");
        assert_eq!(config.custom_mappings.len(), 2);
        assert_eq!(config.custom_mappings[0], ["something", "Custom Dir"]);
        assert_eq!(config.custom_mappings[1], ["other", "Another Dir"]);
    }

    #[test]
    fn from_toml_str_default_custom_mappings_is_empty() {
        let toml = "";
        let config = DirMoveConfig::from_toml_str(toml).expect("should parse empty config");
        assert!(config.custom_mappings.is_empty());
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

#[cfg(test)]
mod custom_mapping_tests {
    use super::*;

    #[test]
    fn new_normalizes_pattern_to_lowercase() {
        let mapping = CustomMapping::new("SomePattern", "Target Dir");
        assert_eq!(mapping.pattern, "somepattern");
    }

    #[test]
    fn new_removes_dots_from_pattern() {
        let mapping = CustomMapping::new("some.pattern", "Target Dir");
        assert_eq!(mapping.pattern, "somepattern");
    }

    #[test]
    fn new_removes_spaces_from_pattern() {
        let mapping = CustomMapping::new("some pattern", "Target Dir");
        assert_eq!(mapping.pattern, "somepattern");
    }

    #[test]
    fn new_preserves_directory_name_case() {
        let mapping = CustomMapping::new("pattern", "Custom Dir Name");
        assert_eq!(mapping.directory, "Custom Dir Name");
    }

    #[test]
    fn new_handles_mixed_separators() {
        let mapping = CustomMapping::new("Some.Pattern Here", "Target");
        assert_eq!(mapping.pattern, "somepatternhere");
    }
}

#[cfg(test)]
mod cli_args_tests {
    use super::*;
    use crate::config::Config;
    use clap::Parser;

    #[test]
    fn parses_multiple_include_patterns() {
        let args =
            DirMoveArgs::try_parse_from(["test", "-n", "*.mp4", "-n", "*.mkv", "-n", "*.avi"]).expect("should parse");
        assert_eq!(args.include, vec!["*.mp4", "*.mkv", "*.avi"]);
    }

    #[test]
    fn parses_multiple_exclude_patterns() {
        let args =
            DirMoveArgs::try_parse_from(["test", "-e", "*.txt", "-e", "*.nfo", "-e", "*.jpg"]).expect("should parse");
        assert_eq!(args.exclude, vec!["*.txt", "*.nfo", "*.jpg"]);
    }

    #[test]
    fn parses_multiple_prefix_ignores() {
        let args = DirMoveArgs::try_parse_from(["test", "-i", "the", "-i", "a", "-i", "an"]).expect("should parse");
        assert_eq!(args.prefix_ignore, vec!["the", "a", "an"]);
    }

    #[test]
    fn parses_multiple_prefix_overrides() {
        let args = DirMoveArgs::try_parse_from(["test", "-o", "special", "-o", "custom"]).expect("should parse");
        assert_eq!(args.prefix_override, vec!["special", "custom"]);
    }

    #[test]
    fn parses_multiple_unpack_directories() {
        let args =
            DirMoveArgs::try_parse_from(["test", "-u", "subs", "-u", "sample", "-u", "screens"]).expect("should parse");
        assert_eq!(args.unpack_directory, vec!["subs", "sample", "screens"]);
    }

    #[test]
    fn parses_group_size() {
        let args = DirMoveArgs::try_parse_from(["test", "-g", "5"]).expect("should parse");
        assert_eq!(args.group, Some(5));
    }

    #[test]
    fn default_group_size_is_none() {
        let args = DirMoveArgs::try_parse_from(["test"]).expect("should parse");
        assert_eq!(args.group, None);
    }

    #[test]
    fn parses_min_prefix_chars() {
        let args = DirMoveArgs::try_parse_from(["test", "-m", "8"]).expect("should parse");
        assert_eq!(args.min_prefix_chars, Some(8));
    }

    #[test]
    fn parses_min_prefix_chars_long_form() {
        let args = DirMoveArgs::try_parse_from(["test", "--min-chars", "10"]).expect("should parse");
        assert_eq!(args.min_prefix_chars, Some(10));
    }

    #[test]
    fn default_min_prefix_chars_is_none() {
        let args = DirMoveArgs::try_parse_from(["test"]).expect("should parse");
        assert_eq!(args.min_prefix_chars, None);
    }

    #[test]
    fn rejects_invalid_min_prefix_chars() {
        let result = DirMoveArgs::try_parse_from(["test", "-m", "not_a_number"]);
        assert!(result.is_err());
    }

    #[test]
    fn config_from_args_uses_cli_min_prefix_chars() {
        let args = DirMoveArgs::try_parse_from(["test", "-m", "8"]).expect("should parse");
        let config = Config::from_args(args).expect("config should parse");
        assert_eq!(config.min_prefix_chars, 8);
    }

    #[test]
    fn config_from_args_default_min_prefix_chars_is_five() {
        let args = DirMoveArgs::try_parse_from(["test"]).expect("should parse");
        let config = Config::from_args(args).expect("config should parse");
        assert_eq!(config.min_prefix_chars, 5);
    }

    #[test]
    fn parses_combined_short_flags() {
        let args = DirMoveArgs::try_parse_from(["test", "-acrv"]).expect("should parse");
        assert!(args.auto);
        assert!(args.create);
        assert!(args.recurse);
        assert!(args.verbose);
    }

    #[test]
    fn parses_long_flags() {
        let args = DirMoveArgs::try_parse_from(["test", "--auto", "--create", "--recurse", "--verbose"])
            .expect("should parse");
        assert!(args.auto);
        assert!(args.create);
        assert!(args.recurse);
        assert!(args.verbose);
    }

    #[test]
    fn parses_path_argument() {
        let args = DirMoveArgs::try_parse_from(["test", "/some/path"]).expect("should parse");
        assert!(args.path.is_some());
        assert_eq!(args.path.unwrap().to_string_lossy(), "/some/path");
    }

    #[test]
    fn rejects_invalid_group_size() {
        let result = DirMoveArgs::try_parse_from(["test", "-g", "not_a_number"]);
        assert!(result.is_err());
    }

    #[test]
    fn parses_multiple_ignored_group_names() {
        let args =
            DirMoveArgs::try_parse_from(["test", "-I", "episode", "-I", "video", "-I", "part"]).expect("should parse");
        assert_eq!(args.ignored_group_name, vec!["episode", "video", "part"]);
    }

    #[test]
    fn parses_multiple_ignored_group_parts() {
        let args =
            DirMoveArgs::try_parse_from(["test", "-P", "x265", "-P", "x264", "-P", "HEVC"]).expect("should parse");
        assert_eq!(args.ignored_group_part, vec!["x265", "x264", "HEVC"]);
    }

    #[test]
    fn parses_ignored_group_parts_long_form() {
        let args = DirMoveArgs::try_parse_from(["test", "--ignore-group-part", "x265", "--ignore-group-part", "TEST"])
            .expect("should parse");
        assert_eq!(args.ignored_group_part, vec!["x265", "TEST"]);
    }

    #[test]
    fn config_from_args_ignored_group_parts_lowercase() {
        let args = DirMoveArgs::try_parse_from(["test", "-P", "X265", "-P", "Hevc"]).expect("should parse");
        let config = Config::from_args(args).expect("config should parse");
        // CLI ignored group parts should be stored as lowercase
        assert!(config.ignored_group_parts.contains(&"x265".to_string()));
        assert!(config.ignored_group_parts.contains(&"hevc".to_string()));
    }

    #[test]
    fn parses_ignored_group_names_long_form() {
        let args = DirMoveArgs::try_parse_from(["test", "--ignore-group", "chapter", "--ignore-group", "scene"])
            .expect("should parse");
        assert_eq!(args.ignored_group_name, vec!["chapter", "scene"]);
    }

    #[test]
    fn config_from_args_ignored_group_names_lowercase() {
        let args = DirMoveArgs::try_parse_from(["test", "-I", "EPISODE", "-I", "Video"]).expect("should parse");
        let config = Config::from_args(args).expect("config should parse");
        // CLI ignored group names should be stored as lowercase
        assert!(config.ignored_group_names.contains(&"episode".to_string()));
        assert!(config.ignored_group_names.contains(&"video".to_string()));
    }

    #[test]
    fn empty_arrays_by_default() {
        let args = DirMoveArgs::try_parse_from(["test"]).expect("should parse");
        assert!(args.include.is_empty());
        assert!(args.exclude.is_empty());
        assert!(args.prefix_ignore.is_empty());
        assert!(args.prefix_override.is_empty());
        assert!(args.ignored_group_name.is_empty());
        assert!(args.unpack_directory.is_empty());
    }

    #[test]
    fn config_from_args_includes_cli_patterns() {
        let args = DirMoveArgs::try_parse_from(["test", "-n", "*.mp4", "-n", "*.mkv"]).expect("should parse");
        let config = Config::from_args(args).expect("config should parse");
        // CLI patterns should be included (may also have user config patterns)
        assert!(config.include.contains(&"*.mp4".to_string()));
        assert!(config.include.contains(&"*.mkv".to_string()));
    }

    #[test]
    fn config_from_args_cli_flags_enable_options() {
        // CLI boolean flags should enable options (OR with user config)
        let args = DirMoveArgs::try_parse_from(["test", "-a", "-c", "-r", "-v"]).expect("should parse");
        let config = Config::from_args(args).expect("config should parse");
        assert!(config.auto);
        assert!(config.create);
        assert!(config.recurse);
        assert!(config.verbose);
    }

    #[test]
    fn config_from_args_includes_unpack_dirs_lowercase() {
        let args = DirMoveArgs::try_parse_from(["test", "-u", "SUBS", "-u", "Sample"]).expect("should parse");
        let config = Config::from_args(args).expect("config should parse");
        // CLI unpack dirs should be included as lowercase
        assert!(config.unpack_directory_names.contains(&"subs".to_string()));
        assert!(config.unpack_directory_names.contains(&"sample".to_string()));
    }

    #[test]
    fn config_from_args_print_enables_dryrun() {
        let args = DirMoveArgs::try_parse_from(["test", "-p"]).expect("should parse");
        let config = Config::from_args(args).expect("config should parse");
        assert!(config.dryrun);
    }

    #[test]
    fn config_from_args_force_enables_overwrite() {
        let args = DirMoveArgs::try_parse_from(["test", "-f"]).expect("should parse");
        let config = Config::from_args(args).expect("config should parse");
        assert!(config.overwrite);
    }

    #[test]
    fn parses_custom_mapping_cli_arg() {
        let args = DirMoveArgs::try_parse_from(["test", "-M", "pattern:dirname"]).expect("should parse");
        assert_eq!(args.custom_mapping, vec!["pattern:dirname"]);
    }

    #[test]
    fn parses_multiple_custom_mappings() {
        let args =
            DirMoveArgs::try_parse_from(["test", "-M", "one:Dir One", "-M", "two:Dir Two"]).expect("should parse");
        assert_eq!(args.custom_mapping, vec!["one:Dir One", "two:Dir Two"]);
    }

    #[test]
    fn parses_custom_mapping_long_form() {
        let args = DirMoveArgs::try_parse_from(["test", "--map", "pattern:dirname"]).expect("should parse");
        assert_eq!(args.custom_mapping, vec!["pattern:dirname"]);
    }

    #[test]
    fn config_from_args_parses_custom_mappings() {
        let args = DirMoveArgs::try_parse_from(["test", "-M", "something:Custom Dir"]).expect("should parse");
        let config = Config::from_args(args).expect("config should parse");
        assert_eq!(config.custom_mappings.len(), 1);
        assert_eq!(config.custom_mappings[0].pattern, "something");
        assert_eq!(config.custom_mappings[0].directory, "Custom Dir");
    }

    #[test]
    fn config_from_args_normalizes_custom_mapping_pattern() {
        let args = DirMoveArgs::try_parse_from(["test", "-M", "Some.Pattern:Target"]).expect("should parse");
        let config = Config::from_args(args).expect("config should parse");
        assert_eq!(config.custom_mappings[0].pattern, "somepattern");
    }

    #[test]
    fn config_from_args_warns_on_invalid_mapping_format() {
        let args = DirMoveArgs::try_parse_from(["test", "-M", "invalid_no_colon"]).expect("should parse");
        let config = Config::from_args(args).expect("config should parse");
        // Invalid format should be skipped
        assert!(config.custom_mappings.is_empty());
    }
}

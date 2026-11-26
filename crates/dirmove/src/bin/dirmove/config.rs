//! Configuration parsing for the `dirmove` binary.
//!
//! Reads the user config file, combines it with the command line arguments into `Config`,
//! and adds group names to the ignored names in the user config file.

use std::fs;

use anyhow::Result;
use serde::Deserialize;
use toml_edit::DocumentMut;

use cli_tools_common::print_yellow;

use cli_tools_dirmove::grouping::{DEFAULT_MIN_GROUP_SIZE, DEFAULT_MIN_PREFIX_CHARS};
use cli_tools_dirmove::{CustomMapping, GroupingOptions, NameFilter, ScanOptions, utils};

use crate::DirMoveArgs;

/// Options for a `dirmove` run, combined from CLI arguments and the user config file.
///
/// The run flags control prompting, printing, and whether anything is moved,
/// and the library option types hold the matching and grouping rules.
#[derive(Debug, Clone, Default)]
#[allow(clippy::struct_excessive_bools)]
pub struct Config {
    /// Confirm every prompt automatically.
    pub auto: bool,
    /// Create directories for files that share a name prefix.
    pub create: bool,
    /// Patterns that route matching files to a fixed directory.
    pub custom_mappings: Vec<CustomMapping>,
    /// Print debug information.
    pub debug: bool,
    /// Only print what would be done.
    pub dryrun: bool,
    /// Skip whole directory merge offers and only move files.
    pub files_only: bool,
    /// Overwrite existing files and directories.
    pub overwrite: bool,
    /// Print verbose output.
    pub verbose: bool,
    /// Directory names whose contents are moved up into the parent directory, matched case-insensitively.
    pub unpack_directory_names: Vec<String>,
    /// Ignored prefix and ignored group name rules.
    pub name_filter: NameFilter,
    /// Include and exclude patterns and the recursion setting.
    pub scan: ScanOptions,
    /// Group size, prefix length, ignored part, and prefix override rules.
    pub grouping: GroupingOptions,
}

/// Config from the `[dirmove]` section of the user config file.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
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

impl DirMoveConfig {
    /// Parse configuration from a TOML string.
    ///
    /// # Errors
    /// Returns an error if the TOML string is invalid.
    #[cfg(test)]
    pub fn from_toml_str(toml_str: &str) -> Result<Self> {
        cli_tools_common::parse_user_config_section(toml_str, "dirmove")
    }

    /// Try to read user config from the file if it exists.
    /// Otherwise, fall back to default config.
    ///
    /// # Errors
    /// Returns an error if config file exists but cannot be read or parsed.
    fn get_user_config() -> Result<Self> {
        cli_tools_common::load_user_config_section("dirmove")
    }

    /// Create the final config by combining command line args with this user config.
    ///
    /// List options from the user config come first, followed by the ones from the command line.
    fn into_config(self, args: DirMoveArgs) -> Config {
        let user_config = self;

        // Command line mappings use the "pattern:dirname" format.
        let cli_mappings = args.custom_mapping.into_iter().filter_map(|mapping| {
            let Some((pattern, directory)) = mapping.split_once(':') else {
                print_yellow!("Invalid custom mapping format '{mapping}', expected 'pattern:dirname'");
                return None;
            };
            Some([pattern.to_string(), directory.to_string()])
        });

        let custom_mappings: Vec<CustomMapping> = user_config
            .custom_mappings
            .into_iter()
            .chain(cli_mappings)
            .filter_map(|pair| {
                let mapping = CustomMapping::new(&pair[0], &pair[1]);
                if mapping.is_none() {
                    print_yellow!(
                        "Ignoring custom mapping to '{}' with a pattern that is empty after normalization",
                        pair[1]
                    );
                }
                mapping
            })
            .collect();

        let recurse = args.recurse || user_config.recurse;
        let unpack_directory_names = user_config
            .unpack_directories
            .into_iter()
            .chain(args.unpack_directory)
            .collect();

        Config {
            auto: args.auto || user_config.auto,
            create: args.create || user_config.create,
            custom_mappings,
            debug: args.debug || user_config.debug,
            dryrun: args.print || user_config.dryrun,
            files_only: args.files_only,
            overwrite: args.force || user_config.overwrite,
            verbose: args.verbose || user_config.verbose,
            unpack_directory_names,
            name_filter: NameFilter::new(
                user_config.prefix_ignores.into_iter().chain(args.prefix_ignore),
                user_config
                    .ignored_group_names
                    .into_iter()
                    .chain(args.ignored_group_name),
            ),
            scan: ScanOptions::new(
                user_config.include.into_iter().chain(args.include),
                user_config.exclude.into_iter().chain(args.exclude),
                recurse,
            ),
            grouping: GroupingOptions::new(
                args.group
                    .or(user_config.min_group_size)
                    .unwrap_or(DEFAULT_MIN_GROUP_SIZE),
                args.min_prefix_chars
                    .or(user_config.min_prefix_chars)
                    .unwrap_or(DEFAULT_MIN_PREFIX_CHARS),
                user_config
                    .ignored_group_parts
                    .into_iter()
                    .chain(args.ignored_group_part),
                user_config.prefix_overrides.into_iter().chain(args.prefix_override),
            ),
        }
    }
}

/// Create config from given command line args and user config file.
///
/// # Errors
/// Returns an error if the config file cannot be read or parsed.
pub fn config_from_args(args: DirMoveArgs) -> Result<Config> {
    Ok(DirMoveConfig::get_user_config()?.into_config(args))
}

/// Add a group name to `ignored_group_names` in the user config file.
/// The name is stored as-is (not normalized) so the config stays human-readable.
/// Returns `Ok(true)` if the name was added, `Ok(false)` if it was already present.
pub fn add_ignored_group_name_to_config(name: &str) -> Result<bool> {
    let Some(config_path) = cli_tools_common::config_path() else {
        anyhow::bail!("Could not determine config file path");
    };
    if config_path
        .file_name()
        .is_some_and(|file_name| file_name == "sample_config.toml")
    {
        anyhow::bail!("Refusing to modify repository test fixture {}", config_path.display());
    }
    add_ignored_group_name_to_config_at(&config_path, name)
}

/// Add a group name to `ignored_group_names` in the config file at `config_path`,
/// creating parent directories if needed.
fn add_ignored_group_name_to_config_at(config_path: &std::path::Path, name: &str) -> Result<bool> {
    let content = match fs::read_to_string(config_path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => {
            return Err(anyhow::anyhow!(
                "Failed to read config file {}: {error}",
                config_path.display()
            ));
        }
    };

    let (new_content, added) = add_ignored_name_to_toml(&content, name)?;

    if added {
        if let Some(parent) = config_path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)
                .map_err(|error| anyhow::anyhow!("Failed to create config directory {}: {error}", parent.display()))?;
        }
        cli_tools_common::write_atomically(config_path, new_content)
            .map_err(|error| anyhow::anyhow!("Failed to write config file {}: {error}", config_path.display()))?;
    }

    Ok(added)
}

/// Add a group name to `ignored_group_names` in a TOML document string.
/// The name is stored as-is (not normalized) so the config stays human-readable.
/// Inserts the name in case-insensitive alphabetical order and formats the array with one entry per line.
/// Returns the modified document string and whether the name was added.
fn add_ignored_name_to_toml(content: &str, name: &str) -> Result<(String, bool)> {
    let mut document = content
        .parse::<DocumentMut>()
        .map_err(|error| anyhow::anyhow!("Failed to parse config file as TOML: {error}"))?;

    // Find or create the [dirmove] section
    let dirmove_table = document
        .entry("dirmove")
        .or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()))
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("'dirmove' is not a table in config"))?;

    // Find or create the ignored_group_names array
    if !dirmove_table.contains_key("ignored_group_names") {
        dirmove_table["ignored_group_names"] = toml_edit::Item::Value(toml_edit::Value::Array(toml_edit::Array::new()));
    }
    let array = dirmove_table["ignored_group_names"]
        .as_array_mut()
        .ok_or_else(|| anyhow::anyhow!("'ignored_group_names' is not an array in config"))?;

    // Check if already present by comparing normalized forms
    let normalized_new = utils::normalize_name(name);
    let already_present = array.iter().any(|item| {
        item.as_str()
            .is_some_and(|existing| utils::normalize_name(existing) == normalized_new)
    });

    if already_present {
        return Ok((document.to_string(), false));
    }

    // Find the alphabetically correct insertion position (case-insensitive)
    let name_lower = name.to_lowercase();
    let insert_index = array
        .iter()
        .position(|item| {
            item.as_str()
                .is_some_and(|existing| existing.to_lowercase() > name_lower)
        })
        .unwrap_or(array.len());

    // For multiline arrays, use insert_formatted with matching decor so the new entry
    // gets the same indentation as existing entries. For inline arrays (or empty arrays),
    // use plain insert which applies default inline formatting.
    let multiline_decor = array.get(0).and_then(|item| {
        let decor = item.decor();
        let prefix = decor.prefix()?.as_str()?;
        prefix.contains('\n').then(|| decor.clone())
    });

    if let Some(decor) = multiline_decor {
        let mut value = toml_edit::Value::from(name);
        *value.decor_mut() = decor;
        array.insert_formatted(insert_index, value);
    } else {
        array.insert(insert_index, name);
    }

    Ok((document.to_string(), true))
}

/// Create config from command line args only, ignoring the user config file.
#[cfg(test)]
fn config_from_args_default_config(args: DirMoveArgs) -> Config {
    DirMoveConfig::default().into_config(args)
}

#[cfg(test)]
mod test_add_ignored_name_to_toml {
    use super::*;

    #[test]
    fn adds_to_empty_config() {
        let (result, added) = add_ignored_name_to_toml("", "Season One").expect("should succeed");
        assert!(added);
        let doc = DirMoveConfig::from_toml_str(&result).expect("should parse");
        assert_eq!(doc.ignored_group_names, vec!["Season One"]);
        assert!(result.contains("\"Season One\""), "Should contain the name: {result}");
    }

    #[test]
    fn adds_to_existing_array() {
        let toml = r#"
[dirmove]
ignored_group_names = ["Episode"]
"#;
        let (result, added) = add_ignored_name_to_toml(toml, "Season One").expect("should succeed");
        assert!(added);
        let doc = DirMoveConfig::from_toml_str(&result).expect("should parse");
        assert_eq!(doc.ignored_group_names, vec!["Episode", "Season One"]);
    }

    #[test]
    fn skips_duplicate_exact_match() {
        let toml = r#"
[dirmove]
ignored_group_names = ["Season One"]
"#;
        let (_, added) = add_ignored_name_to_toml(toml, "Season One").expect("should succeed");
        assert!(!added);
    }

    #[test]
    fn skips_duplicate_normalized_match() {
        let toml = r#"
[dirmove]
ignored_group_names = ["SeasonOne"]
"#;
        let (_, added) = add_ignored_name_to_toml(toml, "Season One").expect("should succeed");
        assert!(!added);
    }

    #[test]
    fn skips_duplicate_dotted_match() {
        let toml = r#"
[dirmove]
ignored_group_names = ["Season.One"]
"#;
        let (_, added) = add_ignored_name_to_toml(toml, "Season One").expect("should succeed");
        assert!(!added);
    }

    #[test]
    fn preserves_existing_config() {
        let toml = r#"[dirmove]
auto = true
ignored_group_names = ["Episode"]
min_group_size = 5
"#;
        let (result, added) = add_ignored_name_to_toml(toml, "Chapter").expect("should succeed");
        assert!(added);
        let doc = DirMoveConfig::from_toml_str(&result).expect("should parse");
        assert!(doc.auto);
        assert_eq!(doc.min_group_size, Some(5));
        // "Chapter" sorts before "Episode"
        assert_eq!(doc.ignored_group_names, vec!["Chapter", "Episode"]);
    }

    #[test]
    fn creates_dirmove_section_if_missing() {
        let toml = r"[dots]
debug = false
";
        let (result, added) = add_ignored_name_to_toml(toml, "Video").expect("should succeed");
        assert!(added);
        let doc = DirMoveConfig::from_toml_str(&result).expect("should parse");
        assert_eq!(doc.ignored_group_names, vec!["Video"]);
    }

    #[test]
    fn adds_multiple_names_sequentially_in_alphabetical_order() {
        let (result, added) = add_ignored_name_to_toml("", "Third").expect("should succeed");
        assert!(added);
        let (result, added) = add_ignored_name_to_toml(&result, "First").expect("should succeed");
        assert!(added);
        let (result, added) = add_ignored_name_to_toml(&result, "Second").expect("should succeed");
        assert!(added);
        let doc = DirMoveConfig::from_toml_str(&result).expect("should parse");
        assert_eq!(doc.ignored_group_names, vec!["First", "Second", "Third"]);
    }

    #[test]
    fn case_insensitive_duplicate_detection() {
        let toml = r#"
[dirmove]
ignored_group_names = ["Episode"]
"#;
        let (_, added) = add_ignored_name_to_toml(toml, "EPISODE").expect("should succeed");
        assert!(!added);
    }

    #[test]
    fn inserts_alphabetically_between_existing() {
        let toml = r#"
[dirmove]
ignored_group_names = ["Alpha", "Gamma"]
"#;
        let (result, added) = add_ignored_name_to_toml(toml, "Beta").expect("should succeed");
        assert!(added);
        let doc = DirMoveConfig::from_toml_str(&result).expect("should parse");
        assert_eq!(doc.ignored_group_names, vec!["Alpha", "Beta", "Gamma"]);
    }

    #[test]
    fn inserts_alphabetically_case_insensitive() {
        let toml = r#"
[dirmove]
ignored_group_names = ["alpha", "Gamma"]
"#;
        let (result, added) = add_ignored_name_to_toml(toml, "BETA").expect("should succeed");
        assert!(added);
        let doc = DirMoveConfig::from_toml_str(&result).expect("should parse");
        assert_eq!(doc.ignored_group_names, vec!["alpha", "BETA", "Gamma"]);
    }

    #[test]
    fn inline_array_stays_inline() {
        let toml = r#"[dirmove]
auto = true
ignored_group_names = ["Episode"]
min_group_size = 5
"#;
        let (result, _) = add_ignored_name_to_toml(toml, "Video").expect("should succeed");
        assert!(
            result.contains(r#"ignored_group_names = ["Episode", "Video"]"#),
            "Inline array should stay inline: {result}"
        );
        // Other config values should be preserved
        assert!(result.contains("auto = true"), "Should preserve other config: {result}");
        assert!(
            result.contains("min_group_size = 5"),
            "Should preserve other config: {result}"
        );
    }

    #[test]
    fn multiline_array_stays_multiline() {
        let toml = r#"[dirmove]
ignored_group_names = [
    "Episode",
    "Video",
]
"#;
        let (result, _) = add_ignored_name_to_toml(toml, "Alpha").expect("should succeed");
        // Each entry should be on its own indented line
        assert!(
            result.contains("\n    \"Alpha\","),
            "Should be indented on own line: {result}"
        );
        assert!(
            result.contains("\n    \"Episode\","),
            "Should be indented on own line: {result}"
        );
        assert!(
            result.contains("\n    \"Video\","),
            "Should be indented on own line: {result}"
        );
    }

    #[test]
    fn multiline_array_multiple_sequential_inserts() {
        let toml = r#"[dirmove]
ignored_group_names = [
    "Delta",
    "Golf",
]
"#;
        let (result, _) = add_ignored_name_to_toml(toml, "Alpha").expect("should succeed");
        let (result, _) = add_ignored_name_to_toml(&result, "Echo").expect("should succeed");
        let (result, _) = add_ignored_name_to_toml(&result, "Zulu").expect("should succeed");
        let doc = DirMoveConfig::from_toml_str(&result).expect("should parse");
        assert_eq!(doc.ignored_group_names, vec!["Alpha", "Delta", "Echo", "Golf", "Zulu"]);
        // All entries should be indented on their own lines
        for name in &["Alpha", "Delta", "Echo", "Golf", "Zulu"] {
            assert!(
                result.contains(&format!("\n    \"{name}\",")),
                "'{name}' should be indented on own line: {result}"
            );
        }
    }

    #[test]
    fn preserves_8_space_indentation() {
        let toml = "[dirmove]\nignored_group_names = [\n        \"Delta\",\n        \"Golf\",\n]\n";
        let (result, added) = add_ignored_name_to_toml(toml, "Alpha").expect("should succeed");
        assert!(added);
        let doc = DirMoveConfig::from_toml_str(&result).expect("should parse");
        assert_eq!(doc.ignored_group_names, vec!["Alpha", "Delta", "Golf"]);
        for name in &["Alpha", "Delta", "Golf"] {
            assert!(
                result.contains(&format!("\n        \"{name}\",")),
                "'{name}' should have 8-space indentation: {result}"
            );
        }
    }
}

#[cfg(test)]
mod test_user_config {
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
        assert_eq!(config.include, [] as [String; 0]);
        assert_eq!(config.exclude, [] as [String; 0]);
        assert_eq!(config.ignored_group_names, [] as [String; 0]);
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
        assert_eq!(config.custom_mappings, [] as [[String; 2]; 0]);
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
mod test_cli_args {
    use super::*;
    use clap::Parser;

    #[test]
    fn a_mapping_pattern_that_normalizes_to_nothing_is_dropped() {
        let args = DirMoveArgs::try_parse_from(["test", "-M", ". :Everything", "-M", "foo:Foo"]).expect("should parse");

        let config = config_from_args_default_config(args);

        assert_eq!(config.custom_mappings.len(), 1);
        assert_eq!(config.custom_mappings[0].pattern(), "foo");
    }

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
        let config = config_from_args(args).expect("config should parse");
        assert_eq!(config.grouping.min_prefix_chars(), 8);
    }

    #[test]
    fn config_from_args_default_min_prefix_chars_is_five() {
        let args = DirMoveArgs::try_parse_from(["test"]).expect("should parse");
        let config = config_from_args(args).expect("config should parse");
        assert_eq!(config.grouping.min_prefix_chars(), 5);
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
        assert_eq!(args.path.len(), 1);
        assert_eq!(args.path[0].to_string_lossy(), "/some/path");
    }

    #[test]
    fn parses_multiple_path_arguments() {
        let args = DirMoveArgs::try_parse_from(["test", "/some/path", "/other/path"]).expect("should parse");
        assert_eq!(args.path.len(), 2);
        assert_eq!(args.path[0].to_string_lossy(), "/some/path");
        assert_eq!(args.path[1].to_string_lossy(), "/other/path");
    }

    #[test]
    fn parses_output_argument() {
        let args = DirMoveArgs::try_parse_from(["test", "--output", "/output/path"]).expect("should parse");
        assert_eq!(args.output.len(), 1);
        assert_eq!(args.output[0].to_string_lossy(), "/output/path");
    }

    #[test]
    fn parses_output_short_argument() {
        let args = DirMoveArgs::try_parse_from(["test", "-O", "/output/path"]).expect("should parse");
        assert_eq!(args.output.len(), 1);
        assert_eq!(args.output[0].to_string_lossy(), "/output/path");
    }

    #[test]
    fn parses_multiple_output_arguments() {
        let args =
            DirMoveArgs::try_parse_from(["test", "-O", "/output/path", "-O", "/other/output"]).expect("should parse");
        assert_eq!(args.output.len(), 2);
        assert_eq!(args.output[0].to_string_lossy(), "/output/path");
        assert_eq!(args.output[1].to_string_lossy(), "/other/output");
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
        let config = config_from_args(args).expect("config should parse");
        // CLI ignored group parts should be stored as lowercase
        assert!(config.grouping.has_ignored_group_part(std::iter::once("x265")));
        assert!(config.grouping.has_ignored_group_part(std::iter::once("hevc")));
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
        let config = config_from_args(args).expect("config should parse");
        // CLI ignored group names should be stored as lowercase
        assert!(config.name_filter.matches_ignored_group_name("episode"));
        assert!(config.name_filter.matches_ignored_group_name("video"));
    }

    #[test]
    fn config_from_args_ignored_group_names_normalized() {
        let args = DirMoveArgs::try_parse_from(["test", "-I", "Season One", "-I", "Big.Show", "-I", "SeasonTwo"])
            .expect("should parse");
        let config = config_from_args(args).expect("config should parse");
        // Spaces, dots, and case should all be normalized via normalize_name
        assert!(
            config.name_filter.matches_ignored_group_name("seasonone"),
            "Space-separated name should be normalized to 'seasonone'"
        );
        assert!(
            config.name_filter.matches_ignored_group_name("bigshow"),
            "Dot-separated name should be normalized to 'bigshow'"
        );
        assert!(
            config.name_filter.matches_ignored_group_name("seasontwo"),
            "Concatenated name should be normalized to 'seasontwo'"
        );
    }

    #[test]
    fn empty_arrays_by_default() {
        let args = DirMoveArgs::try_parse_from(["test"]).expect("should parse");
        assert_eq!(args.include, [] as [String; 0]);
        assert_eq!(args.exclude, [] as [String; 0]);
        assert_eq!(args.prefix_ignore, [] as [String; 0]);
        assert_eq!(args.prefix_override, [] as [String; 0]);
        assert_eq!(args.ignored_group_name, [] as [String; 0]);
        assert_eq!(args.unpack_directory, [] as [String; 0]);
    }

    #[test]
    fn config_from_args_includes_cli_patterns() {
        let args = DirMoveArgs::try_parse_from(["test", "-n", "*.mp4", "-n", "*.mkv"]).expect("should parse");
        let config = config_from_args(args).expect("config should parse");
        // CLI patterns should be included (may also have fixture config patterns)
        assert!(config.scan.include().contains(&"*.mp4".to_string()));
        assert!(config.scan.include().contains(&"*.mkv".to_string()));
    }

    #[test]
    fn config_from_args_cli_flags_enable_options() {
        // CLI boolean flags should enable options (OR with fixture config)
        let args = DirMoveArgs::try_parse_from(["test", "-a", "-c", "-r", "-v"]).expect("should parse");
        let config = config_from_args(args).expect("config should parse");
        assert!(config.auto);
        assert!(config.create);
        assert!(config.scan.recurse());
        assert!(config.verbose);
    }

    #[test]
    fn config_from_args_includes_unpack_dirs() {
        let args = DirMoveArgs::try_parse_from(["test", "-u", "SUBS", "-u", "Sample"]).expect("should parse");
        let config = config_from_args(args).expect("config should parse");
        // Case is kept here, the unpacker matches names case-insensitively.
        assert!(config.unpack_directory_names.contains(&"SUBS".to_string()));
        assert!(config.unpack_directory_names.contains(&"Sample".to_string()));
    }

    #[test]
    fn config_from_args_print_enables_dryrun() {
        let args = DirMoveArgs::try_parse_from(["test", "-p"]).expect("should parse");
        let config = config_from_args(args).expect("config should parse");
        assert!(config.dryrun);
    }

    #[test]
    fn config_from_args_force_enables_overwrite() {
        let args = DirMoveArgs::try_parse_from(["test", "-f"]).expect("should parse");
        let config = config_from_args(args).expect("config should parse");
        assert!(config.overwrite);
    }

    #[test]
    fn config_from_args_files_only_enables_option() {
        let args = DirMoveArgs::try_parse_from(["test", "-F"]).expect("should parse");
        let config = config_from_args(args).expect("config should parse");
        assert!(config.files_only);
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
        let config = config_from_args_default_config(args);
        assert_eq!(config.custom_mappings.len(), 1);
        assert_eq!(config.custom_mappings[0].pattern(), "something");
        assert_eq!(config.custom_mappings[0].directory(), "Custom Dir");
    }

    #[test]
    fn config_from_args_normalizes_custom_mapping_pattern() {
        let args = DirMoveArgs::try_parse_from(["test", "-M", "Some.Pattern:Target"]).expect("should parse");
        let config = config_from_args_default_config(args);
        assert_eq!(config.custom_mappings[0].pattern(), "somepattern");
    }

    #[test]
    fn config_from_args_warns_on_invalid_mapping_format() {
        let args = DirMoveArgs::try_parse_from(["test", "-M", "invalid_no_colon"]).expect("should parse");
        let config = config_from_args_default_config(args);
        // Invalid format should be skipped
        assert_eq!(config.custom_mappings, [] as [CustomMapping; 0]);
    }

    #[test]
    fn add_ignored_group_name_creates_missing_parent_directory() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let config_path = temp_dir.path().join("missing_config_dir").join("cli-tools.toml");

        let added = add_ignored_group_name_to_config_at(&config_path, "Episode").expect("create and write config");
        assert!(added);
        let written = fs::read_to_string(&config_path).expect("read config");
        assert!(written.contains("Episode"), "{written}");
    }
}

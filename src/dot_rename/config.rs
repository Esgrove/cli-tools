//! Configuration for dot rename operations.

use std::{fmt, fs};

use anyhow::Context;
use itertools::Itertools;
use regex::Regex;
use serde::Deserialize;

/// Config from the user config file.
#[derive(Debug, Default, Deserialize)]
pub struct DotsConfig {
    #[serde(default = "default_true")]
    pub date_starts_with_year: bool,
    #[serde(default)]
    pub debug: bool,
    #[serde(default)]
    pub directory: bool,
    #[serde(default)]
    pub dryrun: bool,
    #[serde(default)]
    pub increment: bool,
    #[serde(default)]
    pub include: Vec<String>,
    #[serde(default)]
    pub move_date_after_prefix: Vec<String>,
    #[serde(default)]
    pub move_to_end: Vec<String>,
    #[serde(default)]
    pub move_to_start: Vec<String>,
    #[serde(default)]
    pub overwrite: bool,
    #[serde(default)]
    pub prefix_dir: bool,
    #[serde(default)]
    pub prefix_dir_recursive: bool,
    #[serde(default)]
    pub prefix_dir_start: bool,
    #[serde(default)]
    pub suffix_dir: bool,
    #[serde(default)]
    pub suffix_dir_recursive: bool,
    #[serde(default)]
    pub pre_replace: Vec<(String, String)>,
    #[serde(default)]
    pub recurse: bool,
    #[serde(default)]
    pub regex_replace: Vec<(String, String)>,
    #[serde(default)]
    pub remove_random: bool,
    #[serde(default)]
    pub replace: Vec<(String, String)>,
    #[serde(default)]
    pub remove_from_start: Vec<String>,
    #[serde(default)]
    pub verbose: bool,
}

/// Wrapper needed for parsing the config section.
#[derive(Debug, Default, Deserialize)]
struct UserConfig {
    #[serde(default)]
    dots: DotsConfig,
}

/// Final config created from CLI arguments and user config file.
#[derive(Debug, Default)]
pub struct DotRenameConfig {
    pub convert_case: bool,
    pub date_starts_with_year: bool,
    pub debug: bool,
    pub deduplicate_patterns: Vec<(Regex, String)>,
    pub dryrun: bool,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub increment_name: bool,
    pub move_date_after_prefix: Vec<String>,
    pub move_to_end: Vec<String>,
    pub move_to_start: Vec<String>,
    pub overwrite: bool,
    pub pre_replace: Vec<(String, String)>,
    pub prefix: Option<String>,
    pub prefix_dir: bool,
    pub prefix_dir_recursive: bool,
    pub prefix_dir_start: bool,
    pub recurse: bool,
    pub regex_replace: Vec<(Regex, String)>,
    pub regex_replace_after: Vec<(Regex, String)>,
    pub remove_from_start: Vec<String>,
    pub remove_random: bool,
    pub rename_directories: bool,
    pub replace: Vec<(String, String)>,
    pub suffix: Option<String>,
    pub suffix_dir: bool,
    pub suffix_dir_recursive: bool,
    pub verbose: bool,
}

impl DotsConfig {
    /// Try to read user config from the file if it exists.
    /// Otherwise, fall back to default config.
    ///
    /// # Errors
    /// Returns an error if config file exists but cannot be read or parsed.
    pub fn get_user_config() -> anyhow::Result<Self> {
        let Some(path) = crate::config_path() else {
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
    pub fn from_toml_str(toml_str: &str) -> anyhow::Result<Self> {
        toml::from_str::<UserConfig>(toml_str)
            .map(|config| config.dots)
            .with_context(|| "Failed to parse config TOML")
    }

    /// Collect substitutes to replace pairs from a list of pattern-replacement pairs.
    #[must_use]
    pub fn parse_substitutes(substitutes: &[String]) -> Vec<(String, String)> {
        substitutes
            .chunks(2)
            .filter_map(|chunk| {
                if chunk.len() == 2 {
                    let pattern = chunk[0].trim().to_string();
                    let replace = chunk[1].trim().to_string();
                    if pattern.is_empty() {
                        eprintln!("Empty replace pattern: '{pattern}' -> '{replace}'");
                        None
                    } else {
                        Some((pattern, replace))
                    }
                } else {
                    None
                }
            })
            .collect()
    }

    /// Collect removes to replace pairs from a list of patterns to remove.
    #[must_use]
    pub fn parse_removes(removes: &[String]) -> Vec<(String, String)> {
        removes
            .iter()
            .filter_map(|remove| {
                let pattern = remove.trim().to_string();
                let replace = String::new();
                if pattern.is_empty() {
                    eprintln!("Empty remove pattern: '{pattern}'");
                    None
                } else {
                    Some((pattern, replace))
                }
            })
            .collect()
    }

    /// Collect and compile regex substitutes to replace pairs.
    ///
    /// # Errors
    /// Returns an error if any regex pattern is invalid.
    pub fn parse_regex_substitutes(regex_pairs: &[String]) -> anyhow::Result<Vec<(Regex, String)>> {
        regex_pairs
            .chunks(2)
            .filter_map(|chunk| {
                if chunk.len() == 2 {
                    match Regex::new(&chunk[0]).with_context(|| format!("Invalid regex: '{}'", chunk[0])) {
                        Ok(regex) => Some(Ok((regex, chunk[1].clone()))),
                        Err(e) => Some(Err(e)),
                    }
                } else {
                    None
                }
            })
            .collect()
    }

    /// Compile regex patterns from string pairs.
    ///
    /// # Errors
    /// Returns an error if any regex pattern is invalid.
    pub fn compile_regex_patterns(regex_pairs: Vec<(String, String)>) -> anyhow::Result<Vec<(Regex, String)>> {
        let mut compiled_pairs = Vec::new();

        for (pattern, replacement) in regex_pairs {
            let regex = Regex::new(&pattern).with_context(|| format!("Invalid regex: '{pattern}'"))?;
            compiled_pairs.push((regex, replacement));
        }

        Ok(compiled_pairs)
    }
}

impl DotRenameConfig {
    /// Create a minimal config for name formatting only.
    ///
    /// This loads the user config file but doesn't require CLI args.
    /// Useful when you just want to format names without running the full rename operation.
    ///
    /// # Errors
    /// Returns an error if the config file cannot be read or parsed.
    pub fn from_user_config() -> anyhow::Result<Self> {
        let user_config = DotsConfig::get_user_config()?;
        let config_regex = DotsConfig::compile_regex_patterns(user_config.regex_replace).unwrap_or_default();

        let move_date_after_prefix = user_config
            .move_date_after_prefix
            .into_iter()
            .map(|mut s| {
                if !s.ends_with('.') {
                    s.push('.');
                }
                s
            })
            .collect::<Vec<_>>();

        Ok(Self {
            convert_case: false,
            date_starts_with_year: user_config.date_starts_with_year,
            debug: user_config.debug,
            deduplicate_patterns: Vec::new(),
            dryrun: user_config.dryrun,
            include: user_config.include,
            exclude: Vec::new(),
            increment_name: user_config.increment,
            move_date_after_prefix,
            move_to_end: user_config.move_to_end,
            move_to_start: user_config.move_to_start,
            overwrite: user_config.overwrite,
            pre_replace: user_config.pre_replace,
            prefix: None,
            prefix_dir: user_config.prefix_dir || user_config.prefix_dir_start || user_config.prefix_dir_recursive,
            prefix_dir_recursive: user_config.prefix_dir_recursive,
            prefix_dir_start: user_config.prefix_dir_start,
            recurse: user_config.recurse || user_config.prefix_dir_recursive || user_config.suffix_dir_recursive,
            regex_replace: config_regex,
            regex_replace_after: Vec::default(),
            remove_from_start: user_config.remove_from_start,
            remove_random: user_config.remove_random,
            rename_directories: user_config.directory,
            replace: user_config.replace,
            suffix: None,
            suffix_dir: user_config.suffix_dir || user_config.suffix_dir_recursive,
            suffix_dir_recursive: user_config.suffix_dir_recursive,
            verbose: user_config.verbose,
        })
    }
}

impl fmt::Display for DotRenameConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let include = if self.include.is_empty() {
            "include: []".to_string()
        } else {
            "include:\n".to_string() + &*self.include.iter().map(|name| format!("    {name}")).join("\n")
        };
        let exclude = if self.exclude.is_empty() {
            "exclude: []".to_string()
        } else {
            "exclude:\n".to_string() + &*self.exclude.iter().map(|name| format!("    {name}")).join("\n")
        };
        let replace = if self.replace.is_empty() {
            "replace:   []".to_string()
        } else {
            "replace:\n".to_string() + &*self.replace.iter().map(|pair| format!("    {pair:?}")).join("\n")
        };
        let regex_replace = if self.regex_replace.is_empty() {
            "regex_replace: []".to_string()
        } else {
            "regex_replace:\n".to_string() + &*self.regex_replace.iter().map(|pair| format!("    {pair:?}")).join("\n")
        };
        writeln!(f, "Config:")?;
        writeln!(f, "  debug:      {}", crate::colorize_bool(self.debug))?;
        writeln!(f, "  dryrun:     {}", crate::colorize_bool(self.dryrun))?;
        writeln!(f, "  prefix dir: {}", crate::colorize_bool(self.prefix_dir))?;
        writeln!(f, "  suffix dir: {}", crate::colorize_bool(self.suffix_dir))?;
        writeln!(f, "  overwrite:  {}", crate::colorize_bool(self.overwrite))?;
        writeln!(f, "  recurse:    {}", crate::colorize_bool(self.recurse))?;
        writeln!(f, "  verbose:    {}", crate::colorize_bool(self.verbose))?;
        writeln!(
            f,
            "  prefix:     \"{}\"",
            self.prefix.as_ref().unwrap_or(&String::new())
        )?;
        writeln!(
            f,
            "  suffix:     \"{}\"",
            self.suffix.as_ref().unwrap_or(&String::new())
        )?;
        writeln!(f, "  {include}")?;
        writeln!(f, "  {exclude}")?;
        writeln!(f, "  {replace}")?;
        writeln!(f, "  {regex_replace}")
    }
}

const fn default_true() -> bool {
    true
}

#[cfg(test)]
mod dots_config_tests {
    use super::*;

    #[test]
    fn from_toml_str_parses_empty_config() {
        let toml = "";
        let config = DotsConfig::from_toml_str(toml).unwrap();
        assert!(config.replace.is_empty());
        assert!(config.include.is_empty());
        assert!(!config.debug);
    }

    #[test]
    fn from_toml_str_parses_dots_section() {
        let toml = r"
[dots]
debug = true
dryrun = true
verbose = true
";
        let config = DotsConfig::from_toml_str(toml).unwrap();
        assert!(config.debug);
        assert!(config.dryrun);
        assert!(config.verbose);
    }

    #[test]
    fn from_toml_str_parses_replace_pairs() {
        let toml = r#"
[dots]
replace = [
    [".WEBDL", ""],
    [".HEVC", ".x265"],
]
"#;
        let config = DotsConfig::from_toml_str(toml).unwrap();
        assert_eq!(config.replace.len(), 2);
        assert_eq!(config.replace[0], (".WEBDL".to_string(), String::new()));
        assert_eq!(config.replace[1], (".HEVC".to_string(), ".x265".to_string()));
    }

    #[test]
    fn from_toml_str_parses_regex_replace() {
        let toml = r#"
[dots]
regex_replace = [
    ["\\d{4}", "YEAR"],
]
"#;
        let config = DotsConfig::from_toml_str(toml).unwrap();
        assert_eq!(config.regex_replace.len(), 1);
        assert_eq!(config.regex_replace[0].0, "\\d{4}");
        assert_eq!(config.regex_replace[0].1, "YEAR");
    }

    #[test]
    fn from_toml_str_parses_include_list() {
        let toml = r#"
[dots]
include = ["pattern1", "pattern2", "pattern3"]
"#;
        let config = DotsConfig::from_toml_str(toml).unwrap();
        assert_eq!(config.include, vec!["pattern1", "pattern2", "pattern3"]);
    }

    #[test]
    fn from_toml_str_parses_move_lists() {
        let toml = r#"
[dots]
move_to_start = ["PREFIX"]
move_to_end = ["SUFFIX"]
move_date_after_prefix = ["Artist"]
"#;
        let config = DotsConfig::from_toml_str(toml).unwrap();
        assert_eq!(config.move_to_start, vec!["PREFIX"]);
        assert_eq!(config.move_to_end, vec!["SUFFIX"]);
        assert_eq!(config.move_date_after_prefix, vec!["Artist"]);
    }

    #[test]
    fn from_toml_str_default_date_starts_with_year_is_true() {
        let toml = "[dots]";
        let config = DotsConfig::from_toml_str(toml).unwrap();
        assert!(config.date_starts_with_year);
    }

    #[test]
    fn from_toml_str_can_override_date_starts_with_year() {
        let toml = r"
[dots]
date_starts_with_year = false
";
        let config = DotsConfig::from_toml_str(toml).unwrap();
        assert!(!config.date_starts_with_year);
    }

    #[test]
    fn from_toml_str_parses_prefix_dir_options() {
        let toml = r"
[dots]
prefix_dir = true
prefix_dir_recursive = true
prefix_dir_start = true
";
        let config = DotsConfig::from_toml_str(toml).unwrap();
        assert!(config.prefix_dir);
        assert!(config.prefix_dir_recursive);
        assert!(config.prefix_dir_start);
    }

    #[test]
    fn from_toml_str_invalid_toml_returns_error() {
        let toml = "this is not valid toml {{{";
        let result = DotsConfig::from_toml_str(toml);
        assert!(result.is_err());
    }
}

#[cfg(test)]
mod parse_substitutes_tests {
    use super::*;

    #[test]
    fn parses_valid_pairs() {
        let input = vec![
            "old1".to_string(),
            "new1".to_string(),
            "old2".to_string(),
            "new2".to_string(),
        ];
        let result = DotsConfig::parse_substitutes(&input);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0], ("old1".to_string(), "new1".to_string()));
        assert_eq!(result[1], ("old2".to_string(), "new2".to_string()));
    }

    #[test]
    fn ignores_incomplete_pair() {
        let input = vec!["old1".to_string(), "new1".to_string(), "orphan".to_string()];
        let result = DotsConfig::parse_substitutes(&input);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn skips_empty_pattern() {
        let input = vec![String::new(), "replacement".to_string()];
        let result = DotsConfig::parse_substitutes(&input);
        assert!(result.is_empty());
    }

    #[test]
    fn trims_whitespace() {
        let input = vec!["  pattern  ".to_string(), "  replacement  ".to_string()];
        let result = DotsConfig::parse_substitutes(&input);
        assert_eq!(result[0], ("pattern".to_string(), "replacement".to_string()));
    }

    #[test]
    fn handles_empty_input() {
        let input: Vec<String> = vec![];
        let result = DotsConfig::parse_substitutes(&input);
        assert!(result.is_empty());
    }
}

#[cfg(test)]
mod parse_removes_tests {
    use super::*;

    #[test]
    fn parses_valid_removes() {
        let input = vec!["pattern1".to_string(), "pattern2".to_string()];
        let result = DotsConfig::parse_removes(&input);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0], ("pattern1".to_string(), String::new()));
        assert_eq!(result[1], ("pattern2".to_string(), String::new()));
    }

    #[test]
    fn skips_empty_patterns() {
        let input = vec!["valid".to_string(), String::new(), "  ".to_string()];
        let result = DotsConfig::parse_removes(&input);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0, "valid");
    }

    #[test]
    fn trims_whitespace() {
        let input = vec!["  pattern  ".to_string()];
        let result = DotsConfig::parse_removes(&input);
        assert_eq!(result[0].0, "pattern");
    }
}

#[cfg(test)]
mod parse_regex_substitutes_tests {
    use super::*;

    #[test]
    fn parses_valid_regex_pairs() {
        let input = vec![
            r"\d+".to_string(),
            "NUMBER".to_string(),
            r"[a-z]+".to_string(),
            "LETTERS".to_string(),
        ];
        let result = DotsConfig::parse_regex_substitutes(&input).unwrap();
        assert_eq!(result.len(), 2);
        assert!(result[0].0.is_match("123"));
        assert_eq!(result[0].1, "NUMBER");
    }

    #[test]
    fn returns_error_for_invalid_regex() {
        let input = vec!["[invalid".to_string(), "replacement".to_string()];
        let result = DotsConfig::parse_regex_substitutes(&input);
        assert!(result.is_err());
    }

    #[test]
    fn ignores_incomplete_pairs() {
        let input = vec![r"\d+".to_string()];
        let result = DotsConfig::parse_regex_substitutes(&input).unwrap();
        assert!(result.is_empty());
    }
}

#[cfg(test)]
mod compile_regex_patterns_tests {
    use super::*;

    #[test]
    fn compiles_valid_patterns() {
        let input = vec![
            (r"\d{4}".to_string(), "YEAR".to_string()),
            (r"[A-Z]+".to_string(), "UPPER".to_string()),
        ];
        let result = DotsConfig::compile_regex_patterns(input).unwrap();
        assert_eq!(result.len(), 2);
        assert!(result[0].0.is_match("2024"));
        assert!(result[1].0.is_match("ABC"));
    }

    #[test]
    fn returns_error_for_invalid_pattern() {
        let input = vec![("[invalid".to_string(), "replacement".to_string())];
        let result = DotsConfig::compile_regex_patterns(input);
        assert!(result.is_err());
    }

    #[test]
    fn handles_empty_input() {
        let input: Vec<(String, String)> = vec![];
        let result = DotsConfig::compile_regex_patterns(input).unwrap();
        assert!(result.is_empty());
    }
}

#[cfg(test)]
mod dot_rename_config_display_tests {
    use super::*;

    #[test]
    fn display_formats_config() {
        let config = DotRenameConfig {
            debug: true,
            dryrun: false,
            verbose: true,
            prefix: Some("PREFIX".to_string()),
            suffix: None,
            ..Default::default()
        };
        let display = format!("{config}");
        assert!(display.contains("Config:"));
        assert!(display.contains("debug:"));
        assert!(display.contains("dryrun:"));
        assert!(display.contains("PREFIX"));
    }

    #[test]
    fn display_shows_include_exclude() {
        let config = DotRenameConfig {
            include: vec!["inc1".to_string(), "inc2".to_string()],
            exclude: vec!["exc1".to_string()],
            ..Default::default()
        };
        let display = format!("{config}");
        assert!(display.contains("inc1"));
        assert!(display.contains("inc2"));
        assert!(display.contains("exc1"));
    }

    #[test]
    fn display_shows_empty_lists() {
        let config = DotRenameConfig::default();
        let display = format!("{config}");
        assert!(display.contains("include: []"));
        assert!(display.contains("exclude: []"));
        assert!(display.contains("replace:   []"));
    }
}

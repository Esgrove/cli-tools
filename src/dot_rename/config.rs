//! Configuration for dot rename operations.

use std::{fmt, fs};

use anyhow::Context;
use colored::Colorize;
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
    #[must_use]
    pub fn get_user_config() -> Self {
        crate::config::CONFIG_PATH
            .as_deref()
            .and_then(|path| {
                fs::read_to_string(path)
                    .map_err(|e| {
                        eprintln!(
                            "{}",
                            format!("Error reading config file {}: {}", path.display(), e).red()
                        );
                    })
                    .ok()
            })
            .and_then(|config_string| {
                toml::from_str::<UserConfig>(&config_string)
                    .map_err(|e| {
                        eprintln!("{}", format!("Error reading config file: {e}").red());
                    })
                    .ok()
            })
            .map(|config| config.dots)
            .unwrap_or_default()
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
    #[must_use]
    pub fn for_name_formatting() -> Self {
        let user_config = DotsConfig::get_user_config();
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

        Self {
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
        }
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

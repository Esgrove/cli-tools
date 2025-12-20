use std::{fmt, fs};

use anyhow::Context;
use colored::Colorize;
use itertools::Itertools;
use regex::Regex;
use serde::Deserialize;

use crate::Args;

/// Config from the user config file
#[derive(Debug, Default, Deserialize)]
struct DotsConfig {
    #[serde(default = "default_true")]
    date_starts_with_year: bool,
    #[serde(default)]
    debug: bool,
    #[serde(default)]
    directory: bool,
    #[serde(default)]
    dryrun: bool,
    #[serde(default)]
    increment: bool,
    #[serde(default)]
    include: Vec<String>,
    #[serde(default)]
    move_date_after_prefix: Vec<String>,
    #[serde(default)]
    move_to_end: Vec<String>,
    #[serde(default)]
    move_to_start: Vec<String>,
    #[serde(default)]
    overwrite: bool,
    #[serde(default)]
    prefix_dir: bool,
    #[serde(default)]
    prefix_dir_start: bool,
    #[serde(default)]
    suffix_dir: bool,
    #[serde(default)]
    pre_replace: Vec<(String, String)>,
    #[serde(default)]
    recurse: bool,
    #[serde(default)]
    regex_replace: Vec<(String, String)>,
    #[serde(default)]
    remove_random: bool,
    #[serde(default)]
    replace: Vec<(String, String)>,
    #[serde(default)]
    remove_from_start: Vec<String>,
    #[serde(default)]
    verbose: bool,
}

/// Wrapper needed for parsing the config section.
#[derive(Debug, Default, Deserialize)]
struct UserConfig {
    #[serde(default)]
    dots: DotsConfig,
}

/// Final config created from CLI arguments and user config file.
#[derive(Debug, Default)]
pub struct Config {
    pub(crate) convert_case: bool,
    pub(crate) date_starts_with_year: bool,
    pub(crate) debug: bool,
    pub(crate) deduplicate_patterns: Vec<(Regex, String)>,
    pub(crate) dryrun: bool,
    pub(crate) include: Vec<String>,
    pub(crate) exclude: Vec<String>,
    pub(crate) increment_name: bool,
    pub(crate) move_date_after_prefix: Vec<String>,
    pub(crate) move_to_end: Vec<String>,
    pub(crate) move_to_start: Vec<String>,
    pub(crate) overwrite: bool,
    pub(crate) pre_replace: Vec<(String, String)>,
    pub(crate) prefix: Option<String>,
    pub(crate) prefix_dir: bool,
    pub(crate) prefix_dir_start: bool,
    pub(crate) recurse: bool,
    pub(crate) regex_replace: Vec<(Regex, String)>,
    pub(crate) regex_replace_after: Vec<(Regex, String)>,
    pub(crate) remove_from_start: Vec<String>,
    pub(crate) remove_random: bool,
    pub(crate) rename_directories: bool,
    pub(crate) replace: Vec<(String, String)>,
    pub(crate) suffix: Option<String>,
    pub(crate) suffix_dir: bool,
    pub(crate) verbose: bool,
}

impl Config {
    /// Create config from given command line args and user config file.
    pub fn from_args(args: Args) -> anyhow::Result<Self> {
        let user_config = DotsConfig::get_user_config();
        let substitutes = args.parse_substitutes();
        let removes = args.parse_removes();

        // Compile regex patterns for deduplication (non-empty replacements from substitutes)
        let mut seen_replacements = std::collections::HashSet::new();
        let deduplicate_patterns: Vec<(Regex, String)> = substitutes
            .iter()
            .filter(|(_, replacement)| !replacement.is_empty() && seen_replacements.insert(replacement.as_str()))
            .filter_map(|(_, replacement)| {
                let escaped = regex::escape(replacement);
                Regex::new(&format!(r"({escaped}){{2,}}"))
                    .ok()
                    .map(|re| (re, replacement.clone()))
            })
            .collect();

        let mut replace = substitutes;
        let mut regex_replace = args.parse_regex_substitutes()?;
        let config_regex = Self::compile_regex_patterns(user_config.regex_replace)?;

        let include: Vec<String> = user_config
            .include
            .into_iter()
            .chain(replace.iter().map(|(pattern, _)| pattern.clone()))
            .chain(args.include)
            .unique()
            .collect();

        replace.extend(removes);
        replace.extend(user_config.replace);
        let replace: Vec<(String, String)> = replace.into_iter().unique().collect();

        regex_replace.extend(config_regex);

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
            convert_case: args.case,
            date_starts_with_year: !args.year || user_config.date_starts_with_year,
            debug: args.debug || user_config.debug,
            deduplicate_patterns,
            dryrun: args.print || user_config.dryrun,
            include,
            exclude: args.exclude,
            increment_name: args.increment || user_config.increment,
            move_date_after_prefix,
            move_to_end: user_config.move_to_end,
            move_to_start: user_config.move_to_start,
            overwrite: args.force || user_config.overwrite,
            pre_replace: user_config.pre_replace,
            prefix: args.prefix,
            prefix_dir: args.prefix_dir
                || args.prefix_dir_start
                || user_config.prefix_dir
                || user_config.prefix_dir_start,
            prefix_dir_start: args.prefix_dir_start || user_config.prefix_dir_start,
            recurse: args.recurse || user_config.recurse,
            regex_replace,
            regex_replace_after: Vec::default(),
            remove_from_start: user_config.remove_from_start,
            remove_random: args.random || user_config.remove_random,
            rename_directories: args.directory || user_config.directory,
            replace,
            suffix: args.suffix,
            suffix_dir: args.suffix_dir || user_config.suffix_dir,
            verbose: args.verbose || user_config.verbose,
        })
    }

    fn compile_regex_patterns(regex_pairs: Vec<(String, String)>) -> anyhow::Result<Vec<(Regex, String)>> {
        let mut compiled_pairs = Vec::new();

        for (pattern, replacement) in regex_pairs {
            let regex = Regex::new(&pattern).with_context(|| format!("Invalid regex: '{pattern}'"))?;
            compiled_pairs.push((regex, replacement));
        }

        Ok(compiled_pairs)
    }
}

impl DotsConfig {
    /// Try to read user config from the file if it exists.
    /// Otherwise, fall back to default config.
    fn get_user_config() -> Self {
        cli_tools::config::CONFIG_PATH
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
}

impl fmt::Display for Config {
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
        writeln!(f, "  debug:      {}", cli_tools::colorize_bool(self.debug))?;
        writeln!(f, "  dryrun:     {}", cli_tools::colorize_bool(self.dryrun))?;
        writeln!(f, "  prefix dir: {}", cli_tools::colorize_bool(self.prefix_dir))?;
        writeln!(f, "  suffix dir: {}", cli_tools::colorize_bool(self.suffix_dir))?;
        writeln!(f, "  overwrite:  {}", cli_tools::colorize_bool(self.overwrite))?;
        writeln!(f, "  recurse:    {}", cli_tools::colorize_bool(self.recurse))?;
        writeln!(f, "  verbose:    {}", cli_tools::colorize_bool(self.verbose))?;
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

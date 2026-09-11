//! Configuration for the `slb` binary.
//!
//! Reads the `[slb]` section of the user config file,
//! merges it with command line arguments,
//! and builds the formatting options used by the library.

use std::fs;

use anyhow::{Context, Result};
use clap::ValueEnum;
use serde::Deserialize;

use cli_tools::semantic_line_breaks::types::{
    DEFAULT_ABBREVIATIONS, DEFAULT_DIRECTIVE_PREFIXES, DEFAULT_PRESERVE_LOWERCASE, DEFAULT_TAB_WIDTH,
};
use cli_tools::semantic_line_breaks::{FileKind, FormatOptions, RuleSet, ViolationKind};

use crate::Args;

/// Directory names skipped by default when walking directories.
pub const DEFAULT_EXCLUDES: &[&str] = &[
    "target",
    "node_modules",
    "build",
    "dist",
    "cdk.out",
    "coverage",
    ".venv",
    "venv",
    "vendor",
];

/// User configuration from the `[slb]` section of the config file.
#[derive(Debug, Default, Deserialize)]
pub struct SlbConfig {
    #[serde(default)]
    pub abbreviations: Vec<String>,
    #[serde(default)]
    pub allow_word_break: bool,
    #[serde(default)]
    pub clause_starters: Vec<String>,
    #[serde(default)]
    pub directive_prefixes: Vec<String>,
    #[serde(default)]
    pub exclude: Vec<String>,
    #[serde(default)]
    pub extensions: Vec<String>,
    #[serde(default)]
    pub join_sentences: bool,
    #[serde(default)]
    pub preserve_lowercase: Vec<String>,
    #[serde(default)]
    pub rules: Vec<String>,
    #[serde(default)]
    pub use_project_config: Option<bool>,
    #[serde(default)]
    pub verbose: bool,
    #[serde(default)]
    pub width: Option<usize>,
}

/// Wrapper needed for parsing the config section.
#[derive(Debug, Default, Deserialize)]
struct UserConfig {
    #[serde(default)]
    slb: SlbConfig,
}

/// Final configuration created from command line arguments and the user config file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub abbreviations: Vec<String>,
    pub allow_word_break: bool,
    pub clause_starters: Vec<String>,
    pub directive_prefixes: Vec<String>,
    pub exclude: Vec<String>,
    pub extensions: Vec<String>,
    pub fix: bool,
    pub join_sentences: bool,
    pub kind: Option<FileKind>,
    pub preserve_lowercase: Vec<String>,
    pub print: bool,
    pub quiet: bool,
    pub rules: RuleSet,
    pub stdin: bool,
    pub project_width: bool,
    pub verbose: bool,
    pub width: Option<usize>,
}

impl SlbConfig {
    /// Try to read user config from the file if it exists.
    /// Otherwise, fall back to default config.
    ///
    /// # Errors
    /// Returns an error if the config file exists but cannot be read or parsed.
    pub fn get_user_config() -> Result<Self> {
        let Some(path) = cli_tools::config_path() else {
            return Ok(Self::default());
        };
        match fs::read_to_string(path) {
            Ok(content) => Self::from_toml_str(&content)
                .map_err(|error| anyhow::anyhow!("Failed to parse config file {}:\n{error}", path.display())),
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
    pub fn from_toml_str(toml_str: &str) -> Result<Self> {
        toml::from_str::<UserConfig>(toml_str)
            .map(|config| config.slb)
            .with_context(|| "Failed to parse config TOML")
    }
}

impl Config {
    /// Create the final config from command line arguments and the user config file.
    ///
    /// Command line values win over the config file, which wins over the built-in defaults.
    /// Extension lists (abbreviations, directive prefixes, lowercase words) add to the defaults.
    /// Clause starters and excludes replace the defaults when given.
    ///
    /// # Errors
    /// Returns an error if the config file cannot be read or parsed or names an unknown rule.
    pub fn from_args(args: &Args) -> Result<Self> {
        Self::from_args_and_user_config(args, SlbConfig::get_user_config()?)
    }

    /// Create the final config from command line arguments and the given user config.
    ///
    /// # Errors
    /// Returns an error if the user config names an unknown rule.
    pub fn from_args_and_user_config(args: &Args, user_config: SlbConfig) -> Result<Self> {
        let rules = if args.rules.is_empty() {
            if user_config.rules.is_empty() {
                RuleSet::ALL
            } else {
                let kinds = user_config
                    .rules
                    .iter()
                    .map(|name| {
                        ViolationKind::from_str(name, true)
                            .map_err(|_| anyhow::anyhow!("Unknown rule in config file: '{name}'"))
                    })
                    .collect::<Result<Vec<_>>>()?;
                RuleSet::from_kinds(&kinds)
            }
        } else {
            RuleSet::from_kinds(&args.rules)
        };

        let exclude = if args.exclude.is_empty() {
            if user_config.exclude.is_empty() {
                to_strings(DEFAULT_EXCLUDES)
            } else {
                user_config.exclude
            }
        } else {
            args.exclude.clone()
        };

        let extensions = if args.extensions.is_empty() {
            user_config.extensions
        } else {
            args.extensions.clone()
        };

        Ok(Self {
            abbreviations: extend_defaults(DEFAULT_ABBREVIATIONS, user_config.abbreviations),
            allow_word_break: args.word_break || user_config.allow_word_break,
            clause_starters: user_config.clause_starters,
            directive_prefixes: extend_defaults(DEFAULT_DIRECTIVE_PREFIXES, user_config.directive_prefixes),
            exclude,
            extensions: extensions
                .into_iter()
                .map(|extension| extension.trim_start_matches('.').to_ascii_lowercase())
                .collect(),
            fix: args.fix,
            join_sentences: args.join_sentences || user_config.join_sentences,
            kind: args.kind,
            preserve_lowercase: extend_defaults(DEFAULT_PRESERVE_LOWERCASE, user_config.preserve_lowercase),
            print: args.print,
            quiet: args.quiet,
            rules,
            stdin: args.stdin,
            project_width: !args.ignore_project_config && user_config.use_project_config.unwrap_or(true),
            verbose: args.verbose || user_config.verbose,
            width: args.width.or(user_config.width),
        })
    }

    /// Build the formatting options for the given resolved width.
    #[must_use]
    pub fn format_options(&self, max_width: usize) -> FormatOptions {
        FormatOptions {
            max_width,
            tab_width: DEFAULT_TAB_WIDTH,
            join_sentences: self.join_sentences,
            allow_word_break: self.allow_word_break,
            rules: self.rules,
            abbreviations: self.abbreviations.clone(),
            clause_starters: self.clause_starters.clone(),
            directive_prefixes: self.directive_prefixes.clone(),
            preserve_lowercase: self.preserve_lowercase.clone(),
        }
    }
}

/// Built-in defaults followed by the user's additions, lowercased and without duplicates.
fn extend_defaults(defaults: &[&str], extra: Vec<String>) -> Vec<String> {
    let mut values = to_strings(defaults);
    for value in extra {
        let value = value.trim().to_lowercase();
        if !value.is_empty() && !values.contains(&value) {
            values.push(value);
        }
    }
    values
}

/// Convert a static string list into owned strings.
fn to_strings(values: &[&str]) -> Vec<String> {
    values.iter().map(std::string::ToString::to_string).collect()
}

#[cfg(test)]
mod test_slb_config {
    use super::*;

    #[test]
    fn from_toml_str_parses_empty_config() {
        let config = SlbConfig::from_toml_str("").expect("empty config should parse");
        assert!(config.width.is_none());
        assert!(config.rules.is_empty());
        assert!(!config.join_sentences);
        assert!(config.use_project_config.is_none());
    }

    #[test]
    fn from_toml_str_parses_slb_section() {
        let toml = r#"
[slb]
width = 100
use_project_config = false
rules = ["semicolon", "trailing"]
join_sentences = true
allow_word_break = true
extensions = ["rs", "md"]
exclude = ["target"]
abbreviations = ["approx."]
clause_starters = ["meanwhile"]
directive_prefixes = ["todo"]
preserve_lowercase = ["ffmpeg"]
"#;
        let config = SlbConfig::from_toml_str(toml).expect("config should parse");
        assert_eq!(config.width, Some(100));
        assert_eq!(config.use_project_config, Some(false));
        assert_eq!(config.rules, vec!["semicolon", "trailing"]);
        assert!(config.join_sentences);
        assert!(config.allow_word_break);
        assert_eq!(config.extensions, vec!["rs", "md"]);
        assert_eq!(config.exclude, vec!["target"]);
        assert_eq!(config.clause_starters, vec!["meanwhile"]);
    }

    #[test]
    fn from_toml_str_ignores_other_sections() {
        let config = SlbConfig::from_toml_str("[dots]\ndebug = true\n").expect("config should parse");
        assert!(config.width.is_none());
    }

    #[test]
    fn from_toml_str_invalid_toml_returns_error() {
        assert!(SlbConfig::from_toml_str("[slb\nwidth = ").is_err());
    }

    #[test]
    fn extend_defaults_adds_lowercased_unique_values() {
        let values = extend_defaults(&["a", "b"], vec!["B".to_string(), " c ".to_string(), String::new()]);
        assert_eq!(values, vec!["a", "b", "c"]);
    }
}

#[cfg(test)]
mod test_config_from_args {
    use clap::Parser;

    use super::*;

    #[test]
    fn cli_rules_and_width_override_defaults() {
        let args = Args::try_parse_from([
            "slb",
            "--rules",
            "semicolon,em-dash",
            "--width",
            "80",
            "--exclude",
            "docs",
        ])
        .expect("arguments should parse");
        let config = Config::from_args(&args).expect("config should build");
        assert!(config.rules.semicolon);
        assert!(config.rules.em_dash);
        assert!(!config.rules.trailing_comment);
        assert_eq!(config.width, Some(80));
        assert_eq!(config.exclude, vec!["docs"]);
        assert!(config.project_width);
    }

    /// During tests the user config resolves to `tests/fixtures/sample_config.toml`,
    /// whose `[slb]` section sets `exclude = ["target"]`, `clause_starters = ["meanwhile"]`,
    /// and `abbreviations = ["approx."]`.
    #[test]
    fn fixture_config_values_are_merged_with_defaults() {
        let args = Args::try_parse_from(["slb", "--extensions", ".RS"]).expect("arguments should parse");
        let config = Config::from_args(&args).expect("config should build");
        assert_eq!(config.rules, RuleSet::ALL);
        assert_eq!(config.exclude, vec!["target"]);
        assert_eq!(config.extensions, vec!["rs"]);
        assert_eq!(config.clause_starters, vec!["meanwhile"]);
        assert!(config.abbreviations.contains(&"e.g.".to_string()));
        assert!(config.abbreviations.contains(&"approx.".to_string()));
        assert_eq!(
            config.abbreviations.iter().filter(|value| *value == "approx.").count(),
            1
        );
        assert_eq!(config.width, Some(110));
    }

    #[test]
    fn ignore_project_config_flag_disables_discovery() {
        let args = Args::try_parse_from(["slb", "-i"]).expect("arguments should parse");
        let config = Config::from_args(&args).expect("config should build");
        assert!(!config.project_width);
        let options = config.format_options(90);
        assert_eq!(options.max_width, 90);
    }
}

#[cfg(test)]
mod test_config_merge {
    use clap::Parser;

    use super::*;

    /// Build a config from arguments and an explicit user config, without reading the config file.
    fn config(arguments: &[&str], toml: &str) -> Result<Config> {
        let args = Args::try_parse_from(arguments).expect("arguments should parse");
        let user_config = SlbConfig::from_toml_str(toml).expect("user config should parse");
        Config::from_args_and_user_config(&args, user_config)
    }

    #[test]
    fn an_empty_user_config_gives_the_built_in_defaults() {
        let config = config(&["slb"], "").expect("config should build");
        assert_eq!(config.rules, RuleSet::ALL);
        assert_eq!(config.exclude, to_strings(DEFAULT_EXCLUDES));
        assert!(config.extensions.is_empty());
        assert!(config.width.is_none());
        assert!(config.project_width);
        assert!(!config.join_sentences);
        assert!(!config.allow_word_break);
        assert!(config.clause_starters.is_empty());
        assert_eq!(config.abbreviations, to_strings(DEFAULT_ABBREVIATIONS));
    }

    #[test]
    fn the_user_config_rules_are_used_when_no_rules_option_is_given() {
        let config = config(&["slb"], "[slb]\nrules = [\"semicolon\", \"em-dash\"]\n").expect("config should build");
        assert!(config.rules.semicolon);
        assert!(config.rules.em_dash);
        assert!(!config.rules.line_too_long);
        assert!(!config.rules.trailing_comment);
    }

    #[test]
    fn the_rules_option_wins_over_the_user_config() {
        let config =
            config(&["slb", "--rules", "trailing"], "[slb]\nrules = [\"semicolon\"]\n").expect("config should build");
        assert!(config.rules.trailing_comment);
        assert!(!config.rules.semicolon);
    }

    #[test]
    fn an_unknown_rule_in_the_user_config_is_an_error() {
        let error = config(&["slb"], "[slb]\nrules = [\"bogus\"]\n").expect_err("an unknown rule should fail");
        assert!(format!("{error:#}").contains("Unknown rule in config file: 'bogus'"));
    }

    #[test]
    fn the_user_config_excludes_replace_the_defaults() {
        let config = config(&["slb"], "[slb]\nexclude = [\"generated\"]\n").expect("config should build");
        assert_eq!(config.exclude, vec!["generated"]);

        let overridden = config_with_exclude_option();
        assert_eq!(overridden.exclude, vec!["docs"]);
    }

    /// Config with an exclude given on the command line and another one in the user config.
    fn config_with_exclude_option() -> Config {
        config(&["slb", "--exclude", "docs"], "[slb]\nexclude = [\"generated\"]\n").expect("config should build")
    }

    #[test]
    fn the_user_config_width_and_flags_are_merged() {
        let config = config(
            &["slb"],
            "[slb]\nwidth = 100\njoin_sentences = true\nallow_word_break = true\nuse_project_config = false\nverbose = true\n",
        )
        .expect("config should build");
        assert_eq!(config.width, Some(100));
        assert!(config.join_sentences);
        assert!(config.allow_word_break);
        assert!(!config.project_width);
        assert!(config.verbose);
    }

    #[test]
    fn the_width_option_wins_over_the_user_config() {
        let config = config(&["slb", "--width", "80"], "[slb]\nwidth = 100\n").expect("config should build");
        assert_eq!(config.width, Some(80));
    }

    #[test]
    fn extensions_from_the_user_config_are_normalized() {
        let config = config(&["slb"], "[slb]\nextensions = [\".RS\", \"MD\"]\n").expect("config should build");
        assert_eq!(config.extensions, vec!["rs", "md"]);
    }

    #[test]
    fn the_format_options_carry_the_merged_values() {
        let config = config(
            &["slb"],
            "[slb]\nclause_starters = [\"meanwhile\"]\nabbreviations = [\"approx.\"]\npreserve_lowercase = [\"ffmpeg\"]\n",
        )
        .expect("config should build");
        let options = config.format_options(95);
        assert_eq!(options.max_width, 95);
        assert_eq!(options.tab_width, DEFAULT_TAB_WIDTH);
        assert_eq!(options.clause_starters, vec!["meanwhile"]);
        assert!(options.abbreviations.contains(&"approx.".to_string()));
        assert!(options.preserve_lowercase.contains(&"ffmpeg".to_string()));
        assert_eq!(options.rules, RuleSet::ALL);
    }
}

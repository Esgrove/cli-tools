//! Configuration module for visaparse.
//!
//! Handles reading configuration from CLI arguments and the user config file.

use std::fs;
use std::path::PathBuf;

use anyhow::{Result, anyhow};
use serde::Deserialize;

use crate::VisaParseArgs;

/// Default number of totals to display in verbose output.
pub const DEFAULT_NUM_TOTALS: usize = 20;

/// User configuration from the config file.
#[derive(Debug, Default, Deserialize)]
pub struct VisaParseConfig {
    /// How many total sums to print with verbose output.
    #[serde(default)]
    pub number: Option<usize>,
    /// Only print information without writing to file.
    #[serde(default)]
    pub print: bool,
    /// Print verbose output.
    #[serde(default)]
    pub verbose: bool,
}

/// Wrapper needed for parsing the config file section.
#[derive(Debug, Default, Deserialize)]
struct UserConfig {
    #[serde(default)]
    visaparse: VisaParseConfig,
}

impl VisaParseConfig {
    /// Try to read user config from the file if it exists.
    /// Otherwise, fall back to default config.
    ///
    /// # Errors
    /// Returns an error if config file exists but cannot be read or parsed.
    pub fn get_user_config() -> Result<Self> {
        let Some(path) = cli_tools::config::CONFIG_PATH.as_deref() else {
            return Ok(Self::default());
        };

        match fs::read_to_string(path) {
            Ok(content) => Self::from_toml_str(&content)
                .map_err(|e| anyhow!("Failed to parse config file {}:\n{e}", path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(anyhow!("Failed to read config file {}: {error}", path.display())),
        }
    }

    /// Parse configuration from a TOML string.
    ///
    /// # Errors
    /// Returns an error if the TOML string is invalid.
    pub fn from_toml_str(toml_str: &str) -> Result<Self> {
        toml::from_str::<UserConfig>(toml_str)
            .map(|config| config.visaparse)
            .map_err(|e| anyhow!("Failed to parse config: {e}"))
    }
}

/// Final config combined from CLI arguments and user config file.
#[derive(Debug)]
pub struct Config {
    /// Input path (file or directory).
    pub input_path: PathBuf,
    /// Output path for generated files.
    pub output_path: PathBuf,
    /// Only print information without writing to file.
    pub print: bool,
    /// How many total sums to print with verbose output.
    pub number: usize,
    /// Print verbose output.
    pub verbose: bool,
}

impl Config {
    /// Create config from given command line args and user config file.
    ///
    /// # Errors
    /// Returns an error if the input/output paths cannot be resolved.
    pub fn from_args(args: &VisaParseArgs) -> Result<Self> {
        Self::from_args_and_config(args, &VisaParseConfig::get_user_config()?)
    }

    /// Create config from given command line args and explicit user config.
    /// This is useful for testing without reading from the config file.
    ///
    /// # Errors
    /// Returns an error if the input/output paths cannot be resolved.
    pub fn from_args_and_config(args: &VisaParseArgs, user_config: &VisaParseConfig) -> Result<Self> {
        let input_path = cli_tools::resolve_input_path(args.path.as_deref())?;
        let output_path = cli_tools::resolve_output_path(args.output.as_deref(), &input_path)?;

        // CLI args take priority over user config
        // Boolean flags: CLI true overrides config, otherwise use config value
        let print = args.print || user_config.print;
        let verbose = args.verbose || user_config.verbose;

        // Number: CLI value takes priority, then config, then default
        let number = args.number.or(user_config.number).unwrap_or(DEFAULT_NUM_TOTALS);

        Ok(Self {
            input_path,
            output_path,
            print,
            number,
            verbose,
        })
    }
}

#[cfg(test)]
mod test_visa_parse_config {
    use super::*;

    #[test]
    fn from_toml_str_parses_empty_config() {
        let toml = "";
        let config = VisaParseConfig::from_toml_str(toml).expect("should parse empty config");
        assert!(config.number.is_none());
        assert!(!config.print);
        assert!(!config.verbose);
    }

    #[test]
    fn from_toml_str_parses_visaparse_section() {
        let toml = r"
[visaparse]
number = 50
print = true
verbose = true
";
        let config = VisaParseConfig::from_toml_str(toml).expect("should parse config");
        assert_eq!(config.number, Some(50));
        assert!(config.print);
        assert!(config.verbose);
    }

    #[test]
    fn from_toml_str_parses_partial_config() {
        let toml = r"
[visaparse]
number = 30
";
        let config = VisaParseConfig::from_toml_str(toml).expect("should parse config");
        assert_eq!(config.number, Some(30));
        assert!(!config.print);
        assert!(!config.verbose);
    }

    #[test]
    fn from_toml_str_ignores_other_sections() {
        let toml = r"
[qtorrent]
verbose = true

[visaparse]
number = 15
";
        let config = VisaParseConfig::from_toml_str(toml).expect("should parse config");
        assert_eq!(config.number, Some(15));
        assert!(!config.verbose);
    }

    #[test]
    fn from_toml_str_invalid_toml_returns_error() {
        let toml = "this is not valid toml {{{";
        let result = VisaParseConfig::from_toml_str(toml);
        assert!(result.is_err());
    }

    #[test]
    fn from_toml_str_wrong_type_returns_error() {
        let toml = r#"
[visaparse]
number = "not a number"
"#;
        let result = VisaParseConfig::from_toml_str(toml);
        assert!(result.is_err());
    }

    #[test]
    fn default_config_has_expected_values() {
        let config = VisaParseConfig::default();
        assert!(config.number.is_none());
        assert!(!config.print);
        assert!(!config.verbose);
    }
}

#[cfg(test)]
mod test_config_from_args_and_config {
    use super::*;

    /// Helper to create Args with specified values.
    fn make_args(
        path: Option<PathBuf>,
        output: Option<String>,
        print: bool,
        number: Option<usize>,
        verbose: bool,
    ) -> VisaParseArgs {
        VisaParseArgs {
            path,
            output,
            print,
            number,
            verbose,
        }
    }

    /// Helper to create a user config with specified values.
    fn make_user_config(number: Option<usize>, print: bool, verbose: bool) -> VisaParseConfig {
        VisaParseConfig { number, print, verbose }
    }

    #[test]
    fn cli_number_overrides_config_number() {
        let args = make_args(Some(PathBuf::from(".")), None, false, Some(100), false);
        let user_config = make_user_config(Some(50), false, false);

        let config = Config::from_args_and_config(&args, &user_config).expect("should create config");

        assert_eq!(config.number, 100);
    }

    #[test]
    fn config_number_used_when_cli_not_provided() {
        let args = make_args(Some(PathBuf::from(".")), None, false, None, false);
        let user_config = make_user_config(Some(75), false, false);

        let config = Config::from_args_and_config(&args, &user_config).expect("should create config");

        assert_eq!(config.number, 75);
    }

    #[test]
    fn default_number_used_when_neither_provided() {
        let args = make_args(Some(PathBuf::from(".")), None, false, None, false);
        let user_config = make_user_config(None, false, false);

        let config = Config::from_args_and_config(&args, &user_config).expect("should create config");

        assert_eq!(config.number, DEFAULT_NUM_TOTALS);
    }

    #[test]
    fn cli_print_true_overrides_config_print_false() {
        let args = make_args(Some(PathBuf::from(".")), None, true, None, false);
        let user_config = make_user_config(None, false, false);

        let config = Config::from_args_and_config(&args, &user_config).expect("should create config");

        assert!(config.print);
    }

    #[test]
    fn config_print_true_enables_print_when_cli_false() {
        let args = make_args(Some(PathBuf::from(".")), None, false, None, false);
        let user_config = make_user_config(None, true, false);

        let config = Config::from_args_and_config(&args, &user_config).expect("should create config");

        assert!(config.print);
    }

    #[test]
    fn cli_verbose_true_overrides_config_verbose_false() {
        let args = make_args(Some(PathBuf::from(".")), None, false, None, true);
        let user_config = make_user_config(None, false, false);

        let config = Config::from_args_and_config(&args, &user_config).expect("should create config");

        assert!(config.verbose);
    }

    #[test]
    fn config_verbose_true_enables_verbose_when_cli_false() {
        let args = make_args(Some(PathBuf::from(".")), None, false, None, false);
        let user_config = make_user_config(None, false, true);

        let config = Config::from_args_and_config(&args, &user_config).expect("should create config");

        assert!(config.verbose);
    }

    #[test]
    fn both_cli_and_config_verbose_true_results_in_verbose() {
        let args = make_args(Some(PathBuf::from(".")), None, false, None, true);
        let user_config = make_user_config(None, false, true);

        let config = Config::from_args_and_config(&args, &user_config).expect("should create config");

        assert!(config.verbose);
    }

    #[test]
    fn both_cli_and_config_false_results_in_false() {
        let args = make_args(Some(PathBuf::from(".")), None, false, None, false);
        let user_config = make_user_config(None, false, false);

        let config = Config::from_args_and_config(&args, &user_config).expect("should create config");

        assert!(!config.print);
        assert!(!config.verbose);
    }

    #[test]
    fn all_options_combined() {
        let args = make_args(Some(PathBuf::from(".")), None, true, Some(42), true);
        let user_config = make_user_config(Some(99), true, true);

        let config = Config::from_args_and_config(&args, &user_config).expect("should create config");

        // CLI number takes priority
        assert_eq!(config.number, 42);
        // Both true = true (OR logic for booleans)
        assert!(config.print);
        assert!(config.verbose);
    }

    #[test]
    fn config_from_default_user_config() {
        let args = make_args(Some(PathBuf::from(".")), None, false, None, false);
        let user_config = VisaParseConfig::default();

        let config = Config::from_args_and_config(&args, &user_config).expect("should create config");

        assert_eq!(config.number, DEFAULT_NUM_TOTALS);
        assert!(!config.print);
        assert!(!config.verbose);
    }

    #[test]
    fn input_path_is_resolved() {
        let args = make_args(Some(PathBuf::from(".")), None, false, None, false);
        let user_config = VisaParseConfig::default();

        let config = Config::from_args_and_config(&args, &user_config).expect("should create config");

        // Input path should be resolved (not just ".")
        assert!(config.input_path.is_absolute() || config.input_path.components().count() > 1);
    }
}

#[cfg(test)]
mod test_config_cli_parsing {
    use super::*;
    use clap::Parser;

    #[test]
    fn parses_number_flag_short() {
        let args = VisaParseArgs::try_parse_from(["test", "-n", "50"]).expect("should parse");
        assert_eq!(args.number, Some(50));
    }

    #[test]
    fn parses_number_flag_long() {
        let args = VisaParseArgs::try_parse_from(["test", "--number", "100"]).expect("should parse");
        assert_eq!(args.number, Some(100));
    }

    #[test]
    fn parses_print_flag_short() {
        let args = VisaParseArgs::try_parse_from(["test", "-p"]).expect("should parse");
        assert!(args.print);
    }

    #[test]
    fn parses_print_flag_long() {
        let args = VisaParseArgs::try_parse_from(["test", "--print"]).expect("should parse");
        assert!(args.print);
    }

    #[test]
    fn parses_verbose_flag_short() {
        let args = VisaParseArgs::try_parse_from(["test", "-v"]).expect("should parse");
        assert!(args.verbose);
    }

    #[test]
    fn parses_verbose_flag_long() {
        let args = VisaParseArgs::try_parse_from(["test", "--verbose"]).expect("should parse");
        assert!(args.verbose);
    }

    #[test]
    fn parses_output_flag_short() {
        let args = VisaParseArgs::try_parse_from(["test", "-o", "/output/path"]).expect("should parse");
        assert_eq!(args.output, Some("/output/path".to_string()));
    }

    #[test]
    fn parses_output_flag_long() {
        let args = VisaParseArgs::try_parse_from(["test", "--output", "/output/path"]).expect("should parse");
        assert_eq!(args.output, Some("/output/path".to_string()));
    }

    #[test]
    fn parses_path_positional() {
        let args = VisaParseArgs::try_parse_from(["test", "/some/path"]).expect("should parse");
        assert_eq!(args.path, Some(PathBuf::from("/some/path")));
    }

    #[test]
    fn parses_combined_flags() {
        let args = VisaParseArgs::try_parse_from(["test", "-pv", "-n", "25"]).expect("should parse");
        assert!(args.print);
        assert!(args.verbose);
        assert_eq!(args.number, Some(25));
    }

    #[test]
    fn parses_all_options() {
        let args =
            VisaParseArgs::try_parse_from(["test", "/input/file.xml", "-o", "/output/dir", "-p", "-n", "30", "-v"])
                .expect("should parse");

        assert_eq!(args.path, Some(PathBuf::from("/input/file.xml")));
        assert_eq!(args.output, Some("/output/dir".to_string()));
        assert!(args.print);
        assert_eq!(args.number, Some(30));
        assert!(args.verbose);
    }

    #[test]
    fn defaults_when_no_args() {
        let args = VisaParseArgs::try_parse_from(["test"]).expect("should parse");
        assert!(args.path.is_none());
        assert!(args.output.is_none());
        assert!(!args.print);
        assert!(args.number.is_none());
        assert!(!args.verbose);
    }

    #[test]
    fn rejects_invalid_number() {
        let result = VisaParseArgs::try_parse_from(["test", "-n", "not_a_number"]);
        assert!(result.is_err());
    }

    #[test]
    fn rejects_negative_number() {
        let result = VisaParseArgs::try_parse_from(["test", "-n", "-5"]);
        assert!(result.is_err());
    }
}

#[cfg(test)]
mod test_config_integration {
    use super::*;

    #[test]
    fn cli_args_with_toml_config_merges_correctly() {
        let toml = r"
[visaparse]
number = 100
verbose = true
";
        let user_config = VisaParseConfig::from_toml_str(toml).expect("should parse");

        // CLI provides print=true and number=50, config has number=100 and verbose=true
        let args = VisaParseArgs {
            path: Some(PathBuf::from(".")),
            output: None,
            print: true,
            number: Some(50),
            verbose: false,
        };

        let config = Config::from_args_and_config(&args, &user_config).expect("should create config");

        // CLI number takes priority
        assert_eq!(config.number, 50);
        // CLI print is true
        assert!(config.print);
        // Config verbose is true (OR with CLI false)
        assert!(config.verbose);
    }

    #[test]
    fn empty_toml_uses_defaults() {
        let toml = "";
        let user_config = VisaParseConfig::from_toml_str(toml).expect("should parse");

        let args = VisaParseArgs {
            path: Some(PathBuf::from(".")),
            output: None,
            print: false,
            number: None,
            verbose: false,
        };

        let config = Config::from_args_and_config(&args, &user_config).expect("should create config");

        assert_eq!(config.number, DEFAULT_NUM_TOTALS);
        assert!(!config.print);
        assert!(!config.verbose);
    }

    #[test]
    fn full_toml_config_no_cli_overrides() {
        let toml = r"
[visaparse]
number = 42
print = true
verbose = true
";
        let user_config = VisaParseConfig::from_toml_str(toml).expect("should parse");

        let args = VisaParseArgs {
            path: Some(PathBuf::from(".")),
            output: None,
            print: false,
            number: None,
            verbose: false,
        };

        let config = Config::from_args_and_config(&args, &user_config).expect("should create config");

        assert_eq!(config.number, 42);
        assert!(config.print);
        assert!(config.verbose);
    }

    #[test]
    fn cli_zero_number_is_valid_override() {
        let toml = r"
[visaparse]
number = 100
";
        let user_config = VisaParseConfig::from_toml_str(toml).expect("should parse");

        let args = VisaParseArgs {
            path: Some(PathBuf::from(".")),
            output: None,
            print: false,
            number: Some(0),
            verbose: false,
        };

        let config = Config::from_args_and_config(&args, &user_config).expect("should create config");

        // CLI number of 0 should override config
        assert_eq!(config.number, 0);
    }
}

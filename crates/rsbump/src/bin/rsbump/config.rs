//! Configuration for rsbump.
//!
//! Combines the CLI arguments with the `[rsbump]` section of the user config file.

use std::num::NonZeroUsize;
use std::path::PathBuf;

use anyhow::Result;
use serde::Deserialize;

use crate::Args;
use cli_tools_rsbump::Options;

/// Default search depth for projects under the input paths.
const DEFAULT_MAX_DEPTH: usize = 5;

/// Config from the `[rsbump]` section of the user config file.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
#[expect(clippy::struct_excessive_bools, reason = "independent config flags")]
struct RsbumpConfig {
    #[serde(default)]
    update: bool,
    #[serde(default)]
    clean: bool,
    #[serde(default)]
    check: bool,
    #[serde(default)]
    commit: bool,
    #[serde(default)]
    force: bool,
    #[serde(default)]
    verbose: bool,
    #[serde(default)]
    jobs: Option<usize>,
    #[serde(default)]
    max_depth: Option<usize>,
}

/// Final config combined from CLI arguments and user config file.
#[derive(Debug)]
pub struct Config {
    /// Resolved input paths.
    pub(crate) paths: Vec<PathBuf>,
    /// Step options for each project.
    pub(crate) options: Options,
    /// Number of projects processed in parallel.
    pub(crate) jobs: usize,
    /// Print the output of successful commands and the steps that changed nothing.
    pub(crate) verbose: bool,
    /// Search depth for projects.
    pub(crate) max_depth: usize,
}

impl RsbumpConfig {
    /// Try to read user config from the file if it exists.
    /// Otherwise, fall back to default config.
    ///
    /// # Errors
    /// Returns an error if config file exists but cannot be read or parsed.
    fn get_user_config() -> Result<Self> {
        cli_tools_common::load_user_config_section("rsbump")
    }

    /// Parse config from a TOML string.
    ///
    /// # Errors
    /// Returns an error if the TOML string is invalid.
    #[cfg(test)]
    fn from_toml_str(toml_str: &str) -> Result<Self> {
        cli_tools_common::parse_user_config_section(toml_str, "rsbump")
    }
}

impl Config {
    /// Create config from given command line args and user config file.
    ///
    /// # Errors
    /// Returns an error if the config file cannot be read or parsed, or an input path cannot be resolved.
    pub fn from_args(args: &Args) -> Result<Self> {
        let user_config = RsbumpConfig::get_user_config()?;
        Self::from_args_and_user_config(args, &user_config)
    }

    /// Combine the CLI arguments with an already loaded user config.
    fn from_args_and_user_config(args: &Args, user_config: &RsbumpConfig) -> Result<Self> {
        let paths = if args.paths.is_empty() {
            vec![cli_tools_common::resolve_input_path(None)?]
        } else {
            args.paths
                .iter()
                .map(|path| cli_tools_common::resolve_input_path(Some(path)))
                .collect::<Result<Vec<_>>>()?
        };
        let jobs = match args.jobs.or(user_config.jobs) {
            Some(0) => anyhow::bail!("jobs must be at least 1"),
            Some(jobs) => jobs,
            None => std::thread::available_parallelism().map_or(4, NonZeroUsize::get),
        };

        Ok(Self {
            paths,
            options: Options {
                update: args.update || user_config.update,
                clean: args.clean || user_config.clean,
                check: args.check || user_config.check,
                commit: args.commit || user_config.commit,
                force: args.force || user_config.force,
                dry_run: args.print,
            },
            jobs,
            verbose: args.verbose || user_config.verbose,
            max_depth: args.max_depth.or(user_config.max_depth).unwrap_or(DEFAULT_MAX_DEPTH),
        })
    }
}

#[cfg(test)]
mod test_rsbump_config {
    use super::*;

    use clap::Parser;

    #[test]
    fn empty_config_uses_defaults() {
        let config = RsbumpConfig::from_toml_str("").expect("empty config should parse");
        assert!(!config.update);
        assert!(!config.commit);
        assert_eq!(config.jobs, None);
    }

    #[test]
    fn parses_rsbump_section_and_ignores_others() {
        let toml = r"
[video_stats]
recurse = true

[rsbump]
update = true
check = true
jobs = 2
max_depth = 3
";
        let config = RsbumpConfig::from_toml_str(toml).expect("config should parse");
        assert!(config.update);
        assert!(config.check);
        assert!(!config.clean);
        assert_eq!(config.jobs, Some(2));
        assert_eq!(config.max_depth, Some(3));
    }

    #[test]
    fn invalid_value_type_returns_error() {
        let toml = "[rsbump]\nupdate = \"yes\"\n";
        assert!(RsbumpConfig::from_toml_str(toml).is_err());
    }

    #[test]
    fn args_and_config_are_combined() {
        let args = Args::try_parse_from(["rsbump", "-c", "-j", "3"]).expect("valid args");
        let user_config = RsbumpConfig {
            update: true,
            jobs: Some(8),
            max_depth: Some(2),
            ..RsbumpConfig::default()
        };
        let config = Config::from_args_and_user_config(&args, &user_config).expect("valid config");
        assert!(config.options.update);
        assert!(config.options.clean);
        assert!(!config.options.check);
        assert_eq!(config.jobs, 3);
        assert_eq!(config.max_depth, 2);
        assert_eq!(config.paths.len(), 1);
    }
}

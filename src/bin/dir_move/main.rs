mod config;
mod dir_move;

use std::path::PathBuf;

use clap::{CommandFactory, Parser};
use clap_complete::Shell;

use crate::dir_move::DirMove;

#[derive(Parser)]
#[command(author, version, name = env!("CARGO_BIN_NAME"), about = "Move files to directories based on name")]
struct DirMoveArgs {
    /// Optional input directory or file
    #[arg(value_hint = clap::ValueHint::AnyPath)]
    path: Option<PathBuf>,

    /// Auto-confirm all prompts without asking
    #[arg(short = 'a', long)]
    auto: bool,

    /// Create directories for files with matching prefixes
    #[arg(short = 'c', long)]
    create: bool,

    /// Print debug information
    #[arg(short = 'D', long)]
    debug: bool,

    /// Overwrite existing files
    #[arg(short = 'f', long)]
    force: bool,

    /// Include files that match the given pattern
    #[arg(short = 'n', long, num_args = 1, action = clap::ArgAction::Append, name = "INCLUDE")]
    include: Vec<String>,

    /// Exclude files that match the given pattern
    #[arg(short = 'e', long, num_args = 1, action = clap::ArgAction::Append, name = "EXCLUDE")]
    exclude: Vec<String>,

    /// Ignore prefix when matching filenames
    #[arg(short = 'i', long = "ignore", num_args = 1, action = clap::ArgAction::Append, name = "IGNORE")]
    prefix_ignore: Vec<String>,

    /// Override prefix to use for directory names
    #[arg(short = 'o', long = "override", num_args = 1, action = clap::ArgAction::Append, name = "OVERRIDE")]
    prefix_override: Vec<String>,

    /// Directory name to "unpack" by moving its contents to the parent directory
    #[arg(short = 'u', long = "unpack", num_args = 1, action = clap::ArgAction::Append, name = "NAME")]
    unpack_directory: Vec<String>,

    /// Minimum number of matching files needed to create a group
    #[arg(short = 'g', long, name = "COUNT", default_value_t = 3)]
    group: usize,

    /// Only print changes without moving files
    #[arg(short = 'p', long)]
    print: bool,

    /// Recurse into subdirectories
    #[arg(short = 'r', long)]
    recurse: bool,

    /// Generate shell completion
    #[arg(short = 'l', long, name = "SHELL")]
    completion: Option<Shell>,

    /// Print verbose output
    #[arg(short = 'v', long)]
    verbose: bool,
}

fn main() -> anyhow::Result<()> {
    let args = DirMoveArgs::parse();
    if let Some(ref shell) = args.completion {
        cli_tools::generate_shell_completion(*shell, DirMoveArgs::command(), true, env!("CARGO_BIN_NAME"))
    } else {
        DirMove::try_from_args(args)?.run()
    }
}

#[cfg(test)]
mod cli_args_tests {
    use super::*;
    use crate::config::Config;

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
        assert_eq!(args.group, 5);
    }

    #[test]
    fn default_group_size_is_3() {
        let args = DirMoveArgs::try_parse_from(["test"]).expect("should parse");
        assert_eq!(args.group, 3);
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
    fn empty_arrays_by_default() {
        let args = DirMoveArgs::try_parse_from(["test"]).expect("should parse");
        assert!(args.include.is_empty());
        assert!(args.exclude.is_empty());
        assert!(args.prefix_ignore.is_empty());
        assert!(args.prefix_override.is_empty());
        assert!(args.unpack_directory.is_empty());
    }

    #[test]
    fn config_from_args_includes_cli_patterns() {
        let args = DirMoveArgs::try_parse_from(["test", "-n", "*.mp4", "-n", "*.mkv"]).expect("should parse");
        let config = Config::from_args(args);
        // CLI patterns should be included (may also have user config patterns)
        assert!(config.include.contains(&"*.mp4".to_string()));
        assert!(config.include.contains(&"*.mkv".to_string()));
    }

    #[test]
    fn config_from_args_cli_flags_enable_options() {
        // CLI boolean flags should enable options (OR with user config)
        let args = DirMoveArgs::try_parse_from(["test", "-a", "-c", "-r", "-v"]).expect("should parse");
        let config = Config::from_args(args);
        assert!(config.auto);
        assert!(config.create);
        assert!(config.recurse);
        assert!(config.verbose);
    }

    #[test]
    fn config_from_args_includes_unpack_dirs_lowercase() {
        let args = DirMoveArgs::try_parse_from(["test", "-u", "SUBS", "-u", "Sample"]).expect("should parse");
        let config = Config::from_args(args);
        // CLI unpack dirs should be included as lowercase
        assert!(config.unpack_directory_names.contains(&"subs".to_string()));
        assert!(config.unpack_directory_names.contains(&"sample".to_string()));
    }

    #[test]
    fn config_from_args_print_enables_dryrun() {
        let args = DirMoveArgs::try_parse_from(["test", "-p"]).expect("should parse");
        let config = Config::from_args(args);
        assert!(config.dryrun);
    }

    #[test]
    fn config_from_args_force_enables_overwrite() {
        let args = DirMoveArgs::try_parse_from(["test", "-f"]).expect("should parse");
        let config = Config::from_args(args);
        assert!(config.overwrite);
    }
}

mod config;
mod dupe_find;
mod tui;

use std::path::PathBuf;

use clap::{CommandFactory, Parser};
use clap_complete::Shell;

use crate::dupe_find::DupeFind;

#[derive(Parser)]
#[command(author, version, name = env!("CARGO_BIN_NAME"), about = "Find duplicate video files based on identifier patterns")]
struct Args {
    /// Input directories to search
    #[arg(value_hint = clap::ValueHint::DirPath)]
    paths: Vec<PathBuf>,

    /// Identifier patterns to search for (regex)
    #[arg(short = 'g', long, num_args = 1, action = clap::ArgAction::Append, name = "PATTERN")]
    pattern: Vec<String>,

    /// File extensions to include
    #[arg(short = 'e', long, num_args = 1, action = clap::ArgAction::Append, name = "EXTENSION")]
    extension: Vec<String>,

    /// Move duplicates to a "Duplicates" directory
    #[arg(short = 'm', long = "move")]
    move_files: bool,

    /// Only print changes without moving files
    #[arg(short = 'p', long)]
    print: bool,

    /// Recurse into subdirectories
    #[arg(short = 'r', long)]
    recurse: bool,

    /// Use default paths from config file
    #[arg(short = 'd', long)]
    default: bool,

    /// Generate shell completion
    #[arg(short = 'l', long, name = "SHELL")]
    completion: Option<Shell>,

    /// Print verbose output
    #[arg(short = 'v', long)]
    verbose: bool,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    if let Some(ref shell) = args.completion {
        cli_tools::generate_shell_completion(*shell, Args::command(), true, env!("CARGO_BIN_NAME"))
    } else {
        DupeFind::new(args)?.run()
    }
}

#[cfg(test)]
mod cli_args_tests {
    use super::*;

    #[test]
    fn parses_multiple_pattern_args() {
        let args = Args::try_parse_from(["test", "-g", "ABC-\\d+", "-g", "XYZ-\\d+"]).expect("should parse");
        assert_eq!(args.pattern.len(), 2);
        assert_eq!(args.pattern[0], "ABC-\\d+");
        assert_eq!(args.pattern[1], "XYZ-\\d+");
    }

    #[test]
    fn parses_multiple_extension_args() {
        let args = Args::try_parse_from(["test", "-e", "mp4", "-e", "mkv", "-e", "avi"]).expect("should parse");
        assert_eq!(args.extension, vec!["mp4", "mkv", "avi"]);
    }

    #[test]
    fn parses_multiple_paths() {
        let args = Args::try_parse_from(["test", "/path/one", "/path/two", "/path/three"]).expect("should parse");
        assert_eq!(args.paths.len(), 3);
    }

    #[test]
    fn parses_long_form_pattern() {
        let args = Args::try_parse_from(["test", "--pattern", "TEST-\\d{4}"]).expect("should parse");
        assert_eq!(args.pattern, vec!["TEST-\\d{4}"]);
    }

    #[test]
    fn parses_long_form_extension() {
        let args = Args::try_parse_from(["test", "--extension", "mp4"]).expect("should parse");
        assert_eq!(args.extension, vec!["mp4"]);
    }

    #[test]
    fn parses_move_flag() {
        let args = Args::try_parse_from(["test", "-m"]).expect("should parse");
        assert!(args.move_files);

        let args = Args::try_parse_from(["test", "--move"]).expect("should parse");
        assert!(args.move_files);
    }

    #[test]
    fn parses_combined_flags() {
        let args = Args::try_parse_from(["test", "-mprv"]).expect("should parse");
        assert!(args.move_files);
        assert!(args.print);
        assert!(args.recurse);
        assert!(args.verbose);
    }

    #[test]
    fn parses_default_paths_flag() {
        let args = Args::try_parse_from(["test", "-d"]).expect("should parse");
        assert!(args.default);

        let args = Args::try_parse_from(["test", "--default"]).expect("should parse");
        assert!(args.default);
    }

    #[test]
    fn empty_by_default() {
        let args = Args::try_parse_from(["test"]).expect("should parse");
        assert!(args.paths.is_empty());
        assert!(args.pattern.is_empty());
        assert!(args.extension.is_empty());
        assert!(!args.move_files);
        assert!(!args.print);
        assert!(!args.recurse);
        assert!(!args.default);
        assert!(!args.verbose);
    }

    #[test]
    fn parses_complex_regex_pattern() {
        let args = Args::try_parse_from(["test", "-g", r"[A-Z]{3,4}-\d{3,4}"]).expect("should parse");
        assert_eq!(args.pattern[0], r"[A-Z]{3,4}-\d{3,4}");
    }

    #[test]
    fn parses_paths_with_patterns_and_extensions() {
        let args = Args::try_parse_from([
            "test", "/videos", "/movies", "-g", "ABC-\\d+", "-g", "XYZ-\\d+", "-e", "mp4", "-e", "mkv", "-r", "-v",
        ])
        .expect("should parse");

        assert_eq!(args.paths.len(), 2);
        assert_eq!(args.pattern.len(), 2);
        assert_eq!(args.extension.len(), 2);
        assert!(args.recurse);
        assert!(args.verbose);
    }
}

#[cfg(test)]
mod config_from_args_tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn config_includes_cli_regex_patterns() {
        let args = Args::try_parse_from(["test", "-g", "ABC-\\d+", "-g", "XYZ-\\d+"]).expect("should parse");
        let config = Config::from_args(args).expect("should create config");
        // CLI patterns should be included (may also have user config patterns)
        assert!(config.patterns.len() >= 2);
        // Verify CLI patterns work
        assert!(config.patterns.iter().any(|p| p.is_match("ABC-123")));
        assert!(config.patterns.iter().any(|p| p.is_match("XYZ-456")));
    }

    #[test]
    fn config_includes_cli_extensions_normalized() {
        let args = Args::try_parse_from(["test", "-e", ".MP4", "-e", "MKV", "-e", ".avi"]).expect("should parse");
        let config = Config::from_args(args).expect("should create config");
        // CLI extensions should be included as lowercase without leading dot
        assert!(config.extensions.contains(&"mp4".to_string()));
        assert!(config.extensions.contains(&"mkv".to_string()));
        assert!(config.extensions.contains(&"avi".to_string()));
    }

    #[test]
    fn config_rejects_invalid_regex() {
        let args = Args::try_parse_from(["test", "-g", "[invalid(regex"]).expect("should parse");
        let result = Config::from_args(args);
        assert!(result.is_err());
    }

    #[test]
    fn config_print_enables_dryrun() {
        let args = Args::try_parse_from(["test", "-p"]).expect("should parse");
        let config = Config::from_args(args).expect("should create config");
        assert!(config.dryrun);
    }

    #[test]
    fn config_has_extensions() {
        let args = Args::try_parse_from(["test"]).expect("should parse");
        let config = Config::from_args(args).expect("should create config");
        // Should have extensions (from CLI, config, or defaults)
        assert!(!config.extensions.is_empty());
    }

    #[test]
    fn config_move_flag_enables_move_files() {
        let args = Args::try_parse_from(["test", "-m"]).expect("should parse");
        let config = Config::from_args(args).expect("should create config");
        assert!(config.move_files);
    }

    #[test]
    fn config_recurse_flag_enables_recurse() {
        let args = Args::try_parse_from(["test", "-r"]).expect("should parse");
        let config = Config::from_args(args).expect("should create config");
        assert!(config.recurse);
    }

    #[test]
    fn config_verbose_flag_enables_verbose() {
        let args = Args::try_parse_from(["test", "-v"]).expect("should parse");
        let config = Config::from_args(args).expect("should create config");
        assert!(config.verbose);
    }
}

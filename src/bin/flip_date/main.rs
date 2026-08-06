//! Date-flipping command-line entrypoint.
//!
//! Parses file or directory mode options and delegates date transformations.

mod config;
mod flip_date;

use std::path::PathBuf;

use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::Shell;

#[derive(Parser)]
#[command(
    author,
    version,
    name = env!("CARGO_BIN_NAME"),
    about = "Flip dates in file and directory names to start with year"
)]
pub struct Args {
    #[command(subcommand)]
    command: Option<FlipDateCommand>,

    /// Optional input directory or file
    #[arg(value_hint = clap::ValueHint::AnyPath)]
    path: Option<PathBuf>,

    /// Use directory rename mode
    #[arg(short, long)]
    dir: bool,

    /// Overwrite existing
    #[arg(short, long)]
    force: bool,

    /// Specify file extensions
    #[arg(short, long, num_args = 1, action = clap::ArgAction::Append, value_name = "EXTENSION", conflicts_with = "dir")]
    extensions: Option<Vec<String>>,

    /// Assume year is first in short dates
    #[arg(short, long)]
    year: bool,

    /// Only print changes without renaming
    #[arg(short, long)]
    print: bool,

    /// Recurse into subdirectories
    #[arg(short, long)]
    recurse: bool,

    /// Swap year and day around
    #[arg(short, long)]
    swap: bool,

    /// Print verbose output
    #[arg(short, long, global = true)]
    verbose: bool,
}

/// Subcommands for flipdate.
#[derive(Subcommand)]
enum FlipDateCommand {
    /// Generate shell completion script
    #[command(name = "completion")]
    Completion {
        /// Shell to generate completion for
        #[arg(value_enum)]
        shell: Shell,

        /// Install completion script to the shell's completion directory
        #[arg(short = 'I', long)]
        install: bool,
    },
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    if let Some(FlipDateCommand::Completion { shell, install }) = &args.command {
        return cli_tools::generate_shell_completion(
            *shell,
            Args::command(),
            *install,
            args.verbose,
            env!("CARGO_BIN_NAME"),
        );
    }
    let path = cli_tools::resolve_input_path(args.path.as_deref())?;
    let config = flip_date::Config::from_args(args)?;
    if config.directory_mode {
        flip_date::date_flip_directories(path, &config)
    } else {
        flip_date::date_flip_files(&path, &config)
    }
}

#[cfg(test)]
mod test_args {
    use super::*;

    #[test]
    fn parses_defaults_and_all_operational_flags() {
        let defaults = Args::try_parse_from(["flipdate"]).expect("default arguments should parse");
        assert!(defaults.command.is_none());
        assert!(defaults.path.is_none());
        assert!(defaults.extensions.is_none());
        assert!(!defaults.dir);
        assert!(!defaults.force);
        assert!(!defaults.year);
        assert!(!defaults.print);
        assert!(!defaults.recurse);
        assert!(!defaults.swap);
        assert!(!defaults.verbose);

        let configured = Args::try_parse_from([
            "flipdate",
            "files",
            "--force",
            "--year",
            "--print",
            "--recurse",
            "--swap",
            "--verbose",
            "--extensions",
            "mp4",
            "--extensions",
            "mkv",
        ])
        .expect("operational flags should parse");
        assert_eq!(configured.path, Some(PathBuf::from("files")));
        assert_eq!(configured.extensions, Some(vec!["mp4".to_string(), "mkv".to_string()]));
        assert!(configured.force);
        assert!(configured.year);
        assert!(configured.print);
        assert!(configured.recurse);
        assert!(configured.swap);
        assert!(configured.verbose);
    }

    #[test]
    fn directory_mode_conflicts_with_extensions() {
        assert!(Args::try_parse_from(["flipdate", "--dir", "--extensions", "mp4"]).is_err());
    }

    #[test]
    fn parses_completion_and_has_valid_command_definition() {
        let args = Args::try_parse_from(["flipdate", "completion", "bash", "--install"])
            .expect("completion command should parse");
        assert!(matches!(
            args.command,
            Some(FlipDateCommand::Completion {
                shell: Shell::Bash,
                install: true
            })
        ));
        Args::command().debug_assert();
    }
}

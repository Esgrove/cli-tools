//! Video resolution command-line entrypoint.
//!
//! Parses rename and deletion options, builds configuration, and runs asynchronous processing.

mod cli;
mod config;
mod resolution;

use std::path::PathBuf;

use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::Shell;

use config::Config;

#[derive(Parser, Debug)]
#[command(author, version, name = env!("CARGO_BIN_NAME"), about = "Add video resolution to filenames")]
pub struct Args {
    #[command(subcommand)]
    command: Option<VideoResolutionCommand>,

    /// Optional input directory or file path
    #[arg(value_hint = clap::ValueHint::AnyPath)]
    path: Option<PathBuf>,

    /// Enable debug prints
    #[arg(short = 'D', long)]
    debug: bool,

    /// Delete files with width or height smaller than limit (default: 500)
    #[arg(short = 'x', long)]
    #[allow(clippy::option_option)]
    delete: Option<Option<u32>>,

    /// Overwrite existing files
    #[arg(short = 'f', long)]
    force: bool,

    /// Only print file names without renaming or deleting
    #[arg(short = 'p', long)]
    print: bool,

    /// Recurse into subdirectories
    #[arg(short = 'r', long)]
    recurse: bool,

    /// Print verbose output
    #[arg(short = 'v', long, global = true)]
    verbose: bool,
}

/// Subcommands for vres.
#[derive(Subcommand, Debug)]
enum VideoResolutionCommand {
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    if let Some(VideoResolutionCommand::Completion { shell, install }) = &args.command {
        return cli_tools::generate_shell_completion(
            *shell,
            Args::command(),
            *install,
            args.verbose,
            env!("CARGO_BIN_NAME"),
        );
    }
    let config = Config::try_from_args(&args)?;
    cli::run(config).await
}

#[cfg(test)]
mod test_args {
    use super::*;

    #[test]
    fn parses_defaults_and_operational_flags() {
        let defaults = Args::try_parse_from(["vres"]).expect("default arguments should parse");
        assert!(defaults.command.is_none());
        assert!(defaults.path.is_none());
        assert!(defaults.delete.is_none());
        assert!(!defaults.debug);
        assert!(!defaults.force);
        assert!(!defaults.print);
        assert!(!defaults.recurse);
        assert!(!defaults.verbose);

        let configured = Args::try_parse_from(["vres", "videos", "-D", "-f", "-p", "-r", "-v"])
            .expect("operational flags should parse");
        assert_eq!(configured.path, Some(PathBuf::from("videos")));
        assert!(configured.debug);
        assert!(configured.force);
        assert!(configured.print);
        assert!(configured.recurse);
        assert!(configured.verbose);
    }

    #[test]
    fn distinguishes_delete_without_and_with_limit() {
        let default_limit = Args::try_parse_from(["vres", "--delete"]).expect("bare delete should parse");
        assert_eq!(default_limit.delete, Some(None));

        let explicit_limit = Args::try_parse_from(["vres", "--delete", "720"]).expect("delete limit should parse");
        assert_eq!(explicit_limit.delete, Some(Some(720)));
        assert!(Args::try_parse_from(["vres", "--delete", "invalid"]).is_err());
    }

    #[test]
    fn parses_completion_and_has_valid_command_definition() {
        let args =
            Args::try_parse_from(["vres", "completion", "bash", "--install"]).expect("completion command should parse");
        assert!(matches!(
            args.command,
            Some(VideoResolutionCommand::Completion {
                shell: Shell::Bash,
                install: true
            })
        ));
        Args::command().debug_assert();
    }
}

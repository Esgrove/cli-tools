//! visaparse - Parse Finvoice XML credit card statement files.
//!
//! This CLI tool parses Finvoice XML credit card statement files and generates
//! CSV and Excel reports with purchase data and statistics.

mod config;
mod parse;

use std::path::PathBuf;

use anyhow::Result;
use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::Shell;

pub use crate::config::Config;
use crate::parse::visa_parse;

/// Command line arguments for visaparse.
#[derive(Parser, Debug)]
#[command(
    author,
    version,
    name = env!("CARGO_BIN_NAME"),
    about = "Parse Finvoice XML credit card statement files"
)]
pub struct VisaParseArgs {
    #[command(subcommand)]
    command: Option<VisaParseCommand>,

    /// Optional input directory or XML file path
    #[arg(value_hint = clap::ValueHint::AnyPath)]
    pub path: Option<PathBuf>,

    /// Optional output path (default is the input directory)
    #[arg(short, long, name = "OUTPUT_PATH")]
    pub output: Option<String>,

    /// Only print information without writing to file
    #[arg(short, long)]
    pub print: bool,

    /// How many total sums to print with verbose output
    #[arg(short, long)]
    pub number: Option<usize>,

    /// Print verbose output
    #[arg(short, long, global = true)]
    pub verbose: bool,
}

/// Subcommands for visaparse.
#[derive(Subcommand, Debug)]
enum VisaParseCommand {
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

fn main() -> Result<()> {
    let args = VisaParseArgs::parse();
    if let Some(VisaParseCommand::Completion { shell, install }) = &args.command {
        return cli_tools::generate_shell_completion(
            *shell,
            VisaParseArgs::command(),
            *install,
            args.verbose,
            env!("CARGO_BIN_NAME"),
        );
    }
    let config = Config::from_args(&args)?;
    visa_parse(&config)
}

#[cfg(test)]
mod test_args {
    use super::*;

    #[test]
    fn parses_defaults() {
        let args = VisaParseArgs::try_parse_from(["visaparse"]).expect("default arguments should parse");
        assert!(args.command.is_none());
        assert!(args.path.is_none());
        assert!(args.output.is_none());
        assert!(!args.print);
        assert!(args.number.is_none());
        assert!(!args.verbose);
    }

    #[test]
    fn parses_combined_flags() {
        let args = VisaParseArgs::try_parse_from(["visaparse", "statements", "-o", "out.csv", "-p", "-n", "12", "-v"])
            .expect("combined arguments should parse");
        assert_eq!(args.path, Some(PathBuf::from("statements")));
        assert_eq!(args.output.as_deref(), Some("out.csv"));
        assert!(args.print);
        assert_eq!(args.number, Some(12));
        assert!(args.verbose);
    }

    #[test]
    fn rejects_a_non_numeric_count() {
        assert!(VisaParseArgs::try_parse_from(["visaparse", "--number", "many"]).is_err());
    }

    #[test]
    fn parses_completion_and_has_valid_command_definition() {
        let args = VisaParseArgs::try_parse_from(["visaparse", "completion", "bash", "--install"])
            .expect("completion should parse");
        assert!(matches!(
            args.command,
            Some(VisaParseCommand::Completion {
                shell: Shell::Bash,
                install: true
            })
        ));
        VisaParseArgs::command().debug_assert();
    }
}

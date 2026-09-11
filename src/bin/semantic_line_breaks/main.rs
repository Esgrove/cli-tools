//! slb - Check and format prose with semantic line breaks.
//!
//! This CLI tool scans source files and Markdown documents for comments, docstrings, and paragraphs
//! that violate the semantic line break style,
//! reports the violations, and optionally reflows the prose and moves trailing comments above the code.
//!
//! This module defines the command line arguments and the entry point.
//! File discovery lives in [`files`], the check and fix modes in [`cli`], and reporting in [`output`].

mod cli;
mod config;
mod files;
mod output;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::Shell;

use cli_tools::print_error;
use cli_tools::semantic_line_breaks::FileKind;

#[derive(Parser)]
#[command(
    author,
    version,
    name = env!("CARGO_BIN_NAME"),
    about = "Check and format prose in comments, docstrings, and Markdown with semantic line breaks"
)]
pub struct Args {
    #[command(subcommand)]
    command: Option<SlbCommand>,

    /// Files or directories to check. Defaults to the current directory
    #[arg(value_hint = clap::ValueHint::AnyPath)]
    paths: Vec<PathBuf>,

    /// Rewrite files in place
    #[arg(short, long)]
    fix: bool,

    /// Show the changes fix mode would make without writing
    #[arg(short, long)]
    print: bool,

    /// Also pack consecutive short sentences up to the line limit
    #[arg(short, long)]
    join_sentences: bool,

    /// Maximum line length including indentation and comment marker (default: from project config or 120)
    #[arg(short, long, value_name = "N")]
    width: Option<usize>,

    /// Do not read the line length from project config files such as .editorconfig, rustfmt.toml, and pyproject.toml
    #[arg(short, long)]
    ignore_project_config: bool,

    /// Rules to enable (default: all)
    #[arg(short = 'R', long, value_delimiter = ',', value_name = "RULES")]
    rules: Vec<cli_tools::semantic_line_breaks::ViolationKind>,

    /// Only process files with these extensions
    #[arg(short, long, num_args = 1, action = clap::ArgAction::Append, value_name = "EXTENSION")]
    extensions: Vec<String>,

    /// Skip paths with a directory or file name equal to this text
    #[arg(short = 'x', long, num_args = 1, action = clap::ArgAction::Append, value_name = "PATTERN")]
    exclude: Vec<String>,

    /// Force the file kind, required with --stdin
    #[arg(short = 't', long = "type", value_name = "KIND")]
    kind: Option<FileKind>,

    /// Read text from stdin and write the formatted result to stdout
    #[arg(short, long)]
    stdin: bool,

    /// Allow breaking at a plain word boundary when no clause boundary fits
    #[arg(short = 'b', long)]
    word_break: bool,

    /// Only print the summary
    #[arg(short, long)]
    quiet: bool,

    /// Print processed files and the resolved line width
    #[arg(short, long, global = true)]
    verbose: bool,
}

/// Subcommands for `slb`.
#[derive(Subcommand)]
enum SlbCommand {
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

/// Parse command line arguments and run the requested mode.
fn main() -> ExitCode {
    let args = Args::parse();
    let result = if let Some(SlbCommand::Completion { shell, install }) = &args.command {
        cli_tools::generate_shell_completion(*shell, Args::command(), *install, args.verbose, env!("CARGO_BIN_NAME"))
            .map(|()| ExitCode::SUCCESS)
    } else {
        cli::run(&args)
    };
    match result {
        Ok(code) => code,
        Err(error) => {
            print_error!("{error:#}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod test_args {
    use super::*;

    #[test]
    fn parses_defaults() {
        let args = Args::try_parse_from(["slb"]).expect("default arguments should parse");
        assert!(args.command.is_none());
        assert!(args.paths.is_empty());
        assert!(!args.fix);
        assert!(!args.print);
        assert!(args.width.is_none());
        assert!(args.rules.is_empty());
        assert!(args.kind.is_none());
        assert!(!args.stdin);
    }

    #[test]
    fn parses_combined_flags() {
        let args = Args::try_parse_from([
            "slb",
            "src",
            "README.md",
            "--fix",
            "--print",
            "-j",
            "-w",
            "100",
            "-R",
            "too-long,mid-clause",
            "-e",
            "rs",
            "-e",
            "md",
            "-x",
            "target",
            "-t",
            "rust",
            "-b",
            "-q",
            "-v",
        ])
        .expect("combined arguments should parse");
        assert_eq!(args.paths, vec![PathBuf::from("src"), PathBuf::from("README.md")]);
        assert!(args.fix);
        assert!(args.print);
        assert!(args.join_sentences);
        assert_eq!(args.width, Some(100));
        assert_eq!(args.rules.len(), 2);
        assert_eq!(args.extensions, vec!["rs", "md"]);
        assert_eq!(args.exclude, vec!["target"]);
        assert_eq!(args.kind, Some(FileKind::Rust));
        assert!(args.word_break);
        assert!(args.quiet);
        assert!(args.verbose);
    }

    #[test]
    fn rejects_unknown_rule() {
        assert!(Args::try_parse_from(["slb", "--rules", "bogus"]).is_err());
    }

    #[test]
    fn parses_completion_and_has_valid_command_definition() {
        let args = Args::try_parse_from(["slb", "completion", "zsh", "--install"]).expect("completion should parse");
        assert!(matches!(
            args.command,
            Some(SlbCommand::Completion {
                shell: Shell::Zsh,
                install: true
            })
        ));
        Args::command().debug_assert();
    }
}

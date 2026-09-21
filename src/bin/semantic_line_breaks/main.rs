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
mod line_selection;
mod output;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::Shell;

use cli_tools::print_error;
use cli_tools::semantic_line_breaks::FileKind;

use crate::line_selection::LineSpec;

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
    #[arg(short, long, conflicts_with = "fix")]
    print: bool,

    /// Also pack consecutive short sentences up to the line limit
    #[arg(short, long)]
    join_sentences: bool,

    /// Maximum line length including indentation and comment marker (default: from project config or 120)
    #[arg(short, long, value_name = "N")]
    width: Option<usize>,

    /// Do not read the line length from project config files such as .editorconfig, rustfmt.toml, or pyproject.toml
    #[arg(short, long)]
    ignore_project_config: bool,

    /// Rules to enable
    #[arg(short = 'R', long, value_delimiter = ',', value_name = "RULES")]
    rules: Vec<cli_tools::semantic_line_breaks::ViolationKind>,

    /// Also move trailing comments to their own line above the code
    #[arg(short = 'T', long)]
    trailing: bool,

    /// Only check and fix these lines, for example "10-25" or "src/main.rs:14"
    ///
    /// Takes lines and ranges for a single file, such as "10" or "10-25,40",
    /// or locations in the form the report prints, such as "src/main.rs:14" or "src/main.rs:14-20",
    /// so a reported violation can be pasted back in as the thing to fix.
    /// A column after the line is ignored.
    /// Commas separate values, and a range following a location belongs to the file it named.
    /// A paragraph overlapping the selection is reflowed in full,
    /// since reflow joins and splits a paragraph as one unit.
    #[arg(short = 'l', long, num_args = 1, action = clap::ArgAction::Append, value_name = "RANGES")]
    lines: Vec<LineSpec>,

    /// Only process files with these extensions
    #[arg(short, long, num_args = 1, action = clap::ArgAction::Append, value_name = "EXTENSION")]
    extensions: Vec<String>,

    /// Skip paths with a directory or file name equal to this text, in addition to the default excludes
    #[arg(short = 'x', long, num_args = 1, action = clap::ArgAction::Append, value_name = "PATTERN")]
    exclude: Vec<String>,

    /// Do not skip paths ignored by git
    #[arg(short = 'n', long)]
    no_ignore: bool,

    /// Force the file kind, required with --stdin
    #[arg(short = 't', long = "type", value_name = "KIND")]
    kind: Option<FileKind>,

    /// Read text from stdin and write the formatted result to stdout
    #[arg(short, long)]
    stdin: bool,

    /// Allow plain word boundaries even when a semantic boundary also fits
    #[arg(short = 'b', long)]
    word_break: bool,

    /// Treat the width as a hard cap instead of allowing a small overflow past it
    #[arg(short = 'S', long)]
    strict: bool,

    /// Number of worker threads, 0 for one per core
    #[arg(short = 'J', long, value_name = "N", default_value_t = 0)]
    jobs: usize,

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
            "-S",
            "-q",
            "-v",
        ])
        .expect("combined arguments should parse");
        assert_eq!(args.paths, vec![PathBuf::from("src"), PathBuf::from("README.md")]);
        assert!(args.fix);
        assert!(args.join_sentences);
        assert_eq!(args.width, Some(100));
        assert_eq!(args.rules.len(), 2);
        assert_eq!(args.extensions, vec!["rs", "md"]);
        assert_eq!(args.exclude, vec!["target"]);
        assert_eq!(args.kind, Some(FileKind::Rust));
        assert!(args.word_break);
        assert!(args.strict);
        assert!(args.quiet);
        assert!(args.verbose);
    }

    #[test]
    fn rejects_printing_and_fixing_together() {
        // Fix mode writes the files, which is exactly what print mode promises not to do.
        assert!(Args::try_parse_from(["slb", "--fix", "--print"]).is_err());
        assert!(Args::try_parse_from(["slb", "-p", "-f"]).is_err());
        assert!(Args::try_parse_from(["slb", "--fix"]).is_ok());
        assert!(Args::try_parse_from(["slb", "--print"]).is_ok());
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

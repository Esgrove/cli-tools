//! Entry point and CLI argument definitions for `dirmove`.

#![cfg_attr(test, allow(clippy::panic_in_result_fn))]

mod config;
mod database;
mod dir_move;
mod prompt;
mod report;

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use cli_tools_common::CompletionArgs;

use crate::dir_move::DirMove;

/// Subcommands for `dirmove database`.
#[derive(Subcommand)]
enum DatabaseAction {
    /// List stored directory names (default when no subcommand is given)
    List,
    /// Add or update a directory name in the database
    Add {
        /// Directory name to store
        name: String,
    },
    /// Remove a directory name from the database
    Remove {
        /// Directory name to remove
        name: String,
    },
    /// Clear all directory names from the database
    Clear,
}

/// Subcommands for dirmove.
#[derive(Subcommand)]
enum DirMoveCommand {
    /// Generate shell completion script
    #[command(name = "completion")]
    Completion(CompletionArgs),

    /// Inspect or manage directory names stored in the database
    #[command(name = "database", visible_alias = "db")]
    Database {
        #[command(subcommand)]
        action: Option<DatabaseAction>,
    },
}

/// Command-line arguments for the `dirmove` binary.
#[derive(Parser)]
#[command(author, version = cli_tools_version::VERSION_INFO, name = env!("CARGO_BIN_NAME"), about = "Move files to directories based on name")]
struct DirMoveArgs {
    #[command(subcommand)]
    command: Option<DirMoveCommand>,

    /// Optional input directories or files
    #[arg(value_hint = clap::ValueHint::AnyPath, num_args = 0..)]
    path: Vec<PathBuf>,

    /// Optional output directory, defaults to the input directory
    #[arg(short = 'O', long, value_hint = clap::ValueHint::DirPath, num_args = 1, action = clap::ArgAction::Append)]
    output: Vec<PathBuf>,

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

    /// Only match input files, do not offer directory merges
    #[arg(short = 'F', long)]
    files_only: bool,

    /// Include files that match the given pattern
    #[arg(short = 'n', long, num_args = 1, action = clap::ArgAction::Append, name = "INCLUDE")]
    include: Vec<String>,

    /// Exclude files that match the given pattern
    #[arg(short = 'e', long, num_args = 1, action = clap::ArgAction::Append, name = "EXCLUDE")]
    exclude: Vec<String>,

    /// Ignore prefix when matching filenames
    #[arg(short = 'i', long = "ignore", num_args = 1, action = clap::ArgAction::Append, name = "IGNORE")]
    prefix_ignore: Vec<String>,

    /// Group name to ignore
    #[arg(short = 'I', long = "ignore-group", num_args = 1, action = clap::ArgAction::Append, name = "GROUP")]
    ignored_group_name: Vec<String>,

    /// Ignore groups with a name part equal to this, ignoring case
    #[arg(short = 'P', long = "ignore-group-part", num_args = 1, action = clap::ArgAction::Append, name = "PART")]
    ignored_group_part: Vec<String>,

    /// Override prefix to use for directory names
    #[arg(short = 'o', long = "override", num_args = 1, action = clap::ArgAction::Append, name = "OVERRIDE")]
    prefix_override: Vec<String>,

    /// Directory name to "unpack" by moving its contents to the parent directory
    #[arg(short = 'u', long = "unpack", num_args = 1, action = clap::ArgAction::Append, name = "NAME")]
    unpack_directory: Vec<String>,

    /// Name to directory mapping pair (pattern:dirname)
    #[arg(short = 'M', long = "map", num_args = 1, action = clap::ArgAction::Append, name = "MAPPING")]
    custom_mapping: Vec<String>,

    /// Minimum number of matching files needed to create a group
    #[arg(short = 'g', long, name = "COUNT")]
    group: Option<usize>,

    /// Minimum character count for prefixes to be valid group names
    #[arg(short = 'm', long = "min-chars", name = "CHARS")]
    min_prefix_chars: Option<usize>,

    /// Only print changes without moving files
    #[arg(short = 'p', long)]
    print: bool,

    /// Recurse into subdirectories
    #[arg(short = 'r', long)]
    recurse: bool,

    /// Print verbose output
    #[arg(short = 'v', long, global = true)]
    verbose: bool,
}

/// Parse CLI arguments and run the requested `dirmove` command.
fn main() -> anyhow::Result<()> {
    let args = DirMoveArgs::parse();
    match &args.command {
        Some(DirMoveCommand::Completion(completion)) => {
            completion.generate::<DirMoveArgs>(args.verbose, env!("CARGO_BIN_NAME"))
        }
        Some(DirMoveCommand::Database { action }) => match action {
            None | Some(DatabaseAction::List) => database::print_entries(),
            Some(DatabaseAction::Add { name }) => database::add_entry(name),
            Some(DatabaseAction::Remove { name }) => database::remove_entry(name),
            Some(DatabaseAction::Clear) => database::clear_entries(),
        },
        None => DirMove::try_from_args(args)?.run(),
    }
}

#[cfg(test)]
mod test_args {
    use super::*;

    #[test]
    fn parses_defaults() {
        let args = DirMoveArgs::try_parse_from(["dirmove"]).expect("default arguments should parse");
        assert!(args.command.is_none());
        assert_eq!(args.path, [] as [PathBuf; 0]);
        assert_eq!(args.output, [] as [PathBuf; 0]);
        assert!(!args.auto);
        assert!(!args.create);
        assert!(!args.debug);
        assert!(!args.force);
        assert!(!args.files_only);
        assert!(args.group.is_none());
        assert!(args.min_prefix_chars.is_none());
        assert!(!args.print);
        assert!(!args.recurse);
        assert!(!args.verbose);
    }

    #[test]
    fn parses_combined_flags() {
        let args = DirMoveArgs::try_parse_from([
            "dirmove", "input", "second", "-O", "out", "-a", "-c", "-D", "-f", "-F", "-p", "-r", "-v", "-g", "3", "-m",
            "4",
        ])
        .expect("combined arguments should parse");
        assert_eq!(args.path, vec![PathBuf::from("input"), PathBuf::from("second")]);
        assert_eq!(args.output, vec![PathBuf::from("out")]);
        assert!(args.auto);
        assert!(args.create);
        assert!(args.debug);
        assert!(args.force);
        assert!(args.files_only);
        assert!(args.print);
        assert!(args.recurse);
        assert!(args.verbose);
        assert_eq!(args.group, Some(3));
        assert_eq!(args.min_prefix_chars, Some(4));
    }

    #[test]
    fn the_repeatable_pattern_options_collect_every_value() {
        let args = DirMoveArgs::try_parse_from([
            "dirmove",
            "-n",
            "keep",
            "-n",
            "also-keep",
            "-e",
            "drop",
            "-i",
            "prefix",
            "-I",
            "group",
            "-P",
            "part",
            "-o",
            "override",
            "-u",
            "unpack",
            "-M",
            "pattern:dirname",
        ])
        .expect("repeated arguments should parse");
        assert_eq!(args.include, vec!["keep", "also-keep"]);
        assert_eq!(args.exclude, vec!["drop"]);
        assert_eq!(args.prefix_ignore, vec!["prefix"]);
        assert_eq!(args.ignored_group_name, vec!["group"]);
        assert_eq!(args.ignored_group_part, vec!["part"]);
        assert_eq!(args.prefix_override, vec!["override"]);
        assert_eq!(args.unpack_directory, vec!["unpack"]);
        assert_eq!(args.custom_mapping, vec!["pattern:dirname"]);
    }

    #[test]
    fn rejects_an_unknown_option() {
        assert!(DirMoveArgs::try_parse_from(["dirmove", "--bogus"]).is_err());
    }

    #[test]
    fn parses_completion_and_has_valid_command_definition() {
        use clap::CommandFactory;
        use clap_complete::Shell;

        let args = DirMoveArgs::try_parse_from(["dirmove", "completion", "zsh", "--install"])
            .expect("completion should parse");
        assert!(matches!(
            args.command,
            Some(DirMoveCommand::Completion(CompletionArgs {
                shell: Shell::Zsh,
                install: true
            }))
        ));
        let db_remove =
            DirMoveArgs::try_parse_from(["dirmove", "db", "remove", "OldShow"]).expect("db remove should parse");
        assert!(matches!(
            db_remove.command,
            Some(DirMoveCommand::Database {
                action: Some(DatabaseAction::Remove { .. })
            })
        ));
        DirMoveArgs::command().debug_assert();
    }

    #[test]
    fn parses_database_subcommand_and_alias() {
        for name in ["database", "db"] {
            let args = DirMoveArgs::try_parse_from(["dirmove", name]).expect("database command should parse");
            assert!(matches!(args.command, Some(DirMoveCommand::Database { action: None })));
        }
    }
}

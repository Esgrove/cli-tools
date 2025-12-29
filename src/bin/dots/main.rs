//! dots - Rename files to use dot formatting.
//!
//! This CLI tool renames files using dot-separated formatting,
//! with support for various transformations like date reordering,
//! prefix/suffix addition, and pattern-based replacements.

mod config;

use std::path::PathBuf;

use anyhow::Result;
use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::Shell;

use cli_tools::dot_rename::DotRename;

#[derive(Parser)]
#[command(author, version, name = env!("CARGO_BIN_NAME"), about = "Rename files to use dot formatting")]
struct DotsCli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Optional input directory or file
    #[arg(value_hint = clap::ValueHint::AnyPath, global = true)]
    path: Option<PathBuf>,

    /// Convert casing
    #[arg(short = 'c', long, global = true)]
    case: bool,

    /// Enable debug prints
    #[arg(short = 'D', long, global = true)]
    debug: bool,

    /// Rename directories
    #[arg(short = 'd', long, global = true)]
    directory: bool,

    /// Overwrite existing files
    #[arg(short = 'f', long, global = true)]
    force: bool,

    /// Include files that match the given pattern
    #[arg(short = 'n', long, num_args = 1, action = clap::ArgAction::Append, name = "INCLUDE", global = true)]
    include: Vec<String>,

    /// Exclude files that match the given pattern
    #[arg(short = 'e', long, num_args = 1, action = clap::ArgAction::Append, name = "EXCLUDE", global = true)]
    exclude: Vec<String>,

    /// Increment conflicting file name with running index
    #[arg(short = 'i', long, global = true)]
    increment: bool,

    /// Only print changes without renaming files
    #[arg(short = 'p', long, global = true)]
    print: bool,

    /// Recurse into subdirectories
    #[arg(short = 'r', long, global = true)]
    recurse: bool,

    /// Append prefix to the start
    #[arg(short = 'x', long)]
    prefix: Option<String>,

    /// Prefix files with directory name
    #[arg(short = 'b', long, conflicts_with = "prefix")]
    prefix_dir: bool,

    /// Force prefix name to the start
    #[arg(short = 'B', long, conflicts_with = "prefix")]
    prefix_dir_start: bool,

    /// Prefix files with their parent directory name
    #[arg(short = 'R', long, conflicts_with = "prefix")]
    prefix_dir_recursive: bool,

    /// Suffix files with directory name
    #[arg(short = 'j', long, conflicts_with = "suffix")]
    suffix_dir: bool,

    /// Suffix files with their parent directory name
    #[arg(short = 'J', long, conflicts_with = "suffix")]
    suffix_dir_recursive: bool,

    /// Append suffix to the end
    #[arg(short = 'u', long)]
    suffix: Option<String>,

    /// Substitute pattern with replacement in filenames
    #[arg(short = 's', long, num_args = 2, action = clap::ArgAction::Append, value_names = ["PATTERN", "REPLACEMENT"], global = true)]
    substitute: Vec<String>,

    /// Remove random strings
    #[arg(short = 'm', long, global = true)]
    random: bool,

    /// Remove pattern from filenames
    #[arg(short = 'z', long, num_args = 1, action = clap::ArgAction::Append, name = "PATTERN", global = true)]
    remove: Vec<String>,

    /// Substitute regex pattern with replacement in filenames
    #[arg(short = 'g', long, num_args = 2, action = clap::ArgAction::Append, value_names = ["PATTERN", "REPLACEMENT"], global = true)]
    regex: Vec<String>,

    /// Assume year is last in short dates
    #[arg(short = 'y', long, global = true)]
    year: bool,

    /// Print verbose output
    #[arg(short = 'v', long, global = true)]
    verbose: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Prefix files with a name or parent directory name
    #[command(name = "prefix")]
    Prefix {
        /// Input directory
        #[arg(value_hint = clap::ValueHint::DirPath)]
        path: Option<PathBuf>,

        /// Prefix name (if not specified, uses parent directory name)
        #[arg(short = 'x', long)]
        name: Option<String>,

        /// Force prefix to always be at the start of the filename
        #[arg(short = 'S', long)]
        start: bool,

        /// Use each file's parent directory name as prefix
        #[arg(short = 'R', long)]
        recursive: bool,
    },

    /// Suffix files with a name or parent directory name
    #[command(name = "suffix")]
    Suffix {
        /// Input directory
        #[arg(value_hint = clap::ValueHint::DirPath)]
        path: Option<PathBuf>,

        /// Suffix name (if not specified, uses parent directory name)
        #[arg(short = 'x', long)]
        name: Option<String>,

        /// Use each file's parent directory name as suffix
        #[arg(short = 'R', long)]
        recursive: bool,
    },

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

impl DotsCli {
    /// Apply subcommand options to the main args.
    fn apply_subcommand(&mut self) {
        match &self.command {
            Some(Command::Prefix {
                path,
                name,
                start,
                recursive,
            }) => {
                if path.is_some() {
                    self.path = path.clone();
                }
                if let Some(prefix_name) = name {
                    self.prefix = Some(prefix_name.clone());
                } else if *recursive {
                    self.prefix_dir_recursive = true;
                } else {
                    self.prefix_dir = true;
                }
                if *start {
                    self.prefix_dir_start = true;
                }
            }
            Some(Command::Suffix { path, name, recursive }) => {
                if path.is_some() {
                    self.path = path.clone();
                }
                if let Some(suffix_name) = name {
                    self.suffix = Some(suffix_name.clone());
                } else if *recursive {
                    self.suffix_dir_recursive = true;
                } else {
                    self.suffix_dir = true;
                }
            }
            Some(Command::Completion { .. }) | None => {}
        }
    }

    /// Build the config and run the rename operation.
    fn run(self) -> Result<()> {
        let path_given = self.path.is_some();
        let root = cli_tools::resolve_input_path(self.path.as_deref())?;
        let config = crate::config::build_config(&self)?;

        DotRename::new(root, config, path_given).run()
    }
}

fn main() -> Result<()> {
    let mut cli = DotsCli::parse();
    cli.apply_subcommand();
    if let Some(Command::Completion { shell, install }) = &cli.command {
        cli_tools::generate_shell_completion(*shell, DotsCli::command(), *install, env!("CARGO_BIN_NAME"))
    } else {
        cli.run()
    }
}

//! The `rsbump` binary.
//!
//! Reads the command line arguments and the user config,
//! bumps the Rust projects under the given paths with the `cli_tools_rsbump` library,
//! and prints each project report as it finishes and a summary at the end.

#![cfg_attr(test, allow(clippy::panic_in_result_fn))]

mod config;
mod report;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

use cli_tools_common::{CompletionArgs, count_label, print_bold, print_yellow};
use cli_tools_rsbump::{RustVersion, bump_projects, check_tools, ensure_none_failed, find_projects};

use crate::config::Config;
use crate::report::ReportPrinter;

/// Subcommands for `rsbump`.
#[derive(Subcommand)]
enum RsbumpCommand {
    /// Generate shell completion script
    #[command(name = "completion")]
    Completion(CompletionArgs),
}

#[derive(Parser)]
#[command(
    author,
    version = cli_tools_version::VERSION_INFO,
    name = env!("CARGO_BIN_NAME"),
    about = "Bump Rust projects to the local Rust toolchain version"
)]
pub struct Args {
    #[command(subcommand)]
    command: Option<RsbumpCommand>,

    /// Directories or Cargo.toml files to search for Rust projects. Defaults to current directory.
    #[arg(value_hint = clap::ValueHint::AnyPath)]
    paths: Vec<PathBuf>,

    /// Upgrade dependencies with "cargo upgrade --incompatible" and "cargo update"
    #[arg(short, long)]
    update: bool,

    /// Run "cargo clean"
    #[arg(short, long)]
    clean: bool,

    /// Run "cargo clippy --fix" and "cargo test" after updating
    #[arg(short = 'k', long)]
    check: bool,

    /// Commit the changes in each git repository
    #[arg(short = 'g', long)]
    commit: bool,

    /// Include projects whose git repository has uncommitted changes
    #[arg(short, long)]
    force: bool,

    /// Only print what would change without changing anything
    #[arg(short, long)]
    print: bool,

    /// Number of projects processed in parallel. Defaults to the number of CPUs.
    #[arg(short, long, value_name = "N")]
    jobs: Option<usize>,

    /// Maximum directory depth when searching for projects [default: 5]
    #[arg(short, long, value_name = "N")]
    max_depth: Option<usize>,

    /// Print the output of successful commands too
    #[arg(short, long, global = true)]
    verbose: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();

    if let Some(RsbumpCommand::Completion(completion)) = &args.command {
        return completion.generate::<Args>(args.verbose, env!("CARGO_BIN_NAME"));
    }

    let config = Config::from_args(&args)?;
    run(&config)
}

/// Find the projects and process them in parallel.
fn run(config: &Config) -> Result<()> {
    let toolchain = RustVersion::from_local_toolchain()?;
    if let Some(version) = &toolchain.prerelease {
        println!(
            "Local toolchain is a pre-release ({version}), targeting the installed stable release {}",
            toolchain.target
        );
    }
    check_tools(config.options)?;

    let roots = find_projects(&config.paths, config.max_depth)?;
    if roots.is_empty() {
        print_yellow!("No Rust projects found");
        return Ok(());
    }

    print_bold!(
        "Bumping {} to Rust {}",
        count_label(roots.len(), "project", "projects"),
        toolchain.target
    );
    if config.options.dry_run {
        print_yellow!("Dry run, nothing is changed");
    }
    println!();

    let working_directory = std::env::current_dir().ok();
    let printer = ReportPrinter::new(working_directory.as_deref(), config.verbose);
    let reports = bump_projects(&roots, toolchain.target, config.options, config.jobs, &printer)?;

    report::print_summary(&reports, working_directory.as_deref());
    ensure_none_failed(&reports)
}

#[cfg(test)]
mod test_args {
    use super::*;

    #[test]
    fn verify_cli() {
        use clap::CommandFactory;
        Args::command().debug_assert();
    }

    #[test]
    fn parses_multiple_paths_and_flags() {
        let args = Args::try_parse_from(["rsbump", "a", "b", "-u", "-k", "-g", "-j", "2"]).expect("valid args");
        assert_eq!(args.paths, vec![PathBuf::from("a"), PathBuf::from("b")]);
        assert!(args.update);
        assert!(args.check);
        assert!(args.commit);
        assert_eq!(args.jobs, Some(2));
    }
}

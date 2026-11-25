//! The `rsbump` binary.
//!
//! Finds Rust projects under the given paths and bumps them to the local Rust toolchain version.
//! Sets `rust-version` in `Cargo.toml`, pins `rust` Docker images to the same version,
//! and optionally upgrades dependencies, cleans, verifies, and commits.
//! Projects are processed in parallel.

#![cfg_attr(test, allow(clippy::panic_in_result_fn))]

mod config;
mod discovery;
mod dockerfile;
mod manifest;
mod project;
mod registry;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::Shell;
use colored::Colorize;
use rayon::prelude::*;

use cli_tools_common::{command_exists, count_label, path_to_string_relative_to, print_bold, print_yellow};

use crate::config::Config;
use crate::discovery::find_projects;
use crate::manifest::RustVersion;
use crate::project::{GitState, Outcome, Report, RunContext, process_project};
use crate::registry::DigestResolver;

/// Subcommands for `rsbump`.
#[derive(Subcommand)]
enum RsbumpCommand {
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

#[derive(Parser)]
#[command(
    author,
    version,
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

    if let Some(RsbumpCommand::Completion { shell, install }) = &args.command {
        return cli_tools_common::generate_shell_completion(
            *shell,
            Args::command(),
            *install,
            args.verbose,
            env!("CARGO_BIN_NAME"),
        );
    }

    let config = Config::from_args(&args)?;
    run(&config)
}

/// Find the projects and process them in parallel.
fn run(config: &Config) -> Result<()> {
    let target = RustVersion::from_local_toolchain()?;
    if config.options.update && !command_exists("cargo-upgrade") {
        anyhow::bail!("cargo upgrade not found, install it with: cargo install cargo-edit");
    }

    let roots = find_projects(&config.paths, config.max_depth);
    if roots.is_empty() {
        print_yellow!("No Rust projects found");
        return Ok(());
    }

    print_bold!(
        "Bumping {} to Rust {target}",
        count_label(roots.len(), "project", "projects")
    );
    if config.options.dryrun {
        print_yellow!("Dry run, nothing is changed");
    }
    println!();

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(config.jobs)
        .build()
        .context("Failed to build thread pool")?;

    let git_states: Vec<GitState> =
        pool.install(|| roots.par_iter().map(|root| GitState::read(root, &roots)).collect());

    let resolver = DigestResolver::new()?;
    let commit_lock = Mutex::new(());
    let context = RunContext {
        target,
        options: config.options,
        resolver: &resolver,
        roots: &roots,
        commit_lock: &commit_lock,
    };
    let working_directory = std::env::current_dir().ok();

    let reports: Vec<Report> = pool.install(|| {
        roots
            .par_iter()
            .zip(git_states.par_iter())
            .map(|(root, git)| {
                let report = process_project(root, *git, &context);
                print_report(&report, working_directory.as_deref());
                report
            })
            .collect()
    });

    print_summary(&reports, working_directory.as_deref());

    let failed = reports
        .iter()
        .filter(|report| matches!(report.outcome, Outcome::Failed(_)))
        .count();
    if failed > 0 {
        anyhow::bail!("{} failed", count_label(failed, "project", "projects"));
    }
    Ok(())
}

/// Print one project report as a single block, so parallel reports do not interleave.
fn print_report(report: &Report, working_directory: Option<&Path>) {
    let status = match &report.outcome {
        Outcome::Updated => "updated".green(),
        Outcome::Unchanged => "unchanged".dimmed(),
        Outcome::SkippedDirty => "skipped".yellow(),
        Outcome::Failed(_) => "failed".red(),
    };
    let mut block = format!(
        "{} {status}\n",
        display_path(&report.root, working_directory).bold().magenta()
    );
    for line in &report.lines {
        block.push_str(line);
        block.push('\n');
    }
    let mut stdout = std::io::stdout().lock();
    // Printing is best effort, a closed stdout should not abort the remaining projects.
    let _ = writeln!(stdout, "{block}");
}

/// Print the totals and the projects that need attention.
fn print_summary(reports: &[Report], working_directory: Option<&Path>) {
    let count = |predicate: fn(&Report) -> bool| reports.iter().filter(|report| predicate(report)).count();
    let updated = count(|report| report.outcome == Outcome::Updated);
    let unchanged = count(|report| report.outcome == Outcome::Unchanged);
    let committed = count(|report| report.committed);

    print_bold!("Summary");
    println!("  {} {updated}", "Updated:".green());
    println!("  {} {unchanged}", "Unchanged:".dimmed());
    if committed > 0 {
        println!("  {} {committed}", "Committed:".cyan());
    }

    let list = |label: &str, filter: &dyn Fn(&Report) -> bool| {
        let paths: Vec<String> = reports
            .iter()
            .filter(|report| filter(report))
            .map(|report| display_path(&report.root, working_directory))
            .collect();
        if !paths.is_empty() {
            println!("  {label} {}", paths.len());
            for path in paths {
                println!("    {path}");
            }
        }
    };
    list(&"No rust-version:".yellow().to_string(), &|report| {
        report.missing_rust_version
    });
    list(&"Skipped, uncommitted changes:".yellow().to_string(), &|report| {
        report.outcome == Outcome::SkippedDirty
    });
    list(&"Failed:".red().to_string(), &|report| {
        matches!(report.outcome, Outcome::Failed(_))
    });
}

/// Project path relative to the working directory, or "." for the working directory itself.
fn display_path(path: &Path, working_directory: Option<&Path>) -> String {
    let display = path_to_string_relative_to(path, working_directory);
    if display.is_empty() { ".".to_string() } else { display }
}

#[cfg(test)]
mod test_args {
    use super::*;

    #[test]
    fn verify_cli() {
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

#[cfg(test)]
mod test_display_path {
    use super::*;

    #[test]
    fn relative_to_working_directory() {
        let working_directory = Path::new("/dev");
        assert_eq!(display_path(Path::new("/dev/app"), Some(working_directory)), "app");
        assert_eq!(display_path(Path::new("/dev"), Some(working_directory)), ".");
        assert_eq!(
            display_path(Path::new("/other/app"), Some(working_directory)),
            "/other/app"
        );
    }
}

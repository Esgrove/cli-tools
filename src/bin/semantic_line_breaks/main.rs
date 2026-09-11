//! slb - Check and format prose with semantic line breaks.
//!
//! This CLI tool scans source files and Markdown documents for comments, docstrings, and paragraphs
//! that violate the semantic line break style,
//! reports the violations, and optionally reflows the prose and moves trailing comments above the code.

mod config;

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::Shell;
use colored::Colorize;
use difference::{Changeset, Difference};
use walkdir::WalkDir;

use cli_tools::semantic_line_breaks::project_config::WidthResolver;
use cli_tools::semantic_line_breaks::types::DEFAULT_MAX_WIDTH;
use cli_tools::semantic_line_breaks::{FileKind, FormatResult, Violation, check, format};
use cli_tools::{print_error, print_yellow};

use crate::config::Config;

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

/// Counters collected over all processed files.
#[derive(Debug, Default)]
struct Summary {
    /// Number of files processed.
    files: usize,
    /// Number of files with at least one violation.
    files_with_violations: usize,
    /// Total number of violations found.
    violations: usize,
    /// Number of violations fix mode can repair.
    fixable: usize,
    /// Number of files rewritten in fix mode.
    files_fixed: usize,
    /// Number of violations remaining after fixing.
    remaining: usize,
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(error) => {
            print_error!("{error:#}");
            ExitCode::from(2)
        }
    }
}

/// Parse arguments and run the requested mode.
fn run() -> Result<ExitCode> {
    let args = Args::parse();

    if let Some(SlbCommand::Completion { shell, install }) = &args.command {
        cli_tools::generate_shell_completion(*shell, Args::command(), *install, args.verbose, env!("CARGO_BIN_NAME"))?;
        return Ok(ExitCode::SUCCESS);
    }

    let config = Config::from_args(&args)?;
    if config.stdin {
        return run_stdin(&config);
    }

    let files = collect_files(&args.paths, &config)?;
    if files.is_empty() {
        print_yellow!("No supported files found");
        return Ok(ExitCode::SUCCESS);
    }

    let mut resolver = WidthResolver::new();
    let mut summary = Summary::default();
    for file in &files {
        if let Err(error) = process_file(file, &config, &mut resolver, &mut summary) {
            print_error!("{}: {error:#}", display_path(file));
        }
    }
    print_summary(&summary, &config);

    let failed = if config.fix {
        summary.remaining > 0
    } else {
        summary.violations > 0
    };
    Ok(if failed { ExitCode::FAILURE } else { ExitCode::SUCCESS })
}

/// Format text from stdin and write the result to stdout.
fn run_stdin(config: &Config) -> Result<ExitCode> {
    let kind = config
        .kind
        .context("The --type option is required when reading from stdin")?;
    let mut input = String::new();
    io::stdin().read_to_string(&mut input).context("Failed to read stdin")?;
    let options = config.format_options(config.width.unwrap_or(DEFAULT_MAX_WIDTH));
    let result = format(&input, kind, &options);
    let output = result.fixed_text.as_deref().unwrap_or(&input);
    io::stdout().write_all(output.as_bytes())?;
    if !config.quiet {
        for violation in &result.violations {
            eprintln!("{}", format_violation("stdin", violation, true));
        }
    }
    let remaining = result
        .fixed_text
        .as_deref()
        .map_or_else(|| result.violations.len(), |fixed| check(fixed, kind, &options).len());
    Ok(if remaining > 0 {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

/// Collect the files to process from the given paths, walking directories recursively.
fn collect_files(paths: &[PathBuf], config: &Config) -> Result<Vec<PathBuf>> {
    let roots: Vec<PathBuf> = if paths.is_empty() {
        vec![cli_tools::resolve_input_path(None)?]
    } else {
        paths
            .iter()
            .map(|path| cli_tools::resolve_input_path(Some(path)))
            .collect::<Result<Vec<_>>>()?
    };

    let mut files = Vec::new();
    for root in roots {
        if root.is_file() {
            if config.kind.is_some() || FileKind::from_path(&root).is_some() {
                files.push(root);
            } else {
                print_yellow!("Skipping unsupported file type: {}", display_path(&root));
            }
            continue;
        }
        let walker = WalkDir::new(&root)
            .into_iter()
            .filter_entry(|entry| !cli_tools::should_skip_entry(entry) && !is_excluded(entry.path(), config));
        for entry in walker.filter_map(Result::ok) {
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.into_path();
            let kind_known = config.kind.is_some() || FileKind::from_path(&path).is_some();
            if kind_known && matches_extensions(&path, config) {
                files.push(path);
            }
        }
    }
    files.sort();
    files.dedup();
    Ok(files)
}

/// Whether the path has a component equal to an exclude pattern, or contains a pattern with a separator.
fn is_excluded(path: &Path, config: &Config) -> bool {
    let path_string = path.to_string_lossy().replace('\\', "/");
    config.exclude.iter().any(|pattern| {
        if pattern.contains('/') {
            path_string.contains(pattern.trim_matches('/'))
        } else {
            path.components()
                .any(|component| component.as_os_str().to_string_lossy() == *pattern)
        }
    })
}

/// Whether the file passes the extension filter.
fn matches_extensions(path: &Path, config: &Config) -> bool {
    if config.extensions.is_empty() {
        return true;
    }
    let extension = path
        .extension()
        .map(|extension| extension.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    config
        .extensions
        .iter()
        .any(|allowed| *allowed == extension || *allowed == file_name)
}

/// Check or fix one file and update the summary.
fn process_file(file: &Path, config: &Config, resolver: &mut WidthResolver, summary: &mut Summary) -> Result<()> {
    let Some(kind) = config.kind.or_else(|| FileKind::from_path(file)) else {
        return Ok(());
    };
    let text = match fs::read_to_string(file) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::InvalidData => {
            if config.verbose {
                print_yellow!("Skipping file with invalid UTF-8: {}", display_path(file));
            }
            return Ok(());
        }
        Err(error) => return Err(error).with_context(|| "Failed to read file"),
    };

    let (width, source) = resolve_width(file, kind, config, resolver);
    let options = config.format_options(width);
    let result = format(&text, kind, &options);
    summary.files += 1;
    let path = display_path(file);

    if config.verbose {
        println!("{} (width {width} from {source})", path.cyan());
    }
    if result.violations.is_empty() {
        return Ok(());
    }
    summary.files_with_violations += 1;
    summary.violations += result.violations.len();
    summary.fixable += result.violations.iter().filter(|violation| violation.fixable).count();

    if config.fix {
        fix_file(file, &path, &text, &result, kind, &options, config, summary)?;
    } else {
        if !config.quiet {
            for violation in &result.violations {
                println!("{}", format_violation(&path, violation, false));
            }
        }
        if config.print
            && let Some(fixed) = &result.fixed_text
        {
            print_diff(&path, &text, fixed);
        }
    }
    Ok(())
}

/// Write the fixed text and report what remains.
#[allow(clippy::too_many_arguments)]
fn fix_file(
    file: &Path,
    path: &str,
    text: &str,
    result: &FormatResult,
    kind: FileKind,
    options: &cli_tools::semantic_line_breaks::FormatOptions,
    config: &Config,
    summary: &mut Summary,
) -> Result<()> {
    let Some(fixed) = &result.fixed_text else {
        summary.remaining += result.violations.len();
        if !config.quiet {
            for violation in &result.violations {
                println!("{}", format_violation(path, violation, true));
            }
        }
        return Ok(());
    };
    fs::write(file, fixed).with_context(|| "Failed to write file")?;
    summary.files_fixed += 1;
    let remaining = check(fixed, kind, options);
    let fixed_count = result.violations.len().saturating_sub(remaining.len());
    if !config.quiet {
        println!(
            "{}",
            format!(
                "Fixed {} in {path}",
                cli_tools::count_label(fixed_count, "violation", "violations")
            )
            .green()
        );
        for violation in &remaining {
            println!("{}", format_violation(path, violation, true));
        }
    }
    if config.print {
        print_diff(path, text, fixed);
    }
    summary.remaining += remaining.len();
    Ok(())
}

/// Resolve the line width for a file and describe where it came from.
fn resolve_width(file: &Path, kind: FileKind, config: &Config, resolver: &mut WidthResolver) -> (usize, String) {
    if let Some(width) = config.width {
        return (width, "options".to_string());
    }
    if config.project_width
        && let Some(source) = resolver.resolve(file, kind)
    {
        return (
            source.width,
            format!("{} in {}", source.key, display_path(&source.file)),
        );
    }
    (DEFAULT_MAX_WIDTH, "default".to_string())
}

/// Format one violation for the terminal.
fn format_violation(path: &str, violation: &Violation, after_fix: bool) -> String {
    let kind = violation.kind.to_string();
    let kind = if violation.fixable && !after_fix {
        kind.red()
    } else {
        kind.yellow()
    };
    let location = violation.column.map_or_else(
        || format!("{path}:{}", violation.line),
        |column| format!("{path}:{}:{column}", violation.line),
    );
    format!("{}: {kind}: {}", location.cyan(), violation.message)
}

/// Print a line based diff between the original and fixed text.
fn print_diff(path: &str, original: &str, fixed: &str) {
    println!("{}", format!("--- {path}").bold());
    let changeset = Changeset::new(original, fixed, "\n");
    for difference in &changeset.diffs {
        match difference {
            Difference::Same(_) => {}
            Difference::Rem(removed) => {
                for line in removed.lines() {
                    println!("{}", format!("- {line}").red());
                }
            }
            Difference::Add(added) => {
                for line in added.lines() {
                    println!("{}", format!("+ {line}").green());
                }
            }
        }
    }
}

/// Print the final summary line.
fn print_summary(summary: &Summary, config: &Config) {
    let files = cli_tools::count_label(summary.files, "file", "files");
    if config.fix {
        let message = format!(
            "Checked {files}, fixed {}, {} remaining",
            cli_tools::count_label(summary.files_fixed, "file", "files"),
            cli_tools::count_label(summary.remaining, "violation", "violations")
        );
        if summary.remaining > 0 {
            println!("{}", message.yellow());
        } else {
            println!("{}", message.green());
        }
    } else if summary.violations == 0 {
        println!("{}", format!("Checked {files}, no violations").green());
    } else {
        println!(
            "{}",
            format!(
                "Checked {files}, found {} in {} ({} fixable)",
                cli_tools::count_label(summary.violations, "violation", "violations"),
                cli_tools::count_label(summary.files_with_violations, "file", "files"),
                summary.fixable
            )
            .yellow()
        );
    }
}

/// Path relative to the current working directory for display.
fn display_path(path: &Path) -> String {
    cli_tools::get_relative_path_from_current_working_directory(path)
        .display()
        .to_string()
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

#[cfg(test)]
mod test_file_filters {
    use super::*;

    fn config_with(exclude: Vec<&str>, extensions: Vec<&str>) -> Config {
        let args = Args::try_parse_from(["slb"]).expect("arguments should parse");
        let mut config = Config::from_args(&args).expect("config should build");
        config.exclude = exclude.into_iter().map(String::from).collect();
        config.extensions = extensions.into_iter().map(String::from).collect();
        config
    }

    #[test]
    fn exclude_matches_whole_components_only() {
        let config = config_with(vec!["build"], vec![]);
        assert!(is_excluded(Path::new("project/build/out.rs"), &config));
        assert!(!is_excluded(Path::new("project/src/builder.rs"), &config));
    }

    #[test]
    fn exclude_with_separator_matches_path_substring() {
        let config = config_with(vec!["docs/generated"], vec![]);
        assert!(is_excluded(Path::new("repo/docs/generated/api.md"), &config));
        assert!(!is_excluded(Path::new("repo/docs/manual/api.md"), &config));
    }

    #[test]
    fn extension_filter_accepts_matching_files() {
        let config = config_with(vec![], vec!["rs", "dockerfile"]);
        assert!(matches_extensions(Path::new("src/main.RS"), &config));
        assert!(matches_extensions(Path::new("Dockerfile"), &config));
        assert!(!matches_extensions(Path::new("README.md"), &config));
        assert!(matches_extensions(Path::new("x.md"), &config_with(vec![], vec![])));
    }
}

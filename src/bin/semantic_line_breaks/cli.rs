//! Check and fix mode processing for `slb`.
//!
//! Runs the requested mode over the collected files or over stdin,
//! resolves the line width for each file, writes fixes in fix mode,
//! and collects the counters used for the final summary.

use std::fs;
use std::io::{self, Read, Write};
use std::path::Path;
use std::process::ExitCode;

use anyhow::{Context, Result};
use colored::Colorize;

use cli_tools::semantic_line_breaks::project_config::WidthResolver;
use cli_tools::semantic_line_breaks::types::DEFAULT_MAX_WIDTH;
use cli_tools::semantic_line_breaks::{FileKind, FormatOptions, FormatResult, check, format};
use cli_tools::{print_error, print_yellow};

use crate::Args;
use crate::config::Config;
use crate::files::{collect_files, display_path};
use crate::output::{Summary, format_violation, print_diff, print_summary};

/// Check or fix all files matching the given arguments and return the process exit code.
///
/// # Errors
/// Returns an error if the configuration cannot be built or the input paths cannot be read.
pub fn run(args: &Args) -> Result<ExitCode> {
    let config = Config::from_args(args)?;
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
    options: &FormatOptions,
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

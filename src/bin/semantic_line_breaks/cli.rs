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
    // The fixed text is only needed to write it or to print the diff,
    // and building it costs an extra pass over every file with a trailing comment.
    let result = if config.fix || config.print {
        format(&text, kind, &options)
    } else {
        FormatResult {
            violations: check(&text, kind, &options),
            fixed_text: None,
        }
    };
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

#[cfg(test)]
mod test_helpers {
    use clap::Parser;
    use tempfile::TempDir;

    use super::*;

    /// Build a config from command line arguments as the binary would.
    pub fn config(arguments: &[&str]) -> Config {
        let args = Args::try_parse_from(arguments).expect("arguments should parse");
        Config::from_args(&args).expect("config should build")
    }

    /// Build a config with no width, so the project config discovery decides the width.
    ///
    /// The user config used during tests sets a width,
    /// which would otherwise always win over the discovered value.
    pub fn config_without_width(arguments: &[&str]) -> Config {
        let mut config = config(arguments);
        config.width = None;
        config
    }

    /// Create a temporary directory with one file and return both.
    ///
    /// The directory name is visible, so the walker does not skip it as hidden.
    pub fn file_with(name: &str, content: &str) -> (TempDir, std::path::PathBuf) {
        let directory = temporary_directory();
        let path = directory.path().join(name);
        fs::write(&path, content).expect("file should be written");
        (directory, path)
    }

    /// Create a temporary directory with a visible name.
    pub fn temporary_directory() -> TempDir {
        tempfile::Builder::new()
            .prefix("slb_test")
            .tempdir()
            .expect("temporary directory should be created")
    }
}

#[cfg(test)]
mod test_process_file {
    use super::test_helpers::*;
    use super::*;

    /// A doc comment that is too long for the given width and can be split at a sentence end.
    const LONG_COMMENT: &str =
        "/// One sentence that is already quite long. Another sentence follows it here.\nfn parse() {}\n";

    #[test]
    fn check_mode_counts_violations_without_touching_the_file() {
        let (_directory, path) = file_with("check.rs", LONG_COMMENT);
        let mut resolver = WidthResolver::new();
        let mut summary = Summary::default();
        process_file(&path, &config(&["slb", "-w", "40", "-q"]), &mut resolver, &mut summary)
            .expect("processing should succeed");

        assert_eq!(summary.files, 1);
        assert_eq!(summary.files_with_violations, 1);
        assert!(summary.violations > 0);
        assert!(summary.fixable > 0);
        assert_eq!(summary.files_fixed, 0);
        assert_eq!(
            fs::read_to_string(&path).expect("file should be readable"),
            LONG_COMMENT,
            "check mode must not write the file"
        );
    }

    #[test]
    fn fix_mode_rewrites_the_file_and_leaves_nothing_fixable() {
        let (_directory, path) = file_with("fix.rs", LONG_COMMENT);
        let mut resolver = WidthResolver::new();
        let mut summary = Summary::default();
        process_file(
            &path,
            &config(&["slb", "-w", "40", "-q", "--fix"]),
            &mut resolver,
            &mut summary,
        )
        .expect("processing should succeed");

        assert_eq!(summary.files_fixed, 1);
        assert_eq!(summary.remaining, 0);
        let fixed = fs::read_to_string(&path).expect("file should be readable");
        assert_ne!(fixed, LONG_COMMENT);
        assert!(fixed.contains("/// One sentence that is already quite long.\n"));
        assert!(fixed.ends_with("fn parse() {}\n"));
    }

    #[test]
    fn a_file_without_violations_is_only_counted() {
        let (_directory, path) = file_with("clean.rs", "/// Short comment.\nfn parse() {}\n");
        let mut resolver = WidthResolver::new();
        let mut summary = Summary::default();
        process_file(&path, &config(&["slb", "-q"]), &mut resolver, &mut summary).expect("processing should succeed");

        assert_eq!(summary.files, 1);
        assert_eq!(summary.files_with_violations, 0);
        assert_eq!(summary.violations, 0);
    }

    #[test]
    fn an_unknown_file_type_is_skipped() {
        let (_directory, path) = file_with("data.bin", "content\n");
        let mut resolver = WidthResolver::new();
        let mut summary = Summary::default();
        process_file(&path, &config(&["slb"]), &mut resolver, &mut summary).expect("processing should succeed");

        assert_eq!(summary.files, 0);
    }

    #[test]
    fn the_forced_kind_overrides_the_file_extension() {
        let (_directory, path) = file_with("comments.unknown", "# A short comment.\n");
        let mut resolver = WidthResolver::new();
        let mut summary = Summary::default();
        process_file(
            &path,
            &config(&["slb", "--type", "shell", "-q"]),
            &mut resolver,
            &mut summary,
        )
        .expect("processing should succeed");

        assert_eq!(summary.files, 1);
    }

    #[test]
    fn invalid_utf8_is_skipped_without_an_error() {
        let (_directory, path) = file_with("clean.rs", "");
        fs::write(&path, [0x2f, 0x2f, 0x20, 0xff, 0xfe, 0x0a]).expect("file should be written");
        let mut resolver = WidthResolver::new();
        let mut summary = Summary::default();
        process_file(&path, &config(&["slb", "-v"]), &mut resolver, &mut summary).expect("processing should succeed");

        assert_eq!(summary.files, 0);
    }

    #[test]
    fn a_missing_file_is_an_error() {
        let directory = temporary_directory();
        let mut resolver = WidthResolver::new();
        let mut summary = Summary::default();
        let error = process_file(
            &directory.path().join("missing.rs"),
            &config(&["slb"]),
            &mut resolver,
            &mut summary,
        )
        .expect_err("a missing file should fail");

        assert!(format!("{error:#}").contains("Failed to read file"));
    }
}

#[cfg(test)]
mod test_fix_file {
    use super::test_helpers::*;
    use super::*;

    #[test]
    fn violations_that_cannot_be_fixed_are_counted_as_remaining() {
        let (_directory, path) = file_with("unfixable.rs", "/// Text.\n");
        let options = FormatOptions::with_width(120);
        let result = FormatResult {
            violations: vec![cli_tools::semantic_line_breaks::Violation {
                line: 1,
                column: None,
                kind: cli_tools::semantic_line_breaks::ViolationKind::EmDash,
                message: "dash at the edge of a sentence cannot be rewritten automatically".to_string(),
                fixable: false,
            }],
            fixed_text: None,
        };
        let mut summary = Summary::default();
        fix_file(
            &path,
            "unfixable.rs",
            "/// Text.\n",
            &result,
            FileKind::Rust,
            &options,
            &config(&["slb", "--fix", "-q"]),
            &mut summary,
        )
        .expect("fixing should succeed");

        assert_eq!(summary.remaining, 1);
        assert_eq!(summary.files_fixed, 0);
        assert_eq!(
            fs::read_to_string(&path).expect("file should be readable"),
            "/// Text.\n",
            "a file without fixed text must not be written"
        );
    }

    #[test]
    fn remaining_violations_are_printed_when_not_quiet() {
        let (_directory, path) = file_with("unfixable.rs", "/// Text.\n");
        let options = FormatOptions::with_width(120);
        let result = FormatResult {
            violations: vec![cli_tools::semantic_line_breaks::Violation {
                line: 1,
                column: None,
                kind: cli_tools::semantic_line_breaks::ViolationKind::EmDash,
                message: "dash at the edge of a sentence cannot be rewritten automatically".to_string(),
                fixable: false,
            }],
            fixed_text: None,
        };
        let mut summary = Summary::default();
        fix_file(
            &path,
            "unfixable.rs",
            "/// Text.\n",
            &result,
            FileKind::Rust,
            &options,
            &config(&["slb", "--fix"]),
            &mut summary,
        )
        .expect("fixing should succeed");

        assert_eq!(summary.remaining, 1);
    }

    #[test]
    fn a_violation_that_survives_the_fix_is_counted_as_remaining() {
        let text = "/// A dash at the end of a sentence \u{2014}\n";
        let (_directory, path) = file_with("dash.rs", text);
        let options = FormatOptions::with_width(120);
        let result = format(text, FileKind::Rust, &options);
        let mut summary = Summary::default();
        fix_file(
            &path,
            "dash.rs",
            text,
            &result,
            FileKind::Rust,
            &options,
            &config(&["slb", "--fix"]),
            &mut summary,
        )
        .expect("fixing should succeed");

        assert!(summary.remaining > 0, "the unfixable dash should remain");
    }

    #[test]
    fn fixed_text_is_written_and_reported() {
        let (_directory, path) = file_with("fixed.rs", "/// Before.\n");
        let options = FormatOptions::with_width(120);
        let result = FormatResult {
            violations: vec![],
            fixed_text: Some("/// After.\n".to_string()),
        };
        let mut summary = Summary::default();
        fix_file(
            &path,
            "fixed.rs",
            "/// Before.\n",
            &result,
            FileKind::Rust,
            &options,
            &config(&["slb", "--fix", "--print"]),
            &mut summary,
        )
        .expect("fixing should succeed");

        assert_eq!(summary.files_fixed, 1);
        assert_eq!(summary.remaining, 0);
        assert_eq!(
            fs::read_to_string(&path).expect("file should be readable"),
            "/// After.\n"
        );
    }
}

#[cfg(test)]
mod test_resolve_width {
    use super::test_helpers::*;
    use super::*;

    #[test]
    fn the_width_option_wins_over_the_project_config() {
        let (_directory, path) = file_with("main.rs", "fn main() {}\n");
        fs::write(
            path.parent().expect("file should have a parent").join("rustfmt.toml"),
            "max_width = 80\n",
        )
        .expect("config should be written");
        let mut resolver = WidthResolver::new();
        let (width, source) = resolve_width(&path, FileKind::Rust, &config(&["slb", "-w", "70"]), &mut resolver);

        assert_eq!(width, 70);
        assert_eq!(source, "options");
    }

    #[test]
    fn the_project_config_is_used_when_no_width_is_given() {
        let (_directory, path) = file_with("main.rs", "fn main() {}\n");
        fs::write(
            path.parent().expect("file should have a parent").join("rustfmt.toml"),
            "max_width = 80\n",
        )
        .expect("config should be written");
        let mut resolver = WidthResolver::new();
        let (width, source) = resolve_width(&path, FileKind::Rust, &config_without_width(&["slb"]), &mut resolver);

        assert_eq!(width, 80);
        assert!(source.starts_with("max_width in "));
        assert!(source.ends_with("rustfmt.toml"));
    }

    #[test]
    fn the_project_config_can_be_ignored() {
        let (_directory, path) = file_with("main.rs", "fn main() {}\n");
        fs::write(
            path.parent().expect("file should have a parent").join("rustfmt.toml"),
            "max_width = 80\n",
        )
        .expect("config should be written");
        let mut resolver = WidthResolver::new();
        let (width, source) = resolve_width(
            &path,
            FileKind::Rust,
            &config_without_width(&["slb", "--ignore-project-config"]),
            &mut resolver,
        );

        assert_eq!(width, DEFAULT_MAX_WIDTH);
        assert_eq!(source, "default");
    }

    #[test]
    fn the_default_width_is_used_without_any_config() {
        let (_directory, path) = file_with("main.rs", "fn main() {}\n");
        let mut resolver = WidthResolver::new();
        let (width, source) = resolve_width(&path, FileKind::Rust, &config_without_width(&["slb"]), &mut resolver);

        assert_eq!(width, DEFAULT_MAX_WIDTH);
        assert_eq!(source, "default");
    }

    #[test]
    fn the_user_config_width_is_reported_as_an_option() {
        let (_directory, path) = file_with("main.rs", "fn main() {}\n");
        let mut resolver = WidthResolver::new();
        let (width, source) = resolve_width(&path, FileKind::Rust, &config(&["slb"]), &mut resolver);

        assert_eq!(width, 110, "the user config used during tests sets the width");
        assert_eq!(source, "options");
    }
}

#[cfg(test)]
mod test_run {
    use super::test_helpers::*;
    use super::*;

    #[test]
    fn a_directory_without_supported_files_succeeds() {
        let (directory, _path) = file_with("notes.bin", "content\n");
        let arguments = ["slb", directory.path().to_str().expect("path should be unicode")];
        let args = clap::Parser::try_parse_from(arguments).expect("arguments should parse");

        let code = run(&args).expect("running should succeed");
        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::SUCCESS));
    }

    #[test]
    fn violations_make_the_run_fail() {
        let (directory, _path) = file_with(
            "long.rs",
            "/// One sentence that is already quite long. Another sentence follows it here.\n",
        );
        let arguments = [
            "slb",
            directory.path().to_str().expect("path should be unicode"),
            "-w",
            "40",
            "-q",
        ];
        let args = clap::Parser::try_parse_from(arguments).expect("arguments should parse");

        let code = run(&args).expect("running should succeed");
        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::FAILURE));
    }

    #[test]
    fn a_clean_directory_succeeds() {
        let (directory, _path) = file_with("clean.rs", "/// Short comment.\nfn parse() {}\n");
        let arguments = ["slb", directory.path().to_str().expect("path should be unicode")];
        let args = clap::Parser::try_parse_from(arguments).expect("arguments should parse");

        let code = run(&args).expect("running should succeed");
        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::SUCCESS));
    }

    #[test]
    fn fix_mode_reports_success_when_everything_was_fixed() {
        let (directory, path) = file_with(
            "fixme.rs",
            "/// One sentence that is already quite long. Another sentence follows it here.\n",
        );
        let arguments = [
            "slb",
            directory.path().to_str().expect("path should be unicode"),
            "-w",
            "40",
            "-q",
            "--fix",
        ];
        let args = clap::Parser::try_parse_from(arguments).expect("arguments should parse");

        let code = run(&args).expect("running should succeed");
        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::SUCCESS));
        assert!(
            fs::read_to_string(&path)
                .expect("file should be readable")
                .contains("/// One sentence that is already quite long.\n")
        );
    }
}

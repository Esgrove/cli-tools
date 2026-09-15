//! Check and fix mode processing for `slb`.
//!
//! Runs the requested mode over the collected files or over stdin,
//! resolves the line width for each file, writes fixes in fix mode,
//! and collects the counters used for the final summary.
//!
//! Files are independent, so they are processed in parallel.
//! Each file renders its own output, which the main thread then prints in file order,
//! so the output does not depend on the order the threads happen to finish in.

use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result};
use colored::Colorize;
use rayon::prelude::*;

use cli_tools::semantic_line_breaks::project_config::{WidthSource, discover_width};
use cli_tools::semantic_line_breaks::types::DEFAULT_MAX_WIDTH;
use cli_tools::semantic_line_breaks::{FileKind, FormatOptions, FormatResult, check, format};
use cli_tools::{print_error, print_yellow};

use crate::Args;
use crate::config::Config;
use crate::files::{collect_files, display_path, display_path_relative};
use crate::output::{Summary, diff_lines, format_violation, print_summary};

/// Directory and file kind a project width applies to.
type WidthKey = (PathBuf, FileKind);

/// Everything one processed file produced, ready to be printed and counted.
#[derive(Debug, Default)]
struct FileOutcome {
    /// Lines to print for the file, already formatted and coloured.
    lines: Vec<String>,
    /// Message to report on the error stream when the file could not be processed.
    error: Option<String>,
    /// Whether the file was read and checked.
    processed: bool,
    /// Violations found in the file.
    violations: usize,
    /// Violations that fix mode can repair.
    fixable: usize,
    /// Whether the file was rewritten.
    written: bool,
    /// Violations left after writing the fixed text.
    remaining: usize,
}

/// State shared by every file of a run, built once before the parallel pass.
struct RunContext<'config> {
    /// Configuration for the run.
    config: &'config Config,
    /// Width source per directory and file kind, empty unless the width comes from a project config file.
    widths: HashMap<WidthKey, Option<WidthSource>>,
    /// Format options per line width the run can use.
    ///
    /// The option lists never change between files, so cloning them once per width
    /// replaces cloning them once per file.
    options: HashMap<usize, FormatOptions>,
    /// Format options for the default width, used when no other width applies.
    default_options: FormatOptions,
    /// Current working directory, so the printed paths can be shortened without asking for it per file.
    working_directory: Option<PathBuf>,
}

/// Line width for one file, the options that carry it, and where the width came from.
struct FileSettings<'context> {
    /// Resolved line width.
    width: usize,
    /// Format options holding that width.
    options: &'context FormatOptions,
    /// Project config file the width came from, when it did.
    source: Option<&'context WidthSource>,
}

impl<'config> RunContext<'config> {
    /// Resolve the project widths for the collected files and build the format options for every width.
    fn new(config: &'config Config, files: &[(PathBuf, FileKind)]) -> Self {
        let widths = resolve_project_widths(config, files);
        let mut options = HashMap::new();
        let discovered = widths
            .values()
            .filter_map(|source| source.as_ref().map(|source| source.width));
        for width in discovered.chain(config.width) {
            options.entry(width).or_insert_with(|| config.format_options(width));
        }
        Self {
            config,
            widths,
            options,
            default_options: config.format_options(DEFAULT_MAX_WIDTH),
            working_directory: env::current_dir().ok(),
        }
    }

    /// Settings for one file.
    ///
    /// The width is read back from the chosen options, so the two can never disagree.
    fn settings(&self, file: &Path, kind: FileKind) -> FileSettings<'_> {
        let source = if self.config.width.is_some() {
            None
        } else {
            self.width_source(file, kind)
        };
        let width = self
            .config
            .width
            .or_else(|| source.map(|source| source.width))
            .unwrap_or(DEFAULT_MAX_WIDTH);
        let options = self.options.get(&width).unwrap_or(&self.default_options);
        FileSettings {
            width: options.max_width,
            options,
            source,
        }
    }

    /// Project width source for the directory of the given file.
    fn width_source(&self, file: &Path, kind: FileKind) -> Option<&WidthSource> {
        let directory = file.parent().map_or_else(PathBuf::new, Path::to_path_buf);
        self.widths.get(&(directory, kind))?.as_ref()
    }

    /// Path of the file as it should be printed.
    fn display(&self, file: &Path) -> String {
        display_path_relative(file, self.working_directory.as_deref())
    }
}

/// Width source for every directory and file kind of the run, resolved once per directory.
///
/// Returns an empty map when the width cannot come from a project config file,
/// so nothing is read from disk in that case.
fn resolve_project_widths(config: &Config, files: &[(PathBuf, FileKind)]) -> HashMap<WidthKey, Option<WidthSource>> {
    if config.width.is_some() || !config.project_width {
        return HashMap::new();
    }
    let mut directories: HashMap<WidthKey, PathBuf> = HashMap::new();
    for (file, kind) in files {
        let directory = file.parent().map_or_else(PathBuf::new, Path::to_path_buf);
        directories.entry((directory, *kind)).or_insert_with(|| file.clone());
    }
    directories
        .into_par_iter()
        .map(|((directory, kind), file)| ((directory, kind), discover_width(&file, kind)))
        .collect()
}

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

    let outcomes = process_files(&files, &config)?;
    let mut summary = Summary::default();
    for outcome in &outcomes {
        if let Some(error) = &outcome.error {
            print_error!("{error}");
        }
        for line in &outcome.lines {
            println!("{line}");
        }
        summary.add(
            outcome.processed,
            outcome.violations,
            outcome.fixable,
            outcome.written,
            outcome.remaining,
        );
    }
    print_summary(&summary, &config);

    let failed = if config.fix {
        summary.remaining > 0
    } else {
        summary.violations > 0
    };
    Ok(if failed { ExitCode::FAILURE } else { ExitCode::SUCCESS })
}

/// Process every collected file and return the outcomes in file order.
///
/// The files are independent, so they are mapped in parallel and only the printing is sequential.
/// One thread keeps the run single threaded, which makes a timing comparison reproducible.
fn process_files(files: &[(PathBuf, FileKind)], config: &Config) -> Result<Vec<FileOutcome>> {
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(config.jobs)
        .build()
        .context("Failed to start the worker threads")?;
    Ok(pool.install(|| {
        let context = RunContext::new(config, files);
        files
            .par_iter()
            .map(|(file, kind)| process_file(file, *kind, &context))
            .collect()
    }))
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

/// Check or fix one file and return what should be printed and counted for it.
fn process_file(file: &Path, kind: FileKind, context: &RunContext<'_>) -> FileOutcome {
    let config = context.config;
    let mut outcome = FileOutcome::default();
    let text = match fs::read_to_string(file) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::InvalidData => {
            if config.verbose {
                let message = format!("Skipping file with invalid UTF-8: {}", context.display(file));
                outcome.lines.push(message.yellow().to_string());
            }
            return outcome;
        }
        Err(error) => {
            outcome.error = Some(format!("{}: Failed to read file: {error}", context.display(file)));
            return outcome;
        }
    };

    let settings = context.settings(file, kind);
    // The fixed text is only needed to write it or to print the diff,
    // and building it costs an extra pass over every file with a trailing comment.
    let result = if config.fix || config.print {
        format(&text, kind, settings.options)
    } else {
        FormatResult {
            violations: check(&text, kind, settings.options),
            fixed_text: None,
        }
    };
    outcome.processed = true;

    if config.verbose {
        let path = context.display(file);
        let origin = width_origin(&settings, config);
        outcome
            .lines
            .push(format!("{} (width {} from {origin})", path.cyan(), settings.width));
    }
    if result.violations.is_empty() {
        return outcome;
    }
    outcome.violations = result.violations.len();
    outcome.fixable = result.violations.iter().filter(|violation| violation.fixable).count();

    let path = context.display(file);
    if config.fix {
        fix_file(
            file,
            &path,
            &text,
            &result,
            kind,
            settings.options,
            config,
            &mut outcome,
        );
    } else {
        if !config.quiet {
            for violation in &result.violations {
                outcome.lines.push(format_violation(&path, violation, false));
            }
        }
        if config.print
            && let Some(fixed) = &result.fixed_text
        {
            outcome.lines.extend(diff_lines(&path, &text, fixed));
        }
    }
    outcome
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
    outcome: &mut FileOutcome,
) {
    let Some(fixed) = &result.fixed_text else {
        outcome.remaining = result.violations.len();
        if !config.quiet {
            for violation in &result.violations {
                outcome.lines.push(format_violation(path, violation, true));
            }
        }
        return;
    };
    if let Err(error) = fs::write(file, fixed) {
        outcome.error = Some(format!("{path}: Failed to write file: {error}"));
        outcome.remaining = result.violations.len();
        return;
    }
    outcome.written = true;
    let remaining = check(fixed, kind, options);
    let fixed_count = result.violations.len().saturating_sub(remaining.len());
    if !config.quiet {
        let message = format!(
            "Fixed {} in {path}",
            cli_tools::count_label(fixed_count, "violation", "violations")
        );
        outcome.lines.push(message.green().to_string());
        for violation in &remaining {
            outcome.lines.push(format_violation(path, violation, true));
        }
    }
    if config.print {
        outcome.lines.extend(diff_lines(path, text, fixed));
    }
    outcome.remaining = remaining.len();
}

/// Name of the place the line width came from, for the verbose report.
fn width_origin(settings: &FileSettings<'_>, config: &Config) -> String {
    match settings.source {
        Some(source) => format!("{} in {}", source.key, display_path(&source.file)),
        None if config.width.is_some() => "options".to_string(),
        None => "default".to_string(),
    }
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

    /// Resolve the width for one file the way a run does, with the name of where it came from.
    pub fn width_of(path: &std::path::Path, config: &Config) -> (usize, String) {
        let kind = FileKind::from_path(path).expect("the fixture should have a known kind");
        let files = vec![(path.to_path_buf(), kind)];
        let context = RunContext::new(config, &files);
        let settings = context.settings(path, kind);
        let origin = width_origin(&settings, config);
        (settings.width, origin)
    }

    /// Process one file the way a run does, returning its outcome and the summary it folds into.
    pub fn process_one(path: &std::path::Path, config: &Config) -> (FileOutcome, Summary) {
        let Some(kind) = config.kind.or_else(|| FileKind::from_path(path)) else {
            return (FileOutcome::default(), Summary::default());
        };
        let files = vec![(path.to_path_buf(), kind)];
        let context = RunContext::new(config, &files);
        let outcome = process_file(path, kind, &context);
        let mut summary = Summary::default();
        summary.add(
            outcome.processed,
            outcome.violations,
            outcome.fixable,
            outcome.written,
            outcome.remaining,
        );
        (outcome, summary)
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
        let (_outcome, summary) = process_one(&path, &config(&["slb", "-w", "40", "-q"]));

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
        let (_outcome, summary) = process_one(&path, &config(&["slb", "-w", "40", "-q", "--fix"]));

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
        let (_outcome, summary) = process_one(&path, &config(&["slb", "-q"]));

        assert_eq!(summary.files, 1);
        assert_eq!(summary.files_with_violations, 0);
        assert_eq!(summary.violations, 0);
    }

    #[test]
    fn an_unknown_file_type_is_skipped() {
        let (_directory, path) = file_with("data.bin", "content\n");
        let (_outcome, summary) = process_one(&path, &config(&["slb"]));

        assert_eq!(summary.files, 0);
    }

    #[test]
    fn the_forced_kind_overrides_the_file_extension() {
        let (_directory, path) = file_with("comments.unknown", "# A short comment.\n");
        let (_outcome, summary) = process_one(&path, &config(&["slb", "--type", "shell", "-q"]));

        assert_eq!(summary.files, 1);
    }

    #[test]
    fn invalid_utf8_is_skipped_without_an_error() {
        let (_directory, path) = file_with("clean.rs", "");
        fs::write(&path, [0x2f, 0x2f, 0x20, 0xff, 0xfe, 0x0a]).expect("file should be written");
        let (_outcome, summary) = process_one(&path, &config(&["slb", "-v"]));

        assert_eq!(summary.files, 0);
    }

    #[test]
    fn a_missing_file_is_reported_as_an_error() {
        let directory = temporary_directory();
        let (outcome, summary) = process_one(&directory.path().join("missing.rs"), &config(&["slb"]));

        assert_eq!(summary.files, 0);
        let error = outcome.error.expect("a missing file should be reported");
        assert!(error.contains("Failed to read file"), "{error}");
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
                message: "dash at the edge of a sentence cannot be rewritten automatically".into(),
                fixable: false,
            }],
            fixed_text: None,
        };
        let mut outcome = FileOutcome::default();
        fix_file(
            &path,
            "unfixable.rs",
            "/// Text.\n",
            &result,
            FileKind::Rust,
            &options,
            &config(&["slb", "--fix", "-q"]),
            &mut outcome,
        );

        assert_eq!(outcome.remaining, 1);
        assert!(!outcome.written);
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
                message: "dash at the edge of a sentence cannot be rewritten automatically".into(),
                fixable: false,
            }],
            fixed_text: None,
        };
        let mut outcome = FileOutcome::default();
        fix_file(
            &path,
            "unfixable.rs",
            "/// Text.\n",
            &result,
            FileKind::Rust,
            &options,
            &config(&["slb", "--fix"]),
            &mut outcome,
        );

        assert_eq!(outcome.remaining, 1);
        assert_eq!(outcome.lines.len(), 1, "the remaining violation should be reported");
    }

    #[test]
    fn a_violation_that_survives_the_fix_is_counted_as_remaining() {
        let text = "/// A dash at the end of a sentence \u{2014}\n";
        let (_directory, path) = file_with("dash.rs", text);
        let options = FormatOptions::with_width(120);
        let result = format(text, FileKind::Rust, &options);
        let mut outcome = FileOutcome::default();
        fix_file(
            &path,
            "dash.rs",
            text,
            &result,
            FileKind::Rust,
            &options,
            &config(&["slb", "--fix"]),
            &mut outcome,
        );

        assert!(outcome.remaining > 0, "the unfixable dash should remain");
    }

    #[test]
    fn fixed_text_is_written_and_reported() {
        let (_directory, path) = file_with("fixed.rs", "/// Before.\n");
        let options = FormatOptions::with_width(120);
        let result = FormatResult {
            violations: vec![],
            fixed_text: Some("/// After.\n".to_string()),
        };
        let mut outcome = FileOutcome::default();
        fix_file(
            &path,
            "fixed.rs",
            "/// Before.\n",
            &result,
            FileKind::Rust,
            &options,
            &config(&["slb", "--fix", "--print"]),
            &mut outcome,
        );

        assert!(outcome.written);
        assert_eq!(outcome.remaining, 0);
        assert_eq!(
            fs::read_to_string(&path).expect("file should be readable"),
            "/// After.\n"
        );
    }
}

#[cfg(test)]
mod test_width_resolution {
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
        let (width, source) = width_of(&path, &config(&["slb", "-w", "70"]));

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
        let (width, source) = width_of(&path, &config_without_width(&["slb"]));

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
        let (width, source) = width_of(&path, &config_without_width(&["slb", "--ignore-project-config"]));

        assert_eq!(width, DEFAULT_MAX_WIDTH);
        assert_eq!(source, "default");
    }

    #[test]
    fn the_default_width_is_used_without_any_config() {
        let (_directory, path) = file_with("main.rs", "fn main() {}\n");
        let (width, source) = width_of(&path, &config_without_width(&["slb"]));

        assert_eq!(width, DEFAULT_MAX_WIDTH);
        assert_eq!(source, "default");
    }

    #[test]
    fn the_user_config_width_is_reported_as_an_option() {
        let (_directory, path) = file_with("main.rs", "fn main() {}\n");
        let (width, source) = width_of(&path, &config(&["slb"]));

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

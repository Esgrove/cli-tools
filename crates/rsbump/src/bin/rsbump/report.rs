//! Printing `rsbump` project reports and the run summary.
//!
//! `ReportPrinter` prints each project report as one block as soon as the project is done,
//! turning the recorded steps into coloured lines,
//! and `print_summary` prints the totals and the projects that need attention.

use std::io::Write;
use std::path::Path;

use colored::Colorize;

use cli_tools_common::{diff_lines, path_to_string_relative_to, print_bold};
use cli_tools_rsbump::{BumpObserver, Outcome, Report, ReportEntry};

/// Prints each project report when its project finishes.
pub struct ReportPrinter<'a> {
    /// Directory that project paths are shown relative to.
    working_directory: Option<&'a Path>,
    /// Whether to print the output of successful commands and the steps that changed nothing.
    verbose: bool,
}

impl<'a> ReportPrinter<'a> {
    /// Create a printer that shows project paths relative to `working_directory`.
    pub const fn new(working_directory: Option<&'a Path>, verbose: bool) -> Self {
        Self {
            working_directory,
            verbose,
        }
    }
}

impl BumpObserver for ReportPrinter<'_> {
    fn on_project_finished(&self, report: &Report) {
        print_report(report, self.working_directory, self.verbose);
    }
}

/// Print the totals and the projects that need attention.
pub fn print_summary(reports: &[Report], working_directory: Option<&Path>) {
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
    list(&"Failed:".red().to_string(), &|report| report.is_failed());
}

/// Print one project report as a single block, so parallel reports do not interleave.
fn print_report(report: &Report, working_directory: Option<&Path>, verbose: bool) {
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
    for line in report_lines(report, verbose) {
        block.push_str(&line);
        block.push('\n');
    }
    let mut stdout = std::io::stdout().lock();
    // Printing is best effort, a closed stdout should not abort the remaining projects.
    let _ = writeln!(stdout, "{block}");
}

/// Turn the recorded steps of a report into indented output lines.
fn report_lines(report: &Report, verbose: bool) -> Vec<String> {
    let mut lines = Vec::new();
    let mut push = |line: String| lines.push(format!("  {line}"));
    for entry in &report.entries {
        match entry {
            ReportEntry::SkippedDirty => {
                push("Skipped, git repository has uncommitted changes".yellow().to_string());
            }
            ReportEntry::RustVersionBumped { manifest, old, new } => {
                let change = format!("rust-version {} -> {}", old.yellow(), new.green());
                if manifest == "Cargo.toml" {
                    push(change);
                } else {
                    push(format!("{manifest}: {change}"));
                }
            }
            ReportEntry::RustVersionCurrent(version) if verbose => {
                push(format!("rust-version {version} is current").dimmed().to_string());
            }
            ReportEntry::NoRustVersion => push("No rust-version field".yellow().to_string()),
            ReportEntry::Diff {
                path,
                original,
                updated,
            } => {
                for line in diff_lines(path, original, updated) {
                    push(line);
                }
            }
            ReportEntry::DockerfileWarning { path, warning } => {
                push(format!("{path}: {warning}").yellow().to_string());
            }
            ReportEntry::ImagePinned { path, old, new } => push(format!(
                "{path}: {} -> {}",
                shorten_digest(old).yellow(),
                shorten_digest(new).green()
            )),
            ReportEntry::StepSucceeded {
                label,
                output,
                output_is_result,
            } => {
                push(format!("{} {label}", "✓".green()));
                if verbose || *output_is_result {
                    for line in indented_output(output) {
                        push(line);
                    }
                }
            }
            ReportEntry::StepFailed { label, output } => {
                push(format!("{} {label} failed", "✗".red()));
                for line in indented_output(output) {
                    push(line);
                }
            }
            ReportEntry::Failed(message) => push(format!("{} {message}", "✗".red())),
            ReportEntry::WouldRun(command) => push(format!("Would run {command}").dimmed().to_string()),
            ReportEntry::NotARepository => {
                push("Not a git repository, nothing committed".dimmed().to_string());
            }
            ReportEntry::NotCommittingDirty => push(
                "Not committing, the repository had uncommitted changes before the update"
                    .yellow()
                    .to_string(),
            ),
            ReportEntry::NothingToCommit if verbose => push("Nothing to commit".dimmed().to_string()),
            ReportEntry::RustVersionCurrent(_) | ReportEntry::NothingToCommit => {}
        }
    }
    lines
}

/// Command output lines, indented one level deeper than the step, without blank lines.
fn indented_output(output: &str) -> impl Iterator<Item = String> + '_ {
    output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| format!("  {line}"))
}

/// Shorten a digest in an image reference for display.
fn shorten_digest(reference: &str) -> String {
    reference.split_once("@sha256:").map_or_else(
        || reference.to_string(),
        |(image, digest)| {
            let short: String = digest.chars().take(12).collect();
            format!("{image}@sha256:{short}…")
        },
    )
}

/// Project path relative to the working directory, or "." for the working directory itself.
fn display_path(path: &Path, working_directory: Option<&Path>) -> String {
    let display = path_to_string_relative_to(path, working_directory);
    if display.is_empty() { ".".to_string() } else { display }
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

#[cfg(test)]
mod test_shorten_digest {
    use super::*;

    #[test]
    fn shortens_digest() {
        let reference = "rust:1-slim@sha256:6f14b2b4b278ec0d50aa1d69541c304ce0f95e0f6268487aa9924af1c2e9aebe";
        assert_eq!(shorten_digest(reference), "rust:1-slim@sha256:6f14b2b4b278…");
    }

    #[test]
    fn keeps_reference_without_digest() {
        assert_eq!(shorten_digest("rust:1.100"), "rust:1.100");
    }
}

#[cfg(test)]
mod test_report_lines {
    use std::path::PathBuf;

    use super::*;

    /// Build a report holding the given entries.
    fn report_with(entries: Vec<ReportEntry>) -> Report {
        Report {
            root: PathBuf::from("/dev/app"),
            outcome: Outcome::Updated,
            missing_rust_version: false,
            committed: false,
            entries,
        }
    }

    /// Render a report and remove the colour codes, so the result does not depend on the terminal.
    fn plain_lines(report: &Report, verbose: bool) -> Vec<String> {
        report_lines(report, verbose)
            .iter()
            .map(|line| strip_colors(line))
            .collect()
    }

    /// Remove ANSI colour escape sequences from a line.
    fn strip_colors(line: &str) -> String {
        let mut plain = String::with_capacity(line.len());
        let mut characters = line.chars();
        while let Some(character) = characters.next() {
            if character == '\u{1b}' {
                characters.by_ref().find(|&next| next == 'm');
            } else {
                plain.push(character);
            }
        }
        plain
    }

    #[test]
    fn member_manifests_are_named_and_the_root_manifest_is_not() {
        let report = report_with(vec![
            ReportEntry::RustVersionBumped {
                manifest: "Cargo.toml".to_string(),
                old: "1.90".to_string(),
                new: "1.100".to_string(),
            },
            ReportEntry::RustVersionBumped {
                manifest: "crates/own/Cargo.toml".to_string(),
                old: "1.90".to_string(),
                new: "1.100".to_string(),
            },
        ]);

        assert_eq!(
            plain_lines(&report, false),
            [
                "  rust-version 1.90 -> 1.100",
                "  crates/own/Cargo.toml: rust-version 1.90 -> 1.100"
            ]
        );
    }

    #[test]
    fn successful_step_output_is_shown_only_when_verbose_or_when_it_is_the_result() {
        let step = |output_is_result| ReportEntry::StepSucceeded {
            label: "cargo update".to_string(),
            output: "Updating crates\n\n  Locking 1 package\n".to_string(),
            output_is_result,
        };

        assert_eq!(
            plain_lines(&report_with(vec![step(false)]), false),
            ["  ✓ cargo update"]
        );
        let shown = ["  ✓ cargo update", "    Updating crates", "      Locking 1 package"];
        assert_eq!(plain_lines(&report_with(vec![step(false)]), true), shown);
        assert_eq!(plain_lines(&report_with(vec![step(true)]), false), shown);
    }

    #[test]
    fn failed_step_output_is_always_shown() {
        let report = report_with(vec![ReportEntry::StepFailed {
            label: "cargo test --workspace".to_string(),
            output: "test failed".to_string(),
        }]);

        assert_eq!(
            plain_lines(&report, false),
            ["  ✗ cargo test --workspace failed", "    test failed"]
        );
    }

    #[test]
    fn quiet_entries_are_only_shown_when_verbose() {
        let report = report_with(vec![
            ReportEntry::RustVersionCurrent("1.100".to_string()),
            ReportEntry::NothingToCommit,
        ]);

        assert_eq!(plain_lines(&report, false), Vec::<String>::new());
        assert_eq!(
            plain_lines(&report, true),
            ["  rust-version 1.100 is current", "  Nothing to commit"]
        );
    }

    #[test]
    fn pinned_images_show_short_digests() {
        let report = report_with(vec![ReportEntry::ImagePinned {
            path: "Dockerfile".to_string(),
            old: "rust:1-slim".to_string(),
            new: "rust:1.100-slim@sha256:6f14b2b4b278ec0d50aa1d69541c304ce0f95e0f6268487aa9924af1c2e9aebe".to_string(),
        }]);

        assert_eq!(
            plain_lines(&report, false),
            ["  Dockerfile: rust:1-slim -> rust:1.100-slim@sha256:6f14b2b4b278…"]
        );
    }
}

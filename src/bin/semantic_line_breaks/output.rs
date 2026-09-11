//! Terminal reporting for `slb`.
//!
//! Holds the counters collected while processing files
//! and formats violations, diffs, and the final summary line for the terminal.

use colored::Colorize;
use difference::{Changeset, Difference};

use cli_tools::semantic_line_breaks::Violation;

use crate::config::Config;

/// Counters collected over all processed files.
#[derive(Debug, Default)]
pub struct Summary {
    /// Number of files processed.
    pub files: usize,
    /// Number of files with at least one violation.
    pub files_with_violations: usize,
    /// Total number of violations found.
    pub violations: usize,
    /// Number of violations fix mode can repair.
    pub fixable: usize,
    /// Number of files rewritten in fix mode.
    pub files_fixed: usize,
    /// Number of violations remaining after fixing.
    pub remaining: usize,
}

/// Format one violation for the terminal.
pub fn format_violation(path: &str, violation: &Violation, after_fix: bool) -> String {
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
pub fn print_diff(path: &str, original: &str, fixed: &str) {
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
pub fn print_summary(summary: &Summary, config: &Config) {
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

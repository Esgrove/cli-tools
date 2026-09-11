//! Terminal reporting for `slb`.
//!
//! Holds the counters collected while processing files
//! and formats violations, diffs, and the final summary line for the terminal.
//! The message building is kept separate from the printing so it can be tested.

use colored::{ColoredString, Colorize};
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
    for line in diff_lines(path, original, fixed) {
        println!("{line}");
    }
}

/// Print the final summary line.
pub fn print_summary(summary: &Summary, config: &Config) {
    println!("{}", summary_message(summary, config));
}

/// Lines of a line based diff between the original and fixed text, including the file header.
fn diff_lines(path: &str, original: &str, fixed: &str) -> Vec<String> {
    let mut lines = vec![format!("--- {path}").bold().to_string()];
    let changeset = Changeset::new(original, fixed, "\n");
    for difference in &changeset.diffs {
        match difference {
            Difference::Same(_) => {}
            Difference::Rem(removed) => {
                lines.extend(removed.lines().map(|line| format!("- {line}").red().to_string()));
            }
            Difference::Add(added) => {
                lines.extend(added.lines().map(|line| format!("+ {line}").green().to_string()));
            }
        }
    }
    lines
}

/// The final summary line for the processed files.
fn summary_message(summary: &Summary, config: &Config) -> ColoredString {
    let files = cli_tools::count_label(summary.files, "file", "files");
    if config.fix {
        let message = format!(
            "Checked {files}, fixed {}, {} remaining",
            cli_tools::count_label(summary.files_fixed, "file", "files"),
            cli_tools::count_label(summary.remaining, "violation", "violations")
        );
        if summary.remaining > 0 {
            message.yellow()
        } else {
            message.green()
        }
    } else if summary.violations == 0 {
        format!("Checked {files}, no violations").green()
    } else {
        format!(
            "Checked {files}, found {} in {} ({} fixable)",
            cli_tools::count_label(summary.violations, "violation", "violations"),
            cli_tools::count_label(summary.files_with_violations, "file", "files"),
            summary.fixable
        )
        .yellow()
    }
}

#[cfg(test)]
mod test_violation_messages {
    use cli_tools::semantic_line_breaks::ViolationKind;

    use super::*;

    fn violation(kind: ViolationKind, column: Option<usize>, fixable: bool) -> Violation {
        Violation {
            line: 7,
            column,
            kind,
            message: "line is 130 characters, limit is 120".to_string(),
            fixable,
        }
    }

    #[test]
    fn includes_the_column_only_when_known() {
        let without_column = format_violation("src/lib.rs", &violation(ViolationKind::LineTooLong, None, true), false);
        assert!(without_column.contains("src/lib.rs:7:"));
        assert!(!without_column.contains("src/lib.rs:7:4"));

        let with_column = format_violation(
            "src/lib.rs",
            &violation(ViolationKind::TrailingComment, Some(42), true),
            false,
        );
        assert!(with_column.contains("src/lib.rs:7:42"));
    }

    #[test]
    fn includes_the_rule_name_and_message() {
        let message = format_violation("a.md", &violation(ViolationKind::MidClauseBreak, None, true), false);
        assert!(message.contains("mid-clause"));
        assert!(message.contains("line is 130 characters, limit is 120"));
    }

    #[test]
    fn unfixable_and_after_fix_violations_are_formatted() {
        let fixable_before_fix = format_violation("a.rs", &violation(ViolationKind::Semicolon, None, true), false);
        let fixable_after_fix = format_violation("a.rs", &violation(ViolationKind::Semicolon, None, true), true);
        let unfixable = format_violation("a.rs", &violation(ViolationKind::Semicolon, None, false), false);
        for message in [&fixable_before_fix, &fixable_after_fix, &unfixable] {
            assert!(message.contains("semicolon"));
            assert!(message.contains("a.rs:7"));
        }
    }
}

#[cfg(test)]
mod test_diff_lines {
    use super::*;

    #[test]
    fn lists_removed_and_added_lines_with_a_header() {
        let lines = diff_lines(
            "a.rs",
            "// one sentence. And another.\n",
            "// one sentence.\n// And another.\n",
        );
        assert!(lines.first().is_some_and(|header| header.contains("--- a.rs")));
        assert!(
            lines
                .iter()
                .any(|line| line.contains("- // one sentence. And another."))
        );
        assert!(lines.iter().any(|line| line.contains("+ // one sentence.")));
        assert!(lines.iter().any(|line| line.contains("+ // And another.")));
    }

    #[test]
    fn unchanged_text_produces_only_the_header() {
        let lines = diff_lines("a.rs", "// same\n", "// same\n");
        assert_eq!(lines.len(), 1);
    }
}

#[cfg(test)]
mod test_summary_message {
    use clap::Parser;

    use super::*;
    use crate::Args;

    fn config(arguments: &[&str]) -> Config {
        let args = Args::try_parse_from(arguments).expect("arguments should parse");
        Config::from_args(&args).expect("config should build")
    }

    #[test]
    fn check_mode_reports_no_violations() {
        let summary = Summary {
            files: 1,
            ..Summary::default()
        };
        let message = summary_message(&summary, &config(&["slb"])).to_string();
        assert!(message.contains("Checked 1 file, no violations"));
    }

    #[test]
    fn check_mode_counts_violations_files_and_fixable() {
        let summary = Summary {
            files: 3,
            files_with_violations: 2,
            violations: 5,
            fixable: 4,
            ..Summary::default()
        };
        let message = summary_message(&summary, &config(&["slb"])).to_string();
        assert!(message.contains("Checked 3 files, found 5 violations in 2 files (4 fixable)"));
    }

    #[test]
    fn fix_mode_reports_fixed_files_and_remaining_violations() {
        let summary = Summary {
            files: 2,
            files_fixed: 1,
            remaining: 1,
            ..Summary::default()
        };
        let message = summary_message(&summary, &config(&["slb", "--fix"])).to_string();
        assert!(message.contains("Checked 2 files, fixed 1 file, 1 violation remaining"));
    }

    #[test]
    fn fix_mode_with_nothing_remaining_is_reported_as_done() {
        let summary = Summary {
            files: 1,
            files_fixed: 1,
            ..Summary::default()
        };
        let message = summary_message(&summary, &config(&["slb", "--fix"])).to_string();
        assert!(message.contains("Checked 1 file, fixed 1 file, 0 violation remaining"));
    }
}

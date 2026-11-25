//! Terminal output for the events and the outcome counts of adding torrents.
//!
//! Prints each `AddEvent` the library reports, hiding the per file details unless verbose,
//! and builds the summary printed at the end of a run.

use colored::Colorize;

use cli_tools_qtorrent::{AddEvent, AddObserver, TorrentStats};

/// Prints the events of adding torrents.
pub struct EventPrinter {
    /// Print the per file renames and failures.
    verbose: bool,
}

impl EventPrinter {
    /// Create a printer that shows the per file details when verbose.
    pub const fn new(verbose: bool) -> Self {
        Self { verbose }
    }
}

impl AddObserver for EventPrinter {
    fn on_event(&mut self, event: AddEvent<'_>) {
        match event {
            AddEvent::Added { name, is_multi_file } => {
                if is_multi_file {
                    println!("  {} Added with folder name: {}", "✓".green(), name.green());
                } else {
                    println!("  {} Added with name: {}", "✓".green(), name.green());
                }
            }
            AddEvent::RenamedOnDisk { old_name, new_name } => {
                println!("  {} Renamed on disk:", "✓".green());
                cli_tools_common::show_diff(old_name, new_name);
            }
            AddEvent::RenameOnDiskFailed {
                old_name,
                new_name,
                error,
            } => {
                if let Some(error) = error {
                    cli_tools_common::print_yellow!("Could not rename file/folder after retries: {error}");
                }
                println!(
                    "  {} You may need to manually rename in qBittorrent: {old_name} → {new_name}",
                    "⚠".yellow(),
                );
            }
            AddEvent::ExistingRenamed { old_name, new_name } => {
                println!("  {} Renamed:", "✓".green());
                cli_tools_common::show_diff(old_name, new_name);
            }
            AddEvent::ExistingRenamedOnDisk { old_name, new_name } => {
                println!(
                    "  {} Renamed on disk: {} → {}",
                    "✓".green(),
                    old_name.dimmed(),
                    new_name.green()
                );
            }
            AddEvent::ExistingRenameOnDiskFailed(error) => {
                cli_tools_common::print_yellow!("Could not rename file/folder on disk: {error}");
            }
            AddEvent::RenamingFiles => cli_tools_common::print_cyan("Renaming files..."),
            AddEvent::FileListFailed(error) => {
                cli_tools_common::print_yellow!("Could not get file list for dot-renaming: {error}");
            }
            AddEvent::FileRenamed { old_name, new_name } => {
                if self.verbose {
                    println!("    {} → {}", old_name.dimmed(), new_name.green());
                }
            }
            AddEvent::FileRenameFailed { name, error } => {
                if self.verbose {
                    cli_tools_common::print_yellow!("    Could not rename {name}: {error}");
                }
            }
            AddEvent::FilesRenamed(count) => {
                println!(
                    "  {} Renamed {} with dot formatting",
                    "✓".green(),
                    cli_tools_common::count_label(count, "file", "files")
                );
            }
            AddEvent::FileRenamesIncomplete(count) => {
                if !self.verbose {
                    cli_tools_common::print_yellow!(
                        "  Failed to dot-rename {} (use --verbose for details)",
                        cli_tools_common::count_label(count, "file", "files")
                    );
                }
            }
            AddEvent::ExcludedFilesSkipped(count) => {
                println!(
                    "  {} Set {} to skip",
                    "✓".green(),
                    cli_tools_common::count_label(count, "file", "files")
                );
            }
            AddEvent::ExcludedFilesSkipFailed { count, error } => {
                if let Some(error) = error {
                    cli_tools_common::print_yellow!("{error}");
                }
                println!(
                    "  {} You may need to manually skip {} in qBittorrent",
                    "⚠".yellow(),
                    cli_tools_common::count_label(count, "file", "files")
                );
            }
        }
    }
}

/// Print the run summary.
pub fn print_summary(stats: &TorrentStats) {
    for line in summary_lines(stats) {
        println!("{line}");
    }
}

/// Lines of the run summary, listing only the outcomes that happened.
///
/// The lines are built rather than printed directly so they can be checked in a test.
fn summary_lines(stats: &TorrentStats) -> Vec<String> {
    let mut lines = vec![
        format!("\n{}", "─".repeat(60)),
        "Summary:".bold().to_string(),
        format!("  Total:    {}", stats.total),
    ];
    if stats.success > 0 {
        lines.push(format!(
            "  {}    {} ({})",
            "Added:".green(),
            stats.success,
            cli_tools_common::format_size(stats.total_bytes),
        ));
    }
    if stats.renamed > 0 {
        lines.push(format!("  {}  {}", "Renamed:".cyan(), stats.renamed));
    }
    if stats.duplicate > 0 {
        lines.push(format!("  {} {}", "Existing:".dimmed(), stats.duplicate));
    }
    if stats.skipped > 0 {
        lines.push(format!("  {}  {}", "Skipped:".yellow(), stats.skipped));
    }
    if stats.error > 0 {
        lines.push(format!("  {}   {}", "Failed:".red(), stats.error));
    }
    lines
}

#[cfg(test)]
mod test_summary_lines {
    use super::*;

    /// Message without the terminal color codes.
    fn plain(message: &str) -> String {
        let mut result = String::with_capacity(message.len());
        let mut characters = message.chars();
        while let Some(character) = characters.next() {
            if character != '\u{1b}' {
                result.push(character);
                continue;
            }
            for escape in characters.by_ref() {
                if escape == 'm' {
                    break;
                }
            }
        }
        result
    }

    /// The whole summary as one plain string.
    fn summary(stats: &TorrentStats) -> String {
        plain(&summary_lines(stats).join("\n"))
    }

    #[test]
    fn a_run_with_no_outcomes_reports_only_the_total() {
        let stats = TorrentStats::new(3);

        let text = summary(&stats);

        assert!(text.contains("Total:    3"), "{text}");
        for absent in ["Added:", "Renamed:", "Existing:", "Skipped:", "Failed:"] {
            assert!(!text.contains(absent), "{absent} should be left out: {text}");
        }
    }

    #[test]
    fn added_torrents_report_their_count_and_total_size() {
        let mut stats = TorrentStats::new(2);
        stats.inc_success(1024 * 1024);
        stats.inc_success(1024 * 1024);

        let text = summary(&stats);

        assert!(text.contains("Added:    2 (2.00 MB)"), "{text}");
    }

    #[test]
    fn every_outcome_is_listed_when_it_happened() {
        let mut stats = TorrentStats::new(5);
        stats.inc_success(1024);
        stats.inc_renamed();
        stats.inc_duplicate();
        stats.inc_skipped();
        stats.inc_error();

        let text = summary(&stats);

        for expected in ["Added:", "Renamed:", "Existing:", "Skipped:", "Failed:"] {
            assert!(text.contains(expected), "{expected} should be listed: {text}");
        }
    }

    #[test]
    fn printing_the_summary_does_not_panic() {
        let mut stats = TorrentStats::new(1);
        stats.inc_success(1024);
        print_summary(&stats);
        print_summary(&TorrentStats::default());
    }
}

//! Counters collected while adding torrents.
//!
//! Holds the per outcome counts of one run and builds the summary reported at the end.
//! The message building is kept separate from the printing so it can be tested.

use colored::Colorize;

#[derive(Debug, Default)]
pub struct TorrentStats {
    total: usize,
    success: usize,
    skipped: usize,
    duplicate: usize,
    renamed: usize,
    error: usize,
    total_bytes: u64,
}

impl TorrentStats {
    pub const fn new(total: usize) -> Self {
        Self {
            total,
            success: 0,
            skipped: 0,
            duplicate: 0,
            renamed: 0,
            error: 0,
            total_bytes: 0,
        }
    }

    pub const fn inc_success(&mut self, bytes: u64) {
        self.success += 1;
        self.total_bytes += bytes;
    }

    pub const fn inc_skipped(&mut self) {
        self.skipped += 1;
    }

    pub const fn inc_duplicate(&mut self) {
        self.duplicate += 1;
    }

    pub const fn inc_renamed(&mut self) {
        self.renamed += 1;
    }

    pub const fn inc_error(&mut self) {
        self.error += 1;
    }

    pub fn print_summary(&self) {
        for line in self.summary_lines() {
            println!("{line}");
        }
    }

    /// Lines of the run summary, listing only the outcomes that happened.
    #[must_use]
    pub fn summary_lines(&self) -> Vec<String> {
        let mut lines = vec![
            format!("\n{}", "─".repeat(60)),
            "Summary:".bold().to_string(),
            format!("  Total:    {}", self.total),
        ];
        if self.success > 0 {
            lines.push(format!(
                "  {}    {} ({})",
                "Added:".green(),
                self.success,
                cli_tools::format_size(self.total_bytes),
            ));
        }
        if self.renamed > 0 {
            lines.push(format!("  {}  {}", "Renamed:".cyan(), self.renamed));
        }
        if self.duplicate > 0 {
            lines.push(format!("  {} {}", "Existing:".dimmed(), self.duplicate));
        }
        if self.skipped > 0 {
            lines.push(format!("  {}  {}", "Skipped:".yellow(), self.skipped));
        }
        if self.error > 0 {
            lines.push(format!("  {}   {}", "Failed:".red(), self.error));
        }
        lines
    }
}

#[cfg(test)]
mod test_torrent_stats {
    use super::*;

    #[test]
    fn starts_with_correct_total() {
        let stats = TorrentStats::new(10);
        assert_eq!(stats.total, 10);
    }

    #[test]
    fn starts_with_zero_counters() {
        let stats = TorrentStats::new(10);
        assert_eq!(stats.success, 0);
        assert_eq!(stats.skipped, 0);
        assert_eq!(stats.duplicate, 0);
        assert_eq!(stats.renamed, 0);
        assert_eq!(stats.error, 0);
        assert_eq!(stats.total_bytes, 0);
    }

    #[test]
    fn increments_counters() {
        let mut stats = TorrentStats::new(5);
        assert_eq!(stats.total, 5);
        assert_eq!(stats.success, 0);
        assert_eq!(stats.skipped, 0);
        assert_eq!(stats.duplicate, 0);
        assert_eq!(stats.renamed, 0);
        assert_eq!(stats.error, 0);
        assert_eq!(stats.total_bytes, 0);

        stats.inc_success(1024);
        stats.inc_skipped();
        stats.inc_duplicate();
        stats.inc_renamed();
        stats.inc_error();

        assert_eq!(stats.total, 5);
        assert_eq!(stats.success, 1);
        assert_eq!(stats.skipped, 1);
        assert_eq!(stats.duplicate, 1);
        assert_eq!(stats.renamed, 1);
        assert_eq!(stats.error, 1);
        assert_eq!(stats.total_bytes, 1024);
    }
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
        plain(&stats.summary_lines().join("\n"))
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
        stats.print_summary();
        TorrentStats::default().print_summary();
    }
}

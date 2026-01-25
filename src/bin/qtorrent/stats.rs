use colored::Colorize;

#[derive(Debug, Default)]
pub struct TorrentStats {
    total: usize,
    success: usize,
    skipped: usize,
    duplicate: usize,
    renamed: usize,
    error: usize,
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
        }
    }

    pub const fn inc_success(&mut self) {
        self.success += 1;
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
        println!("\n{}", "─".repeat(60));
        println!("{}", "Summary:".bold());
        println!("  Total:    {}", self.total);
        if self.success > 0 {
            println!("  {}    {}", "Added:".green(), self.success);
        }
        if self.renamed > 0 {
            println!("  {}  {}", "Renamed:".cyan(), self.renamed);
        }
        if self.duplicate > 0 {
            println!("  {} {}", "Existing:".dimmed(), self.duplicate);
        }
        if self.skipped > 0 {
            println!("  {}  {}", "Skipped:".yellow(), self.skipped);
        }
        if self.error > 0 {
            println!("  {}   {}", "Failed:".red(), self.error);
        }
    }
}

#[cfg(test)]
mod tests {
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

        stats.inc_success();
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
    }
}

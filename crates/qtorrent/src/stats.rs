//! Counters collected while adding torrents.
//!
//! Holds the per outcome counts of one run, which the caller reports at the end.

/// Number of torrents with each outcome in one run.
#[derive(Debug, Default)]
pub struct TorrentStats {
    /// Number of torrents in the run.
    pub total: usize,
    /// Number of torrents added.
    pub success: usize,
    /// Number of torrents skipped.
    pub skipped: usize,
    /// Number of torrents that already existed and were left as they are.
    pub duplicate: usize,
    /// Number of existing torrents that were renamed.
    pub renamed: usize,
    /// Number of torrents that failed to add.
    pub error: usize,
    /// Total size of the added torrents in bytes.
    pub total_bytes: u64,
}

impl TorrentStats {
    /// Create counters for a run with the given number of torrents.
    #[must_use]
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

    /// Count an added torrent and its size.
    pub const fn inc_success(&mut self, bytes: u64) {
        self.success += 1;
        self.total_bytes += bytes;
    }

    /// Count a skipped torrent.
    pub const fn inc_skipped(&mut self) {
        self.skipped += 1;
    }

    /// Count an existing torrent left as it is.
    pub const fn inc_duplicate(&mut self) {
        self.duplicate += 1;
    }

    /// Count a renamed existing torrent.
    pub const fn inc_renamed(&mut self) {
        self.renamed += 1;
    }

    /// Count a torrent that failed to add.
    pub const fn inc_error(&mut self) {
        self.error += 1;
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

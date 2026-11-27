//! Printing what the `dirmove` passes find and do.
//!
//! `ConsoleReporter` prints unpacking, file moves, and directory merges with a byte progress bar,
//! and `GroupingProgressBar` shows the progress of grouping files by prefix.
//! The functions format unpack summaries, whole directory merge offer titles,
//! and the debug listings of match groups and grouping names.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use colored::Colorize;

use indicatif::{ProgressBar, ProgressStyle};

use cli_tools_common::{
    count_label, get_relative_path_or_filename, path_to_filename_string, path_to_string_relative, print_error,
    print_magenta, print_yellow,
};
use cli_tools_dirmove::{
    DirectoryInfo, DirectoryScan, FileInfo, FileMatch, GroupingProgress, GroupingStage, MergeEvent, MergeObserver,
    MoveEvent, MoveObserver, PlannedMove, Roots, UnpackEvent, UnpackObserver, UnpackPlan,
};

/// Prints unpacking, file moves, and directory merges with a byte progress bar.
pub struct ConsoleReporter<'a> {
    /// Roots used to display merge paths compactly.
    roots: &'a Roots,
    /// Whether nothing is moved, which changes the merge summary wording.
    dryrun: bool,
    /// Whether to print every moved file and every merge step.
    verbose: bool,
    /// Whether to hide the progress bar.
    hide_progress: bool,
    /// Progress bar of the current batch of file moves.
    progress_bar: ProgressBar,
}

/// Shows grouping progress with one progress bar reused for every stage.
pub struct GroupingProgressBar {
    /// Progress bar reset at the start of each stage.
    progress_bar: ProgressBar,
}

impl<'a> ConsoleReporter<'a> {
    /// Create a reporter, optionally hiding the progress bar.
    pub fn new(roots: &'a Roots, dryrun: bool, verbose: bool, hide_progress: bool) -> Self {
        Self {
            roots,
            dryrun,
            verbose,
            hide_progress,
            progress_bar: ProgressBar::hidden(),
        }
    }

    /// Print a message above the progress bar.
    fn print(&self, print_message: impl FnOnce()) {
        self.progress_bar.suspend(print_message);
    }
}

impl MoveObserver for ConsoleReporter<'_> {
    fn on_event(&mut self, event: MoveEvent<'_>) {
        match event {
            MoveEvent::CreatedDirectory(path) => println!("  Created directory: {}", path_to_filename_string(path)),
            MoveEvent::Started { total_bytes } => {
                self.progress_bar = move_progress_bar(total_bytes, self.hide_progress);
            }
            MoveEvent::FileStarted { index, count, name } => {
                self.progress_bar.set_message(format!("[{}/{count}] {name}", index + 1));
            }
            MoveEvent::MissingFileName(path) => {
                self.print(|| print_error!("Could not get file name for path: {}", path.display()));
            }
            MoveEvent::DuplicateName(path) => self.print(|| {
                print_yellow!(
                    "Skipping {}, another file in this move has the same name",
                    path.display()
                );
            }),
            MoveEvent::SkippedExisting(path) => {
                self.print(|| print_yellow!("Skipping existing file: {}", path.display()));
            }
            MoveEvent::AlreadyInPlace(path) if self.verbose => {
                self.print(|| println!("  Already in place: {}", path.display()));
            }
            MoveEvent::DirectoryInTheWay(path) => {
                self.print(|| print_error!("Cannot replace existing directory with a file: {}", path.display()));
            }
            MoveEvent::RenameFailed {
                source,
                destination,
                error,
            } => self.print(|| {
                print_yellow!(
                    "Rename failed for {} -> {} ({error}), falling back to copy",
                    source.display(),
                    destination.display()
                );
            }),
            MoveEvent::Copied => finish_progress_if_complete(&self.progress_bar),
            MoveEvent::HashVerified(hash) if self.verbose => {
                self.print(|| println!("  Hash verified: BLAKE3 {}", short_hash(hash)));
            }
            MoveEvent::Moved(name) if self.verbose => self.print(|| println!("  Moved: {name}")),
            MoveEvent::Failed { source, error } => {
                self.print(|| print_error!("Failed to move {}: {error}", source.display()));
            }
            MoveEvent::Finished(report) => {
                self.progress_bar.finish_and_clear();
                let action = if self.dryrun { "Would move" } else { "Moved" };
                println!("  {action} {}", count_label(report.moved_count(), "file", "files"));
            }
            _ => {}
        }
    }

    fn on_progress(&mut self, bytes: u64) {
        self.progress_bar.inc(bytes);
    }
}

impl MergeObserver for ConsoleReporter<'_> {
    fn on_merge_event(&mut self, event: MergeEvent<'_>) {
        let verbose = self.verbose;
        match event {
            MergeEvent::Started { source, target } => {
                let source_display = self.roots.merge_display_path(source);
                let target_display = self.roots.merge_display_path(target);
                print_magenta!("{}", format!("Merging: {source_display} -> {target_display}").bold());
            }
            MergeEvent::Overwriting(name) if verbose => println!("  {} Overwriting: {name}", "→".yellow()),
            MergeEvent::WouldMove(name) if verbose => println!("  {} Would move: {name}", "→".cyan()),
            MergeEvent::Skipped(names) => {
                print_yellow!(
                    "Skipped {} that already exist (use -f to overwrite):",
                    count_label(names.len(), "file", "files")
                );
                for name in names {
                    println!("    {name}");
                }
            }
            MergeEvent::DuplicateTarget(source) => {
                print_yellow!(
                    "Skipping {}, another entry in this merge has the same destination",
                    source.display()
                );
            }
            MergeEvent::Moved(count) => {
                let action = if self.dryrun { "Would move" } else { "Moved" };
                println!("  {action} {}", count_label(count, "item", "items"));
            }
            MergeEvent::SourceRemoved(source) if verbose => {
                println!(
                    "  {} Removed empty directory: {}",
                    "→".green(),
                    self.roots.merge_display_path(source)
                );
            }
            MergeEvent::SourceRemoveFailed(source, error) => {
                print_yellow!(
                    "Failed to remove empty directory {}: {error}",
                    self.roots.merge_display_path(source)
                );
            }
            MergeEvent::WouldRemoveSource(source) if verbose => {
                println!(
                    "  {} Would remove empty directory: {}",
                    "→".yellow(),
                    self.roots.merge_display_path(source)
                );
            }
            _ => {}
        }
    }
}

impl UnpackObserver for ConsoleReporter<'_> {
    fn on_unpack_event(&mut self, event: UnpackEvent<'_>) {
        match event {
            UnpackEvent::UnwantedDeleted(directory) => {
                let label = if self.dryrun {
                    "Would delete unwanted:"
                } else {
                    "Deleted unwanted:"
                };
                println!("{} {}", label.yellow(), path_to_string_relative(directory));
            }
            UnpackEvent::UnwantedDeleteFailed { directory, error } => {
                print_error!("Failed to delete {}: {error}", path_to_string_relative(directory));
            }
            UnpackEvent::Planned { parent, plan } => print_unpack_summary(parent, plan, self.verbose),
            UnpackEvent::SkippedExistingDirectory(target) => {
                print_yellow!("Skipping existing directory: {}", target.display());
            }
            UnpackEvent::SkippedExistingFile(target) => {
                print_yellow!("Skipping existing file: {}", target.display());
            }
            UnpackEvent::DuplicateTarget(source) => {
                print_yellow!(
                    "Skipping {}, another entry moves to the same name",
                    path_to_string_relative(source)
                );
            }
            UnpackEvent::MoveFailed { source, error } => {
                print_error!("Failed to move {}: {error:#}", path_to_string_relative(source));
            }
            UnpackEvent::Pruned(directory) if self.verbose => {
                println!("  {} Removed empty dir: {}", "→".green(), directory.display());
            }
            UnpackEvent::Pruned(_) => {}
        }
    }
}

impl GroupingProgressBar {
    /// Create a grouping progress bar, hidden when `hide_progress` is set.
    pub fn new(hide_progress: bool) -> Self {
        let progress_bar = if hide_progress {
            ProgressBar::hidden()
        } else {
            let progress_bar = ProgressBar::new(0);
            progress_bar.set_style(
                ProgressStyle::default_bar()
                    .template("[{elapsed_precise}] {bar:40.cyan/blue} {pos}/{len} {msg}")
                    .expect("Failed to set progress bar template")
                    .progress_chars("=> "),
            );
            progress_bar
        };
        Self { progress_bar }
    }
}

impl GroupingProgress for GroupingProgressBar {
    fn start_stage(&self, stage: GroupingStage, length: u64) {
        self.progress_bar.reset();
        self.progress_bar.set_length(length);
        self.progress_bar.set_message(match stage {
            GroupingStage::CollectingPrefixes => "Collecting prefixes",
            GroupingStage::MatchingFilesToGroups => "Matching files to groups",
        });
    }

    fn advance(&self) {
        self.progress_bar.inc(1);
    }

    fn finish_stage(&self) {
        self.progress_bar.finish_and_clear();
    }
}

/// Print summary of what will be unpacked.
fn print_unpack_summary(target_dir: &Path, info: &UnpackPlan, verbose: bool) {
    let dir_display = path_to_string_relative(target_dir);
    let file_count = info.file_moves.len();
    let dir_count = info.directory_moves.len();

    let mut counts = Vec::new();
    if dir_count > 0 {
        counts.push(format!("{dir_count} d"));
    }
    if file_count > 0 {
        counts.push(format!("{file_count} f"));
    }
    let counts_str = if counts.is_empty() {
        String::new()
    } else {
        format!(" ({})", counts.join(", "))
    };
    let header = format!("Unpacking: {dir_display}{counts_str}");

    print_magenta!("{}", header.bold());

    for PlannedMove { source, target } in &info.directory_moves {
        let src_display = get_relative_path_or_filename(source, target_dir);
        let dst_display = get_relative_path_or_filename(target, target_dir);
        println!("  {src_display} -> {dst_display}");
    }
    if verbose {
        for PlannedMove { source, target } in &info.file_moves {
            let src_display = get_relative_path_or_filename(source, target_dir);
            let dst_display = get_relative_path_or_filename(target, target_dir);
            println!("  {src_display} -> {dst_display}");
        }
    }
}

/// Print match groups with file counts, sorted by count descending.
pub fn print_match_debug(matches: &HashMap<usize, Vec<FileMatch>>, directories: &[DirectoryInfo]) {
    eprintln!("Directory match groups:");
    let mut sorted_matches: Vec<_> = matches.iter().collect();
    sorted_matches.sort_by_key(|entry| std::cmp::Reverse(entry.1.len()));
    for (&directory_index, matched_files) in sorted_matches {
        let Some(directory) = directories.get(directory_index) else {
            continue;
        };
        eprintln!(
            "  {} -> {}",
            directory.name,
            count_label(matched_files.len(), "file", "files")
        );
    }
    if matches.is_empty() {
        eprintln!("  (no matches found)");
    }
    eprintln!();
}

/// Print the unique processed names used for grouping.
pub fn print_grouping_names_debug(files_with_names: &[FileInfo]) {
    let unique_names: HashSet<_> = files_with_names
        .iter()
        .map(|file| file.filtered_name.as_str())
        .collect();
    let mut sorted_names: Vec<_> = unique_names.into_iter().collect();
    sorted_names.sort_unstable();
    eprintln!("Unique processed names for grouping:");
    for name in sorted_names {
        eprintln!("  {name}");
    }
}

/// Print the resolved config and the input and output roots.
pub fn print_run_debug(config: &impl std::fmt::Debug, roots: &Roots) {
    eprintln!("Config: {config:#?}");
    eprintln!("Input roots:");
    for input_root in roots.input() {
        eprintln!("  {}", input_root.display());
    }
    eprintln!("Output roots:");
    for output_root in roots.output() {
        eprintln!("  {}", output_root.display());
    }
}

/// Print the destination directories a scan left out by the exclude patterns and the name filter.
pub fn print_scan_skips(scan: &DirectoryScan) {
    for directory in &scan.excluded {
        println!("Excluding directory: {}", path_to_string_relative(directory));
    }
    for directory in &scan.ignored {
        println!("Ignoring directory: {}", path_to_string_relative(directory));
    }
}

/// Print the ignored prefixes and each file name left after stripping them.
pub fn print_prefix_ignores_debug(prefix_ignores: &[String], files_with_names: &[FileInfo]) {
    eprintln!("prefix_ignores: {prefix_ignores:?}");
    eprintln!("Original names after stripping prefix_ignores:");
    for file_info in files_with_names {
        eprintln!("  {}", file_info.original_name);
    }
}

/// Print a group heading with its file count, followed by one already highlighted file name per line.
pub fn print_file_group(heading: &str, highlighted_names: &[String]) {
    println!("{heading}: {}", count_label(highlighted_names.len(), "file", "files"));
    for name in highlighted_names {
        println!("  {name}");
    }
}

/// Print whether the directory an offer moves files into already exists or will be created.
pub fn print_target_directory_status(exists: bool, name: &str) {
    if exists {
        println!("  {} Directory already exists: {}", "→".green(), name.cyan());
    } else {
        println!("  {} Will create directory: {}", "→".yellow(), name.cyan());
    }
}

/// Print that the user skipped an offer.
pub fn print_skipped() {
    println!("  Skipped");
}

/// Print the outcome of saving a group name to `ignored_group_names` in the config file.
pub fn print_ignored_group_name_saved(name: &str, result: &anyhow::Result<bool>) {
    match result {
        Ok(true) => println!(
            "  {} Added '{}' to ignored_group_names in config",
            "✓".green(),
            name.cyan()
        ),
        Ok(false) => println!(
            "  {} '{}' already in ignored_group_names config",
            "→".dimmed(),
            name.cyan()
        ),
        Err(error) => print_error!("Failed to update config: {error}"),
    }
}

/// Format the title for a whole directory merge offer.
pub fn directory_merge_title(source_file_counts: &[(&Path, usize)]) -> String {
    source_file_counts
        .first()
        .map_or_else(String::new, |(source_directory, _)| {
            path_to_filename_string(source_directory)
        })
}

/// Return the confirmation prompt for a whole directory merge offer.
pub const fn directory_merge_confirmation_prompt(source_directory_count: usize) -> &'static str {
    if source_directory_count == 1 {
        "Merge directory to this directory?"
    } else {
        "Merge directories to this directory?"
    }
}

/// Create a progress bar for moving file bytes.
fn move_progress_bar(length: u64, hide_progress: bool) -> ProgressBar {
    if hide_progress {
        return ProgressBar::hidden();
    }

    let progress_bar = ProgressBar::new(length);
    progress_bar.set_style(
        ProgressStyle::default_bar()
            .template("{spinner:.green} [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({eta}) {msg}")
            .unwrap_or_else(|_| ProgressStyle::default_bar())
            .progress_chars("█▓░"),
    );
    progress_bar
}

/// Finish the progress bar once its tracked byte count is complete.
fn finish_progress_if_complete(progress_bar: &ProgressBar) {
    if progress_bar
        .length()
        .is_some_and(|length| progress_bar.position() >= length)
    {
        progress_bar.finish();
    }
}

/// Format a BLAKE3 hash for compact verbose output.
fn short_hash(hash: &blake3::Hash) -> String {
    hash.to_string().chars().take(12).collect()
}

#[cfg(test)]
mod test_directory_merge_title {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn directory_merge_title_uses_single_source_directory() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let input_root = tmp.path().join("input");
        let output_root = tmp.path().join("output");
        let input_directory = input_root.join("Example");
        std::fs::create_dir_all(&input_directory)?;
        std::fs::create_dir_all(&output_root)?;

        let source_file_counts = [(input_directory.as_path(), 0)];

        assert_eq!(directory_merge_title(&source_file_counts), "Example");
        Ok(())
    }

    #[test]
    fn directory_merge_title_uses_plain_name_for_multiple_sources() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let input_root_a = tmp.path().join("input_a");
        let input_root_b = tmp.path().join("input_b");
        let output_root = tmp.path().join("output");
        let input_directory_a = input_root_a.join("Example");
        let input_directory_b = input_root_b.join("Example");
        std::fs::create_dir_all(&input_directory_a)?;
        std::fs::create_dir_all(&input_directory_b)?;
        std::fs::create_dir_all(&output_root)?;

        let source_file_counts = [(input_directory_a.as_path(), 0), (input_directory_b.as_path(), 0)];

        assert_eq!(directory_merge_title(&source_file_counts), "Example");
        Ok(())
    }
}

#[cfg(test)]
mod test_move_progress {
    use super::*;

    #[test]
    fn a_hidden_bar_reports_no_length() {
        let bar = move_progress_bar(1024, true);

        assert!(bar.is_hidden());
    }

    #[test]
    fn a_visible_bar_carries_the_given_length() {
        let bar = move_progress_bar(1024, false);

        assert_eq!(bar.length(), Some(1024));
        bar.finish_and_clear();
    }

    #[test]
    fn finish_progress_only_marks_complete_bar_finished() {
        let incomplete = ProgressBar::with_draw_target(Some(10), indicatif::ProgressDrawTarget::hidden());
        incomplete.set_position(9);
        finish_progress_if_complete(&incomplete);
        assert!(!incomplete.is_finished());

        let complete = ProgressBar::with_draw_target(Some(10), indicatif::ProgressDrawTarget::hidden());
        complete.set_position(10);
        finish_progress_if_complete(&complete);
        assert!(complete.is_finished());
    }

    #[test]
    fn short_hash_uses_first_twelve_hex_characters() {
        let hash = blake3::hash(b"contents");
        let hash_string = hash.to_string();
        let expected = hash_string.get(..12).expect("BLAKE3 hash should contain 12 characters");
        assert_eq!(short_hash(&hash), expected);
    }
}

#[cfg(test)]
mod test_directory_merge_confirmation_prompt {
    use super::*;

    #[test]
    fn directory_merge_confirmation_prompt_uses_singular_for_one_source() {
        assert_eq!(
            directory_merge_confirmation_prompt(1),
            "Merge directory to this directory?"
        );
    }

    #[test]
    fn directory_merge_confirmation_prompt_uses_plural_for_multiple_sources() {
        assert_eq!(
            directory_merge_confirmation_prompt(2),
            "Merge directories to this directory?"
        );
    }
}

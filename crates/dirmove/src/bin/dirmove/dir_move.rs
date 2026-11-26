//! Interactive `dirmove` run.
//!
//! `DirMove` holds the resolved roots, the config, and the directory name database,
//! and runs the passes in order: unpacking, prefixed directory merging, database sync,
//! moving input directories and files to matching output directories,
//! and creating directories for file groups.
//! The library components decide what to do, and this module prints the offers, prompts, and moves.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use colored::Colorize;

use cli_tools_common::locks::ActiveLockStore;
use cli_tools_common::{
    TargetReservations, count_label, format_text_with_highlight, path_to_filename_string, print_bold, print_error,
    print_yellow,
};
use cli_tools_dirmove::{
    DirectoryInfo, DirectoryMatcher, DirectoryMerger, FileInfo, FileMatch, PrefixGrouper, Roots, Scanner, Unpacker,
    matcher, merge, mover, utils,
};
use cli_tools_media::scan_cache::ScanCache;

use crate::DirMoveArgs;
use crate::config::{Config, add_ignored_group_name_to_config, config_from_args};
use crate::database::Database;
use crate::prompt::{self, PromptResult};
use crate::report;

/// Whether progress bars are hidden, which keeps test output clean.
const HIDE_PROGRESS: bool = cfg!(test);

/// Result of offering one directory match group to the user.
enum DirectoryMatchOutcome {
    /// Files were moved, or would be moved in dry-run mode, and should not be offered again.
    Consumed(Vec<PathBuf>),
    /// Files are still available for lower-priority matches.
    Available,
}

/// Runtime state and configuration for a `dirmove` invocation.
///
/// Files and source directories are gathered from every input root
/// and matched against destination directories under every output root.
/// Implicit destinations use the first output root.
pub struct DirMove {
    /// Input and output roots.
    roots: Roots,
    /// Resolved options.
    config: Config,
    /// Directory name database, missing when it could not be opened.
    database: Option<Database>,
}

impl DirectoryMatchOutcome {
    /// Return the files that should not be offered again.
    fn consumed_files(self) -> Vec<PathBuf> {
        match self {
            Self::Consumed(files) => files,
            Self::Available => Vec::new(),
        }
    }
}

impl DirMove {
    /// Build a `DirMove` instance from CLI arguments.
    ///
    /// Positional input paths and `--output` paths are independent sets, not pairs.
    /// All input files
    /// and candidate input directories are matched against directories found under all output roots.
    /// If no output roots are provided, the input roots are also used as output roots.
    pub fn try_from_args(args: DirMoveArgs) -> anyhow::Result<Self> {
        let roots = Roots::resolve(&args.path, &args.output)?;
        let config = config_from_args(args)?;
        if config.debug {
            report::print_run_debug(&config, &roots);
        }
        let dirmove = Self::new_with_roots(roots, config);
        if dirmove.config.debug {
            dirmove.print_database_debug_info();
        }
        Ok(dirmove)
    }

    /// Run the configured operations in the standard `dirmove` order.
    ///
    /// Existing maintenance steps run first, then whole-directory merge offers are made
    /// before normal file-to-directory matching.
    /// Create mode runs last and uses the first output root for newly-created directories.
    pub fn run(&mut self) -> anyhow::Result<()> {
        // Unwanted directories are deleted and unpack directories unpacked in a single directory walk.
        self.unpack_directories();
        self.merge_prefixed_directories()?;
        let directories = self.collect_directories()?;
        self.update_database_with_directories(&directories);
        self.move_files_to_dir_with_directories(&directories)?;

        if self.config.create {
            self.create_dirs_and_move_files()?;
        }

        Ok(())
    }

    /// Create a `DirMove` from resolved roots and open the default database.
    ///
    /// A dry run never writes to the database, so it only opens a database that already exists.
    fn new_with_roots(roots: Roots, config: Config) -> Self {
        let database = if config.dryrun {
            Database::open_existing()
        } else {
            Database::open_default().map(Some)
        };
        let database = match database {
            Ok(database) => database,
            Err(error) => {
                print_yellow!("Failed to open database: {error}");
                None
            }
        };
        Self {
            roots,
            config,
            database,
        }
    }

    /// Return the scanner for the configured roots.
    const fn scanner(&self) -> Scanner<'_> {
        Scanner::new(&self.roots, &self.config.scan, &self.config.name_filter)
    }

    /// Return the matcher for destination directories under the output roots.
    fn matcher(&self) -> DirectoryMatcher<'_> {
        DirectoryMatcher::new(&self.roots, &self.config.name_filter, &self.config.custom_mappings)
    }

    /// Return the prefix grouper for create mode.
    const fn grouper(&self) -> PrefixGrouper<'_> {
        PrefixGrouper::new(&self.config.grouping, &self.config.name_filter)
    }

    /// Return the reporter that prints file moves and directory merges.
    fn reporter(&self) -> report::ConsoleReporter<'_> {
        report::ConsoleReporter::new(&self.roots, self.config.dryrun, self.config.verbose, HIDE_PROGRESS)
    }

    /// Delete unwanted directories and unpack configured directory names across every input root.
    ///
    /// This pass runs before file matching
    /// so cleaned and unpacked input trees are reflected in later file and directory merge offers.
    fn unpack_directories(&self) {
        let unpacker = Unpacker::new(
            &self.config.unpack_directory_names,
            self.config.overwrite,
            self.config.dryrun,
        );
        unpacker.unpack(self.roots.input(), self.config.scan.recurse(), &mut self.reporter());
    }

    /// Merge directories that have a `prefix_ignore` prefix into matching directories without the prefix.
    ///
    /// For example, if `prefix_ignore` contains "studio" and there are directories "studio alpha beta"
    /// and "alpha beta", the contents of "studio alpha beta" will be merged into "alpha beta".
    fn merge_prefixed_directories(&self) -> anyhow::Result<()> {
        let name_filter = &self.config.name_filter;
        if !name_filter.has_prefix_ignores() {
            return Ok(());
        }

        for pair in merge::find_prefix_merge_pairs(self.roots.output(), self.config.scan.recurse(), name_filter)? {
            self.merge_directory_contents(&pair.source, &pair.target)?;
        }

        Ok(())
    }

    /// Merge contents from the source directory into the target directory and print each step.
    fn merge_directory_contents(&self, source: &Path, target: &Path) -> anyhow::Result<()> {
        DirectoryMerger::new(self.config.overwrite, self.config.dryrun).merge(source, target, &mut self.reporter())
    }

    /// Print debug information about the database contents.
    fn print_database_debug_info(&self) {
        if let Some(ref db) = self.database {
            db.print_debug_info();
        } else {
            eprintln!("Database not available");
        }
    }

    /// Update the database with directory names already collected under every output root.
    ///
    /// Uses one batch transaction to update display casing when directories change on disk.
    /// Dry-run mode does not write to the database.
    fn update_database_with_directories(&mut self, directories: &[DirectoryInfo]) {
        if self.config.dryrun {
            return;
        }
        let grouper = self.grouper();
        let names: Vec<String> = directories
            .iter()
            .map(|directory_info| path_to_filename_string(&directory_info.path))
            .filter(|directory_name| grouper.accepts_group_name(directory_name))
            .collect();
        let Some(ref mut database) = self.database else {
            return;
        };

        if let Err(error) = database.batch_upsert(&names)
            && self.config.debug
        {
            eprintln!("Failed to update directory names in database: {error}");
        }
    }

    /// Add or update a directory name in the database.
    /// Does nothing in dry-run mode.
    fn add_to_database(&self, name: &str) {
        if self.config.dryrun {
            return;
        }
        let Some(ref db) = self.database else {
            return;
        };

        if self.grouper().accepts_group_name(name)
            && let Err(e) = db.upsert(name)
            && self.config.debug
        {
            eprintln!("Failed to update database: {e}");
        }
    }

    /// Get the database formatting of a directory name, but only if the database has an exact match.
    fn get_database_suggestion_for_prefix(&self, dir_name: &str) -> Option<String> {
        self.database.as_ref()?.exact_match_display_name(dir_name)
    }

    /// Collect candidate destination directories and print the excluded and ignored ones in verbose mode.
    fn collect_directories(&self) -> anyhow::Result<Vec<DirectoryInfo>> {
        let scan = self.scanner().collect_directories()?;
        if self.config.verbose {
            report::print_scan_skips(&scan);
        }
        Ok(scan.directories)
    }

    /// Match input files to existing output directories and move confirmed file groups.
    #[cfg(test)]
    fn move_files_to_dir(&self) -> anyhow::Result<()> {
        let directories = self.collect_directories()?;
        self.move_files_to_dir_with_directories(&directories)
    }

    /// Match input files to already-collected output directories and move confirmed file groups.
    fn move_files_to_dir_with_directories(&self, directories: &[DirectoryInfo]) -> anyhow::Result<()> {
        if directories.is_empty() {
            return Ok(());
        }

        if !self.config.files_only {
            self.move_input_directories_to_matching_output(directories)?;
        }

        let files_in_root = self.scanner().collect_files()?;
        if files_in_root.is_empty() {
            return Ok(());
        }

        // Custom mappings take priority.
        // Files remain available for normal matching if a custom suggestion is skipped or fails.
        let mut consumed_files = HashSet::new();
        let remaining_files = self.process_custom_mapping_matches(&files_in_root, directories, &mut consumed_files)?;
        if remaining_files.is_empty() {
            return Ok(());
        }

        let matches = self.matcher().match_files(&remaining_files, directories);
        if self.config.debug {
            report::print_match_debug(&matches, directories);
        }
        if matches.is_empty() {
            return Ok(());
        }

        if self.config.verbose {
            print_bold!(
                "Found {} with files to move:\n",
                count_label(matches.len(), "directory match", "directory matches")
            );
        }
        self.offer_directory_matches(matches, &remaining_files, directories, &mut consumed_files)
    }

    /// Process custom mapping matches, moving files matching custom patterns to their target directories.
    /// Returns files that are still available for later suggestions.
    fn process_custom_mapping_matches(
        &self,
        files: &[PathBuf],
        directories: &[DirectoryInfo],
        consumed_files: &mut HashSet<PathBuf>,
    ) -> anyhow::Result<Vec<PathBuf>> {
        if self.config.custom_mappings.is_empty() {
            return Ok(files.to_vec());
        }

        let custom_matches = self.matcher().match_custom_mappings(files, directories);
        if !custom_matches.is_empty() {
            if self.config.verbose {
                print_bold!(
                    "Found {} with files to move:\n",
                    count_label(custom_matches.len(), "custom mapping match", "custom mapping matches")
                );
            }
            self.offer_directory_matches(custom_matches, files, directories, consumed_files)?;
        }

        Ok(files
            .iter()
            .filter(|file_path| file_is_available(file_path, consumed_files))
            .cloned()
            .collect())
    }

    /// Offer each directory's matched files in directory priority order.
    ///
    /// Files consumed by an earlier offer are left out of later ones,
    /// and a directory with no files left is not offered.
    fn offer_directory_matches(
        &self,
        mut matches: HashMap<usize, Vec<FileMatch>>,
        files: &[PathBuf],
        directories: &[DirectoryInfo],
        consumed_files: &mut HashSet<PathBuf>,
    ) -> anyhow::Result<()> {
        for directory_index in self.matcher().sorted_directory_indices(directories) {
            let Some(file_matches) = matches.remove(&directory_index) else {
                continue;
            };

            let mut available_files = Vec::with_capacity(file_matches.len());
            for file_match in file_matches {
                let file_path = files
                    .get(file_match.index)
                    .ok_or_else(|| anyhow::anyhow!("File match index {} is out of bounds", file_match.index))?;
                if file_is_available(file_path, consumed_files) {
                    available_files.push(file_match);
                }
            }
            if available_files.is_empty() {
                continue;
            }

            let directory = directories
                .get(directory_index)
                .ok_or_else(|| anyhow::anyhow!("Directory match index {directory_index} is out of bounds"))?;
            let outcome = self.process_directory_match(directory, files, &available_files)?;
            consumed_files.extend(outcome.consumed_files());
        }

        Ok(())
    }

    /// Print one directory match offer, and move the files when confirmed.
    fn process_directory_match(
        &self,
        dir: &DirectoryInfo,
        source_files: &[PathBuf],
        file_matches: &[FileMatch],
    ) -> anyhow::Result<DirectoryMatchOutcome> {
        let file_paths: Vec<_> = file_matches
            .iter()
            .map(|file_match| {
                source_files
                    .get(file_match.index)
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("File match index {} is out of bounds", file_match.index))
            })
            .collect::<anyhow::Result<_>>()?;
        let dir_display = self.roots.directory_display_path(dir);
        let move_to_display = self.roots.move_to_display_path(&dir.path, &file_paths);
        let highlighted_names: Vec<String> = file_matches
            .iter()
            .zip(&file_paths)
            .map(|(file_match, file_path)| {
                format_text_with_highlight(&path_to_filename_string(file_path), Some(file_match.range))
            })
            .collect();
        report::print_file_group(&dir_display.cyan().bold().to_string(), &highlighted_names);

        println!("{} Move to: {move_to_display}", "→".green());

        if self.config.dryrun {
            let report =
                mover::preview_file_moves(&dir.path, &file_paths, self.config.overwrite, &mut self.reporter())?;
            println!();
            return Ok(DirectoryMatchOutcome::Consumed(report.moved_files));
        }

        let confirmed =
            self.config.auto || cli_tools_common::get_user_confirmation("Move files to this directory?", false)?;
        let outcome = if confirmed {
            match self.move_files_to_target_dir(&dir.path, &file_paths) {
                Ok(report) => DirectoryMatchOutcome::Consumed(report.moved_files),
                Err(e) => {
                    print_error!("Failed to move files to {}: {e}", dir.path.display());
                    DirectoryMatchOutcome::Available
                }
            }
        } else {
            report::print_skipped();
            DirectoryMatchOutcome::Available
        };

        println!();
        Ok(outcome)
    }

    /// Offer whole-directory merges for input directories with matching output directories.
    ///
    /// A source directory matches when its normalized directory name equals an output directory's normalized name.
    /// This pass is skipped entirely by `--files-only`.
    fn move_input_directories_to_matching_output(&self, directories: &[DirectoryInfo]) -> anyhow::Result<()> {
        let source_directories = self.scanner().collect_input_directories()?;
        if source_directories.is_empty() {
            return Ok(());
        }

        let groups = self
            .matcher()
            .match_input_directories(source_directories, directories)?;
        if groups.is_empty() {
            return Ok(());
        }

        print_bold!(
            "Found {} with directories to merge:\n",
            count_label(groups.len(), "directory merge match", "directory merge matches")
        );

        for (target_directory, source_directories) in groups {
            self.process_directory_merge_match(target_directory, &source_directories)?;
        }

        Ok(())
    }

    /// Print and optionally execute one whole-directory merge offer.
    ///
    /// Confirmed merges move the contents of each source directory into the selected target output directory
    /// and remove the source directory if it becomes empty.
    /// Existing target entries are skipped unless `--force` is enabled.
    /// A destination reserved by an earlier source cannot be overwritten in the same offer.
    fn process_directory_merge_match(
        &self,
        target_directory: &DirectoryInfo,
        source_directories: &[PathBuf],
    ) -> anyhow::Result<()> {
        let target_display = self.roots.merge_display_path(&target_directory.path);
        let source_file_counts: Vec<_> = source_directories
            .iter()
            .map(|source_directory| {
                (
                    source_directory.as_path(),
                    cli_tools_common::count_files(source_directory),
                )
            })
            .collect();
        let file_count = source_file_counts.iter().map(|(_, count)| *count).sum();
        println!(
            "{}: {}, {}",
            report::directory_merge_title(&source_file_counts).cyan().bold(),
            count_label(source_directories.len(), "directory", "directories"),
            count_label(file_count, "file", "files")
        );

        if source_file_counts.len() > 1 {
            for (source_directory, source_file_count) in source_file_counts {
                println!(
                    "  {} ({})",
                    source_directory.display(),
                    count_label(source_file_count, "file", "files")
                );
            }
        }

        println!("  {} Merge to: {target_display}", "→".green());

        if !self.config.dryrun {
            let confirmed = self.config.auto
                || cli_tools_common::get_user_confirmation(
                    report::directory_merge_confirmation_prompt(source_directories.len()),
                    false,
                )?;

            if confirmed {
                let merger = DirectoryMerger::new(self.config.overwrite, false);
                let mut reservations = TargetReservations::new();
                let mut reporter = self.reporter();
                for source_directory in source_directories {
                    if let Err(error) = merger.merge_with_reservations(
                        source_directory,
                        &target_directory.path,
                        &mut reservations,
                        &mut reporter,
                    ) {
                        print_error!("Failed to merge {}: {error:#}", source_directory.display());
                    }
                }
            } else {
                report::print_skipped();
            }
        }
        println!();

        Ok(())
    }

    /// Create directories for files with matching prefixes and move files into them.
    ///
    /// Only considers files directly in the input roots, not recursively.
    /// Files can match multiple groups, and they remain available until actually moved.
    fn create_dirs_and_move_files(&self) -> anyhow::Result<()> {
        let files_with_names = self.scanner().collect_files_with_names()?;
        if self.config.debug {
            report::print_grouping_names_debug(&files_with_names);
        }

        let mut moved_files: HashSet<PathBuf> = HashSet::new();
        // Group names ignored during this session with the "s" answer, shared across custom mappings and prefix groups.
        let mut runtime_ignored: HashSet<String> = HashSet::new();
        self.process_custom_mappings_create_mode(&files_with_names, &mut moved_files, &mut runtime_ignored)?;

        if self.config.debug {
            report::print_prefix_ignores_debug(self.config.name_filter.prefix_ignores(), &files_with_names);
        }

        let grouper = self.grouper();
        let progress = report::GroupingProgressBar::new(HIDE_PROGRESS);
        let groups = grouper.collect_all_prefix_groups_with_progress(&files_with_names, &progress);
        let groups_to_process = grouper.groups_in_processing_order(groups);
        if groups_to_process.is_empty() {
            if self.config.verbose {
                println!(
                    "No file groups with {} or more matching prefixes found",
                    self.config.grouping.min_group_size()
                );
            }
            return Ok(());
        }

        let total_groups = groups_to_process.len();
        print_bold!(
            "Found {} with matching prefixes:\n",
            count_label(total_groups, "group", "groups")
        );

        let parent_dir_normalized = self.roots.first_output_normalized_name();
        let mut group_number: usize = 0;

        for (prefix, group) in groups_to_process {
            let available_files: Vec<_> = group.files.into_iter().filter(|f| !moved_files.contains(f)).collect();
            if available_files.len() < self.config.grouping.min_group_size() {
                continue;
            }

            let dir_name = grouper.directory_name_for_prefix(&prefix);
            if grouper.should_skip_group(&dir_name, parent_dir_normalized.as_deref(), &runtime_ignored) {
                continue;
            }

            group_number += 1;
            self.process_single_group(
                &dir_name,
                &available_files,
                &mut moved_files,
                &mut runtime_ignored,
                group_number,
                total_groups,
            )?;
        }

        Ok(())
    }

    /// Print group information, prompt the user, and move files for a single prefix group.
    fn process_single_group(
        &self,
        dir_name: &str,
        available_files: &[PathBuf],
        moved_files: &mut HashSet<PathBuf>,
        runtime_ignored: &mut HashSet<String>,
        group_number: usize,
        total_groups: usize,
    ) -> anyhow::Result<()> {
        let database_suggestion = self.get_database_suggestion_for_prefix(dir_name);

        let progress = format!("[{group_number}/{total_groups}]").dimmed();
        let normalized_dir_name = utils::normalize_name(dir_name);
        let highlighted_names: Vec<String> = available_files
            .iter()
            .map(|file_path| prompt::format_filename_with_normalized_highlight(file_path, &normalized_dir_name))
            .collect();
        report::print_file_group(&format!("{progress} {}", dir_name.cyan().bold()), &highlighted_names);

        let Some(prompt_result) = self.offer_new_directory(dir_name, database_suggestion.as_deref())? else {
            return Ok(());
        };
        match prompt_result {
            PromptResult::Confirmed(target_path) => self.move_group(&target_path, available_files, moved_files),
            PromptResult::SaveToIgnored => Self::save_ignored_group_name(dir_name, runtime_ignored),
            PromptResult::Skipped => {}
        }
        println!();

        Ok(())
    }

    /// Save a group name to `ignored_group_names` in the config file,
    /// and skip later groups with the same name for the rest of the run.
    fn save_ignored_group_name(dir_name: &str, runtime_ignored: &mut HashSet<String>) {
        report::print_ignored_group_name_saved(dir_name, &add_ignored_group_name_to_config(dir_name));
        runtime_ignored.insert(utils::normalize_name(dir_name));
    }

    /// Process custom mappings in create mode, offering to create directories for matching files.
    /// Updates `moved_files` with files that were successfully moved and records saved ignores in `runtime_ignored`.
    fn process_custom_mappings_create_mode(
        &self,
        files_with_names: &[FileInfo],
        moved_files: &mut HashSet<PathBuf>,
        runtime_ignored: &mut HashSet<String>,
    ) -> anyhow::Result<()> {
        if self.config.custom_mappings.is_empty() {
            return Ok(());
        }

        let mapping_groups = matcher::group_files_by_custom_mapping(files_with_names, &self.config.custom_mappings);
        if mapping_groups.is_empty() {
            return Ok(());
        }

        print_bold!(
            "Found {}:\n",
            count_label(mapping_groups.len(), "custom mapping group", "custom mapping groups")
        );

        let grouper = self.grouper();
        let parent_dir_normalized = self.roots.first_output_normalized_name();

        for (dir_name, files) in mapping_groups {
            if files.is_empty()
                || grouper.should_skip_group(&dir_name, parent_dir_normalized.as_deref(), runtime_ignored)
            {
                continue;
            }

            let highlighted_names: Vec<String> = files
                .iter()
                .map(|file| format_text_with_highlight(&path_to_filename_string(&file.path), Some(file.range)))
                .collect();
            report::print_file_group(
                &format!("{} (custom mapping)", dir_name.cyan().bold()),
                &highlighted_names,
            );

            if let Some(prompt_result) = self.offer_new_directory(&dir_name, None)? {
                match prompt_result {
                    PromptResult::Confirmed(target_path) => {
                        let file_paths: Vec<_> = files.iter().map(|file| file.path.clone()).collect();
                        self.move_group(&target_path, &file_paths, moved_files);
                    }
                    PromptResult::SaveToIgnored => Self::save_ignored_group_name(&dir_name, runtime_ignored),
                    PromptResult::Skipped => {}
                }
                println!();
            }
        }

        Ok(())
    }

    /// Show whether a group's directory exists and ask where to move the group.
    ///
    /// Returns `None` in dry-run mode, where nothing is moved,
    /// and confirms the suggested directory without asking in auto mode.
    fn offer_new_directory(
        &self,
        dir_name: &str,
        database_suggestion: Option<&str>,
    ) -> anyhow::Result<Option<PromptResult>> {
        let dir_path = self.roots.first_output().join(dir_name);
        report::print_target_directory_status(dir_path.exists(), dir_name);

        if self.config.dryrun {
            println!();
            return Ok(None);
        }

        if self.config.auto {
            return Ok(Some(PromptResult::Confirmed(dir_path)));
        }
        prompt::prompt_for_directory_name(self.roots.first_output(), dir_name, database_suggestion).map(Some)
    }

    /// Move a confirmed group of files, recording the moved files and the directory name.
    fn move_group(&self, target_path: &Path, files: &[PathBuf], moved_files: &mut HashSet<PathBuf>) {
        match self.move_files_to_target_dir(target_path, files) {
            Ok(report) => {
                moved_files.extend(report.moved_files);
                self.add_to_database(&path_to_filename_string(target_path));
            }
            Err(e) => {
                print_error!("Failed to process {}: {e}", target_path.display());
            }
        }
    }

    /// Move files to the target directory, creating it if needed, while skipping locked files
    /// and updating shared `ScanCache` paths for moved files.
    fn move_files_to_target_dir(&self, dir_path: &Path, files: &[PathBuf]) -> anyhow::Result<mover::MoveReport> {
        let lock_store = if self.config.dryrun || cfg!(test) {
            None
        } else {
            ActiveLockStore::open_default("dirmove").ok()
        };

        let mut unlocked_files = Vec::with_capacity(files.len());
        let mut locked_skips = Vec::new();
        for file in files {
            if let Some(ref store) = lock_store
                && let Ok(Some(holder)) = store.is_locked_by_other(file)
            {
                print_yellow!(
                    "Skipping locked file (held by {} pid {}): {}",
                    holder.tool,
                    holder.pid,
                    file.display()
                );
                locked_skips.push(file.clone());
                continue;
            }
            unlocked_files.push(file.clone());
        }

        let mut report =
            mover::move_files_to_target_dir(dir_path, &unlocked_files, self.config.overwrite, &mut self.reporter())?;
        report.skipped_files.extend(locked_skips);

        if !self.config.dryrun
            && !cfg!(test)
            && !report.moved_files.is_empty()
            && let Ok(mut cache) = ScanCache::open()
        {
            for old_path in &report.moved_files {
                let file_name = path_to_filename_string(old_path);
                let new_path = dir_path.join(&file_name);
                let _ = cache.move_cached_path(old_path, &new_path);
            }
        }

        Ok(report)
    }
}

/// Return whether a file was not moved by an earlier offer and still exists, so another offer can include it.
fn file_is_available(file_path: &Path, consumed_files: &HashSet<PathBuf>) -> bool {
    !consumed_files.contains(file_path) && file_path.exists()
}

#[cfg(test)]
mod test_helpers {
    use super::*;
    pub use cli_tools_dirmove::{CustomMapping, GroupingOptions, ScanOptions};

    /// Create a `DirMove` for testing with no database.
    pub fn make_dirmove(root: PathBuf, config: Config) -> DirMove {
        make_dirmove_with_output(root.clone(), root, config)
    }

    /// Create a test `DirMove` with one input root and one output root.
    pub fn make_dirmove_with_output(root: PathBuf, output_root: PathBuf, config: Config) -> DirMove {
        make_dirmove_with_roots(vec![root], vec![output_root], config)
    }

    /// Create a test `DirMove` with independent input and output root sets.
    ///
    /// The first output root is used for implicit create-mode destinations,
    /// matching production initialization behavior.
    pub fn make_dirmove_with_roots(input_roots: Vec<PathBuf>, output_roots: Vec<PathBuf>, config: Config) -> DirMove {
        DirMove {
            roots: Roots::new(input_roots, output_roots),
            config,
            database: None,
        }
    }

    /// Assert that a path exists.
    pub fn assert_exists(path: &Path) {
        assert!(path.exists(), "Expected path to exist: {}", path.display());
    }

    /// Assert that a path does not exist.
    pub fn assert_not_exists(path: &Path) {
        assert!(!path.exists(), "Expected path to NOT exist: {}", path.display());
    }
}

#[cfg(test)]
mod test_custom_mappings {
    use super::test_helpers::*;
    use super::*;
    use tempfile::TempDir;

    /// Helper to create a `DirMove` with custom mappings.
    fn make_dirmove_with_mappings(root: PathBuf, mappings: Vec<(&str, &str)>) -> DirMove {
        let custom_mappings = mappings
            .into_iter()
            .map(|(pattern, dir)| CustomMapping::new(pattern, dir).unwrap())
            .collect();
        let config = Config {
            auto: true,
            custom_mappings,
            grouping: GroupingOptions::new(1, 1, Vec::<String>::new(), Vec::<String>::new()),
            ..Default::default()
        };
        make_dirmove(root, config)
    }

    #[test]
    fn process_custom_mappings_returns_unmatched_files() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().to_path_buf();

        // Create files
        std::fs::write(root.join("something.video.mp4"), "")?;
        std::fs::write(root.join("other.file.mp4"), "")?;

        // Create target directory
        std::fs::create_dir(root.join("Custom Dir"))?;

        let dirmove = make_dirmove_with_mappings(root.clone(), vec![("something", "Custom Dir")]);
        let files = vec![root.join("something.video.mp4"), root.join("other.file.mp4")];
        let directories = dirmove.collect_directories()?;

        let mut moved_files = HashSet::new();
        let remaining = dirmove.process_custom_mapping_matches(&files, &directories, &mut moved_files)?;

        // "other.file.mp4" should remain since it doesn't match the pattern
        assert_eq!(remaining.len(), 1);
        assert!(remaining[0].ends_with("other.file.mp4"));
        Ok(())
    }

    #[test]
    fn custom_mapping_success_prevents_later_normal_match() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().to_path_buf();
        let file_name = "Show.Name.something.mp4";
        std::fs::write(root.join(file_name), "video")?;
        std::fs::create_dir(root.join("Custom Dir"))?;
        std::fs::create_dir(root.join("Show Name"))?;

        let config = Config {
            auto: true,
            files_only: true,
            custom_mappings: vec![CustomMapping::new("something", "Custom Dir").unwrap()],
            ..Default::default()
        };
        let dirmove = make_dirmove(root.clone(), config);

        dirmove.move_files_to_dir()?;

        assert_not_exists(&root.join(file_name));
        assert_exists(&root.join("Custom Dir").join(file_name));
        assert_not_exists(&root.join("Show Name").join(file_name));
        Ok(())
    }

    #[test]
    fn dryrun_custom_mapping_consumes_file_for_later_suggestions() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().to_path_buf();
        let file_name = "Show.Name.something.mp4";
        let file_path = root.join(file_name);
        std::fs::write(&file_path, "video")?;
        std::fs::create_dir(root.join("Custom Dir"))?;
        std::fs::create_dir(root.join("Show Name"))?;

        let config = Config {
            dryrun: true,
            custom_mappings: vec![CustomMapping::new("something", "Custom Dir").unwrap()],
            ..Default::default()
        };
        let dirmove = make_dirmove(root.clone(), config);
        let directories = dirmove.collect_directories()?;
        let mut consumed_files = HashSet::new();

        let remaining = dirmove.process_custom_mapping_matches(
            std::slice::from_ref(&file_path),
            &directories,
            &mut consumed_files,
        )?;

        assert_eq!(remaining, [] as [PathBuf; 0]);
        assert!(consumed_files.contains(&file_path));
        assert_exists(&file_path);
        assert_not_exists(&root.join("Custom Dir").join(file_name));
        Ok(())
    }

    #[test]
    fn custom_mapping_skipped_existing_file_can_match_normal_directory() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().to_path_buf();
        let file_name = "Show.Name.something.mp4";
        std::fs::write(root.join(file_name), "source")?;
        std::fs::create_dir(root.join("Custom Dir"))?;
        std::fs::write(root.join("Custom Dir").join(file_name), "existing")?;
        std::fs::create_dir(root.join("Show Name"))?;

        let config = Config {
            auto: true,
            files_only: true,
            custom_mappings: vec![CustomMapping::new("something", "Custom Dir").unwrap()],
            ..Default::default()
        };
        let dirmove = make_dirmove(root.clone(), config);

        dirmove.move_files_to_dir()?;

        assert_not_exists(&root.join(file_name));
        assert_eq!(
            std::fs::read_to_string(root.join("Custom Dir").join(file_name))?,
            "existing"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("Show Name").join(file_name))?,
            "source"
        );
        Ok(())
    }

    #[test]
    fn process_custom_mappings_empty_when_no_mappings() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("file1.mp4"), "")?;
        std::fs::write(root.join("file2.mp4"), "")?;

        let config = Config {
            custom_mappings: vec![],
            dryrun: true,
            ..Default::default()
        };
        let dirmove = make_dirmove(root.clone(), config);
        let files = vec![root.join("file1.mp4"), root.join("file2.mp4")];
        let directories = vec![];

        let mut moved_files = HashSet::new();
        let remaining = dirmove.process_custom_mapping_matches(&files, &directories, &mut moved_files)?;

        // All files should remain since there are no mappings
        assert_eq!(remaining.len(), 2);
        Ok(())
    }

    #[test]
    fn process_custom_mappings_case_insensitive_pattern() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().to_path_buf();

        // File with different case than pattern
        std::fs::write(root.join("SOMETHING.Video.mp4"), "")?;
        std::fs::create_dir(root.join("Target"))?;

        let dirmove = make_dirmove_with_mappings(root.clone(), vec![("something", "Target")]);
        let files = vec![root.join("SOMETHING.Video.mp4")];
        let directories = dirmove.collect_directories()?;

        let mut moved_files = HashSet::new();
        let remaining = dirmove.process_custom_mapping_matches(&files, &directories, &mut moved_files)?;

        // File should be matched despite case difference
        assert_eq!(remaining, [] as [PathBuf; 0]);
        Ok(())
    }

    #[test]
    fn process_custom_mappings_ignores_dots_in_pattern() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().to_path_buf();

        // File with dots
        std::fs::write(root.join("Some.Thing.Video.mp4"), "")?;
        std::fs::create_dir(root.join("Target"))?;

        // Pattern without dots should match file with dots
        let dirmove = make_dirmove_with_mappings(root.clone(), vec![("something", "Target")]);
        let files = vec![root.join("Some.Thing.Video.mp4")];
        let directories = dirmove.collect_directories()?;

        let mut moved_files = HashSet::new();
        let remaining = dirmove.process_custom_mapping_matches(&files, &directories, &mut moved_files)?;

        assert_eq!(remaining, [] as [PathBuf; 0]);
        Ok(())
    }

    #[test]
    fn process_custom_mappings_no_match_without_directory() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("something.video.mp4"), "")?;
        // No target directory created

        let dirmove = make_dirmove_with_mappings(root.clone(), vec![("something", "Missing Dir")]);
        let files = vec![root.join("something.video.mp4")];
        let directories = vec![];

        let mut moved_files = HashSet::new();
        let remaining = dirmove.process_custom_mapping_matches(&files, &directories, &mut moved_files)?;

        // File should remain since target directory doesn't exist
        assert_eq!(remaining.len(), 1);
        Ok(())
    }

    #[test]
    fn multiple_mappings_first_match_wins() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("abc.xyz.video.mp4"), "")?;
        std::fs::create_dir(root.join("Dir A"))?;
        std::fs::create_dir(root.join("Dir B"))?;

        // Both patterns match, but first should win
        let dirmove = make_dirmove_with_mappings(root.clone(), vec![("abc", "Dir A"), ("xyz", "Dir B")]);
        let files = vec![root.join("abc.xyz.video.mp4")];
        let directories = dirmove.collect_directories()?;

        let mut moved_files = HashSet::new();
        let remaining = dirmove.process_custom_mapping_matches(&files, &directories, &mut moved_files)?;

        // File should be matched to first mapping
        assert_eq!(remaining, [] as [PathBuf; 0]);
        Ok(())
    }

    #[test]
    fn create_mode_custom_mapping_skips_current_dir_name() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Example");
        std::fs::create_dir(&root)?;

        std::fs::write(root.join("Example.one.mp4"), "")?;
        std::fs::write(root.join("Example.two.mp4"), "")?;
        std::fs::write(root.join("Example.three.mp4"), "")?;

        let config = Config {
            auto: true,
            create: true,
            custom_mappings: vec![CustomMapping::new("example", "Example").unwrap()],
            grouping: GroupingOptions::new(10, 0, Vec::<String>::new(), Vec::<String>::new()),
            ..Default::default()
        };
        let dirmove = make_dirmove(root.clone(), config);

        dirmove.create_dirs_and_move_files()?;

        assert_not_exists(&root.join("Example"));
        assert_exists(&root.join("Example.one.mp4"));
        assert_exists(&root.join("Example.two.mp4"));
        assert_exists(&root.join("Example.three.mp4"));
        Ok(())
    }
}

#[cfg(test)]
mod test_output_root {
    use super::test_helpers::*;
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn directory_matching_reads_dirs_from_output_root() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let input_root = tmp.path().join("input");
        let output_root = tmp.path().join("output");
        std::fs::create_dir(&input_root)?;
        std::fs::create_dir(&output_root)?;
        std::fs::create_dir(output_root.join("Example Show"))?;
        std::fs::write(input_root.join("Example.Show.S01E01.mp4"), "video")?;

        let config = Config {
            auto: true,
            ..Default::default()
        };
        let dirmove = make_dirmove_with_output(input_root.clone(), output_root.clone(), config);

        dirmove.move_files_to_dir()?;

        assert_not_exists(&input_root.join("Example.Show.S01E01.mp4"));
        assert_exists(&output_root.join("Example Show").join("Example.Show.S01E01.mp4"));
        Ok(())
    }

    #[test]
    fn directory_matching_ignores_nested_output_dirs_without_recurse() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let input_root = tmp.path().join("input");
        let output_root = tmp.path().join("output");
        let nested_target = output_root.join("something").join("example");
        std::fs::create_dir(&input_root)?;
        std::fs::create_dir_all(&nested_target)?;
        std::fs::write(input_root.join("Example.S01E01.mp4"), "video")?;

        let config = Config {
            auto: true,
            ..Default::default()
        };
        let dirmove = make_dirmove_with_output(input_root.clone(), output_root, config);

        dirmove.move_files_to_dir()?;

        assert_exists(&input_root.join("Example.S01E01.mp4"));
        assert_not_exists(&nested_target.join("Example.S01E01.mp4"));
        Ok(())
    }

    #[test]
    fn directory_matching_recurses_output_root_when_enabled() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let input_root = tmp.path().join("input");
        let output_root = tmp.path().join("output");
        let nested_target = output_root.join("something").join("example");
        std::fs::create_dir(&input_root)?;
        std::fs::create_dir_all(&nested_target)?;
        std::fs::write(input_root.join("Example.S01E01.mp4"), "video")?;

        let config = Config {
            auto: true,
            scan: ScanOptions::new(Vec::<String>::new(), Vec::<String>::new(), true),
            ..Default::default()
        };
        let dirmove = make_dirmove_with_output(input_root.clone(), output_root, config);

        dirmove.move_files_to_dir()?;

        assert_not_exists(&input_root.join("Example.S01E01.mp4"));
        assert_exists(&nested_target.join("Example.S01E01.mp4"));
        Ok(())
    }

    #[test]
    fn directory_matching_prefers_deeper_recursive_match() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let input_root = tmp.path().join("input");
        let output_root = tmp.path().join("output");
        let root_target = output_root.join("example");
        let nested_target = output_root.join("something").join("example");
        std::fs::create_dir(&input_root)?;
        std::fs::create_dir_all(&root_target)?;
        std::fs::create_dir_all(&nested_target)?;
        std::fs::write(input_root.join("Example.S01E01.mp4"), "video")?;

        let config = Config {
            auto: true,
            scan: ScanOptions::new(Vec::<String>::new(), Vec::<String>::new(), true),
            ..Default::default()
        };
        let dirmove = make_dirmove_with_output(input_root.clone(), output_root, config);

        dirmove.move_files_to_dir()?;

        assert_not_exists(&input_root.join("Example.S01E01.mp4"));
        assert_not_exists(&root_target.join("Example.S01E01.mp4"));
        assert_exists(&nested_target.join("Example.S01E01.mp4"));
        Ok(())
    }

    #[test]
    fn directory_matching_prefers_longer_match_at_same_depth() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let input_root = tmp.path().join("input");
        let output_root = tmp.path().join("output");
        let shorter_target = output_root.join("something").join("example");
        let longer_target = output_root.join("something").join("example show");
        std::fs::create_dir(&input_root)?;
        std::fs::create_dir_all(&shorter_target)?;
        std::fs::create_dir_all(&longer_target)?;
        std::fs::write(input_root.join("Example.Show.S01E01.mp4"), "video")?;

        let config = Config {
            auto: true,
            scan: ScanOptions::new(Vec::<String>::new(), Vec::<String>::new(), true),
            ..Default::default()
        };
        let dirmove = make_dirmove_with_output(input_root.clone(), output_root, config);

        dirmove.move_files_to_dir()?;

        assert_not_exists(&input_root.join("Example.Show.S01E01.mp4"));
        assert_not_exists(&shorter_target.join("Example.Show.S01E01.mp4"));
        assert_exists(&longer_target.join("Example.Show.S01E01.mp4"));
        Ok(())
    }

    #[test]
    fn directory_matching_routes_one_input_to_multiple_output_roots() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let input_root = tmp.path().join("input");
        let output_root_a = tmp.path().join("output_a");
        let output_root_b = tmp.path().join("output_b");
        let some_name_target = output_root_a.join("somename");
        let another_name_target = output_root_b.join("anothername");
        std::fs::create_dir(&input_root)?;
        std::fs::create_dir_all(&some_name_target)?;
        std::fs::create_dir_all(&another_name_target)?;
        std::fs::write(input_root.join("somename.mp4"), "one")?;
        std::fs::write(input_root.join("anothername.mp4"), "two")?;

        let config = Config {
            auto: true,
            ..Default::default()
        };
        let dirmove = make_dirmove_with_roots(vec![input_root.clone()], vec![output_root_a, output_root_b], config);

        dirmove.move_files_to_dir()?;

        assert_not_exists(&input_root.join("somename.mp4"));
        assert_not_exists(&input_root.join("anothername.mp4"));
        assert_exists(&some_name_target.join("somename.mp4"));
        assert_exists(&another_name_target.join("anothername.mp4"));
        Ok(())
    }

    #[test]
    fn directory_matching_routes_multiple_inputs_to_one_output_root() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let input_root_a = tmp.path().join("input_a");
        let input_root_b = tmp.path().join("input_b");
        let output_root = tmp.path().join("output");
        let some_name_target = output_root.join("somename");
        let another_name_target = output_root.join("anothername");
        std::fs::create_dir(&input_root_a)?;
        std::fs::create_dir(&input_root_b)?;
        std::fs::create_dir_all(&some_name_target)?;
        std::fs::create_dir_all(&another_name_target)?;
        std::fs::write(input_root_a.join("somename.mp4"), "one")?;
        std::fs::write(input_root_b.join("anothername.mp4"), "two")?;

        let config = Config {
            auto: true,
            ..Default::default()
        };
        let dirmove = make_dirmove_with_roots(
            vec![input_root_a.clone(), input_root_b.clone()],
            vec![output_root],
            config,
        );

        dirmove.move_files_to_dir()?;

        assert_not_exists(&input_root_a.join("somename.mp4"));
        assert_not_exists(&input_root_b.join("anothername.mp4"));
        assert_exists(&some_name_target.join("somename.mp4"));
        assert_exists(&another_name_target.join("anothername.mp4"));
        Ok(())
    }

    #[test]
    fn directory_matching_merges_same_named_input_directory_first() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let input_root = tmp.path().join("input");
        let output_root = tmp.path().join("output");
        let input_directory = input_root.join("Example");
        let output_directory = output_root.join("Example");
        std::fs::create_dir(&input_root)?;
        std::fs::create_dir_all(&input_directory)?;
        std::fs::create_dir_all(&output_directory)?;
        std::fs::write(input_directory.join("Example.S01E01.mp4"), "video")?;

        let config = Config {
            auto: true,
            ..Default::default()
        };
        let dirmove = make_dirmove_with_output(input_root, output_root, config);

        dirmove.move_files_to_dir()?;

        assert_not_exists(&input_directory);
        assert_exists(&output_directory.join("Example.S01E01.mp4"));
        Ok(())
    }

    #[test]
    fn move_to_display_uses_full_path_when_any_file_has_a_different_root() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let input_root_a = tmp.path().join("input_a");
        let input_root_b = tmp.path().join("input_b");
        let output_directory = input_root_a.join("Example");
        let input_file_a = input_root_a.join("Example.S01E01.mp4");
        let input_file_b = input_root_b.join("Example.S01E02.mp4");
        std::fs::create_dir_all(&output_directory)?;
        std::fs::create_dir_all(&input_root_b)?;
        std::fs::write(&input_file_a, "one")?;
        std::fs::write(&input_file_b, "two")?;

        let dirmove = make_dirmove_with_roots(
            vec![input_root_a, input_root_b],
            vec![output_directory.clone()],
            Config::default(),
        );

        assert_eq!(
            dirmove
                .roots
                .move_to_display_path(&output_directory, &[input_file_a, input_file_b]),
            output_directory.display().to_string()
        );
        Ok(())
    }

    #[test]
    fn files_only_skips_same_named_input_directory_merge() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let input_root = tmp.path().join("input");
        let output_root = tmp.path().join("output");
        let input_directory = input_root.join("Example");
        let output_directory = output_root.join("Example");
        std::fs::create_dir(&input_root)?;
        std::fs::create_dir_all(&input_directory)?;
        std::fs::create_dir_all(&output_directory)?;
        std::fs::write(input_directory.join("Example.S01E01.mp4"), "video")?;

        let config = Config {
            auto: true,
            files_only: true,
            ..Default::default()
        };
        let dirmove = make_dirmove_with_output(input_root, output_root, config);

        dirmove.move_files_to_dir()?;

        assert_exists(&input_directory.join("Example.S01E01.mp4"));
        assert_not_exists(&output_directory.join("Example.S01E01.mp4"));
        Ok(())
    }

    #[test]
    fn custom_mapping_prefers_deeper_match_across_output_roots() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let input_root = tmp.path().join("input");
        let output_root_a = tmp.path().join("output_a");
        let output_root_b = tmp.path().join("output_b");
        let shallow_target = output_root_a.join("Target");
        let deep_target = output_root_b.join("nested").join("Target");
        std::fs::create_dir(&input_root)?;
        std::fs::create_dir_all(&shallow_target)?;
        std::fs::create_dir_all(&deep_target)?;
        std::fs::write(input_root.join("mapped.file.mp4"), "video")?;

        let config = Config {
            auto: true,
            custom_mappings: vec![CustomMapping::new("mapped", "Target").unwrap()],
            scan: ScanOptions::new(Vec::<String>::new(), Vec::<String>::new(), true),
            ..Default::default()
        };
        let dirmove = make_dirmove_with_roots(vec![input_root.clone()], vec![output_root_a, output_root_b], config);

        dirmove.move_files_to_dir()?;

        assert_not_exists(&input_root.join("mapped.file.mp4"));
        assert_not_exists(&shallow_target.join("mapped.file.mp4"));
        assert_exists(&deep_target.join("mapped.file.mp4"));
        Ok(())
    }

    #[test]
    fn create_mode_creates_directories_in_output_root() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let input_root = tmp.path().join("input");
        let output_root = tmp.path().join("output");
        std::fs::create_dir(&input_root)?;
        std::fs::create_dir(&output_root)?;
        std::fs::write(input_root.join("Show.Name.S01E01.mp4"), "one")?;
        std::fs::write(input_root.join("Show.Name.S01E02.mp4"), "two")?;
        std::fs::write(input_root.join("Show.Name.S01E03.mp4"), "three")?;

        let config = Config {
            auto: true,
            create: true,
            grouping: GroupingOptions::new(3, 4, Vec::<String>::new(), Vec::<String>::new()),
            ..Default::default()
        };
        let dirmove = make_dirmove_with_output(input_root.clone(), output_root.clone(), config);

        dirmove.create_dirs_and_move_files()?;

        assert_not_exists(&input_root.join("Show.Name.S01E01.mp4"));
        assert_exists(&output_root.join("Show Name").join("Show.Name.S01E01.mp4"));
        assert_exists(&output_root.join("Show Name").join("Show.Name.S01E02.mp4"));
        assert_exists(&output_root.join("Show Name").join("Show.Name.S01E03.mp4"));
        Ok(())
    }

    #[test]
    fn create_mode_uses_first_output_root_when_multiple_outputs_given() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let input_root = tmp.path().join("input");
        let output_root_a = tmp.path().join("output_a");
        let output_root_b = tmp.path().join("output_b");
        std::fs::create_dir(&input_root)?;
        std::fs::create_dir(&output_root_a)?;
        std::fs::create_dir(&output_root_b)?;
        std::fs::write(input_root.join("Show.Name.S01E01.mp4"), "one")?;
        std::fs::write(input_root.join("Show.Name.S01E02.mp4"), "two")?;
        std::fs::write(input_root.join("Show.Name.S01E03.mp4"), "three")?;

        let config = Config {
            auto: true,
            create: true,
            grouping: GroupingOptions::new(3, 4, Vec::<String>::new(), Vec::<String>::new()),
            ..Default::default()
        };
        let dirmove = make_dirmove_with_roots(
            vec![input_root.clone()],
            vec![output_root_a.clone(), output_root_b.clone()],
            config,
        );

        dirmove.create_dirs_and_move_files()?;

        assert_not_exists(&input_root.join("Show.Name.S01E01.mp4"));
        assert_exists(&output_root_a.join("Show Name").join("Show.Name.S01E01.mp4"));
        assert_not_exists(&output_root_b.join("Show Name"));
        Ok(())
    }

    #[test]
    fn custom_mapping_create_mode_uses_output_root() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let input_root = tmp.path().join("input");
        let output_root = tmp.path().join("output");
        std::fs::create_dir(&input_root)?;
        std::fs::create_dir(&output_root)?;
        std::fs::write(input_root.join("Example.one.mp4"), "one")?;
        std::fs::write(input_root.join("Example.two.mp4"), "two")?;
        std::fs::write(input_root.join("Example.three.mp4"), "three")?;

        let config = Config {
            auto: true,
            create: true,
            custom_mappings: vec![CustomMapping::new("example", "Example").unwrap()],
            grouping: GroupingOptions::new(10, 0, Vec::<String>::new(), Vec::<String>::new()),
            ..Default::default()
        };
        let dirmove = make_dirmove_with_output(input_root.clone(), output_root.clone(), config);

        dirmove.create_dirs_and_move_files()?;

        assert_not_exists(&input_root.join("Example.one.mp4"));
        assert_exists(&output_root.join("Example").join("Example.one.mp4"));
        assert_exists(&output_root.join("Example").join("Example.two.mp4"));
        assert_exists(&output_root.join("Example").join("Example.three.mp4"));
        Ok(())
    }
}

#[cfg(test)]
mod test_directory_matching_moves {
    use super::test_helpers::*;
    use super::*;

    #[test]
    fn moved_file_is_not_offered_to_later_directory_match() -> anyhow::Result<()> {
        let tmp = tempfile::TempDir::new()?;
        let root = tmp.path().to_path_buf();
        std::fs::create_dir(root.join("Show"))?;
        std::fs::create_dir(root.join("Show Name"))?;
        let file_name = "Show.Name.Episode.mp4";
        std::fs::write(root.join(file_name), "video")?;

        let config = Config {
            auto: true,
            files_only: true,
            ..Default::default()
        };
        let dirmove = make_dirmove(root.clone(), config);

        dirmove.move_files_to_dir()?;

        assert!(!root.join(file_name).exists());
        assert!(root.join("Show Name").join(file_name).exists());
        assert!(!root.join("Show").join(file_name).exists());
        Ok(())
    }

    #[test]
    fn later_directory_match_can_move_file_skipped_by_preferred_match() -> anyhow::Result<()> {
        let tmp = tempfile::TempDir::new()?;
        let root = tmp.path().to_path_buf();
        let file_name = "Show.Name.Episode.mp4";
        std::fs::write(root.join(file_name), "source")?;
        std::fs::create_dir(root.join("Show"))?;
        std::fs::create_dir(root.join("Show Name"))?;
        std::fs::write(root.join("Show Name").join(file_name), "existing")?;

        let config = Config {
            auto: true,
            files_only: true,
            ..Default::default()
        };
        let dirmove = make_dirmove(root.clone(), config);

        dirmove.move_files_to_dir()?;

        assert_not_exists(&root.join(file_name));
        assert_eq!(
            std::fs::read_to_string(root.join("Show Name").join(file_name))?,
            "existing"
        );
        assert_eq!(std::fs::read_to_string(root.join("Show").join(file_name))?, "source");
        Ok(())
    }
}

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};

use colored::Colorize;
use itertools::Itertools;
use walkdir::WalkDir;

use cli_tools::{
    get_relative_path_or_filename, path_to_filename_string, path_to_string_relative, print_bold, print_error,
    print_magenta, print_warning,
};

use crate::config::Config;
use crate::types::{DirectoryInfo, FileInfo, MoveInfo, UnpackInfo};
use crate::{DirMoveArgs, utils};

#[derive(Debug)]
pub struct DirMove {
    root: PathBuf,
    config: Config,
}

impl DirMove {
    pub const fn new(root: PathBuf, config: Config) -> Self {
        Self { root, config }
    }

    pub fn try_from_args(args: DirMoveArgs) -> anyhow::Result<Self> {
        let root = cli_tools::resolve_input_path(args.path.as_deref())?;
        let config = Config::from_args(args);
        if config.debug {
            eprintln!("Config: {config:#?}");
            eprintln!("Root: {}", root.display());
        }
        Ok(Self::new(root, config))
    }

    pub fn run(&self) -> anyhow::Result<()> {
        // Delete unwanted directories and unpack configured directory names.
        // These are combined into a single directory walk for efficiency.
        self.unpack_directories()?;

        // Normal directory matching and moving
        self.move_files_to_dir()?;

        // Create dirs by common filename prefix and move files
        if self.config.create {
            self.create_dirs_and_move_files()?;
        }

        Ok(())
    }

    /// Delete unwanted directories and unpack directories with names matching config.
    ///
    /// For each matching directory `.../<match>/...`, move its entire contents to the parent directory,
    /// preserving the structure below `<match>`. For example:
    ///
    /// `Example/Videos/Name/file2.txt` -> `Example/Name/file2.txt`
    ///
    /// Prunes empty directories that were touched by this unpack operation or already empty directories that match.
    fn unpack_directories(&self) -> anyhow::Result<()> {
        let (unwanted, candidates) = self.collect_unwanted_and_unpack_candidates();

        // Delete unwanted directories first (deepest first for safe deletion).
        for dir in unwanted {
            let relative = path_to_string_relative(&dir);
            if self.config.dryrun {
                println!("{} {relative}", "Would delete unwanted:".yellow());
            } else {
                match std::fs::remove_dir_all(&dir) {
                    Ok(()) => {
                        println!("{} {relative}", "Deleted unwanted:".yellow());
                    }
                    Err(err) => {
                        print_error!("Failed to delete {relative}: {err}");
                    }
                }
            }
        }

        if candidates.is_empty() {
            return Ok(());
        }

        // Track directories we touched so we can prune empties safely.
        let mut touched_dirs: HashSet<PathBuf> = HashSet::new();

        for directory in candidates {
            if !directory.exists() {
                continue;
            }

            let Some(parent) = directory.parent().map(Path::to_path_buf) else {
                continue;
            };

            let unpack_info = self.collect_unpack_info(&directory, &parent);
            self.print_unpack_summary(&parent, &unpack_info);

            if self.config.dryrun {
                continue;
            }

            // Move directories that don't match unpack names directly.
            for MoveInfo { source, target } in &unpack_info.directory_moves {
                self.move_directory(source, target, &mut touched_dirs)?;
            }

            // Move individual files.
            for MoveInfo { source, target } in &unpack_info.file_moves {
                self.unpack_move_one_file(source, target, &mut touched_dirs)?;
            }

            touched_dirs.insert(directory.clone());
            touched_dirs.insert(parent);

            self.prune_empty_dirs_under(&directory, &mut touched_dirs)?;
        }

        Ok(())
    }

    /// Collect unwanted directories to delete and unpack candidates in a single walk.
    /// Returns (`unwanted_dirs`, `unpack_candidates`) both sorted deepest first.
    fn collect_unwanted_and_unpack_candidates(&self) -> (Vec<PathBuf>, Vec<PathBuf>) {
        let mut unwanted = Vec::new();
        let mut candidates = Vec::new();

        let walker = if self.config.recurse {
            WalkDir::new(&self.root)
        } else {
            WalkDir::new(&self.root).max_depth(1)
        };

        for entry in walker
            .into_iter()
            .filter_entry(|e| {
                // Allow unwanted directories through so we can delete them,
                // but skip other hidden/system directories
                e.file_name().to_str().is_some_and(utils::is_unwanted_directory) || !cli_tools::should_skip_entry(e)
            })
            .filter_map(Result::ok)
        {
            if !entry.file_type().is_dir() {
                continue;
            }
            if entry.path() == self.root.as_path() {
                continue;
            }

            let Some(name) = entry.path().file_name().and_then(|n| n.to_str()) else {
                continue;
            };

            if utils::is_unwanted_directory(name) {
                unwanted.push(entry.path().to_path_buf());
            } else if self.config.unpack_directory_names.contains(&name.to_lowercase()) {
                candidates.push(entry.path().to_path_buf());
            }
        }

        // Sort unwanted deepest first for safe deletion.
        unwanted.sort_by_key(|p| std::cmp::Reverse(p.components().count()));

        // Sort by depth (shallowest first) to find root unpack directories.
        candidates.sort_by_key(|p| p.components().count());

        // Filter to keep only "root" unpack directories - those without an ancestor
        // that is also a candidate. This ensures we process the full move chain once
        // from the topmost unpack directory rather than separately for each level.
        let mut root_candidates = Vec::new();
        for candidate in &candidates {
            let has_ancestor_candidate = root_candidates.iter().any(|root: &PathBuf| candidate.starts_with(root));
            if !has_ancestor_candidate {
                root_candidates.push(candidate.clone());
            }
        }

        // Sort deepest first for processing order.
        root_candidates.sort_by_key(|p| std::cmp::Reverse(p.components().count()));

        (unwanted, root_candidates)
    }

    /// Information about what needs to be moved during an unpack operation.
    fn collect_unpack_info(&self, directory: &Path, parent: &Path) -> UnpackInfo {
        let mut info = UnpackInfo::default();

        let Ok(entries) = std::fs::read_dir(directory) else {
            return info;
        };

        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };

            let target = parent.join(name);

            if path.is_file() {
                info.file_moves.push(MoveInfo::new(path, target));
            } else if path.is_dir() {
                // If subdirectory name matches an unpack directory name, recurse into it.
                // Otherwise, check if it contains nested unpack directories.
                if self.config.unpack_directory_names.contains(&name.to_lowercase()) {
                    // Recursively collect from nested matching directories.
                    let nested_info = self.collect_unpack_info(&path, parent);
                    info.file_moves.extend(nested_info.file_moves);
                    info.directory_moves.extend(nested_info.directory_moves);
                } else if self.contains_unpack_directory(&path) {
                    // Non-matching directory contains nested unpack dirs, recurse into it
                    // with this directory as the new parent.
                    let nested_info = self.collect_unpack_info(&path, &target);
                    info.file_moves.extend(nested_info.file_moves);
                    info.directory_moves.extend(nested_info.directory_moves);
                } else {
                    // No nested unpack dirs, move the entire directory directly (more efficient).
                    info.directory_moves.push(MoveInfo::new(path, target));
                }
            }
        }

        info
    }

    /// Check if a directory contains any subdirectories matching unpack names (recursively).
    fn contains_unpack_directory(&self, dir: &Path) -> bool {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return false;
        };

        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }

            if let Some(name) = path.file_name().and_then(|n| n.to_str())
                && self.config.unpack_directory_names.contains(&name.to_lowercase())
            {
                return true;
            }

            // Recursively check subdirectories.
            if self.contains_unpack_directory(&path) {
                return true;
            }
        }

        false
    }

    /// Print summary of what will be unpacked.
    fn print_unpack_summary(&self, target_dir: &Path, info: &UnpackInfo) {
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

        for MoveInfo { source, target } in &info.directory_moves {
            let src_display = get_relative_path_or_filename(source, target_dir);
            let dst_display = get_relative_path_or_filename(target, target_dir);
            println!("  {src_display} -> {dst_display}");
        }
        if self.config.verbose {
            for MoveInfo { source, target } in &info.file_moves {
                let src_display = get_relative_path_or_filename(source, target_dir);
                let dst_display = get_relative_path_or_filename(target, target_dir);
                println!("  {src_display} -> {dst_display}");
            }
        }
    }

    /// Move an entire directory to a new location.
    fn move_directory(&self, source: &Path, target: &Path, touched_dirs: &mut HashSet<PathBuf>) -> anyhow::Result<()> {
        if target.exists() && !self.config.overwrite {
            print_warning!("Skipping existing directory: {}", target.display());
            return Ok(());
        }

        if let Some(parent) = source.parent() {
            touched_dirs.insert(parent.to_path_buf());
        }

        // Try rename first (fast, same filesystem).
        // Fall back to recursive copy + remove for cross-device moves.
        if std::fs::rename(source, target).is_err() {
            utils::copy_dir_recursive(source, target)?;
            std::fs::remove_dir_all(source)?;
        }

        Ok(())
    }

    fn unpack_move_one_file(
        &self,
        source: &Path,
        target: &Path,
        touched_dirs: &mut HashSet<PathBuf>,
    ) -> anyhow::Result<()> {
        if target.exists() && !self.config.overwrite {
            print_warning!("Skipping existing file: {}", target.display());
            return Ok(());
        }

        if let Some(dst_parent) = target.parent() {
            if !dst_parent.exists() {
                std::fs::create_dir_all(dst_parent)?;
            }
            touched_dirs.insert(dst_parent.to_path_buf());
        }

        if let Some(src_parent) = source.parent() {
            touched_dirs.insert(src_parent.to_path_buf());
        }

        // Rename is preferred; if it fails (e.g. cross-device), fall back to copy+remove.
        if std::fs::rename(source, target).is_err() {
            std::fs::copy(source, target)?;
            std::fs::remove_file(source)?;
        }

        Ok(())
    }

    /// Prune empty directories under `root_dir`, but only when they are within the subtree and
    /// are considered "touched" by this tool. Also removes `root_dir` itself if it is empty
    /// (even if it was already empty and matched).
    #[allow(clippy::unnecessary_wraps)]
    fn prune_empty_dirs_under(&self, root_dir: &Path, touched_dirs: &mut HashSet<PathBuf>) -> anyhow::Result<()> {
        use walkdir::WalkDir;

        if !root_dir.exists() {
            return Ok(());
        }

        // Walk depth-first so children get removed before parents.
        let mut dirs: Vec<PathBuf> = WalkDir::new(root_dir)
            .follow_links(false)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_dir())
            .map(|e| e.path().to_path_buf())
            .collect();

        dirs.sort_by_key(|p| std::cmp::Reverse(p.components().count()));

        for d in dirs {
            if d != root_dir && !touched_dirs.contains(&d) {
                continue;
            }

            let is_empty = std::fs::read_dir(&d).is_ok_and(|mut it| it.next().is_none());
            if !is_empty {
                continue;
            }

            if self.config.dryrun {
                if self.config.verbose {
                    println!("  {} Would remove empty dir: {}", "→".yellow(), d.display());
                }
                continue;
            }

            if std::fs::remove_dir(&d).is_ok() {
                if self.config.verbose {
                    println!("  {} Removed empty dir: {}", "→".green(), d.display());
                }
                if let Some(p) = d.parent() {
                    touched_dirs.insert(p.to_path_buf());
                }
            }
        }

        Ok(())
    }

    fn move_files_to_dir(&self) -> anyhow::Result<()> {
        // TODO: implement recurse option for dirs
        let _ = self.config.recurse;

        let directories = self.collect_directories_in_root()?;
        if directories.is_empty() {
            return Ok(());
        }

        let files_in_root = self.collect_files_in_root()?;
        if files_in_root.is_empty() {
            return Ok(());
        }

        let matches = self.match_files_to_directories(&files_in_root, &directories);
        if matches.is_empty() {
            return Ok(());
        }

        // Sort by directory name and process
        let groups_to_process: Vec<_> = matches
            .into_iter()
            .map(|(idx, files)| (&directories[idx], files))
            .sorted_by(|a, b| a.0.name.cmp(&b.0.name))
            .collect();

        if self.config.verbose {
            print_bold!(
                "Found {} directory match(es) with files to move:\n",
                groups_to_process.len()
            );
        }

        for (dir, files) in groups_to_process {
            self.process_directory_match(dir, &files)?;
        }

        Ok(())
    }

    fn collect_directories_in_root(&self) -> anyhow::Result<Vec<DirectoryInfo>> {
        let mut dirs = Vec::new();
        for entry in std::fs::read_dir(&self.root)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                let path = entry.path();
                // Skip system directories like $RECYCLE.BIN and unwanted directories
                if cli_tools::is_system_directory_path(&path) {
                    continue;
                }
                let file_name = entry.file_name();
                let dir_name = file_name.to_string_lossy();
                if utils::is_unwanted_directory(&dir_name) {
                    continue;
                }
                let dir_name = dir_name.to_lowercase();
                if !self.config.exclude.is_empty()
                    && self
                        .config
                        .exclude
                        .iter()
                        .any(|pattern| dir_name.contains(&pattern.to_lowercase()))
                {
                    if self.config.verbose {
                        println!("Excluding directory: {}", path_to_string_relative(&entry.path()));
                    }
                    continue;
                }
                dirs.push(DirectoryInfo::new(entry.path()));
            }
        }
        Ok(dirs)
    }

    fn collect_files_in_root(&self) -> anyhow::Result<Vec<PathBuf>> {
        let mut files = Vec::new();
        for entry in std::fs::read_dir(&self.root)? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                let file_name = entry.file_name().to_string_lossy().to_lowercase();

                // Skip files that don't match include patterns (if any specified)
                if !self.config.include.is_empty()
                    && !self
                        .config
                        .include
                        .iter()
                        .any(|pattern| file_name.contains(&pattern.to_lowercase()))
                {
                    continue;
                }
                // Skip files that match exclude patterns
                if self
                    .config
                    .exclude
                    .iter()
                    .any(|pattern| file_name.contains(&pattern.to_lowercase()))
                {
                    continue;
                }

                files.push(entry.path());
            }
        }
        Ok(files)
    }

    /// Match files to directories based on normalized name matching.
    /// Returns a map from directory index (into `dirs`) to the list of matching file paths.
    /// Longer directory names are matched first to prefer more specific matches.
    /// Handles both dot-separated names ("Some.Name") and concatenated names ("`SomeName`").
    fn match_files_to_directories(&self, files: &[PathBuf], dirs: &[DirectoryInfo]) -> HashMap<usize, Vec<PathBuf>> {
        let mut matches: HashMap<usize, Vec<PathBuf>> = HashMap::new();

        // Sort directory indices by name length (longest first) to match more specific names first
        let mut dir_indices: Vec<usize> = (0..dirs.len()).collect();
        dir_indices.sort_by_key(|&i| std::cmp::Reverse(dirs[i].name.len()));

        for file_path in files {
            let Some(file_name) = file_path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };

            // Normalize: replace dots with spaces for matching
            let file_name_spaced = file_name.replace('.', " ").to_lowercase();
            // Also create a concatenated version (no spaces or dots) for matching "SomeName" to "Some Name"
            let file_name_concat = file_name.replace('.', "").to_lowercase();

            // Apply prefix ignores: strip ignored prefixes from the normalized filename
            let file_name_spaced_stripped = self.strip_ignored_prefixes(&file_name_spaced);

            for &idx in &dir_indices {
                // dir.name is already lowercase with spaces
                let dir_name = &dirs[idx].name;
                // Also strip ignored prefixes from directory name for matching
                let dir_name_stripped = self.strip_ignored_prefixes(dir_name);
                // Create concatenated version of directory name (no spaces)
                let dir_name_concat = dir_name.replace(' ', "");
                let dir_name_stripped_concat = dir_name_stripped.replace(' ', "");

                // Skip directories whose name is exactly an ignored prefix
                // (after stripping, the directory name would be empty or unchanged if it's just the prefix)
                if self.is_ignored_prefix(dir_name) {
                    continue;
                }

                // Check multiple matching strategies:
                // 1. Spaced filename contains spaced directory name (e.g., "some name ep1" contains "some name")
                // 2. Concatenated filename contains concatenated directory name (e.g., "somename" contains "somename")
                // 3. With prefix stripping applied to both
                let is_match = file_name_spaced_stripped.contains(&*dir_name_stripped)
                    || file_name_spaced.contains(&*dir_name_stripped)
                    || file_name_spaced_stripped.contains(&**dir_name)
                    || file_name_concat.contains(&dir_name_concat)
                    || file_name_concat.contains(&dir_name_stripped_concat);

                if is_match {
                    matches.entry(idx).or_default().push(file_path.clone());
                    // Only match to first directory found
                    break;
                }
            }
        }

        // Debug: print match groups with file counts, sorted by count descending
        if self.config.debug {
            eprintln!("Directory match groups:");
            let mut sorted_matches: Vec<_> = matches.iter().collect();
            sorted_matches.sort_by(|a, b| b.1.len().cmp(&a.1.len()));
            for (&idx, matched_files) in sorted_matches {
                eprintln!("  {} -> {} file(s)", dirs[idx].name, matched_files.len());
            }
            if matches.is_empty() {
                eprintln!("  (no matches found)");
            }
            eprintln!();
        }

        matches
    }

    fn strip_ignored_dot_prefixes(&self, filename: &str) -> String {
        if self.config.prefix_ignores.is_empty() {
            return filename.to_string();
        }

        let mut result = filename.to_string();
        let mut changed = true;

        // Keep stripping prefixes until no more matches
        while changed {
            changed = false;
            for ignore in &self.config.prefix_ignores {
                let ignore_lower = ignore.to_lowercase();
                let result_lower = result.to_lowercase();
                // Check if filename starts with the ignored prefix followed by a dot
                let prefix_with_dot = format!("{ignore_lower}.");
                if result_lower.starts_with(&prefix_with_dot) {
                    result = result[prefix_with_dot.len()..].to_string();
                    changed = true;
                    break;
                }
            }
        }

        result
    }

    fn process_directory_match(&self, dir: &DirectoryInfo, files: &[PathBuf]) -> anyhow::Result<()> {
        let dir_display = get_relative_path_or_filename(&dir.path, &self.root);
        println!("{}: {} file(s)", dir_display.cyan().bold(), files.len());

        for file_path in files {
            println!("  {}", path_to_filename_string(file_path));
        }

        println!("  {} Move to: {dir_display}", "→".green());

        if !self.config.dryrun {
            let confirmed = if self.config.auto {
                true
            } else {
                print!("{}", "Move files to this directory? (y/n): ".magenta());
                std::io::stdout().flush()?;

                let mut input = String::new();
                std::io::stdin().read_line(&mut input)?;
                input.trim().eq_ignore_ascii_case("y")
            };

            if confirmed {
                if let Err(e) = self.move_files_to_target_dir(&dir.path, files) {
                    print_error!("Failed to move files to {}: {e}", dir.path.display());
                }
            } else {
                println!("  Skipped");
            }
        }
        println!();

        Ok(())
    }

    /// Move files to the target directory, creating it if needed.
    fn move_files_to_target_dir(&self, dir_path: &Path, files: &[PathBuf]) -> anyhow::Result<()> {
        if !dir_path.exists() {
            std::fs::create_dir(dir_path)?;
            println!("  Created directory: {}", path_to_filename_string(dir_path));
        }

        let mut moved_count = 0;
        for file_path in files {
            let file_name = path_to_filename_string(file_path);
            if file_name.is_empty() {
                print_error!("Could not get file name for path: {}", file_path.display());
                continue;
            }
            let new_path = dir_path.join(&file_name);

            if new_path.exists() && !self.config.overwrite {
                print_warning!("Skipping existing file: {}", new_path.display());
                continue;
            }

            match std::fs::rename(file_path, &new_path) {
                Ok(()) => {
                    if self.config.verbose {
                        println!("  Moved: {file_name}");
                    }
                    moved_count += 1;
                }
                Err(e) => print_error!("Failed to move {}: {e}", file_path.display()),
            }
        }
        println!("  Moved {moved_count} files");

        Ok(())
    }

    /// Collect files with their processed names for grouping.
    /// Returns a list of `FileInfo` containing path, original name, and filtered name.
    fn collect_files_with_names(&self) -> anyhow::Result<Vec<FileInfo<'static>>> {
        let mut files_with_names: Vec<FileInfo<'static>> = Vec::new();

        for entry in std::fs::read_dir(&self.root)? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }

            let file_path = entry.path();
            let Some(file_name) = file_path.file_name().and_then(|n| n.to_str()).map(String::from) else {
                continue;
            };

            // Skip hidden files
            if file_name.starts_with('.') {
                continue;
            }

            if !self.config.include.is_empty() && !self.config.include.iter().any(|pattern| file_name.contains(pattern))
            {
                continue;
            }
            if !self.config.exclude.is_empty() && self.config.exclude.iter().any(|pattern| file_name.contains(pattern))
            {
                continue;
            }

            // Strip ignored prefixes, numeric-only parts, resolution patterns, and glue words from filename for grouping purposes
            let original_name = self.strip_ignored_dot_prefixes(&file_name);
            let filtered_name = utils::filter_numeric_resolution_and_glue_parts(&original_name);
            files_with_names.push(FileInfo::new(file_path, original_name, filtered_name));
        }

        // Debug: print unique processed names
        if self.config.debug {
            let unique_names: HashSet<_> = files_with_names.iter().map(|f| f.filtered_name.as_ref()).collect();
            let mut sorted_names: Vec<_> = unique_names.into_iter().collect();
            sorted_names.sort_unstable();
            eprintln!("Unique processed names for grouping:");
            for name in sorted_names {
                eprintln!("  {name}");
            }
        }

        Ok(files_with_names)
    }

    /// Collect all possible prefix groups for files.
    /// Files can appear in multiple groups - they are only excluded once actually moved.
    /// Returns a map from display prefix to list of matching file paths.
    /// Collect all possible prefix groups for files.
    /// Files can appear in multiple groups - they are only excluded once actually moved.
    /// Returns a map from display prefix to (files, `prefix_parts`) where `prefix_parts` indicates specificity.
    fn collect_all_prefix_groups(
        &self,
        files_with_names: &[FileInfo<'_>],
    ) -> HashMap<String, (Vec<PathBuf>, usize, usize)> {
        // Use normalized keys (no dots, lowercase) for grouping to handle
        // both case variations and dot-separated vs concatenated prefixes
        // Value is (original_prefix, files, prefix_parts, has_concatenated_form, min_start_position)
        let mut prefix_groups: HashMap<String, (String, Vec<PathBuf>, usize, bool, usize)> = HashMap::new();

        // First pass: collect prefix candidates from each file
        for file_info in files_with_names {
            let prefix_candidates = utils::find_prefix_candidates(
                &file_info.filtered_name,
                files_with_names,
                self.config.min_group_size,
                self.config.min_prefix_chars,
            );

            // Add file to ALL matching prefix groups, not just the best one
            for candidate in prefix_candidates {
                // Skip candidates that match ignored group names
                let candidate_normalized = utils::normalize_prefix(&candidate.prefix);
                if self.config.ignored_group_names.contains(&candidate_normalized) {
                    continue;
                }

                // Skip candidates where any part matches an ignored_group_part
                // This filters out groups like "DL x265 TEST" when "x265" is in ignored_group_parts
                let candidate_parts: Vec<&str> = candidate.prefix.split('.').collect();
                if candidate_parts.iter().any(|part| {
                    self.config
                        .ignored_group_parts
                        .iter()
                        .any(|ignored| part.eq_ignore_ascii_case(ignored))
                }) {
                    continue;
                }

                // Verify this file itself has the prefix parts contiguous in its original name.
                // This prevents adding files where filtering made non-adjacent parts appear adjacent.
                let prefix_parts_vec: Vec<&str> = candidate.prefix.split('.').collect();
                if !utils::parts_are_contiguous_in_original(&file_info.original_name, &prefix_parts_vec) {
                    continue;
                }

                let key = utils::normalize_prefix(&candidate.prefix);
                let is_concatenated = !candidate.prefix.contains('.');

                if let Some((stored_prefix, files, existing_parts, has_concat, min_pos)) = prefix_groups.get_mut(&key) {
                    files.push(file_info.path_buf());
                    // Keep the highest specificity (most parts)
                    *existing_parts = (*existing_parts).max(candidate.part_count);
                    // Track minimum start position across all files
                    *min_pos = (*min_pos).min(candidate.start_position);
                    // Prefer concatenated (no-dot) form for directory names
                    if is_concatenated && !*has_concat {
                        *stored_prefix = candidate.prefix.into_owned();
                        *has_concat = true;
                    }
                } else {
                    prefix_groups.insert(
                        key,
                        (
                            candidate.prefix.into_owned(),
                            vec![file_info.path_buf()],
                            candidate.part_count,
                            is_concatenated,
                            candidate.start_position,
                        ),
                    );
                }
            }
        }

        // Second pass: for each file, check if it should be added to existing groups
        // where the file's first part(s) START WITH the group's prefix.
        // This handles cases like JosephExampleTV matching JosephExample group.
        let group_keys: Vec<String> = prefix_groups.keys().cloned().collect();
        for file_info in files_with_names {
            let file_path = file_info.path_buf();

            for group_key in &group_keys {
                // Skip if file is already in this group
                if let Some((_, files, _, _, _)) = prefix_groups.get(group_key)
                    && files.contains(&file_path)
                {
                    continue;
                }

                // Check if the file matches this group via prefix_matches_normalized
                // (which includes starts_with logic)
                if utils::prefix_matches_normalized(&file_info.filtered_name, group_key) {
                    // Also verify contiguity in original
                    // For this check, we need to reconstruct the prefix parts from the group key
                    // Since the key is normalized (no dots), we check if the original starts with it
                    if utils::parts_are_contiguous_in_original(&file_info.original_name, &[group_key.as_str()])
                        && let Some((_, files, _, _, _)) = prefix_groups.get_mut(group_key)
                    {
                        files.push(file_path.clone());
                    }
                }
            }
        }

        // Convert to final format: display_prefix -> (files, prefix_parts, min_start_position)
        let display_groups: HashMap<String, (Vec<PathBuf>, usize, usize)> = prefix_groups
            .into_values()
            .map(|(prefix, files, parts, _, min_pos)| (prefix, (files, parts, min_pos)))
            .collect();

        // Apply prefix overrides: if a group's prefix starts with an override, use the override
        self.apply_prefix_overrides(display_groups)
    }

    /// Create directories for files with matching prefixes and move files into them.
    /// Only considers files directly in the base path (not recursive).
    /// Files can match multiple groups - they remain available until actually moved.
    fn create_dirs_and_move_files(&self) -> anyhow::Result<()> {
        let files_with_names = self.collect_files_with_names()?;
        let prefix_groups = self.collect_all_prefix_groups(&files_with_names);

        // Sort groups by:
        // 1. Start position (earlier in filename = lower value = first)
        // 2. Prefix length (longer = first, for same position)
        // 3. Alphabetically (for ties)
        // This biases towards prefixes from the start of filenames.
        // Filter out groups smaller than the configured minimum.
        let min_group_size = self.config.min_group_size;
        let mut groups_to_process: Vec<_> = prefix_groups
            .into_iter()
            .filter(|(_, (files, _, _))| files.len() >= min_group_size)
            .sorted_by(|a, b| {
                let (_, (_, _, pos_a)) = a;
                let (_, (_, _, pos_b)) = b;
                // First by position (ascending - earlier is better)
                pos_a
                    .cmp(pos_b)
                    // Then by length (descending - longer is more specific)
                    .then_with(|| b.0.len().cmp(&a.0.len()))
                    // Finally alphabetically
                    .then_with(|| a.0.cmp(&b.0))
            })
            .collect();

        if groups_to_process.is_empty() {
            if self.config.verbose {
                println!("No file groups with {min_group_size} or more matching prefixes found.");
            }
            return Ok(());
        }

        // Track files that have been moved to avoid offering them again
        let mut moved_files: HashSet<PathBuf> = HashSet::new();

        // Count initial groups for display (before filtering by moved files)
        let initial_group_count = groups_to_process.len();
        print_bold!("Found {} group(s) with matching prefixes:\n", initial_group_count);

        while !groups_to_process.is_empty() {
            let (prefix, (files, _, _)) = groups_to_process.remove(0);

            // Filter out already moved files
            let available_files: Vec<_> = files.into_iter().filter(|f| !moved_files.contains(f)).collect();

            if available_files.len() < min_group_size {
                continue;
            }

            let dir_name = prefix.replace('.', " ");
            let dir_path = self.root.join(&dir_name);
            let dir_exists = dir_path.exists();

            println!("{}: {} files", dir_name.cyan().bold(), available_files.len());
            for file_path in &available_files {
                println!("  {}", path_to_filename_string(file_path));
            }

            if dir_exists {
                println!("  {} Directory already exists", "→".green());
            } else {
                println!("  {} Will create directory: {dir_name}", "→".yellow());
            }

            if !self.config.dryrun {
                let confirmed = if self.config.auto {
                    true
                } else {
                    print!("{}", "Create directory and move files? (y/n): ".magenta());
                    std::io::stdout().flush()?;

                    let mut input = String::new();
                    std::io::stdin().read_line(&mut input)?;
                    input.trim().eq_ignore_ascii_case("y")
                };

                if confirmed {
                    if let Err(e) = self.move_files_to_target_dir(&dir_path, &available_files) {
                        print_error!("Failed to process {}: {e}", dir_name);
                    } else {
                        // Mark files as moved
                        moved_files.extend(available_files);
                    }
                } else {
                    println!("  Skipped");
                    // Files remain available for other groups since they weren't moved
                }
            }
            println!();
        }

        Ok(())
    }

    /// Apply prefix overrides to groups.
    /// If files in a group start with an override prefix, merge them under the override name.
    fn apply_prefix_overrides(
        &self,
        groups: HashMap<String, (Vec<PathBuf>, usize, usize)>,
    ) -> HashMap<String, (Vec<PathBuf>, usize, usize)> {
        if self.config.prefix_overrides.is_empty() {
            return groups;
        }

        let mut result: HashMap<String, (Vec<PathBuf>, usize, usize)> = HashMap::new();

        for (prefix, (files, prefix_parts, min_pos)) in groups {
            // Check if any override matches: either the prefix starts with override,
            // or the override starts with the prefix (override is more specific),
            // or any file in the group starts with the override
            let target_prefix = self
                .config
                .prefix_overrides
                .iter()
                .find(|&override_prefix| {
                    prefix.starts_with(override_prefix)
                        || override_prefix.starts_with(&prefix)
                        || files.iter().any(|f| {
                            f.file_name()
                                .and_then(|n| n.to_str())
                                .is_some_and(|name| name.starts_with(override_prefix))
                        })
                })
                .map_or(prefix, std::clone::Clone::clone);

            let entry = result
                .entry(target_prefix)
                .or_insert_with(|| (Vec::new(), prefix_parts, min_pos));
            entry.0.extend(files);
            // Keep the highest specificity
            entry.1 = entry.1.max(prefix_parts);
            // Keep the minimum start position
            entry.2 = entry.2.min(min_pos);
        }

        result
    }

    /// Strip ignored prefixes from a normalized filename (spaces as separators).
    /// Recursively removes any matching prefix from the start of the filename.
    fn strip_ignored_prefixes<'a>(&self, filename: &'a str) -> Cow<'a, str> {
        if self.config.prefix_ignores.is_empty() {
            return Cow::Borrowed(filename);
        }

        let mut result = filename;
        let mut changed = true;

        // Keep stripping prefixes until no more matches
        while changed {
            changed = false;
            for ignore in &self.config.prefix_ignores {
                let ignore_lower = ignore.to_lowercase();
                // Check if filename starts with the ignored prefix followed by a space
                let prefix_with_space = format!("{ignore_lower} ");
                if result.starts_with(&prefix_with_space) {
                    result = result.strip_prefix(&prefix_with_space).unwrap_or(result);
                    changed = true;
                    break;
                }
            }
        }

        if result == filename {
            Cow::Borrowed(filename)
        } else {
            Cow::Owned(result.to_string())
        }
    }

    /// Check if a name exactly matches one of the ignored prefixes.
    fn is_ignored_prefix(&self, name: &str) -> bool {
        self.config
            .prefix_ignores
            .iter()
            .any(|ignore| ignore.eq_ignore_ascii_case(name))
    }
}

#[cfg(test)]
pub mod test_helpers {
    use super::*;
    use crate::types::PrefixCandidate;

    /// Create test files with path, original name, and filtered name.
    /// For simple tests, the original and filtered names are the same.
    pub fn make_test_files(names: &[&str]) -> Vec<FileInfo<'static>> {
        names
            .iter()
            .map(|n| FileInfo::new(PathBuf::from(*n), (*n).to_string(), (*n).to_string()))
            .collect()
    }

    /// Create `FileInfo` from original filenames by applying the standard filtering.
    /// The original name and filtered name are both derived from the input.
    pub fn make_filtered_files(names: &[&str]) -> Vec<FileInfo<'static>> {
        names
            .iter()
            .map(|name| {
                let filtered = utils::filter_numeric_resolution_and_glue_parts(name);
                FileInfo::new(PathBuf::from(*name), (*name).to_string(), filtered)
            })
            .collect()
    }

    pub fn write_file(path: &Path, contents: &str) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, contents)?;
        Ok(())
    }

    pub fn assert_exists(path: &Path) {
        assert!(path.exists(), "Expected path to exist: {}", path.display());
    }

    pub fn assert_not_exists(path: &Path) {
        assert!(!path.exists(), "Expected path to NOT exist: {}", path.display());
    }

    pub fn make_test_config_with_ignores(prefix_overrides: Vec<String>, prefix_ignores: Vec<String>) -> Config {
        Config {
            auto: false,
            create: false,
            debug: false,
            dryrun: true,
            include: Vec::new(),
            exclude: Vec::new(),
            ignored_group_names: Vec::new(),
            ignored_group_parts: Vec::new(),
            min_group_size: 3,
            min_prefix_chars: 5,
            overwrite: false,
            prefix_ignores,
            prefix_overrides,
            recurse: false,
            verbose: false,
            unpack_directory_names: Vec::new(),
        }
    }

    pub fn make_unpack_config(unpack_names: Vec<&str>, recurse: bool, dryrun: bool, overwrite: bool) -> Config {
        Config {
            auto: true,
            create: false,
            debug: false,
            dryrun,
            include: Vec::new(),
            exclude: Vec::new(),
            ignored_group_names: Vec::new(),
            ignored_group_parts: Vec::new(),
            min_group_size: 3,
            min_prefix_chars: 5,
            overwrite,
            prefix_ignores: Vec::new(),
            prefix_overrides: Vec::new(),
            recurse,
            verbose: false,
            unpack_directory_names: unpack_names.into_iter().map(String::from).collect(),
        }
    }

    pub fn make_test_dirmove_with_ignores(prefix_overrides: Vec<String>, prefix_ignores: Vec<String>) -> DirMove {
        DirMove {
            root: PathBuf::from("."),
            config: make_test_config_with_ignores(prefix_overrides, prefix_ignores),
        }
    }

    pub fn make_test_dirmove(prefix_overrides: Vec<String>) -> DirMove {
        make_test_dirmove_with_ignores(prefix_overrides, Vec::new())
    }

    /// Helper to create a `PrefixCandidate` for test assertions.
    pub fn candidate(
        prefix: &str,
        match_count: usize,
        part_count: usize,
        start_position: usize,
    ) -> PrefixCandidate<'static> {
        PrefixCandidate::new(Cow::Owned(prefix.to_string()), match_count, part_count, start_position)
    }

    pub fn make_test_dirs(names: &[&str]) -> Vec<DirectoryInfo> {
        names
            .iter()
            .map(|n| DirectoryInfo {
                path: PathBuf::from(*n),
                name: n.to_lowercase(),
            })
            .collect()
    }

    pub fn make_file_paths(names: &[&str]) -> Vec<PathBuf> {
        names.iter().map(|n| PathBuf::from(*n)).collect()
    }
}

#[cfg(test)]
mod test_prefix_groups {
    use super::*;

    #[test]
    fn files_appear_in_multiple_groups() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("Show.Name.S01E01.mp4"), "").unwrap();
        std::fs::write(root.join("Show.Name.S01E02.mp4"), "").unwrap();
        std::fs::write(root.join("Show.Name.S01E03.mp4"), "").unwrap();
        std::fs::write(root.join("Show.Other.S01E01.mp4"), "").unwrap();
        std::fs::write(root.join("Show.Other.S01E02.mp4"), "").unwrap();

        let config = Config {
            auto: false,
            create: true,
            debug: false,
            dryrun: true,
            include: Vec::new(),
            exclude: Vec::new(),
            ignored_group_names: Vec::new(),
            ignored_group_parts: Vec::new(),
            min_group_size: 2,
            min_prefix_chars: 1,
            overwrite: false,
            prefix_ignores: Vec::new(),
            prefix_overrides: Vec::new(),
            recurse: false,
            verbose: false,
            unpack_directory_names: Vec::new(),
        };

        let dirmove = DirMove::new(root, config);
        let files_with_names = dirmove.collect_files_with_names().unwrap();
        let groups = dirmove.collect_all_prefix_groups(&files_with_names);

        // Keys are normalized (lowercase, no dots)
        assert!(groups.contains_key("Show.Name"), "Should have Show.Name group");
        assert!(groups.contains_key("Show.Other"), "Should have Show.Other group");
        assert!(groups.contains_key("Show"), "Should have Show group");
        assert_eq!(groups.get("Show.Name").unwrap().0.len(), 3);
        assert_eq!(groups.get("Show.Other").unwrap().0.len(), 2);
        assert_eq!(groups.get("Show").unwrap().0.len(), 5);
    }

    #[test]
    fn order_by_prefix_length() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("Alpha.Beta.Gamma.part1.mp4"), "").unwrap();
        std::fs::write(root.join("Alpha.Beta.Gamma.part2.mp4"), "").unwrap();
        std::fs::write(root.join("Alpha.Beta.Delta.part1.mp4"), "").unwrap();
        std::fs::write(root.join("Alpha.Beta.Delta.part2.mp4"), "").unwrap();

        let config = Config {
            auto: false,
            create: true,
            debug: false,
            dryrun: true,
            include: Vec::new(),
            exclude: Vec::new(),
            ignored_group_names: Vec::new(),
            ignored_group_parts: Vec::new(),
            min_group_size: 2,
            min_prefix_chars: 1,
            overwrite: false,
            prefix_ignores: Vec::new(),
            prefix_overrides: Vec::new(),
            recurse: false,
            verbose: false,
            unpack_directory_names: Vec::new(),
        };

        let dirmove = DirMove::new(root, config);
        let files_with_names = dirmove.collect_files_with_names().unwrap();
        let groups = dirmove.collect_all_prefix_groups(&files_with_names);

        assert!(groups.contains_key("Alpha.Beta.Gamma"));
        assert!(groups.contains_key("Alpha.Beta.Delta"));
        assert!(groups.contains_key("Alpha.Beta"));
        assert!(groups.contains_key("Alpha"));
        assert_eq!(groups.get("Alpha.Beta.Gamma").unwrap().0.len(), 2);
        assert_eq!(groups.get("Alpha.Beta.Delta").unwrap().0.len(), 2);
        assert_eq!(groups.get("Alpha.Beta").unwrap().0.len(), 4);
        assert_eq!(groups.get("Alpha").unwrap().0.len(), 4);
    }

    #[test]
    fn no_files_no_groups() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        let config = Config {
            auto: false,
            create: true,
            debug: false,
            dryrun: true,
            include: Vec::new(),
            exclude: Vec::new(),
            ignored_group_names: Vec::new(),
            ignored_group_parts: Vec::new(),
            min_group_size: 2,
            min_prefix_chars: 1,
            overwrite: false,
            prefix_ignores: Vec::new(),
            prefix_overrides: Vec::new(),
            recurse: false,
            verbose: false,
            unpack_directory_names: Vec::new(),
        };

        let dirmove = DirMove::new(root, config);
        let files_with_names = dirmove.collect_files_with_names().unwrap();
        let groups = dirmove.collect_all_prefix_groups(&files_with_names);

        assert!(groups.is_empty());
    }

    #[test]
    fn single_file_no_group() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("Lonely.File.mp4"), "").unwrap();

        let config = Config {
            auto: false,
            create: true,
            debug: false,
            dryrun: true,
            include: Vec::new(),
            exclude: Vec::new(),
            ignored_group_names: Vec::new(),
            ignored_group_parts: Vec::new(),
            min_group_size: 2,
            min_prefix_chars: 1,
            overwrite: false,
            prefix_ignores: Vec::new(),
            prefix_overrides: Vec::new(),
            recurse: false,
            verbose: false,
            unpack_directory_names: Vec::new(),
        };

        let dirmove = DirMove::new(root, config);
        let files_with_names = dirmove.collect_files_with_names().unwrap();
        let groups = dirmove.collect_all_prefix_groups(&files_with_names);

        assert!(groups.is_empty());
    }

    #[test]
    fn files_with_completely_different_prefixes() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("Alpha.One.mp4"), "").unwrap();
        std::fs::write(root.join("Beta.Two.mp4"), "").unwrap();
        std::fs::write(root.join("Gamma.Three.mp4"), "").unwrap();

        let config = Config {
            auto: false,
            create: true,
            debug: false,
            dryrun: true,
            include: Vec::new(),
            exclude: Vec::new(),
            ignored_group_names: Vec::new(),
            ignored_group_parts: Vec::new(),
            min_group_size: 2,
            min_prefix_chars: 1,
            overwrite: false,
            prefix_ignores: Vec::new(),
            prefix_overrides: Vec::new(),
            recurse: false,
            verbose: false,
            unpack_directory_names: Vec::new(),
        };

        let dirmove = DirMove::new(root, config);
        let files_with_names = dirmove.collect_files_with_names().unwrap();
        let groups = dirmove.collect_all_prefix_groups(&files_with_names);

        assert!(groups.is_empty());
    }

    #[test]
    fn large_file_set_multiple_series() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        for episode in 1..=10 {
            std::fs::write(root.join(format!("SeriesA.Season1.E{episode:02}.mp4")), "").unwrap();
        }
        for episode in 1..=8 {
            std::fs::write(root.join(format!("SeriesA.Season2.E{episode:02}.mp4")), "").unwrap();
        }
        for episode in 1..=5 {
            std::fs::write(root.join(format!("SeriesB.Season1.E{episode:02}.mp4")), "").unwrap();
        }

        let config = Config {
            auto: false,
            create: true,
            debug: false,
            dryrun: true,
            include: Vec::new(),
            exclude: Vec::new(),
            ignored_group_names: Vec::new(),
            ignored_group_parts: Vec::new(),
            min_group_size: 3,
            min_prefix_chars: 1,
            overwrite: false,
            prefix_ignores: Vec::new(),
            prefix_overrides: Vec::new(),
            recurse: false,
            verbose: false,
            unpack_directory_names: Vec::new(),
        };

        let dirmove = DirMove::new(root, config);
        let files_with_names = dirmove.collect_files_with_names().unwrap();
        let groups = dirmove.collect_all_prefix_groups(&files_with_names);

        assert!(groups.contains_key("SeriesA.Season1"));
        assert!(groups.contains_key("SeriesA.Season2"));
        assert!(groups.contains_key("SeriesB.Season1"));
        assert!(groups.contains_key("SeriesA"));
        assert!(groups.contains_key("SeriesB"));
        assert_eq!(groups.get("SeriesA.Season1").unwrap().0.len(), 10);
        assert_eq!(groups.get("SeriesA.Season2").unwrap().0.len(), 8);
        assert_eq!(groups.get("SeriesA").unwrap().0.len(), 18);
        assert_eq!(groups.get("SeriesB.Season1").unwrap().0.len(), 5);
        assert_eq!(groups.get("SeriesB").unwrap().0.len(), 5);
    }

    #[test]
    fn min_group_size_affects_grouping() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("Show.Episode.01.mp4"), "").unwrap();
        std::fs::write(root.join("Show.Episode.02.mp4"), "").unwrap();

        let config_low = Config {
            auto: false,
            create: true,
            debug: false,
            dryrun: true,
            include: Vec::new(),
            exclude: Vec::new(),
            ignored_group_names: Vec::new(),
            ignored_group_parts: Vec::new(),
            min_group_size: 2,
            min_prefix_chars: 1,
            overwrite: false,
            prefix_ignores: Vec::new(),
            prefix_overrides: Vec::new(),
            recurse: false,
            verbose: false,
            unpack_directory_names: Vec::new(),
        };

        let dirmove_low = DirMove::new(root.clone(), config_low);
        let files_with_names = dirmove_low.collect_files_with_names().unwrap();
        let groups_low = dirmove_low.collect_all_prefix_groups(&files_with_names);
        assert!(groups_low.contains_key("Show.Episode"));
        assert!(groups_low.contains_key("Show"));

        let config_high = Config {
            auto: false,
            create: true,
            debug: false,
            dryrun: true,
            include: Vec::new(),
            exclude: Vec::new(),
            ignored_group_names: Vec::new(),
            ignored_group_parts: Vec::new(),
            min_group_size: 5,
            min_prefix_chars: 1,
            overwrite: false,
            prefix_ignores: Vec::new(),
            prefix_overrides: Vec::new(),
            recurse: false,
            verbose: false,
            unpack_directory_names: Vec::new(),
        };

        let dirmove_high = DirMove::new(root, config_high);
        let files_with_names = dirmove_high.collect_files_with_names().unwrap();
        let groups_high = dirmove_high.collect_all_prefix_groups(&files_with_names);
        // With min_group_size=5, groups with only 2 files are excluded
        assert!(groups_high.is_empty());
    }

    #[test]
    fn case_variations_grouped_together() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("MyShow.Episode.01.mp4"), "").unwrap();
        std::fs::write(root.join("MYSHOW.Episode.02.mp4"), "").unwrap();
        std::fs::write(root.join("myshow.Episode.03.mp4"), "").unwrap();
        std::fs::write(root.join("Myshow.Episode.04.mp4"), "").unwrap();

        let config = Config {
            auto: false,
            create: true,
            debug: false,
            dryrun: true,
            include: Vec::new(),
            exclude: Vec::new(),
            ignored_group_names: Vec::new(),
            ignored_group_parts: Vec::new(),
            min_group_size: 3,
            min_prefix_chars: 1,
            overwrite: false,
            prefix_ignores: Vec::new(),
            prefix_overrides: Vec::new(),
            recurse: false,
            verbose: false,
            unpack_directory_names: Vec::new(),
        };

        let dirmove = DirMove::new(root, config);
        let files_with_names = dirmove.collect_files_with_names().unwrap();
        let groups = dirmove.collect_all_prefix_groups(&files_with_names);

        // All case variations should be in the same group
        // Key will be from first file processed (original case preserved for display)
        // Check that some group has all 4 files
        let max_group_size = groups.values().map(|(files, _, _)| files.len()).max().unwrap_or(0);
        assert_eq!(max_group_size, 4, "Should have a group with all 4 case variations");
    }

    #[test]
    fn dot_separated_and_concatenated_grouped_together() {
        // Dot-separated and concatenated prefixes should be grouped together
        // because prefixes_match_normalized handles this
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // These have different structures: "Photo.Lab" vs "PhotoLab"
        std::fs::write(root.join("Photo.Lab.Image.01.jpg"), "").unwrap();
        std::fs::write(root.join("PhotoLab.Image.02.jpg"), "").unwrap();
        std::fs::write(root.join("Photolab.Image.03.jpg"), "").unwrap();
        std::fs::write(root.join("PHOTOLAB.Image.04.jpg"), "").unwrap();

        let config = Config {
            auto: false,
            create: true,
            debug: false,
            dryrun: true,
            include: Vec::new(),
            exclude: Vec::new(),
            ignored_group_names: Vec::new(),
            ignored_group_parts: Vec::new(),
            min_group_size: 2,
            min_prefix_chars: 1,
            overwrite: false,
            prefix_ignores: Vec::new(),
            prefix_overrides: Vec::new(),
            recurse: false,
            verbose: false,
            unpack_directory_names: Vec::new(),
        };

        let dirmove = DirMove::new(root, config);
        let files_with_names = dirmove.collect_files_with_names().unwrap();

        let groups = dirmove.collect_all_prefix_groups(&files_with_names);

        // All 4 files should be grouped together (PhotoLab = Photo.Lab = Photolab = PHOTOLAB)
        let max_group_size = groups.values().map(|(files, _, _)| files.len()).max().unwrap_or(0);
        assert_eq!(max_group_size, 4, "Should have a group with all 4 files");

        // Should prefer the concatenated form (no dots) for the directory name
        // The group key should be "PhotoLab" not "Photo.Lab"
        assert!(
            groups.contains_key("PhotoLab"),
            "Should prefer concatenated prefix 'PhotoLab' over dotted 'Photo.Lab'"
        );
    }

    #[test]
    fn prefers_concatenated_prefix_for_directory_name() {
        // When files have both "Darkko.TV" and "DarkkoTV" forms, prefer "DarkkoTV"
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("Darkko.TV.Episode.01.mp4"), "").unwrap();
        std::fs::write(root.join("DarkkoTV.Episode.02.mp4"), "").unwrap();
        std::fs::write(root.join("Darkko.TV.Episode.03.mp4"), "").unwrap();

        let config = Config {
            auto: false,
            create: true,
            debug: false,
            dryrun: true,
            include: Vec::new(),
            exclude: Vec::new(),
            ignored_group_names: Vec::new(),
            ignored_group_parts: Vec::new(),
            min_group_size: 2,
            min_prefix_chars: 1,
            overwrite: false,
            prefix_ignores: Vec::new(),
            prefix_overrides: Vec::new(),
            recurse: false,
            verbose: false,
            unpack_directory_names: Vec::new(),
        };

        let dirmove = DirMove::new(root, config);
        let files_with_names = dirmove.collect_files_with_names().unwrap();
        let groups = dirmove.collect_all_prefix_groups(&files_with_names);

        // Should have a group with all 3 files
        let max_group_size = groups.values().map(|(files, _, _)| files.len()).max().unwrap_or(0);
        assert_eq!(max_group_size, 3, "Should have a group with all 3 files");

        // The group key should be "DarkkoTV" (concatenated) not "Darkko.TV" (dotted)
        assert!(
            groups.contains_key("DarkkoTV"),
            "Should prefer concatenated prefix 'DarkkoTV' over dotted 'Darkko.TV'"
        );
        assert!(
            !groups.contains_key("Darkko.TV"),
            "Should not have separate group for dotted form"
        );
    }

    #[test]
    fn same_prefix_different_case_grouped() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("Summer.Vacation.Photo1.jpg"), "").unwrap();
        std::fs::write(root.join("Summer.Vacation.Photo2.jpg"), "").unwrap();
        std::fs::write(root.join("SUMMER.VACATION.Photo3.jpg"), "").unwrap();
        std::fs::write(root.join("summer.vacation.Photo4.jpg"), "").unwrap();

        let config = Config {
            auto: false,
            create: true,
            debug: false,
            dryrun: true,
            include: Vec::new(),
            exclude: Vec::new(),
            ignored_group_names: Vec::new(),
            ignored_group_parts: Vec::new(),
            min_group_size: 2,
            min_prefix_chars: 1,
            overwrite: false,
            prefix_ignores: Vec::new(),
            prefix_overrides: Vec::new(),
            recurse: false,
            verbose: false,
            unpack_directory_names: Vec::new(),
        };

        let dirmove = DirMove::new(root, config);
        let files_with_names = dirmove.collect_files_with_names().unwrap();
        let groups = dirmove.collect_all_prefix_groups(&files_with_names);

        // All 4 files should be grouped under "Summer.Vacation" (case-insensitive)
        assert!(
            groups.contains_key("Summer.Vacation"),
            "Should have Summer.Vacation group"
        );
        assert_eq!(groups.get("Summer.Vacation").unwrap().0.len(), 4);
    }

    #[test]
    fn min_group_size_respected_in_groups() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // 5 files with same prefix
        std::fs::write(root.join("Gallery.Photos.Img1.jpg"), "").unwrap();
        std::fs::write(root.join("Gallery.Photos.Img2.jpg"), "").unwrap();
        std::fs::write(root.join("Gallery.Photos.Img3.jpg"), "").unwrap();
        std::fs::write(root.join("Gallery.Photos.Img4.jpg"), "").unwrap();
        std::fs::write(root.join("Gallery.Photos.Img5.jpg"), "").unwrap();

        // With min_group_size=6, all prefixes with 5 files are excluded
        let config_6 = Config {
            auto: false,
            create: true,
            debug: false,
            dryrun: true,
            include: Vec::new(),
            exclude: Vec::new(),
            ignored_group_names: Vec::new(),
            ignored_group_parts: Vec::new(),
            min_group_size: 6,
            min_prefix_chars: 1,
            overwrite: false,
            prefix_ignores: Vec::new(),
            prefix_overrides: Vec::new(),
            recurse: false,
            verbose: false,
            unpack_directory_names: Vec::new(),
        };

        let dirmove_6 = DirMove::new(root.clone(), config_6);
        let files_with_names = dirmove_6.collect_files_with_names().unwrap();
        let groups_6 = dirmove_6.collect_all_prefix_groups(&files_with_names);
        // With min_group_size=6, all prefixes with 5 files are excluded
        assert!(
            groups_6.is_empty(),
            "min_group_size=6 should not find groups with only 5 files"
        );

        // With min_group_size=5, should find the group
        let config_5 = Config {
            auto: false,
            create: true,
            debug: false,
            dryrun: true,
            include: Vec::new(),
            exclude: Vec::new(),
            ignored_group_names: Vec::new(),
            ignored_group_parts: Vec::new(),
            min_group_size: 5,
            min_prefix_chars: 1,
            overwrite: false,
            prefix_ignores: Vec::new(),
            prefix_overrides: Vec::new(),
            recurse: false,
            verbose: false,
            unpack_directory_names: Vec::new(),
        };

        let dirmove_5 = DirMove::new(root.clone(), config_5);
        let files_with_names = dirmove_5.collect_files_with_names().unwrap();
        let groups_5 = dirmove_5.collect_all_prefix_groups(&files_with_names);
        assert!(!groups_5.is_empty(), "min_group_size=5 should find groups");
        assert!(
            groups_5.contains_key("Gallery.Photos"),
            "Should have Gallery.Photos group"
        );
        assert_eq!(groups_5.get("Gallery.Photos").unwrap().0.len(), 5);

        // With min_group_size=3, should still find the same group
        let config_3 = Config {
            auto: false,
            create: true,
            debug: false,
            dryrun: true,
            include: Vec::new(),
            exclude: Vec::new(),
            ignored_group_names: Vec::new(),
            ignored_group_parts: Vec::new(),
            min_group_size: 3,
            min_prefix_chars: 1,
            overwrite: false,
            prefix_ignores: Vec::new(),
            prefix_overrides: Vec::new(),
            recurse: false,
            verbose: false,
            unpack_directory_names: Vec::new(),
        };

        let dirmove_3 = DirMove::new(root, config_3);
        let files_with_names = dirmove_3.collect_files_with_names().unwrap();
        let groups_3 = dirmove_3.collect_all_prefix_groups(&files_with_names);
        assert!(
            groups_3.contains_key("Gallery.Photos"),
            "Should have Gallery.Photos group"
        );
        assert_eq!(groups_3.get("Gallery.Photos").unwrap().0.len(), 5);
    }

    #[test]
    fn prefix_overrides_merge_groups() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("Show.Name.Season1.Episode01.mp4"), "").unwrap();
        std::fs::write(root.join("Show.Name.Season1.Episode02.mp4"), "").unwrap();
        std::fs::write(root.join("Show.Name.Season2.Episode01.mp4"), "").unwrap();
        std::fs::write(root.join("Show.Name.Season2.Episode02.mp4"), "").unwrap();

        let config = Config {
            auto: false,
            create: true,
            debug: false,
            dryrun: true,
            include: Vec::new(),
            exclude: Vec::new(),
            ignored_group_names: Vec::new(),
            ignored_group_parts: Vec::new(),
            min_group_size: 2,
            min_prefix_chars: 1,
            overwrite: false,
            prefix_ignores: Vec::new(),
            prefix_overrides: vec!["Show.Name".to_string()],
            recurse: false,
            verbose: false,
            unpack_directory_names: Vec::new(),
        };

        let dirmove = DirMove::new(root, config);
        let files_with_names = dirmove.collect_files_with_names().unwrap();
        let groups = dirmove.collect_all_prefix_groups(&files_with_names);

        // With prefix_overrides = ["Show.Name"], groups starting with Show.Name should be merged
        assert!(
            groups.contains_key("Show.Name"),
            "Should have Show.Name group from override"
        );
        // Should not have separate Season1/Season2 groups - they're merged into Show.Name
        assert!(
            !groups.contains_key("Show.Name.Season1"),
            "Season1 should be merged into Show.Name"
        );
        assert!(
            !groups.contains_key("Show.Name.Season2"),
            "Season2 should be merged into Show.Name"
        );
        // The Show.Name group should contain entries for all 4 files
        // (files may appear multiple times due to multiple prefix matches)
        let show_name_files = groups.get("Show.Name").unwrap();
        assert!(
            show_name_files.0.len() >= 4,
            "Show.Name group should have at least 4 entries"
        );
    }

    #[test]
    fn prefix_overrides_multiple_overrides() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("ShowA.Season1.Ep01.mp4"), "").unwrap();
        std::fs::write(root.join("ShowA.Season1.Ep02.mp4"), "").unwrap();
        std::fs::write(root.join("ShowB.Season1.Ep01.mp4"), "").unwrap();
        std::fs::write(root.join("ShowB.Season1.Ep02.mp4"), "").unwrap();
        std::fs::write(root.join("ShowC.Season1.Ep01.mp4"), "").unwrap();
        std::fs::write(root.join("ShowC.Season1.Ep02.mp4"), "").unwrap();

        let config = Config {
            auto: false,
            create: true,
            debug: false,
            dryrun: true,
            include: Vec::new(),
            exclude: Vec::new(),
            ignored_group_names: Vec::new(),
            ignored_group_parts: Vec::new(),
            min_group_size: 2,
            min_prefix_chars: 1,
            overwrite: false,
            prefix_ignores: Vec::new(),
            prefix_overrides: vec!["ShowA".to_string(), "ShowB".to_string()],
            recurse: false,
            verbose: false,
            unpack_directory_names: Vec::new(),
        };

        let dirmove = DirMove::new(root, config);
        let files_with_names = dirmove.collect_files_with_names().unwrap();
        let groups = dirmove.collect_all_prefix_groups(&files_with_names);

        // ShowA and ShowB should use overrides (groups merged under override name)
        assert!(groups.contains_key("ShowA"), "Should have ShowA from override");
        assert!(groups.contains_key("ShowB"), "Should have ShowB from override");
        // Each override group should have at least 2 entries (the 2 files)
        assert!(
            groups.get("ShowA").unwrap().0.len() >= 2,
            "ShowA should have at least 2 entries"
        );
        assert!(
            groups.get("ShowB").unwrap().0.len() >= 2,
            "ShowB should have at least 2 entries"
        );
        // ShowC has no override, should have its own group(s)
        let has_showc_group = groups.keys().any(|k| k.starts_with("ShowC"));
        assert!(has_showc_group, "ShowC should have a group");
    }

    #[test]
    fn prefix_overrides_no_match_keeps_original() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("MyShow.Episode.01.mp4"), "").unwrap();
        std::fs::write(root.join("MyShow.Episode.02.mp4"), "").unwrap();
        std::fs::write(root.join("MyShow.Episode.03.mp4"), "").unwrap();

        let config = Config {
            auto: false,
            create: true,
            debug: false,
            dryrun: true,
            include: Vec::new(),
            exclude: Vec::new(),
            ignored_group_names: Vec::new(),
            ignored_group_parts: Vec::new(),
            min_group_size: 2,
            min_prefix_chars: 1,
            overwrite: false,
            prefix_ignores: Vec::new(),
            prefix_overrides: vec!["OtherShow".to_string()],
            recurse: false,
            verbose: false,
            unpack_directory_names: Vec::new(),
        };

        let dirmove = DirMove::new(root, config);
        let files_with_names = dirmove.collect_files_with_names().unwrap();
        let groups = dirmove.collect_all_prefix_groups(&files_with_names);

        // Override doesn't match, so original prefix should be used
        let has_myshow_group = groups.keys().any(|k| k.starts_with("MyShow"));
        assert!(has_myshow_group, "Should have MyShow group (override didn't match)");
        assert!(
            !groups.contains_key("OtherShow"),
            "OtherShow override should not create empty group"
        );
    }

    #[test]
    fn prefix_ignores_strips_prefix_from_grouping() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // Files with and without "www" prefix
        std::fs::write(root.join("www.Example.Show.Ep01.mp4"), "").unwrap();
        std::fs::write(root.join("www.Example.Show.Ep02.mp4"), "").unwrap();
        std::fs::write(root.join("Example.Show.Ep03.mp4"), "").unwrap();

        let config = Config {
            auto: false,
            create: true,
            debug: false,
            dryrun: true,
            include: Vec::new(),
            exclude: Vec::new(),
            ignored_group_names: Vec::new(),
            ignored_group_parts: Vec::new(),
            min_group_size: 3,
            min_prefix_chars: 1,
            overwrite: false,
            prefix_ignores: vec!["www".to_string()],
            prefix_overrides: Vec::new(),
            recurse: false,
            verbose: false,
            unpack_directory_names: Vec::new(),
        };

        let dirmove = DirMove::new(root, config);
        let files_with_names = dirmove.collect_files_with_names().unwrap();
        let groups = dirmove.collect_all_prefix_groups(&files_with_names);

        // With prefix_ignores stripping "www", all 3 files should group together
        // Check that we have groups containing files
        assert!(!groups.is_empty(), "Should have at least one group");
    }

    #[test]
    fn prefix_ignores_and_overrides_combined() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("release.Show.Name.Season1.Ep01.mp4"), "").unwrap();
        std::fs::write(root.join("release.Show.Name.Season1.Ep02.mp4"), "").unwrap();
        std::fs::write(root.join("Show.Name.Season2.Ep01.mp4"), "").unwrap();
        std::fs::write(root.join("Show.Name.Season2.Ep02.mp4"), "").unwrap();

        let config = Config {
            auto: false,
            create: true,
            debug: false,
            dryrun: true,
            include: Vec::new(),
            exclude: Vec::new(),
            ignored_group_names: Vec::new(),
            ignored_group_parts: Vec::new(),
            min_group_size: 2,
            min_prefix_chars: 1,
            overwrite: false,
            prefix_ignores: vec!["release".to_string()],
            prefix_overrides: vec!["Show.Name".to_string()],
            recurse: false,
            verbose: false,
            unpack_directory_names: Vec::new(),
        };

        let dirmove = DirMove::new(root, config);
        let files_with_names = dirmove.collect_files_with_names().unwrap();
        let groups = dirmove.collect_all_prefix_groups(&files_with_names);

        // With override "Show.Name", should have that group
        assert!(groups.contains_key("Show.Name"), "Should have Show.Name group");
        // Should not have separate Season groups - merged into Show.Name
        assert!(
            !groups.contains_key("Show.Name.Season1"),
            "Season1 should be merged into Show.Name"
        );
        assert!(
            !groups.contains_key("Show.Name.Season2"),
            "Season2 should be merged into Show.Name"
        );
        // The Show.Name group should contain entries for all 4 files
        let show_name_files = groups.get("Show.Name").unwrap();
        assert!(
            show_name_files.0.len() >= 4,
            "Show.Name group should have at least 4 entries"
        );
    }
}

#[cfg(test)]
mod test_ignored_group_names {
    use super::*;

    #[test]
    fn ignored_group_name_not_offered() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("Studio.Episode.01.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.Episode.02.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.Episode.03.mp4"), "").unwrap();

        let config = Config {
            auto: false,
            create: true,
            debug: false,
            dryrun: true,
            include: Vec::new(),
            exclude: Vec::new(),
            ignored_group_names: vec!["episode".to_string()],
            ignored_group_parts: Vec::new(),
            min_group_size: 3,
            min_prefix_chars: 1,
            overwrite: false,
            prefix_ignores: Vec::new(),
            prefix_overrides: Vec::new(),
            recurse: false,
            verbose: false,
            unpack_directory_names: Vec::new(),
        };

        let dirmove = DirMove::new(root, config);
        let files_with_names = dirmove.collect_files_with_names().unwrap();
        let groups = dirmove.collect_all_prefix_groups(&files_with_names);

        // "Episode" should NOT be offered as a group name
        assert!(
            !groups.contains_key("Episode"),
            "Ignored group name 'Episode' should not be offered"
        );
        // But "Studio" should still be offered
        assert!(
            groups.contains_key("Studio"),
            "Non-ignored group name 'Studio' should still be offered"
        );
    }

    #[test]
    fn multiple_ignored_group_names() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("Studio.Video.Part.01.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.Video.Part.02.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.Video.Part.03.mp4"), "").unwrap();

        let config = Config {
            auto: false,
            create: true,
            debug: false,
            dryrun: true,
            include: Vec::new(),
            exclude: Vec::new(),
            ignored_group_names: vec!["video".to_string(), "part".to_string()],
            ignored_group_parts: Vec::new(),
            min_group_size: 3,
            min_prefix_chars: 1,
            overwrite: false,
            prefix_ignores: Vec::new(),
            prefix_overrides: Vec::new(),
            recurse: false,
            verbose: false,
            unpack_directory_names: Vec::new(),
        };

        let dirmove = DirMove::new(root, config);
        let files_with_names = dirmove.collect_files_with_names().unwrap();
        let groups = dirmove.collect_all_prefix_groups(&files_with_names);

        // Both "Video" and "Part" should NOT be offered
        assert!(
            !groups.contains_key("Video"),
            "Ignored group name 'Video' should not be offered"
        );
        assert!(
            !groups.contains_key("Part"),
            "Ignored group name 'Part' should not be offered"
        );
        // "Studio" should still be offered
        assert!(
            groups.contains_key("Studio"),
            "Non-ignored group name 'Studio' should still be offered"
        );
    }

    #[test]
    fn ignored_group_names_case_insensitive() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("Studio.EPISODE.01.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.Episode.02.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.episode.03.mp4"), "").unwrap();

        let config = Config {
            auto: false,
            create: true,
            debug: false,
            dryrun: true,
            include: Vec::new(),
            exclude: Vec::new(),
            ignored_group_names: vec!["episode".to_string()], // lowercase in config
            ignored_group_parts: Vec::new(),
            min_group_size: 3,
            min_prefix_chars: 1,
            overwrite: false,
            prefix_ignores: Vec::new(),
            prefix_overrides: Vec::new(),
            recurse: false,
            verbose: false,
            unpack_directory_names: Vec::new(),
        };

        let dirmove = DirMove::new(root, config);
        let files_with_names = dirmove.collect_files_with_names().unwrap();
        let groups = dirmove.collect_all_prefix_groups(&files_with_names);

        // All case variations of "Episode" should be filtered out
        assert!(
            !groups.contains_key("EPISODE"),
            "Ignored group name 'EPISODE' should not be offered"
        );
        assert!(
            !groups.contains_key("Episode"),
            "Ignored group name 'Episode' should not be offered"
        );
        assert!(
            !groups.contains_key("episode"),
            "Ignored group name 'episode' should not be offered"
        );
    }

    #[test]
    fn ignored_multi_part_group_name() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("Studio.Season.One.01.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.Season.One.02.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.Season.One.03.mp4"), "").unwrap();

        let config = Config {
            auto: false,
            create: true,
            debug: false,
            dryrun: true,
            include: Vec::new(),
            exclude: Vec::new(),
            ignored_group_names: vec!["seasonone".to_string()], // normalized form (no dots)
            ignored_group_parts: Vec::new(),
            min_group_size: 3,
            min_prefix_chars: 1,
            overwrite: false,
            prefix_ignores: Vec::new(),
            prefix_overrides: Vec::new(),
            recurse: false,
            verbose: false,
            unpack_directory_names: Vec::new(),
        };

        let dirmove = DirMove::new(root, config);
        let files_with_names = dirmove.collect_files_with_names().unwrap();
        let groups = dirmove.collect_all_prefix_groups(&files_with_names);

        // "Season.One" (normalized to "seasonone") should NOT be offered
        assert!(
            !groups.contains_key("Season.One"),
            "Ignored group name 'Season.One' should not be offered"
        );
        // But "Studio" should still be offered
        assert!(
            groups.contains_key("Studio"),
            "Non-ignored group name 'Studio' should still be offered"
        );
    }

    #[test]
    fn empty_ignored_list_allows_all() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("Studio.Episode.01.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.Episode.02.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.Episode.03.mp4"), "").unwrap();

        let config = Config {
            auto: false,
            create: true,
            debug: false,
            dryrun: true,
            include: Vec::new(),
            exclude: Vec::new(),
            ignored_group_names: Vec::new(), // empty list
            ignored_group_parts: Vec::new(),
            min_group_size: 3,
            min_prefix_chars: 1,
            overwrite: false,
            prefix_ignores: Vec::new(),
            prefix_overrides: Vec::new(),
            recurse: false,
            verbose: false,
            unpack_directory_names: Vec::new(),
        };

        let dirmove = DirMove::new(root, config);
        let files_with_names = dirmove.collect_files_with_names().unwrap();
        let groups = dirmove.collect_all_prefix_groups(&files_with_names);

        // Both "Studio" and "Episode" should be offered when no ignores
        assert!(
            groups.contains_key("Studio"),
            "'Studio' should be offered with empty ignore list"
        );
        assert!(
            groups.contains_key("Episode"),
            "'Episode' should be offered with empty ignore list"
        );
    }

    #[test]
    fn ignored_group_name_with_common_words() {
        // Test realistic scenario with common words to ignore
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("StudioName.Scene.01.Chapter.01.mp4"), "").unwrap();
        std::fs::write(root.join("StudioName.Scene.02.Chapter.02.mp4"), "").unwrap();
        std::fs::write(root.join("StudioName.Scene.03.Chapter.03.mp4"), "").unwrap();

        let config = Config {
            auto: false,
            create: true,
            debug: false,
            dryrun: true,
            include: Vec::new(),
            exclude: Vec::new(),
            ignored_group_names: vec!["scene".to_string(), "chapter".to_string()],
            ignored_group_parts: Vec::new(),
            min_group_size: 3,
            min_prefix_chars: 5,
            overwrite: false,
            prefix_ignores: Vec::new(),
            prefix_overrides: Vec::new(),
            recurse: false,
            verbose: false,
            unpack_directory_names: Vec::new(),
        };

        let dirmove = DirMove::new(root, config);
        let files_with_names = dirmove.collect_files_with_names().unwrap();
        let groups = dirmove.collect_all_prefix_groups(&files_with_names);

        // Common words "Scene" and "Chapter" should NOT be offered
        assert!(
            !groups.contains_key("Scene"),
            "Common word 'Scene' should not be offered"
        );
        assert!(
            !groups.contains_key("Chapter"),
            "Common word 'Chapter' should not be offered"
        );
        // But the actual studio name should still be offered
        assert!(
            groups.contains_key("StudioName"),
            "Actual studio name should still be offered"
        );
    }
}

#[cfg(test)]
mod test_ignored_group_parts {
    use super::*;

    fn make_config_with_ignored_parts(ignored_parts: Vec<&str>) -> Config {
        Config {
            auto: false,
            create: true,
            debug: false,
            dryrun: true,
            include: Vec::new(),
            exclude: Vec::new(),
            ignored_group_names: Vec::new(),
            ignored_group_parts: ignored_parts.into_iter().map(|s| s.to_lowercase()).collect(),
            min_group_size: 3,
            min_prefix_chars: 1,
            overwrite: false,
            prefix_ignores: Vec::new(),
            prefix_overrides: Vec::new(),
            recurse: false,
            verbose: false,
            unpack_directory_names: Vec::new(),
        }
    }

    #[test]
    fn filters_groups_containing_ignored_part() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // Files that would form a group "DL.x265.TEST" without filtering
        std::fs::write(root.join("Show.S01E01.1080p.DL.x265.TEST.mkv"), "").unwrap();
        std::fs::write(root.join("Show.S01E02.1080p.DL.x265.TEST.mkv"), "").unwrap();
        std::fs::write(root.join("Show.S01E03.1080p.DL.x265.TEST.mkv"), "").unwrap();

        let config = make_config_with_ignored_parts(vec!["x265"]);
        let dirmove = DirMove::new(root, config);
        let files_with_names = dirmove.collect_files_with_names().unwrap();
        let groups = dirmove.collect_all_prefix_groups(&files_with_names);

        // Groups containing "x265" as a part should be filtered out
        assert!(
            !groups.contains_key("DL.x265.TEST"),
            "Group containing ignored part 'x265' should not be offered"
        );
        assert!(
            !groups.contains_key("x265.TEST"),
            "Group containing ignored part 'x265' should not be offered"
        );
        assert!(
            !groups.contains_key("DL.x265"),
            "Group containing ignored part 'x265' should not be offered"
        );
        // But "Show" should still be offered
        assert!(groups.contains_key("Show"), "Show should still be offered as a group");
    }

    #[test]
    fn multiple_ignored_parts() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("Studio.Video.x265.HEVC.001.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.Video.x265.HEVC.002.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.Video.x265.HEVC.003.mp4"), "").unwrap();

        let config = make_config_with_ignored_parts(vec!["x265", "HEVC"]);
        let dirmove = DirMove::new(root, config);
        let files_with_names = dirmove.collect_files_with_names().unwrap();
        let groups = dirmove.collect_all_prefix_groups(&files_with_names);

        // Groups with x265 or HEVC should be filtered
        for key in groups.keys() {
            let key_lower = key.to_lowercase();
            assert!(
                !key_lower.contains("x265") && !key_lower.contains("hevc"),
                "Group '{}' should not contain ignored parts",
                key
            );
        }
        // Studio should still be available
        assert!(groups.contains_key("Studio"), "Studio should be offered");
    }

    #[test]
    fn case_insensitive_matching() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("Name.X265.Thing.001.mp4"), "").unwrap();
        std::fs::write(root.join("Name.x265.Thing.002.mp4"), "").unwrap();
        std::fs::write(root.join("Name.X265.Thing.003.mp4"), "").unwrap();

        let config = make_config_with_ignored_parts(vec!["x265"]); // lowercase in config
        let dirmove = DirMove::new(root, config);
        let files_with_names = dirmove.collect_files_with_names().unwrap();
        let groups = dirmove.collect_all_prefix_groups(&files_with_names);

        // Should filter both X265 and x265 variations
        for key in groups.keys() {
            let key_lower = key.to_lowercase();
            assert!(
                !key_lower.contains("x265"),
                "Group '{}' should not contain 'x265' (case-insensitive)",
                key
            );
        }
    }

    #[test]
    fn empty_ignored_parts_allows_all() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("DL.x265.TEST.Video.001.mp4"), "").unwrap();
        std::fs::write(root.join("DL.x265.TEST.Video.002.mp4"), "").unwrap();
        std::fs::write(root.join("DL.x265.TEST.Video.003.mp4"), "").unwrap();

        let config = make_config_with_ignored_parts(vec![]); // empty list
        let dirmove = DirMove::new(root, config);
        let files_with_names = dirmove.collect_files_with_names().unwrap();
        let groups = dirmove.collect_all_prefix_groups(&files_with_names);

        // With empty list, groups containing x265 should be allowed
        assert!(
            groups.keys().any(|k| k.to_lowercase().contains("x265")),
            "With empty ignored_parts, groups with 'x265' should be offered"
        );
    }

    #[test]
    fn does_not_filter_substring_matches() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // "x26" is in ignored_parts, but "x265" should NOT be filtered
        // because we match whole parts, not substrings
        std::fs::write(root.join("Studio.x265.Video.001.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.x265.Video.002.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.x265.Video.003.mp4"), "").unwrap();

        let config = make_config_with_ignored_parts(vec!["x26"]); // partial match
        let dirmove = DirMove::new(root, config);
        let files_with_names = dirmove.collect_files_with_names().unwrap();
        let groups = dirmove.collect_all_prefix_groups(&files_with_names);

        // "x265" should NOT be filtered because "x26" doesn't match the whole part
        assert!(
            groups.keys().any(|k| k.contains("x265")),
            "Partial match 'x26' should not filter 'x265' groups"
        );
    }
}

#[cfg(test)]
mod test_prefix_overrides {
    use super::test_helpers::*;
    use super::*;

    #[test]
    fn no_overrides() {
        let dirmove = make_test_dirmove(Vec::new());
        let mut groups: HashMap<String, (Vec<PathBuf>, usize, usize)> = HashMap::new();
        groups.insert("Some.Name.Thing".to_string(), (vec![PathBuf::from("file1.mp4")], 3, 0));

        let result = dirmove.apply_prefix_overrides(groups);
        assert!(result.contains_key("Some.Name.Thing"));
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn matching_override() {
        let dirmove = make_test_dirmove(vec!["longer.prefix".to_string()]);
        let mut groups: HashMap<String, (Vec<PathBuf>, usize, usize)> = HashMap::new();
        groups.insert(
            "longer.prefix.name".to_string(),
            (vec![PathBuf::from("longer.prefix.name.file.mp4")], 3, 0),
        );

        let result = dirmove.apply_prefix_overrides(groups);
        assert!(result.contains_key("longer.prefix"));
        assert!(!result.contains_key("longer.prefix.name"));
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn merges_groups() {
        let dirmove = make_test_dirmove(vec!["Some.Name".to_string()]);
        let mut groups: HashMap<String, (Vec<PathBuf>, usize, usize)> = HashMap::new();
        groups.insert("Some.Name.Thing".to_string(), (vec![PathBuf::from("file1.mp4")], 3, 0));
        groups.insert("Some.Name.Other".to_string(), (vec![PathBuf::from("file2.mp4")], 3, 0));

        let result = dirmove.apply_prefix_overrides(groups);
        assert!(result.contains_key("Some.Name"));
        assert_eq!(result.len(), 1);
        assert_eq!(result.get("Some.Name").map(|(files, _, _)| files.len()), Some(2));
    }

    #[test]
    fn non_matching() {
        let dirmove = make_test_dirmove(vec!["Other.Prefix".to_string()]);
        let mut groups: HashMap<String, (Vec<PathBuf>, usize, usize)> = HashMap::new();
        groups.insert("Some.Name.Thing".to_string(), (vec![PathBuf::from("file1.mp4")], 3, 0));

        let result = dirmove.apply_prefix_overrides(groups);
        assert!(result.contains_key("Some.Name.Thing"));
        assert!(!result.contains_key("Other.Prefix"));
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn partial_match_only() {
        let dirmove = make_test_dirmove(vec!["Some".to_string()]);
        let mut groups: HashMap<String, (Vec<PathBuf>, usize, usize)> = HashMap::new();
        groups.insert("Something.Else".to_string(), (vec![PathBuf::from("file1.mp4")], 1, 0));
        groups.insert("Some.Name".to_string(), (vec![PathBuf::from("file2.mp4")], 2, 0));

        let result = dirmove.apply_prefix_overrides(groups);
        assert!(result.contains_key("Some"));
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn override_more_specific_than_prefix() {
        let dirmove = make_test_dirmove(vec!["Example.Name".to_string()]);
        let mut groups: HashMap<String, (Vec<PathBuf>, usize, usize)> = HashMap::new();
        groups.insert(
            "Example".to_string(),
            (
                vec![
                    PathBuf::from("Example.Name.Video1.mp4"),
                    PathBuf::from("Example.Name.Video2.mp4"),
                    PathBuf::from("Example.Name.Video3.mp4"),
                ],
                1,
                0,
            ),
        );

        let result = dirmove.apply_prefix_overrides(groups);
        assert!(result.contains_key("Example.Name"));
        assert!(!result.contains_key("Example"));
        assert_eq!(result.len(), 1);
        assert_eq!(result.get("Example.Name").map(|(files, _, _)| files.len()), Some(3));
    }

    #[test]
    fn multiple_overrides() {
        let dirmove = make_test_dirmove(vec!["Show.A".to_string(), "Show.B".to_string()]);
        let mut groups: HashMap<String, (Vec<PathBuf>, usize, usize)> = HashMap::new();
        groups.insert("Show.A.Season1".to_string(), (vec![PathBuf::from("file1.mp4")], 3, 0));
        groups.insert("Show.B.Season1".to_string(), (vec![PathBuf::from("file2.mp4")], 3, 0));
        groups.insert("Show.C.Season1".to_string(), (vec![PathBuf::from("file3.mp4")], 3, 0));

        let result = dirmove.apply_prefix_overrides(groups);
        assert!(result.contains_key("Show.A"));
        assert!(result.contains_key("Show.B"));
        assert!(result.contains_key("Show.C.Season1"));
        assert_eq!(result.len(), 3);
    }

    #[test]
    fn empty_groups() {
        let dirmove = make_test_dirmove(vec!["Some".to_string()]);
        let groups: HashMap<String, (Vec<PathBuf>, usize, usize)> = HashMap::new();

        let result = dirmove.apply_prefix_overrides(groups);
        assert!(result.is_empty());
    }

    #[test]
    fn override_with_case_sensitivity() {
        let dirmove = make_test_dirmove(vec!["show.name".to_string()]);
        let mut groups: HashMap<String, (Vec<PathBuf>, usize, usize)> = HashMap::new();
        groups.insert(
            "Show.Name.Season1".to_string(),
            (vec![PathBuf::from("file1.mp4")], 3, 0),
        );

        let result = dirmove.apply_prefix_overrides(groups);
        assert!(result.contains_key("Show.Name.Season1"));
        assert_eq!(result.len(), 1);
    }
}

#[cfg(test)]
mod test_prefix_ignores {
    use super::test_helpers::*;

    #[test]
    fn no_ignores() {
        let dirmove = make_test_dirmove_with_ignores(Vec::new(), Vec::new());
        let result = dirmove.strip_ignored_prefixes("www example com test");
        assert_eq!(result, "www example com test");
    }

    #[test]
    fn single_ignore() {
        let dirmove = make_test_dirmove_with_ignores(Vec::new(), vec!["www".to_string()]);
        let result = dirmove.strip_ignored_prefixes("www example com test");
        assert_eq!(result, "example com test");
    }

    #[test]
    fn multiple_ignores() {
        let dirmove = make_test_dirmove_with_ignores(Vec::new(), vec!["www".to_string(), "example".to_string()]);
        let result = dirmove.strip_ignored_prefixes("www example com test");
        assert_eq!(result, "com test");
    }

    #[test]
    fn no_match() {
        let dirmove = make_test_dirmove_with_ignores(Vec::new(), vec!["xyz".to_string()]);
        let result = dirmove.strip_ignored_prefixes("www example com test");
        assert_eq!(result, "www example com test");
    }

    #[test]
    fn dot_prefix_single_ignore() {
        let dirmove = make_test_dirmove_with_ignores(Vec::new(), vec!["www".to_string()]);
        let result = dirmove.strip_ignored_dot_prefixes("www.example.com.test");
        assert_eq!(result, "example.com.test");
    }

    #[test]
    fn dot_prefix_multiple_ignores() {
        let dirmove = make_test_dirmove_with_ignores(Vec::new(), vec!["www".to_string(), "example".to_string()]);
        let result = dirmove.strip_ignored_dot_prefixes("www.example.com.test");
        assert_eq!(result, "com.test");
    }

    #[test]
    fn case_insensitive_ignore() {
        let dirmove = make_test_dirmove_with_ignores(Vec::new(), vec!["WWW".to_string()]);
        let result = dirmove.strip_ignored_dot_prefixes("www.example.com");
        assert_eq!(result, "example.com");
    }

    #[test]
    fn ignore_in_middle_not_stripped() {
        let dirmove = make_test_dirmove_with_ignores(Vec::new(), vec!["example".to_string()]);
        let result = dirmove.strip_ignored_dot_prefixes("www.example.com");
        assert_eq!(result, "www.example.com");
    }

    #[test]
    fn is_ignored_prefix_exact_match() {
        let dirmove = make_test_dirmove_with_ignores(Vec::new(), vec!["www".to_string()]);
        assert!(dirmove.is_ignored_prefix("www"));
        assert!(dirmove.is_ignored_prefix("WWW"));
        assert!(!dirmove.is_ignored_prefix("www2"));
    }

    #[test]
    fn repeated_prefix_in_name() {
        let dirmove = make_test_dirmove_with_ignores(Vec::new(), vec!["prefix".to_string()]);
        let result = dirmove.strip_ignored_dot_prefixes("prefix.prefix.name.mp4");
        assert_eq!(result, "name.mp4");
    }

    #[test]
    fn all_parts_are_ignored() {
        let dirmove = make_test_dirmove_with_ignores(Vec::new(), vec!["a".to_string(), "b".to_string()]);
        let result = dirmove.strip_ignored_dot_prefixes("a.b.c");
        assert_eq!(result, "c");
    }
}

#[cfg(test)]
mod test_directory_matching {
    use super::test_helpers::*;
    use super::*;

    #[test]
    fn basic_match() {
        let dirmove = make_test_dirmove(Vec::new());
        let dirs = make_test_dirs(&["Certain Name"]);
        let files = make_file_paths(&[
            "Something.else.Certain.Name.video.1.mp4",
            "Certain.Name.Example.video.2.mp4",
            "Another.Certain.Name.Example.video.3.mp4",
            "Another.Name.Example.video.3.mp4",
            "Cert.Name.Example.video.3.mp4",
        ]);
        let result = dirmove.match_files_to_directories(&files, &dirs);
        assert_eq!(result.len(), 1);
        assert_eq!(result.get(&0).unwrap().len(), 3);
    }

    #[test]
    fn no_matches() {
        let dirmove = make_test_dirmove(Vec::new());
        let dirs = make_test_dirs(&["Unknown Dir"]);
        let files = make_file_paths(&["Some.File.mp4", "Other.File.mp4"]);
        let result = dirmove.match_files_to_directories(&files, &dirs);
        assert!(result.is_empty());
    }

    #[test]
    fn multiple_dirs() {
        let dirmove = make_test_dirmove(Vec::new());
        let dirs = make_test_dirs(&["Show A", "Show B"]);
        let files = make_file_paths(&["Show.A.ep1.mp4", "Show.B.ep1.mp4", "Show.A.ep2.mp4"]);
        let result = dirmove.match_files_to_directories(&files, &dirs);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn case_insensitive() {
        let dirmove = make_test_dirmove(Vec::new());
        let dirs = make_test_dirs(&["Movie Name"]);
        let files = make_file_paths(&["MOVIE.NAME.part1.mp4", "movie.name.part2.mp4", "Movie.Name.part3.mp4"]);
        let result = dirmove.match_files_to_directories(&files, &dirs);
        assert_eq!(result.len(), 1);
        assert_eq!(result.get(&0).unwrap().len(), 3);
    }

    #[test]
    fn partial_match() {
        let dirmove = make_test_dirmove(Vec::new());
        let dirs = make_test_dirs(&["Show Name", "Show"]);
        let files = make_file_paths(&["Show.Name.ep1.mp4", "Show.Other.ep1.mp4"]);
        let result = dirmove.match_files_to_directories(&files, &dirs);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn longer_match_wins() {
        let dirmove = make_test_dirmove(Vec::new());
        let dirs = make_test_dirs(&["Show", "Show Name"]);
        let files = make_file_paths(&["Show.Name.Episode.mp4"]);
        let result = dirmove.match_files_to_directories(&files, &dirs);
        assert_eq!(result.len(), 1);
        assert!(result.contains_key(&1));
    }

    #[test]
    fn empty_files() {
        let dirmove = make_test_dirmove(Vec::new());
        let dirs = make_test_dirs(&["Show Name"]);
        let files: Vec<PathBuf> = Vec::new();
        let result = dirmove.match_files_to_directories(&files, &dirs);
        assert!(result.is_empty());
    }

    #[test]
    fn empty_dirs() {
        let dirmove = make_test_dirmove(Vec::new());
        let dirs: Vec<DirectoryInfo> = Vec::new();
        let files = make_file_paths(&["Show.Name.ep1.mp4"]);
        let result = dirmove.match_files_to_directories(&files, &dirs);
        assert!(result.is_empty());
    }

    #[test]
    fn dots_replaced_with_spaces() {
        let dirmove = make_test_dirmove(Vec::new());
        let dirs = make_test_dirs(&["my show name"]);
        let files = make_file_paths(&["my.show.name.ep1.mp4", "my.show.name.ep2.mp4"]);
        let result = dirmove.match_files_to_directories(&files, &dirs);
        assert_eq!(result.len(), 1);
        assert_eq!(result.get(&0).unwrap().len(), 2);
    }

    #[test]
    fn mixed_separators() {
        let dirmove = make_test_dirmove(Vec::new());
        let dirs = make_test_dirs(&["show name"]);
        let files = make_file_paths(&["Show.Name.ep1.mp4", "show_name_ep2.mp4", "Show-Name-ep3.mp4"]);
        let result = dirmove.match_files_to_directories(&files, &dirs);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn with_prefix_ignore() {
        let dirmove = make_test_dirmove_with_ignores(Vec::new(), vec!["www".to_string()]);
        let dirs = make_test_dirs(&["example"]);
        let files = make_file_paths(&["www.example.file.mp4"]);
        let result = dirmove.match_files_to_directories(&files, &dirs);
        assert_eq!(result.len(), 1);
        assert_eq!(result.get(&0).unwrap().len(), 1);
    }

    #[test]
    fn with_repeated_prefix_ignore() {
        let dirmove = make_test_dirmove_with_ignores(Vec::new(), vec!["prefix".to_string()]);
        let dirs = make_test_dirs(&["name"]);
        let files = make_file_paths(&["prefix.prefix.name.file.mp4"]);
        let result = dirmove.match_files_to_directories(&files, &dirs);
        assert_eq!(result.len(), 1);
        assert_eq!(result.get(&0).unwrap().len(), 1);
    }

    #[test]
    fn with_multiple_prefix_ignores() {
        let dirmove = make_test_dirmove_with_ignores(Vec::new(), vec!["www".to_string(), "ftp".to_string()]);
        let dirs = make_test_dirs(&["example"]);
        let files = make_file_paths(&["www.ftp.example.file.mp4", "ftp.example.other.mp4"]);
        let result = dirmove.match_files_to_directories(&files, &dirs);
        assert_eq!(result.len(), 1);
        assert_eq!(result.get(&0).unwrap().len(), 2);
    }

    #[test]
    fn prefix_ignore_applied_to_both() {
        let dirmove = make_test_dirmove_with_ignores(Vec::new(), vec!["release".to_string()]);
        let dirs = make_test_dirs(&["release show name"]);
        let files = make_file_paths(&["release.show.name.ep1.mp4"]);
        let result = dirmove.match_files_to_directories(&files, &dirs);
        assert_eq!(result.len(), 1);
        assert_eq!(result.get(&0).unwrap().len(), 1);
    }

    #[test]
    fn file_has_prefix_dir_does_not() {
        let dirmove = make_test_dirmove_with_ignores(Vec::new(), vec!["www".to_string()]);
        let dirs = make_test_dirs(&["example"]);
        let files = make_file_paths(&["www.example.file.mp4"]);
        let result = dirmove.match_files_to_directories(&files, &dirs);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn dir_has_prefix_file_does_not() {
        let dirmove = make_test_dirmove_with_ignores(Vec::new(), vec!["www".to_string()]);
        let dirs = make_test_dirs(&["www example"]);
        let files = make_file_paths(&["example.file.mp4"]);
        let result = dirmove.match_files_to_directories(&files, &dirs);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn special_characters_in_names() {
        let dirmove = make_test_dirmove(Vec::new());
        let dirs = make_test_dirs(&["show's name"]);
        let files = make_file_paths(&["show's.name.ep1.mp4"]);
        let result = dirmove.match_files_to_directories(&files, &dirs);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn concatenated_filename_matches_spaced_directory() {
        // Directory "Some Name" should match both "Some.Name." and "SomeName" files
        let dirmove = make_test_dirmove(Vec::new());
        let dirs = make_test_dirs(&["Some Name"]);
        let files = make_file_paths(&[
            "Some.Name.Episode.01.mp4",
            "SomeName.Episode.02.mp4",
            "SOMENAME.Episode.03.mp4",
            "somename.episode.04.mp4",
        ]);
        let result = dirmove.match_files_to_directories(&files, &dirs);
        assert_eq!(result.len(), 1);
        assert_eq!(
            result.get(&0).unwrap().len(),
            4,
            "All 4 files should match directory 'Some Name'"
        );
    }

    #[test]
    fn mixed_concatenated_and_dotted_filenames() {
        // Mix of naming styles should all match the same directory
        let dirmove = make_test_dirmove(Vec::new());
        let dirs = make_test_dirs(&["Photo Lab"]);
        let files = make_file_paths(&[
            "Photo.Lab.Image.01.jpg",
            "PhotoLab.Image.02.jpg",
            "Photolab.Image.03.jpg",
            "PHOTOLAB.Image.04.jpg",
            "photo.lab.image.05.jpg",
        ]);
        let result = dirmove.match_files_to_directories(&files, &dirs);
        assert_eq!(result.len(), 1);
        assert_eq!(
            result.get(&0).unwrap().len(),
            5,
            "All 5 files should match directory 'Photo Lab'"
        );
    }

    #[test]
    fn concatenated_directory_matches_dotted_files() {
        // Directory without spaces should match dot-separated files
        let dirmove = make_test_dirmove(Vec::new());
        let dirs = make_test_dirs(&["SomeName"]);
        let files = make_file_paths(&["Some.Name.Episode.01.mp4", "SomeName.Episode.02.mp4"]);
        let result = dirmove.match_files_to_directories(&files, &dirs);
        assert_eq!(result.len(), 1);
        assert_eq!(
            result.get(&0).unwrap().len(),
            2,
            "Both files should match directory 'SomeName'"
        );
    }

    #[test]
    fn concatenated_directory_matches_dotted_files_without_prefix() {
        // Directory "SomeName" (no spaces) should match files like "some.name.file.mp4"
        let dirmove = make_test_dirmove(Vec::new());
        let dirs = make_test_dirs(&["SomeName"]);
        let files = make_file_paths(&[
            "some.name.file.01.mp4",
            "Some.Name.file.02.mp4",
            "SOME.NAME.file.03.mp4",
        ]);
        let result = dirmove.match_files_to_directories(&files, &dirs);
        assert_eq!(result.len(), 1);
        assert_eq!(
            result.get(&0).unwrap().len(),
            3,
            "All 3 dot-separated files should match concatenated directory 'SomeName'"
        );
    }

    #[test]
    fn concatenated_directory_with_dotted_files_and_prefix() {
        // Directory "SomeName" should match files like "prefix.some.name.file.mp4"
        let dirmove = make_test_dirmove_with_ignores(Vec::new(), vec!["prefix".to_string()]);
        let dirs = make_test_dirs(&["SomeName"]);
        let files = make_file_paths(&[
            "prefix.some.name.file.01.mp4",
            "prefix.SomeName.file.02.mp4",
            "some.name.file.03.mp4",
            "SomeName.file.04.mp4",
        ]);
        let result = dirmove.match_files_to_directories(&files, &dirs);
        assert_eq!(result.len(), 1);
        assert_eq!(
            result.get(&0).unwrap().len(),
            4,
            "All 4 files should match directory 'SomeName'"
        );
    }

    #[test]
    fn numbers_in_directory_name() {
        let dirmove = make_test_dirmove(Vec::new());
        let dirs = make_test_dirs(&["2024 show", "show 2024"]);
        let files = make_file_paths(&["2024.show.ep1.mp4", "show.2024.ep1.mp4"]);
        let result = dirmove.match_files_to_directories(&files, &dirs);
        assert_eq!(result.len(), 2);
    }
}

#[cfg(test)]
mod test_full_flow {
    use super::test_helpers::*;
    use super::*;

    #[test]
    fn simulation_with_filtering() {
        let original_filenames = [
            "ShowName.2023.S01E01.720p.mp4",
            "ShowName.2024.S01E02.720p.mp4",
            "ShowName.2025.S01E03.720p.mp4",
            "OtherShow.2024.Special.mp4",
            "OtherShow.2024.Episode.mp4",
        ];

        let filtered = make_filtered_files(&original_filenames);

        assert_eq!(filtered[0].filtered_name, "ShowName.S01E01.mp4");
        assert_eq!(filtered[1].filtered_name, "ShowName.S01E02.mp4");
        assert_eq!(filtered[2].filtered_name, "ShowName.S01E03.mp4");
        assert_eq!(filtered[3].filtered_name, "OtherShow.Special.mp4");
        assert_eq!(filtered[4].filtered_name, "OtherShow.Episode.mp4");

        let show_candidates = utils::find_prefix_candidates(&filtered[0].filtered_name, &filtered, 3, 1);
        assert_eq!(show_candidates, vec![candidate("ShowName", 3, 1, 0)]);

        let other_candidates = utils::find_prefix_candidates(&filtered[3].filtered_name, &filtered, 2, 1);
        assert_eq!(other_candidates, vec![candidate("OtherShow", 2, 1, 0)]);
    }

    #[test]
    fn with_resolution_numbers() {
        let original_filenames = [
            "MovieName.2024.720.rip.mp4",
            "MovieName.2024.720.other.mp4",
            "MovieName.2024.720.more.mp4",
        ];

        let filtered = make_filtered_files(&original_filenames);

        assert_eq!(filtered[0].filtered_name, "MovieName.rip.mp4");
        assert_eq!(filtered[1].filtered_name, "MovieName.other.mp4");
        assert_eq!(filtered[2].filtered_name, "MovieName.more.mp4");

        let candidates = utils::find_prefix_candidates(&filtered[0].filtered_name, &filtered, 3, 1);
        assert_eq!(candidates, vec![candidate("MovieName", 3, 1, 0)]);
    }

    #[test]
    fn with_resolution_pattern() {
        let original_filenames = [
            "MovieName.2024.1080p.rip.mp4",
            "MovieName.2024.1080p.other.mp4",
            "MovieName.2024.1080p.more.mp4",
        ];

        let filtered = make_filtered_files(&original_filenames);

        assert_eq!(filtered[0].filtered_name, "MovieName.rip.mp4");
        assert_eq!(filtered[1].filtered_name, "MovieName.other.mp4");
        assert_eq!(filtered[2].filtered_name, "MovieName.more.mp4");

        let candidates = utils::find_prefix_candidates(&filtered[0].filtered_name, &filtered, 3, 1);
        assert_eq!(candidates, vec![candidate("MovieName", 3, 1, 0)]);
    }

    #[test]
    fn with_glue_words() {
        let original_filenames = [
            "Show.and.Tell.part1.mp4",
            "Show.and.Tell.part2.mp4",
            "Show.and.Tell.part3.mp4",
        ];

        let filtered = make_filtered_files(&original_filenames);

        assert_eq!(filtered[0].filtered_name, "Show.Tell.part1.mp4");
        assert_eq!(filtered[1].filtered_name, "Show.Tell.part2.mp4");
        assert_eq!(filtered[2].filtered_name, "Show.Tell.part3.mp4");

        let candidates = utils::find_prefix_candidates(&filtered[0].filtered_name, &filtered, 3, 1);
        // Note: Show.Tell won't match because "Show" and "Tell" are not contiguous in original
        // (they're separated by "and")
        // With position-agnostic matching, "Tell" is also found as a candidate
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0], candidate("Show", 3, 1, 0));
        assert_eq!(candidates[1], candidate("Tell", 3, 1, 1));
    }

    #[test]
    fn short_prefix_with_shared_parts() {
        let original_filenames = [
            "ABC.2023.Thing.v1.mp4",
            "ABC.2024.Thing.v2.mp4",
            "ABC.2025.Thing.v3.mp4",
        ];

        let unfiltered = make_test_files(&original_filenames);

        let candidates_unfiltered = utils::find_prefix_candidates(&unfiltered[0].filtered_name, &unfiltered, 3, 1);
        // With position-agnostic matching, "Thing" is also found
        assert_eq!(candidates_unfiltered.len(), 2);
        assert!(
            candidates_unfiltered
                .iter()
                .any(|c| c.prefix == "ABC" && c.match_count == 3)
        );
        assert!(
            candidates_unfiltered
                .iter()
                .any(|c| c.prefix == "Thing" && c.match_count == 3)
        );

        let filtered = make_filtered_files(&original_filenames);

        assert_eq!(filtered[0].filtered_name, "ABC.Thing.v1.mp4");
        assert_eq!(filtered[1].filtered_name, "ABC.Thing.v2.mp4");
        assert_eq!(filtered[2].filtered_name, "ABC.Thing.v3.mp4");

        let candidates = utils::find_prefix_candidates(&filtered[0].filtered_name, &filtered, 3, 1);
        // Note: ABC.Thing won't match because "ABC" and "Thing" are not contiguous in original
        // (they're separated by year like "2023")
        // With position-agnostic matching, "Thing" is also found as a candidate
        assert_eq!(candidates.len(), 2);
        assert!(candidates.iter().any(|c| c.prefix == "ABC" && c.match_count == 3));
        assert!(candidates.iter().any(|c| c.prefix == "Thing" && c.match_count == 3));
    }

    #[test]
    fn files_with_resolution_grouped_correctly() {
        let original_filenames = [
            "Some.Video.1080p.part1.mp4",
            "Some.Video.1080p.part2.mp4",
            "Some.Video.1080p.part3.mp4",
        ];

        let filtered = make_filtered_files(&original_filenames);

        assert_eq!(filtered[0].filtered_name, "Some.Video.part1.mp4");

        let candidates = utils::find_prefix_candidates(&filtered[0].filtered_name, &filtered, 3, 1);
        // With position-agnostic matching, "Video" is also found
        assert!(
            candidates
                .iter()
                .any(|c| c.prefix == "Some.Video" && c.match_count == 3)
        );
        assert!(candidates.iter().any(|c| c.prefix == "Some" && c.match_count == 3));
        assert!(candidates.iter().any(|c| c.prefix == "Video" && c.match_count == 3));

        let some_video = candidates.iter().find(|c| c.prefix == "Some.Video").unwrap();
        let dir_name = some_video.prefix.replace('.', " ");
        assert_eq!(dir_name, "Some Video");
    }

    #[test]
    fn files_with_2160p_resolution() {
        let original_filenames = [
            "Movie.Name.2160p.file1.mp4",
            "Movie.Name.2160p.file2.mp4",
            "Movie.Name.2160p.file3.mp4",
        ];

        let filtered = make_filtered_files(&original_filenames);

        assert_eq!(filtered[0].filtered_name, "Movie.Name.file1.mp4");

        let candidates = utils::find_prefix_candidates(&filtered[0].filtered_name, &filtered, 3, 1);
        // With position-agnostic matching, "Movie" and "Name" are also found
        assert!(
            candidates
                .iter()
                .any(|c| c.prefix == "Movie.Name" && c.match_count == 3)
        );
        assert!(candidates.iter().any(|c| c.prefix == "Movie" && c.match_count == 3));

        let movie_name = candidates.iter().find(|c| c.prefix == "Movie.Name").unwrap();
        let dir_name = movie_name.prefix.replace('.', " ");
        assert_eq!(dir_name, "Movie Name");
    }

    #[test]
    fn files_with_dimension_resolution() {
        let original_filenames = [
            "Cool.Stuff.1920x1080.part1.mp4",
            "Cool.Stuff.1920x1080.part2.mp4",
            "Cool.Stuff.1920x1080.part3.mp4",
        ];

        let filtered = make_filtered_files(&original_filenames);

        assert_eq!(filtered[0].filtered_name, "Cool.Stuff.part1.mp4");

        let candidates = utils::find_prefix_candidates(&filtered[0].filtered_name, &filtered, 3, 1);
        // With position-agnostic matching, "Stuff" is also found
        assert!(
            candidates
                .iter()
                .any(|c| c.prefix == "Cool.Stuff" && c.match_count == 3)
        );
        assert!(candidates.iter().any(|c| c.prefix == "Cool" && c.match_count == 3));
        assert!(candidates.iter().any(|c| c.prefix == "Stuff" && c.match_count == 3));

        let cool_stuff = candidates.iter().find(|c| c.prefix == "Cool.Stuff").unwrap();
        let dir_name = cool_stuff.prefix.replace('.', " ");
        assert_eq!(dir_name, "Cool Stuff");
    }
}

#[cfg(test)]
mod test_unpack {
    use super::test_helpers::*;
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn basic_preserves_structure_and_removes_matched_dir() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Example");
        std::fs::create_dir_all(&root)?;

        let videos = root.join("Videos");
        write_file(&videos.join("Name").join("file2.txt"), "file2")?;
        write_file(&videos.join("file1.txt"), "file1")?;

        let root_for_asserts = root.clone();

        let dirmove = DirMove {
            root,
            config: make_unpack_config(vec!["videos"], true, false, false),
        };

        dirmove.unpack_directories()?;

        assert_exists(&root_for_asserts.join("Name").join("file2.txt"));
        assert_exists(&root_for_asserts.join("file1.txt"));
        assert_not_exists(&root_for_asserts.join("Videos"));

        Ok(())
    }

    #[test]
    fn case_insensitive_dirname_match() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Example");
        std::fs::create_dir_all(&root)?;

        let videos = root.join("ViDeOs");
        write_file(&videos.join("Nested").join("file.txt"), "x")?;

        let root_for_asserts = root.clone();

        let dirmove = DirMove {
            root,
            config: make_unpack_config(vec!["videos"], true, false, false),
        };

        dirmove.unpack_directories()?;

        assert_exists(&root_for_asserts.join("Nested").join("file.txt"));
        assert_not_exists(&root_for_asserts.join("ViDeOs"));

        Ok(())
    }

    #[test]
    fn does_not_prune_unrelated_empty_dirs() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Example");
        std::fs::create_dir_all(&root)?;

        let unrelated_empty = root.join("EmptyUnrelated");
        std::fs::create_dir_all(&unrelated_empty)?;

        let matched_empty = root.join("Videos");
        std::fs::create_dir_all(&matched_empty)?;

        let root_for_asserts = root.clone();

        let dirmove = DirMove {
            root,
            config: make_unpack_config(vec!["videos"], false, false, false),
        };

        dirmove.unpack_directories()?;

        assert_exists(&root_for_asserts.join("EmptyUnrelated"));
        assert_not_exists(&root_for_asserts.join("Videos"));

        Ok(())
    }

    #[test]
    fn moves_non_matching_dirs_directly() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Example");
        std::fs::create_dir_all(&root)?;

        let videos = root.join("Videos");
        let subdir = videos.join("SubDir");
        write_file(&subdir.join("nested").join("deep.txt"), "deep content")?;
        write_file(&subdir.join("file.txt"), "file content")?;

        let root_for_asserts = root.clone();

        let dirmove = DirMove {
            root,
            config: make_unpack_config(vec!["videos"], true, false, false),
        };

        dirmove.unpack_directories()?;

        assert_exists(&root_for_asserts.join("SubDir").join("nested").join("deep.txt"));
        assert_exists(&root_for_asserts.join("SubDir").join("file.txt"));
        assert_not_exists(&root_for_asserts.join("Videos"));

        Ok(())
    }

    #[test]
    fn dryrun_does_not_modify_files() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Example");
        std::fs::create_dir_all(&root)?;

        let videos = root.join("Videos");
        write_file(&videos.join("file1.txt"), "content1")?;
        write_file(&videos.join("SubDir").join("file2.txt"), "content2")?;

        let root_for_asserts = root.clone();

        let dirmove = DirMove {
            root,
            config: make_unpack_config(vec!["videos"], true, true, false),
        };

        dirmove.unpack_directories()?;

        assert_exists(&root_for_asserts.join("Videos").join("file1.txt"));
        assert_exists(&root_for_asserts.join("Videos").join("SubDir").join("file2.txt"));
        assert_not_exists(&root_for_asserts.join("file1.txt"));
        assert_not_exists(&root_for_asserts.join("SubDir"));

        Ok(())
    }

    #[test]
    fn nested_matching_dirs_are_flattened() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Example");
        std::fs::create_dir_all(&root)?;

        let outer_videos = root.join("Videos");
        let inner_videos = outer_videos.join("Videos");
        write_file(&inner_videos.join("file.txt"), "nested content")?;

        let root_for_asserts = root.clone();

        let dirmove = DirMove {
            root,
            config: make_unpack_config(vec!["videos"], true, false, false),
        };

        dirmove.unpack_directories()?;

        assert_exists(&root_for_asserts.join("file.txt"));
        assert_not_exists(&root_for_asserts.join("Videos"));

        Ok(())
    }

    #[test]
    fn multiple_unpack_names() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Example");
        std::fs::create_dir_all(&root)?;

        let videos = root.join("Videos");
        let extras = root.join("Extras");
        write_file(&videos.join("video.mp4"), "video")?;
        write_file(&extras.join("extra.txt"), "extra")?;

        let root_for_asserts = root.clone();

        let dirmove = DirMove {
            root,
            config: make_unpack_config(vec!["videos", "extras"], true, false, false),
        };

        dirmove.unpack_directories()?;

        assert_exists(&root_for_asserts.join("video.mp4"));
        assert_exists(&root_for_asserts.join("extra.txt"));
        assert_not_exists(&root_for_asserts.join("Videos"));
        assert_not_exists(&root_for_asserts.join("Extras"));

        Ok(())
    }

    #[test]
    fn skips_existing_file_without_overwrite() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Example");
        std::fs::create_dir_all(&root)?;

        let videos = root.join("Videos");
        write_file(&videos.join("conflict.txt"), "from videos")?;
        write_file(&root.join("conflict.txt"), "original")?;

        let root_for_asserts = root.clone();

        let dirmove = DirMove {
            root,
            config: make_unpack_config(vec!["videos"], true, false, false),
        };

        dirmove.unpack_directories()?;

        let content = std::fs::read_to_string(root_for_asserts.join("conflict.txt"))?;
        assert_eq!(content, "original");

        Ok(())
    }

    #[test]
    fn overwrites_existing_file_with_overwrite() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Example");
        std::fs::create_dir_all(&root)?;

        let videos = root.join("Videos");
        write_file(&videos.join("conflict.txt"), "from videos")?;
        write_file(&root.join("conflict.txt"), "original")?;

        let root_for_asserts = root.clone();

        let dirmove = DirMove {
            root,
            config: make_unpack_config(vec!["videos"], true, false, true),
        };

        dirmove.unpack_directories()?;

        let content = std::fs::read_to_string(root_for_asserts.join("conflict.txt"))?;
        assert_eq!(content, "from videos");

        Ok(())
    }

    #[test]
    fn skips_existing_directory_without_overwrite() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Example");
        std::fs::create_dir_all(&root)?;

        let videos = root.join("Videos");
        write_file(&videos.join("SubDir").join("new.txt"), "new content")?;
        write_file(&root.join("SubDir").join("existing.txt"), "existing content")?;

        let root_for_asserts = root.clone();

        let dirmove = DirMove {
            root,
            config: make_unpack_config(vec!["videos"], true, false, false),
        };

        dirmove.unpack_directories()?;

        assert_exists(&root_for_asserts.join("SubDir").join("existing.txt"));
        assert_not_exists(&root_for_asserts.join("SubDir").join("new.txt"));

        Ok(())
    }

    #[test]
    fn non_recursive_only_checks_root_level() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Example");
        std::fs::create_dir_all(&root)?;

        let nested = root.join("Parent").join("Videos");
        write_file(&nested.join("file.txt"), "nested")?;

        let root_videos = root.join("Videos");
        write_file(&root_videos.join("root_file.txt"), "root")?;

        let root_for_asserts = root.clone();

        let dirmove = DirMove {
            root,
            config: make_unpack_config(vec!["videos"], false, false, false),
        };

        dirmove.unpack_directories()?;

        assert_exists(&root_for_asserts.join("root_file.txt"));
        assert_not_exists(&root_for_asserts.join("Videos"));
        assert_exists(&root_for_asserts.join("Parent").join("Videos").join("file.txt"));

        Ok(())
    }

    #[test]
    fn deeply_nested_structure() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Example");
        std::fs::create_dir_all(&root)?;

        let videos = root.join("Videos");
        write_file(&videos.join("A").join("B").join("C").join("deep.txt"), "deep")?;
        write_file(&videos.join("A").join("shallow.txt"), "shallow")?;

        let root_for_asserts = root.clone();

        let dirmove = DirMove {
            root,
            config: make_unpack_config(vec!["videos"], true, false, false),
        };

        dirmove.unpack_directories()?;

        assert_exists(&root_for_asserts.join("A").join("B").join("C").join("deep.txt"));
        assert_exists(&root_for_asserts.join("A").join("shallow.txt"));
        assert_not_exists(&root_for_asserts.join("Videos"));

        Ok(())
    }

    #[test]
    fn mixed_files_and_directories() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Example");
        std::fs::create_dir_all(&root)?;

        let videos = root.join("Videos");
        write_file(&videos.join("file1.txt"), "file1")?;
        write_file(&videos.join("file2.txt"), "file2")?;
        write_file(&videos.join("Dir1").join("nested1.txt"), "nested1")?;
        write_file(&videos.join("Dir2").join("nested2.txt"), "nested2")?;

        let root_for_asserts = root.clone();

        let dirmove = DirMove {
            root,
            config: make_unpack_config(vec!["videos"], true, false, false),
        };

        dirmove.unpack_directories()?;

        assert_exists(&root_for_asserts.join("file1.txt"));
        assert_exists(&root_for_asserts.join("file2.txt"));
        assert_exists(&root_for_asserts.join("Dir1").join("nested1.txt"));
        assert_exists(&root_for_asserts.join("Dir2").join("nested2.txt"));
        assert_not_exists(&root_for_asserts.join("Videos"));

        Ok(())
    }

    #[test]
    fn empty_unpack_names_does_nothing() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Example");
        std::fs::create_dir_all(&root)?;

        let videos = root.join("Videos");
        write_file(&videos.join("file.txt"), "content")?;

        let root_for_asserts = root.clone();

        let dirmove = DirMove {
            root,
            config: make_unpack_config(vec![], true, false, false),
        };

        dirmove.unpack_directories()?;

        assert_exists(&root_for_asserts.join("Videos").join("file.txt"));
        assert_not_exists(&root_for_asserts.join("file.txt"));

        Ok(())
    }

    #[test]
    fn no_matching_directories() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Example");
        std::fs::create_dir_all(&root)?;

        let other = root.join("Other");
        write_file(&other.join("file.txt"), "content")?;

        let root_for_asserts = root.clone();

        let dirmove = DirMove {
            root,
            config: make_unpack_config(vec!["videos"], true, false, false),
        };

        dirmove.unpack_directories()?;

        assert_exists(&root_for_asserts.join("Other").join("file.txt"));

        Ok(())
    }

    #[test]
    fn multiple_matching_dirs_at_different_levels() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Example");
        std::fs::create_dir_all(&root)?;

        let root_videos = root.join("Videos");
        write_file(&root_videos.join("file1.txt"), "file1")?;

        let nested_videos = root.join("Parent").join("Videos");
        write_file(&nested_videos.join("file2.txt"), "file2")?;

        let root_for_asserts = root.clone();

        let dirmove = DirMove {
            root,
            config: make_unpack_config(vec!["videos"], true, false, false),
        };

        dirmove.unpack_directories()?;

        assert_exists(&root_for_asserts.join("file1.txt"));
        assert_exists(&root_for_asserts.join("Parent").join("file2.txt"));
        assert_not_exists(&root_for_asserts.join("Videos"));
        assert_not_exists(&root_for_asserts.join("Parent").join("Videos"));

        Ok(())
    }

    #[test]
    fn preserves_file_content() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Example");
        std::fs::create_dir_all(&root)?;

        let videos = root.join("Videos");
        let original_content = "This is the original file content with special chars: äöü 日本語";
        write_file(&videos.join("content.txt"), original_content)?;

        let root_for_asserts = root.clone();

        let dirmove = DirMove {
            root,
            config: make_unpack_config(vec!["videos"], true, false, false),
        };

        dirmove.unpack_directories()?;

        let moved_content = std::fs::read_to_string(root_for_asserts.join("content.txt"))?;
        assert_eq!(moved_content, original_content);

        Ok(())
    }

    #[test]
    fn handles_special_characters_in_names() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Example");
        std::fs::create_dir_all(&root)?;

        let videos = root.join("Videos");
        write_file(&videos.join("file with spaces.txt"), "spaces")?;
        write_file(&videos.join("file-with-dashes.txt"), "dashes")?;
        write_file(&videos.join("file_with_underscores.txt"), "underscores")?;
        write_file(&videos.join("Dir With Spaces").join("nested.txt"), "nested")?;

        let root_for_asserts = root.clone();

        let dirmove = DirMove {
            root,
            config: make_unpack_config(vec!["videos"], true, false, false),
        };

        dirmove.unpack_directories()?;

        assert_exists(&root_for_asserts.join("file with spaces.txt"));
        assert_exists(&root_for_asserts.join("file-with-dashes.txt"));
        assert_exists(&root_for_asserts.join("file_with_underscores.txt"));
        assert_exists(&root_for_asserts.join("Dir With Spaces").join("nested.txt"));
        assert_not_exists(&root_for_asserts.join("Videos"));

        Ok(())
    }

    #[test]
    fn alternating_match_non_match_dirs() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Example");
        std::fs::create_dir_all(&root)?;

        let path = root.join("Videos").join("Other").join("Videos");
        write_file(&path.join("file.txt"), "deep")?;

        let root_for_asserts = root.clone();

        let dirmove = DirMove {
            root,
            config: make_unpack_config(vec!["videos"], true, false, false),
        };

        dirmove.unpack_directories()?;

        assert_exists(&root_for_asserts.join("Other").join("file.txt"));
        assert_not_exists(&root_for_asserts.join("Videos"));
        assert_not_exists(&root_for_asserts.join("Other").join("Videos"));

        Ok(())
    }

    #[test]
    fn collect_info_counts_correctly() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Example");
        std::fs::create_dir_all(&root)?;

        let videos = root.join("Videos");
        write_file(&videos.join("file1.txt"), "1")?;
        write_file(&videos.join("file2.txt"), "2")?;
        write_file(&videos.join("file3.txt"), "3")?;
        write_file(&videos.join("Dir1").join("nested.txt"), "nested")?;
        write_file(&videos.join("Dir2").join("nested.txt"), "nested")?;

        let dirmove = DirMove {
            root: root.clone(),
            config: make_unpack_config(vec!["videos"], true, false, false),
        };

        let info = dirmove.collect_unpack_info(&videos, &root);

        assert_eq!(info.file_moves.len(), 3);
        assert_eq!(info.directory_moves.len(), 2);

        Ok(())
    }

    #[test]
    fn dryrun_preserves_empty_matched_directory() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Example");
        std::fs::create_dir_all(&root)?;

        let videos = root.join("Videos");
        std::fs::create_dir_all(&videos)?;

        let root_for_asserts = root.clone();

        let dirmove = DirMove {
            root,
            config: make_unpack_config(vec!["videos"], true, true, false),
        };

        dirmove.unpack_directories()?;

        assert_exists(&root_for_asserts.join("Videos"));

        Ok(())
    }

    #[test]
    fn nested_chain_single_summary() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Project");
        std::fs::create_dir_all(&root)?;

        let deep_path = root.join("updates").join("1").join("videos");
        write_file(&deep_path.join("file.txt"), "content")?;
        write_file(&deep_path.join("another.txt"), "more content")?;

        let root_for_asserts = root.clone();

        let dirmove = DirMove {
            root,
            config: make_unpack_config(vec!["updates", "1", "videos"], true, false, false),
        };

        dirmove.unpack_directories()?;

        assert_exists(&root_for_asserts.join("file.txt"));
        assert_exists(&root_for_asserts.join("another.txt"));
        assert_not_exists(&root_for_asserts.join("updates"));

        Ok(())
    }

    #[test]
    fn nested_chain_with_non_matching_dir_between() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Project");
        std::fs::create_dir_all(&root)?;

        let path = root.join("updates").join("KeepThis").join("videos");
        write_file(&path.join("file.txt"), "content")?;

        let root_for_asserts = root.clone();

        let dirmove = DirMove {
            root,
            config: make_unpack_config(vec!["updates", "videos"], true, false, false),
        };

        dirmove.unpack_directories()?;

        assert_exists(&root_for_asserts.join("KeepThis").join("file.txt"));
        assert_not_exists(&root_for_asserts.join("updates"));
        assert_not_exists(&root_for_asserts.join("KeepThis").join("videos"));

        Ok(())
    }

    #[test]
    fn nested_chain_multiple_non_matching_dirs() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Root");
        std::fs::create_dir_all(&root)?;

        let path = root
            .join("updates")
            .join("A")
            .join("videos")
            .join("B")
            .join("downloads");
        write_file(&path.join("file.txt"), "content")?;

        let root_for_asserts = root.clone();

        let dirmove = DirMove {
            root,
            config: make_unpack_config(vec!["updates", "videos", "downloads"], true, false, false),
        };

        dirmove.unpack_directories()?;

        assert_exists(&root_for_asserts.join("A").join("B").join("file.txt"));
        assert_not_exists(&root_for_asserts.join("updates"));

        Ok(())
    }

    #[test]
    fn collect_candidates_filters_nested() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Test");
        std::fs::create_dir_all(&root)?;

        let path = root.join("videos").join("updates").join("downloads");
        std::fs::create_dir_all(&path)?;
        write_file(&path.join("file.txt"), "x")?;

        let dirmove = DirMove {
            root: root.clone(),
            config: make_unpack_config(vec!["videos", "updates", "downloads"], true, false, false),
        };

        let (_, candidates) = dirmove.collect_unwanted_and_unpack_candidates();

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0], root.join("videos"));

        Ok(())
    }

    #[test]
    fn separate_trees_get_separate_processing() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Root");
        std::fs::create_dir_all(&root)?;

        write_file(&root.join("DirA").join("videos").join("file1.txt"), "1")?;
        write_file(&root.join("DirB").join("videos").join("file2.txt"), "2")?;

        let root_for_asserts = root.clone();

        let dirmove = DirMove {
            root,
            config: make_unpack_config(vec!["videos"], true, false, false),
        };

        dirmove.unpack_directories()?;

        assert_exists(&root_for_asserts.join("DirA").join("file1.txt"));
        assert_exists(&root_for_asserts.join("DirB").join("file2.txt"));
        assert_not_exists(&root_for_asserts.join("DirA").join("videos"));
        assert_not_exists(&root_for_asserts.join("DirB").join("videos"));

        Ok(())
    }

    #[test]
    fn chain_with_files_at_multiple_levels() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Root");
        std::fs::create_dir_all(&root)?;

        let updates = root.join("updates");
        write_file(&updates.join("file1.txt"), "1")?;
        write_file(&updates.join("videos").join("file2.txt"), "2")?;
        write_file(&updates.join("videos").join("downloads").join("file3.txt"), "3")?;

        let root_for_asserts = root.clone();

        let dirmove = DirMove {
            root,
            config: make_unpack_config(vec!["updates", "videos", "downloads"], true, false, false),
        };

        dirmove.unpack_directories()?;

        assert_exists(&root_for_asserts.join("file1.txt"));
        assert_exists(&root_for_asserts.join("file2.txt"));
        assert_exists(&root_for_asserts.join("file3.txt"));
        assert_not_exists(&root_for_asserts.join("updates"));

        Ok(())
    }

    #[test]
    fn chain_with_non_matching_subdirs_moved_directly() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Root");
        std::fs::create_dir_all(&root)?;

        let keep_me = root.join("updates").join("KeepMe");
        write_file(&keep_me.join("deep").join("file.txt"), "content")?;

        let root_for_asserts = root.clone();

        let dirmove = DirMove {
            root,
            config: make_unpack_config(vec!["updates"], true, false, false),
        };

        dirmove.unpack_directories()?;

        assert_exists(&root_for_asserts.join("KeepMe").join("deep").join("file.txt"));
        assert_not_exists(&root_for_asserts.join("updates"));

        Ok(())
    }

    #[test]
    fn contains_unpack_directory_helper() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Root");
        std::fs::create_dir_all(&root)?;

        let videos = root.join("A").join("B").join("videos");
        write_file(&videos.join("file.txt"), "x")?;
        write_file(&root.join("C").join("nothing_special").join("file.txt"), "y")?;

        let dirmove = DirMove {
            root: root.clone(),
            config: make_unpack_config(vec!["videos"], true, false, false),
        };

        assert!(dirmove.contains_unpack_directory(&root.join("A")));
        assert!(dirmove.contains_unpack_directory(&root.join("A").join("B")));
        assert!(!dirmove.contains_unpack_directory(&root.join("C")));

        Ok(())
    }
}

#[cfg(test)]
mod test_unwanted_directories {
    use super::test_helpers::*;
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn deleted() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Example");
        std::fs::create_dir_all(&root)?;
        let unwanted = root.join(".unwanted");
        std::fs::create_dir(&unwanted)?;
        write_file(&unwanted.join("junk.txt"), "junk")?;

        let dirmove = DirMove {
            root,
            config: make_unpack_config(vec![], false, false, false),
        };

        dirmove.run()?;

        assert_not_exists(&unwanted);
        Ok(())
    }

    #[test]
    fn deleted_recursive() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Example");
        std::fs::create_dir_all(&root)?;
        let subdir = root.join("subdir");
        std::fs::create_dir(&subdir)?;
        let unwanted = subdir.join(".unwanted");
        std::fs::create_dir(&unwanted)?;
        write_file(&unwanted.join("junk.txt"), "junk")?;

        let dirmove = DirMove {
            root,
            config: make_unpack_config(vec![], true, false, false),
        };

        dirmove.run()?;

        assert_not_exists(&unwanted);
        assert_exists(&subdir);
        Ok(())
    }

    #[test]
    fn dryrun_preserves() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Example");
        std::fs::create_dir_all(&root)?;
        let unwanted = root.join(".unwanted");
        std::fs::create_dir(&unwanted)?;
        write_file(&unwanted.join("junk.txt"), "junk")?;

        let dirmove = DirMove {
            root,
            config: make_unpack_config(vec![], false, true, false),
        };

        dirmove.run()?;

        assert_exists(&unwanted);
        Ok(())
    }

    #[test]
    fn case_insensitive() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Example");
        std::fs::create_dir_all(&root)?;
        let unwanted = root.join(".Unwanted");
        std::fs::create_dir(&unwanted)?;
        write_file(&unwanted.join("junk.txt"), "junk")?;

        let dirmove = DirMove {
            root,
            config: make_unpack_config(vec![], false, false, false),
        };

        dirmove.run()?;

        assert_not_exists(&unwanted);
        Ok(())
    }

    #[test]
    fn skipped_in_collect_directories() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Example");
        std::fs::create_dir_all(&root)?;
        let unwanted = root.join(".unwanted");
        std::fs::create_dir(&unwanted)?;
        let normal = root.join("normal");
        std::fs::create_dir(&normal)?;

        let dirmove = DirMove {
            root,
            config: make_unpack_config(vec![], false, true, false),
        };

        let dirs = dirmove.collect_directories_in_root()?;

        assert_eq!(dirs.len(), 1);
        assert_eq!(dirs[0].path, normal);
        Ok(())
    }

    #[test]
    fn collect_unwanted_and_unpack_combined() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Example");
        std::fs::create_dir_all(&root)?;
        let unwanted = root.join(".unwanted");
        std::fs::create_dir(&unwanted)?;
        let videos = root.join("videos");
        std::fs::create_dir(&videos)?;
        write_file(&videos.join("file.txt"), "x")?;

        let dirmove = DirMove {
            root,
            config: make_unpack_config(vec!["videos"], false, false, false),
        };

        let (unwanted_dirs, unpack_candidates) = dirmove.collect_unwanted_and_unpack_candidates();

        assert_eq!(unwanted_dirs.len(), 1);
        assert_eq!(unwanted_dirs[0], unwanted);
        assert_eq!(unpack_candidates.len(), 1);
        assert_eq!(unpack_candidates[0], videos);
        Ok(())
    }
}

/// Comprehensive integration tests with realistic mixed data.
/// These tests verify grouping behavior with:
/// - Multiple potential groups in the same dataset
/// - Non-matching files that should be ignored
/// - Various prefix lengths and naming conventions
/// - Both dotted and concatenated naming styles
#[cfg(test)]
mod test_realistic_grouping {
    use super::*;

    /// Helper to create a test config with specified min group size
    fn make_grouping_config(min_group_size: usize) -> Config {
        Config {
            auto: false,
            create: true,
            debug: false,
            dryrun: true,
            include: Vec::new(),
            exclude: Vec::new(),
            ignored_group_names: Vec::new(),
            ignored_group_parts: Vec::new(),
            min_group_size,
            min_prefix_chars: 1,
            overwrite: false,
            prefix_ignores: Vec::new(),
            prefix_overrides: Vec::new(),
            recurse: false,
            verbose: false,
            unpack_directory_names: Vec::new(),
        }
    }

    /// Helper to create config with prefix ignores
    fn make_config_with_ignores(min_group_size: usize, ignores: Vec<String>) -> Config {
        Config {
            auto: false,
            create: true,
            debug: false,
            dryrun: true,
            include: Vec::new(),
            exclude: Vec::new(),
            ignored_group_names: Vec::new(),
            ignored_group_parts: Vec::new(),
            min_group_size,
            min_prefix_chars: 1,
            overwrite: false,
            prefix_ignores: ignores,
            prefix_overrides: Vec::new(),
            recurse: false,
            verbose: false,
            unpack_directory_names: Vec::new(),
        }
    }

    #[test]
    fn mixed_content_multiple_distinct_groups_with_noise() {
        // Realistic scenario: multiple distinct groups plus unrelated files
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // Group 1: BlueSky.Productions (4 files) - dotted two-part prefix
        std::fs::write(root.join("BlueSky.Productions.Summer.Adventure.1080p.x265.mp4"), "").unwrap();
        std::fs::write(root.join("BlueSky.Productions.Winter.Tale.720p.x265.mp4"), "").unwrap();
        std::fs::write(root.join("BlueSky.Productions.Spring.Romance.1080p.x265.mp4"), "").unwrap();
        std::fs::write(root.join("BlueSky.Productions.Autumn.Mystery.720p.x265.mp4"), "").unwrap();

        // Group 2: ThunderCatStudios (4 files) - concatenated single word prefix
        std::fs::write(root.join("ThunderCatStudios.Epic.Battle.x265.mp4"), "").unwrap();
        std::fs::write(root.join("ThunderCatStudios.Final.Quest.x265.mp4"), "").unwrap();
        std::fs::write(root.join("ThunderCatStudios.Dark.Rising.x265.mp4"), "").unwrap();
        std::fs::write(root.join("ThunderCatStudios.Light.Dawn.x265.mp4"), "").unwrap();

        // Group 3: Ocean.Wave (3 files) - should also match as "OceanWave"
        std::fs::write(root.join("Ocean.Wave.Sunset.Beach.x265.mp4"), "").unwrap();
        std::fs::write(root.join("OceanWave.Tropical.Paradise.mp4"), "").unwrap();
        std::fs::write(root.join("Ocean.Wave.Coral.Reef.x265.mp4"), "").unwrap();

        // Noise: various unrelated single files that should not form groups
        std::fs::write(root.join("RandomStuff.Something.mp4"), "").unwrap();
        std::fs::write(root.join("AnotherRandom.Video.mp4"), "").unwrap();
        std::fs::write(root.join("Standalone.Content.Here.mp4"), "").unwrap();
        std::fs::write(root.join("UniqueFile.NoMatch.mp4"), "").unwrap();

        // Other BlueSky files with different second part - should NOT group with BlueSky.Productions
        std::fs::write(root.join("BlueSky.Entertainment.Comedy.720p.x265.mp4"), "").unwrap();
        std::fs::write(root.join("BlueSky.Media.Documentary.1080p.mp4"), "").unwrap();

        let dirmove = DirMove::new(root, make_grouping_config(2));
        let files_with_names = dirmove.collect_files_with_names().unwrap();
        let groups = dirmove.collect_all_prefix_groups(&files_with_names);

        // Should find BlueSky.Productions as a specific group (4 files)
        assert!(
            groups.contains_key("BlueSky.Productions"),
            "Should find BlueSky.Productions group"
        );
        assert_eq!(
            groups.get("BlueSky.Productions").unwrap().0.len(),
            4,
            "BlueSky.Productions should have 4 files"
        );

        // Should find ThunderCatStudios group (4 files)
        assert!(
            groups.contains_key("ThunderCatStudios"),
            "Should find ThunderCatStudios group"
        );
        assert_eq!(
            groups.get("ThunderCatStudios").unwrap().0.len(),
            4,
            "ThunderCatStudios should have 4 files"
        );

        // Should find OceanWave group (concatenated form preferred) with 3 files
        assert!(
            groups.contains_key("OceanWave") || groups.contains_key("Ocean.Wave"),
            "Should find Ocean.Wave or OceanWave group"
        );
        let ocean_wave_key = if groups.contains_key("OceanWave") {
            "OceanWave"
        } else {
            "Ocean.Wave"
        };
        assert_eq!(
            groups.get(ocean_wave_key).unwrap().0.len(),
            3,
            "OceanWave should have 3 files"
        );

        // Should also have broader "BlueSky" group with all 6 BlueSky files
        assert!(groups.contains_key("BlueSky"), "Should find broader BlueSky group");
        assert_eq!(
            groups.get("BlueSky").unwrap().0.len(),
            6,
            "BlueSky should have 6 files total"
        );
    }

    #[test]
    fn concatenated_vs_dotted_same_name_grouped_with_concat_preferred() {
        // Files with same logical name but different formatting should group together
        // and prefer the concatenated (no-dot) form for directory name
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // Mix of NeonLight, Neon.Light, neonlight, NEONLIGHT
        std::fs::write(root.join("NeonLight.Episode.01.mp4"), "").unwrap();
        std::fs::write(root.join("Neon.Light.Episode.02.mp4"), "").unwrap();
        std::fs::write(root.join("neonlight.Episode.03.mp4"), "").unwrap();
        std::fs::write(root.join("NEONLIGHT.Episode.04.mp4"), "").unwrap();
        std::fs::write(root.join("Neon.light.Episode.05.mp4"), "").unwrap();

        // Unrelated files that should not interfere
        std::fs::write(root.join("DifferentShow.Episode.01.mp4"), "").unwrap();
        std::fs::write(root.join("SomeOther.Content.Video.mp4"), "").unwrap();

        let dirmove = DirMove::new(root, make_grouping_config(2));
        let files_with_names = dirmove.collect_files_with_names().unwrap();
        let groups = dirmove.collect_all_prefix_groups(&files_with_names);

        // All 5 NeonLight variants should be in one group
        // Should prefer concatenated form "NeonLight"
        assert!(
            groups.contains_key("NeonLight"),
            "Should prefer concatenated form NeonLight, got keys: {:?}",
            groups.keys().collect::<Vec<_>>()
        );
        assert_eq!(
            groups.get("NeonLight").unwrap().0.len(),
            5,
            "All 5 NeonLight variants should be grouped"
        );

        // Should NOT have separate groups for dotted forms
        assert!(
            !groups.contains_key("Neon.Light"),
            "Should not have separate Neon.Light group"
        );
    }

    #[test]
    fn three_part_prefix_coexists_with_two_part_and_one_part() {
        // Test that more specific prefixes are found alongside broader ones
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // 3 files with Studio.West.Coast prefix (3-part)
        std::fs::write(root.join("Studio.West.Coast.Video1.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.West.Coast.Video2.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.West.Coast.Video3.mp4"), "").unwrap();

        // 2 files with Studio.West.Mountain prefix (different 3-part)
        std::fs::write(root.join("Studio.West.Mountain.Video1.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.West.Mountain.Video2.mp4"), "").unwrap();

        // 1 file with Studio.West.Valley (not enough for its own 3-part group)
        std::fs::write(root.join("Studio.West.Valley.Video1.mp4"), "").unwrap();

        // 2 files with Studio.East (different 2-part)
        std::fs::write(root.join("Studio.East.Downtown.Video1.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.East.Uptown.Video2.mp4"), "").unwrap();

        // Unrelated noise
        std::fs::write(root.join("Completely.Different.Content.mp4"), "").unwrap();
        std::fs::write(root.join("Random.Other.File.mp4"), "").unwrap();

        let dirmove = DirMove::new(root, make_grouping_config(2));
        let files_with_names = dirmove.collect_files_with_names().unwrap();
        let groups = dirmove.collect_all_prefix_groups(&files_with_names);

        // Should find specific 3-part groups
        assert!(
            groups.contains_key("Studio.West.Coast"),
            "Should find Studio.West.Coast group"
        );
        assert_eq!(groups.get("Studio.West.Coast").unwrap().0.len(), 3);

        assert!(
            groups.contains_key("Studio.West.Mountain"),
            "Should find Studio.West.Mountain group"
        );
        assert_eq!(groups.get("Studio.West.Mountain").unwrap().0.len(), 2);

        // Should also find broader 2-part group (Studio.West) with 6 files
        assert!(groups.contains_key("Studio.West"), "Should find Studio.West group");
        assert_eq!(
            groups.get("Studio.West").unwrap().0.len(),
            6,
            "Studio.West should include all West files"
        );

        // Should find Studio.East group
        assert!(groups.contains_key("Studio.East"), "Should find Studio.East group");
        assert_eq!(groups.get("Studio.East").unwrap().0.len(), 2);

        // Should find broadest 1-part group (Studio) with all 8 Studio files
        assert!(groups.contains_key("Studio"), "Should find Studio group");
        assert_eq!(
            groups.get("Studio").unwrap().0.len(),
            8,
            "Studio should include all Studio files"
        );
    }

    #[test]
    fn long_single_word_prefix_forms_valid_group() {
        // Single long word prefixes should form valid groups
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // Long concatenated prefix (simulating something like "SuperMegaProductions")
        std::fs::write(root.join("SuperMegaProductionsHD.Adventure.One.mp4"), "").unwrap();
        std::fs::write(root.join("SuperMegaProductionsHD.Adventure.Two.mp4"), "").unwrap();
        std::fs::write(root.join("SuperMegaProductionsHD.Comedy.Special.mp4"), "").unwrap();

        // Another long prefix
        std::fs::write(root.join("UltraHighDefinitionStudio.Movie.Alpha.mp4"), "").unwrap();
        std::fs::write(root.join("UltraHighDefinitionStudio.Movie.Beta.mp4"), "").unwrap();

        // Short unrelated files
        std::fs::write(root.join("Short.Video.mp4"), "").unwrap();
        std::fs::write(root.join("Another.Short.mp4"), "").unwrap();

        let dirmove = DirMove::new(root, make_grouping_config(2));
        let files_with_names = dirmove.collect_files_with_names().unwrap();
        let groups = dirmove.collect_all_prefix_groups(&files_with_names);

        // Should find long single-word prefix groups
        assert!(
            groups.contains_key("SuperMegaProductionsHD"),
            "Should find SuperMegaProductionsHD group"
        );
        assert_eq!(groups.get("SuperMegaProductionsHD").unwrap().0.len(), 3);

        assert!(
            groups.contains_key("UltraHighDefinitionStudio"),
            "Should find UltraHighDefinitionStudio group"
        );
        assert_eq!(groups.get("UltraHighDefinitionStudio").unwrap().0.len(), 2);
    }

    #[test]
    fn prefix_ignore_strips_common_prefixes_for_better_grouping() {
        // Test that prefix ignores help group files with common prefixes stripped
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // Files with "Premium" prefix that should be ignored
        std::fs::write(root.join("Premium.GoldenStudio.Film.One.mp4"), "").unwrap();
        std::fs::write(root.join("Premium.GoldenStudio.Film.Two.mp4"), "").unwrap();
        std::fs::write(root.join("Premium.GoldenStudio.Film.Three.mp4"), "").unwrap();

        // Files without Premium prefix but same studio
        std::fs::write(root.join("GoldenStudio.Film.Four.mp4"), "").unwrap();
        std::fs::write(root.join("GoldenStudio.Film.Five.mp4"), "").unwrap();

        // Different studio with Premium prefix
        std::fs::write(root.join("Premium.SilverStudio.Movie.One.mp4"), "").unwrap();
        std::fs::write(root.join("Premium.SilverStudio.Movie.Two.mp4"), "").unwrap();

        // Unrelated
        std::fs::write(root.join("RandomContent.Video.mp4"), "").unwrap();

        let dirmove = DirMove::new(root, make_config_with_ignores(2, vec!["Premium".to_string()]));
        let files_with_names = dirmove.collect_files_with_names().unwrap();
        let groups = dirmove.collect_all_prefix_groups(&files_with_names);

        // With "Premium" ignored, all GoldenStudio files should group together
        assert!(
            groups.contains_key("GoldenStudio"),
            "Should find GoldenStudio group after ignoring Premium prefix"
        );
        assert_eq!(
            groups.get("GoldenStudio").unwrap().0.len(),
            5,
            "GoldenStudio should have all 5 files (Premium stripped)"
        );

        // SilverStudio should also group
        assert!(groups.contains_key("SilverStudio"), "Should find SilverStudio group");
        assert_eq!(groups.get("SilverStudio").unwrap().0.len(), 2);
    }

    #[test]
    fn substring_prefix_does_not_incorrectly_match() {
        // Verify that "Thunder" doesn't match "ThunderCat" files incorrectly
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // ThunderCat files (concatenated)
        std::fs::write(root.join("ThunderCat.Episode.01.mp4"), "").unwrap();
        std::fs::write(root.join("ThunderCat.Episode.02.mp4"), "").unwrap();
        std::fs::write(root.join("ThunderCat.Episode.03.mp4"), "").unwrap();

        // Thunder files (shorter, different)
        std::fs::write(root.join("Thunder.Storm.Video1.mp4"), "").unwrap();
        std::fs::write(root.join("Thunder.Storm.Video2.mp4"), "").unwrap();

        // ThunderBolt files (different concatenation)
        std::fs::write(root.join("ThunderBolt.Action.Movie1.mp4"), "").unwrap();
        std::fs::write(root.join("ThunderBolt.Action.Movie2.mp4"), "").unwrap();

        let dirmove = DirMove::new(root, make_grouping_config(2));
        let files_with_names = dirmove.collect_files_with_names().unwrap();
        let groups = dirmove.collect_all_prefix_groups(&files_with_names);

        // ThunderCat should be its own group with exactly 3 files
        assert!(groups.contains_key("ThunderCat"), "Should find ThunderCat group");
        assert_eq!(
            groups.get("ThunderCat").unwrap().0.len(),
            3,
            "ThunderCat should have exactly 3 files, not mixed with Thunder or ThunderBolt"
        );

        // Thunder.Storm should group (2-part)
        assert!(
            groups.contains_key("Thunder.Storm") || groups.contains_key("Thunder"),
            "Should find Thunder related group"
        );

        // ThunderBolt should be its own group
        assert!(groups.contains_key("ThunderBolt"), "Should find ThunderBolt group");
        assert_eq!(groups.get("ThunderBolt").unwrap().0.len(), 2);
    }

    #[test]
    fn files_with_numeric_and_resolution_parts_filtered_for_grouping() {
        // Test that year, resolution, etc. are filtered out when determining groups
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // Same logical content with different years/resolutions
        std::fs::write(root.join("CreativeArts.Documentary.2019.1080p.mp4"), "").unwrap();
        std::fs::write(root.join("CreativeArts.Documentary.2020.720p.mp4"), "").unwrap();
        std::fs::write(root.join("CreativeArts.Documentary.2021.4K.mp4"), "").unwrap();
        std::fs::write(root.join("CreativeArts.Documentary.2022.1080p.mp4"), "").unwrap();

        // Different content
        std::fs::write(root.join("TechReview.2023.Product.Launch.1080p.mp4"), "").unwrap();
        std::fs::write(root.join("TechReview.2024.Annual.Summary.720p.mp4"), "").unwrap();

        // Unrelated
        std::fs::write(root.join("Random.2020.Content.mp4"), "").unwrap();

        let dirmove = DirMove::new(root, make_grouping_config(2));
        let files_with_names = dirmove.collect_files_with_names().unwrap();
        let groups = dirmove.collect_all_prefix_groups(&files_with_names);

        // All CreativeArts.Documentary files should group despite different years
        assert!(
            groups.contains_key("CreativeArts.Documentary") || groups.contains_key("CreativeArts"),
            "Should find CreativeArts group"
        );

        // Check that we got the more specific group if it exists
        if groups.contains_key("CreativeArts.Documentary") {
            assert_eq!(
                groups.get("CreativeArts.Documentary").unwrap().0.len(),
                4,
                "CreativeArts.Documentary should have 4 files"
            );
        }

        // TechReview should also group
        assert!(groups.contains_key("TechReview"), "Should find TechReview group");
        assert_eq!(groups.get("TechReview").unwrap().0.len(), 2);
    }

    #[test]
    fn multiple_valid_groupings_all_offered() {
        // Test that when files can be grouped multiple ways, all valid groupings are available
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // Files that match multiple groupings:
        // - "Network.Channel.Morning" (3-part, 2 files)
        // - "Network.Channel" (2-part, 4 files)
        // - "Network" (1-part, 6 files)
        std::fs::write(root.join("Network.Channel.Morning.Show1.mp4"), "").unwrap();
        std::fs::write(root.join("Network.Channel.Morning.Show2.mp4"), "").unwrap();
        std::fs::write(root.join("Network.Channel.Evening.Show1.mp4"), "").unwrap();
        std::fs::write(root.join("Network.Channel.Evening.Show2.mp4"), "").unwrap();
        std::fs::write(root.join("Network.Sports.Game1.mp4"), "").unwrap();
        std::fs::write(root.join("Network.Sports.Game2.mp4"), "").unwrap();

        // Completely unrelated
        std::fs::write(root.join("Independent.Film.Festival.mp4"), "").unwrap();

        let dirmove = DirMove::new(root, make_grouping_config(2));
        let files_with_names = dirmove.collect_files_with_names().unwrap();
        let groups = dirmove.collect_all_prefix_groups(&files_with_names);

        // All three levels of grouping should be available
        assert!(
            groups.contains_key("Network.Channel.Morning"),
            "Should find 3-part Network.Channel.Morning group"
        );
        assert_eq!(groups.get("Network.Channel.Morning").unwrap().0.len(), 2);

        assert!(
            groups.contains_key("Network.Channel"),
            "Should find 2-part Network.Channel group"
        );
        assert_eq!(groups.get("Network.Channel").unwrap().0.len(), 4);

        assert!(groups.contains_key("Network"), "Should find 1-part Network group");
        assert_eq!(groups.get("Network").unwrap().0.len(), 6);

        // Network.Sports should also be a valid 2-part group
        assert!(
            groups.contains_key("Network.Sports"),
            "Should find Network.Sports group"
        );
        assert_eq!(groups.get("Network.Sports").unwrap().0.len(), 2);
    }

    #[test]
    fn user_can_reject_specific_group_and_use_broader_one() {
        // Simulates the scenario from the user's example:
        // User has files like "Galaxy.Quest.Episode.01.mp4"
        // Tool offers "Galaxy.Quest.Episode" first (most specific)
        // User rejects it, wants "Galaxy.Quest" instead
        // Both options should be available in the groups
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // 3 files with "Galaxy.Quest.Episode" prefix
        std::fs::write(root.join("Galaxy.Quest.Episode.01.mp4"), "").unwrap();
        std::fs::write(root.join("Galaxy.Quest.Episode.02.mp4"), "").unwrap();
        std::fs::write(root.join("Galaxy.Quest.Episode.03.mp4"), "").unwrap();
        // 2 more files with "Galaxy.Quest" but different third part
        std::fs::write(root.join("Galaxy.Quest.Movie.mp4"), "").unwrap();
        std::fs::write(root.join("Galaxy.Quest.Special.mp4"), "").unwrap();

        let dirmove = DirMove::new(root, make_grouping_config(2));
        let files_with_names = dirmove.collect_files_with_names().unwrap();
        let groups = dirmove.collect_all_prefix_groups(&files_with_names);

        // Most specific: "Galaxy.Quest.Episode" with 3 files
        assert!(
            groups.contains_key("Galaxy.Quest.Episode"),
            "Should offer Galaxy.Quest.Episode as an option"
        );
        assert_eq!(
            groups.get("Galaxy.Quest.Episode").unwrap().0.len(),
            3,
            "Galaxy.Quest.Episode should have 3 files"
        );

        // Less specific: "Galaxy.Quest" with all 5 files
        assert!(
            groups.contains_key("Galaxy.Quest"),
            "Should offer Galaxy.Quest as an alternative option"
        );
        assert_eq!(
            groups.get("Galaxy.Quest").unwrap().0.len(),
            5,
            "Galaxy.Quest should have all 5 files"
        );

        // Least specific: "Galaxy" with all 5 files
        assert!(
            groups.contains_key("Galaxy"),
            "Should offer Galaxy as the broadest option"
        );
        assert_eq!(
            groups.get("Galaxy").unwrap().0.len(),
            5,
            "Galaxy should have all 5 files"
        );
    }

    #[test]
    fn groups_sorted_by_specificity_longest_prefix_first() {
        // Verify that when processing, longer prefixes come before shorter ones
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("Studio.Ghibli.Films.Totoro.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.Ghibli.Films.Spirited.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.Ghibli.Films.Mononoke.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.Ghibli.Shorts.One.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.Ghibli.Shorts.Two.mp4"), "").unwrap();

        let config = Config {
            auto: false,
            create: true,
            debug: false,
            dryrun: true,
            include: Vec::new(),
            exclude: Vec::new(),
            ignored_group_names: Vec::new(),
            ignored_group_parts: Vec::new(),
            min_group_size: 2,
            min_prefix_chars: 1,
            overwrite: false,
            prefix_ignores: Vec::new(),
            prefix_overrides: Vec::new(),
            recurse: false,
            verbose: false,
            unpack_directory_names: Vec::new(),
        };

        let dirmove = DirMove::new(root, config);
        let files_with_names = dirmove.collect_files_with_names().unwrap();
        let groups = dirmove.collect_all_prefix_groups(&files_with_names);

        // Convert to sorted vec like create_dirs_and_move_files does
        let sorted_groups: Vec<_> = groups
            .into_iter()
            .sorted_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.0.cmp(&b.0)))
            .collect();

        // Verify order: longest prefixes first
        let prefixes: Vec<&str> = sorted_groups.iter().map(|(p, _)| p.as_str()).collect();

        // 3-part prefixes should come before 2-part, which come before 1-part
        let films_pos = prefixes.iter().position(|&p| p == "Studio.Ghibli.Films");
        let shorts_pos = prefixes.iter().position(|&p| p == "Studio.Ghibli.Shorts");
        let ghibli_pos = prefixes.iter().position(|&p| p == "Studio.Ghibli");
        let studio_pos = prefixes.iter().position(|&p| p == "Studio");

        assert!(films_pos.is_some(), "Should have Studio.Ghibli.Films group");
        assert!(shorts_pos.is_some(), "Should have Studio.Ghibli.Shorts group");
        assert!(ghibli_pos.is_some(), "Should have Studio.Ghibli group");
        assert!(studio_pos.is_some(), "Should have Studio group");

        // 3-part prefixes before 2-part
        assert!(
            films_pos.unwrap() < ghibli_pos.unwrap(),
            "Studio.Ghibli.Films should come before Studio.Ghibli"
        );
        assert!(
            shorts_pos.unwrap() < ghibli_pos.unwrap(),
            "Studio.Ghibli.Shorts should come before Studio.Ghibli"
        );

        // 2-part before 1-part
        assert!(
            ghibli_pos.unwrap() < studio_pos.unwrap(),
            "Studio.Ghibli should come before Studio"
        );
    }

    #[test]
    fn all_prefix_lengths_offered_complete_hierarchy() {
        // Ensure complete hierarchy of prefixes is offered for user choice
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // Create files where all 3 prefix levels are meaningful choices
        std::fs::write(root.join("Marvel.Studios.Avengers.Endgame.mp4"), "").unwrap();
        std::fs::write(root.join("Marvel.Studios.Avengers.Infinity.mp4"), "").unwrap();
        std::fs::write(root.join("Marvel.Studios.Avengers.Age.mp4"), "").unwrap();
        std::fs::write(root.join("Marvel.Studios.Guardians.Vol1.mp4"), "").unwrap();
        std::fs::write(root.join("Marvel.Studios.Guardians.Vol2.mp4"), "").unwrap();
        std::fs::write(root.join("Marvel.Television.Daredevil.S01.mp4"), "").unwrap();
        std::fs::write(root.join("Marvel.Television.Daredevil.S02.mp4"), "").unwrap();

        let dirmove = DirMove::new(root, make_grouping_config(2));
        let files_with_names = dirmove.collect_files_with_names().unwrap();
        let groups = dirmove.collect_all_prefix_groups(&files_with_names);

        // 3-part groups
        assert!(
            groups.contains_key("Marvel.Studios.Avengers"),
            "Missing 3-part Avengers"
        );
        assert_eq!(groups.get("Marvel.Studios.Avengers").unwrap().0.len(), 3);

        assert!(
            groups.contains_key("Marvel.Studios.Guardians"),
            "Missing 3-part Guardians"
        );
        assert_eq!(groups.get("Marvel.Studios.Guardians").unwrap().0.len(), 2);

        assert!(
            groups.contains_key("Marvel.Television.Daredevil"),
            "Missing 3-part Daredevil"
        );
        assert_eq!(groups.get("Marvel.Television.Daredevil").unwrap().0.len(), 2);

        // 2-part groups
        assert!(groups.contains_key("Marvel.Studios"), "Missing 2-part Studios");
        assert_eq!(groups.get("Marvel.Studios").unwrap().0.len(), 5);

        assert!(groups.contains_key("Marvel.Television"), "Missing 2-part Television");
        assert_eq!(groups.get("Marvel.Television").unwrap().0.len(), 2);

        // 1-part group
        assert!(groups.contains_key("Marvel"), "Missing 1-part Marvel");
        assert_eq!(groups.get("Marvel").unwrap().0.len(), 7);

        // User could choose any of these:
        // - "Marvel.Studios.Avengers" for just Avengers movies (3 files)
        // - "Marvel.Studios" for all Studios content (5 files)
        // - "Marvel" for everything (7 files)
    }

    #[test]
    fn rejected_group_files_remain_available_for_next_group() {
        // When a group is skipped (not moved), its files should still be available
        // for the next offered group
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("Series.Season.Episode.01.mp4"), "").unwrap();
        std::fs::write(root.join("Series.Season.Episode.02.mp4"), "").unwrap();
        std::fs::write(root.join("Series.Season.Special.mp4"), "").unwrap();

        let dirmove = DirMove::new(root, make_grouping_config(2));
        let files_with_names = dirmove.collect_files_with_names().unwrap();
        let groups = dirmove.collect_all_prefix_groups(&files_with_names);

        // Both specific and general groups contain overlapping files
        let specific_files = &groups.get("Series.Season.Episode").unwrap().0;
        let general_files = &groups.get("Series.Season").unwrap().0;

        assert_eq!(specific_files.len(), 2, "Specific group has 2 files");
        assert_eq!(general_files.len(), 3, "General group has 3 files");

        // The 2 files in specific group should also be in general group
        for file in specific_files {
            assert!(general_files.contains(file), "File {file:?} should be in both groups");
        }
    }

    #[test]
    fn min_group_size_respected_correctly() {
        // Verify that min_group_size is respected (not capped at 2)
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // Groups of various sizes
        std::fs::write(root.join("PairOnly.FileA.mp4"), "").unwrap();
        std::fs::write(root.join("PairOnly.FileB.mp4"), "").unwrap();

        std::fs::write(root.join("TripleGroup.FileA.mp4"), "").unwrap();
        std::fs::write(root.join("TripleGroup.FileB.mp4"), "").unwrap();
        std::fs::write(root.join("TripleGroup.FileC.mp4"), "").unwrap();

        // Single files (should NOT form groups)
        std::fs::write(root.join("LonelyFile.NoMatch.mp4"), "").unwrap();
        std::fs::write(root.join("AnotherSingle.Standalone.mp4"), "").unwrap();
        std::fs::write(root.join("ThirdUnique.Content.mp4"), "").unwrap();

        // With min_group_size=5, neither group qualifies (2 and 3 files < 5)
        let dirmove = DirMove::new(root.clone(), make_grouping_config(5));
        let files_with_names = dirmove.collect_files_with_names().unwrap();
        let groups = dirmove.collect_all_prefix_groups(&files_with_names);

        assert!(
            groups.is_empty(),
            "With min_group_size=5, no groups should be found (max is 3 files)"
        );

        // With min_group_size=3, only TripleGroup qualifies
        let dirmove = DirMove::new(root.clone(), make_grouping_config(3));
        let files_with_names = dirmove.collect_files_with_names().unwrap();
        let groups = dirmove.collect_all_prefix_groups(&files_with_names);

        assert!(
            !groups.contains_key("PairOnly"),
            "PairOnly (2 files) should not be found with min_group_size=3"
        );
        assert!(
            groups.contains_key("TripleGroup"),
            "TripleGroup (3 files) should be found with min_group_size=3"
        );
        assert_eq!(groups.get("TripleGroup").unwrap().0.len(), 3);

        // With min_group_size=2, both groups qualify
        let dirmove = DirMove::new(root, make_grouping_config(2));
        let files_with_names = dirmove.collect_files_with_names().unwrap();
        let groups = dirmove.collect_all_prefix_groups(&files_with_names);

        assert!(
            groups.contains_key("PairOnly"),
            "PairOnly (2 files) should be found with min_group_size=2"
        );
        assert!(
            groups.contains_key("TripleGroup"),
            "TripleGroup (3 files) should be found with min_group_size=2"
        );

        // Single files should NOT form groups regardless of threshold
        assert!(
            !groups.contains_key("LonelyFile"),
            "Single files should not form groups"
        );
        assert!(
            !groups.contains_key("AnotherSingle"),
            "Single files should not form groups"
        );
    }

    #[test]
    fn mixed_case_and_format_all_normalized_correctly() {
        // Comprehensive test of case and format normalization
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // All these should be the same group (StarBright)
        std::fs::write(root.join("StarBright.Episode.01.mp4"), "").unwrap();
        std::fs::write(root.join("Star.Bright.Episode.02.mp4"), "").unwrap();
        std::fs::write(root.join("STARBRIGHT.Episode.03.mp4"), "").unwrap();
        std::fs::write(root.join("starbright.Episode.04.mp4"), "").unwrap();
        std::fs::write(root.join("Star.bright.Episode.05.mp4"), "").unwrap();
        std::fs::write(root.join("starBright.Episode.06.mp4"), "").unwrap();

        // Different prefix entirely
        std::fs::write(root.join("MoonGlow.Episode.01.mp4"), "").unwrap();
        std::fs::write(root.join("Moon.Glow.Episode.02.mp4"), "").unwrap();

        let dirmove = DirMove::new(root, make_grouping_config(2));
        let files_with_names = dirmove.collect_files_with_names().unwrap();
        let groups = dirmove.collect_all_prefix_groups(&files_with_names);

        // All StarBright variants should be in ONE group
        // Should prefer concatenated form
        let starbright_key = groups
            .keys()
            .find(|k| k.to_lowercase().replace('.', "") == "starbright")
            .expect("Should find a StarBright group");

        assert_eq!(
            groups.get(starbright_key).unwrap().0.len(),
            6,
            "All 6 StarBright variants should be in one group"
        );

        // MoonGlow should be separate
        let moonglow_key = groups
            .keys()
            .find(|k| k.to_lowercase().replace('.', "") == "moonglow")
            .expect("Should find a MoonGlow group");

        assert_eq!(
            groups.get(moonglow_key).unwrap().0.len(),
            2,
            "MoonGlow should have 2 files"
        );
    }

    #[test]
    fn four_part_prefix_uses_three_part_max() {
        // Prefixes longer than 3 parts should still work, using the first 3 parts
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("One.Two.Three.Four.File1.mp4"), "").unwrap();
        std::fs::write(root.join("One.Two.Three.Four.File2.mp4"), "").unwrap();
        std::fs::write(root.join("One.Two.Three.Different.File3.mp4"), "").unwrap();

        let dirmove = DirMove::new(root, make_grouping_config(2));
        let files_with_names = dirmove.collect_files_with_names().unwrap();
        let groups = dirmove.collect_all_prefix_groups(&files_with_names);

        // Should find "One.Two.Three" as a group with all 3 files
        assert!(groups.contains_key("One.Two.Three"), "Should find 3-part prefix group");
        assert_eq!(groups.get("One.Two.Three").unwrap().0.len(), 3);
    }

    #[test]
    fn alphanumeric_prefix_not_filtered() {
        // Alphanumeric prefixes (not purely numeric) should be kept
        // Note: Purely numeric parts like "24" ARE filtered by design
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("Show24.Hours.Episode.01.mp4"), "").unwrap();
        std::fs::write(root.join("Show24.Hours.Episode.02.mp4"), "").unwrap();
        std::fs::write(root.join("Show24.Hours.Episode.03.mp4"), "").unwrap();

        let dirmove = DirMove::new(root, make_grouping_config(2));
        let files_with_names = dirmove.collect_files_with_names().unwrap();
        let groups = dirmove.collect_all_prefix_groups(&files_with_names);

        // Should find "Show24.Hours" or similar as a valid group
        assert!(
            groups.keys().any(|k| k.to_lowercase().contains("show24")),
            "Should find group with alphanumeric prefix"
        );
    }

    #[test]
    fn single_character_prefix_forms_group() {
        // Single character prefixes should work if they meet threshold
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("X.Files.Episode.01.mp4"), "").unwrap();
        std::fs::write(root.join("X.Files.Episode.02.mp4"), "").unwrap();
        std::fs::write(root.join("X.Files.Episode.03.mp4"), "").unwrap();

        let dirmove = DirMove::new(root, make_grouping_config(2));
        let files_with_names = dirmove.collect_files_with_names().unwrap();
        let groups = dirmove.collect_all_prefix_groups(&files_with_names);

        // Should find "X.Files" as a group
        assert!(
            groups.contains_key("X.Files") || groups.contains_key("XFiles"),
            "Should find X.Files group"
        );
    }

    #[test]
    fn hyphenated_names_treated_as_single_part() {
        // Hyphens within a dot-part should be preserved
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("Spider-Man.Episode.01.mp4"), "").unwrap();
        std::fs::write(root.join("Spider-Man.Episode.02.mp4"), "").unwrap();
        std::fs::write(root.join("Spider-Man.Movie.mp4"), "").unwrap();

        let dirmove = DirMove::new(root, make_grouping_config(2));
        let files_with_names = dirmove.collect_files_with_names().unwrap();
        let groups = dirmove.collect_all_prefix_groups(&files_with_names);

        // Should find "Spider-Man" as a group
        assert!(groups.contains_key("Spider-Man"), "Should preserve hyphen in prefix");
        assert_eq!(groups.get("Spider-Man").unwrap().0.len(), 3);
    }

    #[test]
    fn underscore_names_not_split() {
        // Underscores should not be treated as separators
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("My_Show.Episode.01.mp4"), "").unwrap();
        std::fs::write(root.join("My_Show.Episode.02.mp4"), "").unwrap();

        let dirmove = DirMove::new(root, make_grouping_config(2));
        let files_with_names = dirmove.collect_files_with_names().unwrap();
        let groups = dirmove.collect_all_prefix_groups(&files_with_names);

        // Should find "My_Show" as a group (underscore preserved)
        assert!(groups.contains_key("My_Show"), "Should preserve underscore in prefix");
    }

    #[test]
    fn empty_filename_parts_handled() {
        // Double dots or leading dots should be handled gracefully
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("Show..Name.Episode.01.mp4"), "").unwrap();
        std::fs::write(root.join("Show..Name.Episode.02.mp4"), "").unwrap();

        let dirmove = DirMove::new(root, make_grouping_config(2));
        let files_with_names = dirmove.collect_files_with_names().unwrap();
        let groups = dirmove.collect_all_prefix_groups(&files_with_names);

        // Should handle gracefully without panic
        assert!(!groups.is_empty(), "Should find some group even with double dots");
    }

    #[test]
    fn very_long_filename_prefix_extraction() {
        // Very long filenames should still work
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        let long_name = "A".repeat(50);
        std::fs::write(root.join(format!("{long_name}.Part.01.mp4")), "").unwrap();
        std::fs::write(root.join(format!("{long_name}.Part.02.mp4")), "").unwrap();

        let dirmove = DirMove::new(root, make_grouping_config(2));
        let files_with_names = dirmove.collect_files_with_names().unwrap();
        let groups = dirmove.collect_all_prefix_groups(&files_with_names);

        // Should find the long prefix as a group
        assert!(
            groups.keys().any(|k| k.starts_with(&long_name)),
            "Should handle very long prefixes"
        );
    }

    #[test]
    fn min_group_size_one_creates_single_file_groups() {
        // With min_group_size=1, even single files should form groups
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("Unique.Name.File.mp4"), "").unwrap();
        std::fs::write(root.join("Another.Unique.File.mp4"), "").unwrap();

        let dirmove = DirMove::new(root, make_grouping_config(1));
        let files_with_names = dirmove.collect_files_with_names().unwrap();
        let groups = dirmove.collect_all_prefix_groups(&files_with_names);

        // Both unique files should form their own groups
        assert!(
            groups.len() >= 2,
            "With min_group_size=1, single files should form groups"
        );
    }

    #[test]
    fn files_with_only_extension_no_prefix() {
        // Edge case: files that are just an extension or very short
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join(".mp4"), "").unwrap();
        std::fs::write(root.join("a.mp4"), "").unwrap();

        let dirmove = DirMove::new(root, make_grouping_config(2));
        let files_with_names = dirmove.collect_files_with_names().unwrap();

        // Should not panic, may or may not find groups
        let _groups = dirmove.collect_all_prefix_groups(&files_with_names);
    }

    #[test]
    fn mixed_extensions_same_prefix_grouped() {
        // Files with same prefix but different extensions should be grouped
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("Movie.Name.2024.mp4"), "").unwrap();
        std::fs::write(root.join("Movie.Name.2024.srt"), "").unwrap();
        std::fs::write(root.join("Movie.Name.2024.nfo"), "").unwrap();

        let dirmove = DirMove::new(root, make_grouping_config(2));
        let files_with_names = dirmove.collect_files_with_names().unwrap();
        let groups = dirmove.collect_all_prefix_groups(&files_with_names);

        // All three should be in one group based on "Movie.Name"
        let movie_group = groups.iter().find(|(k, _)| k.to_lowercase().contains("movie"));
        assert!(movie_group.is_some(), "Should find Movie group");
        assert_eq!(
            movie_group.unwrap().1.0.len(),
            3,
            "All extensions should be in same group"
        );
    }

    #[test]
    fn realistic_scenario_seven_files_four_share_two_part_prefix() {
        // Realistic scenario: 7 files starting with "Studio"
        // 4 of them share the two-part prefix "Studio.Alpha"
        // With min_group_size=3, both groups should be offered:
        // - "Studio.Alpha" (4 files)
        // - "Studio" (7 files)
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // 4 files with "Studio.Alpha" prefix
        std::fs::write(root.join("Studio.Alpha.First.Project.1080p.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.Alpha.Second.Work.1080p.x265.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.Alpha.Third.Creation.720p.x265.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.Alpha.Fourth.Piece.720p.x265.mp4"), "").unwrap();

        // 3 more files with different second parts
        std::fs::write(root.join("Studio.Beta.Something.Different.1080p.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.Gamma.Another.Thing.720p.x265.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.Delta.Yet.Another.Item.1080p.mp4"), "").unwrap();

        let dirmove = DirMove::new(root, make_grouping_config(3));
        let files_with_names = dirmove.collect_files_with_names().unwrap();
        let groups = dirmove.collect_all_prefix_groups(&files_with_names);

        // Should find "Studio.Alpha" with 4 files (meets min_group_size=3)
        assert!(
            groups.contains_key("Studio.Alpha"),
            "Should find Studio.Alpha group. Found groups: {:?}",
            groups.keys().collect::<Vec<_>>()
        );
        assert_eq!(
            groups.get("Studio.Alpha").unwrap().0.len(),
            4,
            "Studio.Alpha should have 4 files"
        );

        // Should find "Studio" with 7 files
        assert!(
            groups.contains_key("Studio"),
            "Should find Studio group. Found groups: {:?}",
            groups.keys().collect::<Vec<_>>()
        );
        assert_eq!(groups.get("Studio").unwrap().0.len(), 7, "Studio should have 7 files");
    }

    #[test]
    fn non_contiguous_parts_do_not_form_group() {
        // Files where filtering makes non-adjacent parts appear adjacent should NOT
        // form multi-part prefix groups. Only the single-part prefix should match.
        //
        // Example: "Site.2023.04.13.Person.video.mp4" after filtering becomes
        // "Site.Person.video.mp4", but "Site.Person" should NOT be a valid 2-part
        // prefix because "Site" and "Person" are not adjacent in the original.
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // These files have "Site" and "Person" separated by dates in the original
        std::fs::write(root.join("Site.2023.04.13.Person.First.1080p.mp4"), "").unwrap();
        std::fs::write(root.join("Site.2023.07.15.Person.Second.1080p.mp4"), "").unwrap();
        std::fs::write(root.join("Site.2024.01.01.Person.Third.1080p.mp4"), "").unwrap();
        // This one has Site.Person adjacent
        std::fs::write(root.join("Site.Person.Fourth.1080p.mp4"), "").unwrap();

        let dirmove = DirMove::new(root, make_grouping_config(3));
        let files_with_names = dirmove.collect_files_with_names().unwrap();
        let groups = dirmove.collect_all_prefix_groups(&files_with_names);

        // Should find "Site" with 4 files (all start with Site)
        assert!(
            groups.contains_key("Site"),
            "Should find Site group. Found groups: {:?}",
            groups.keys().collect::<Vec<_>>()
        );
        assert_eq!(groups.get("Site").unwrap().0.len(), 4, "Site should have 4 files");

        // Should NOT find "Site.Person" as a group because only 1 file has them adjacent
        // (the other 3 have dates between Site and Person)
        assert!(
            !groups.contains_key("Site.Person"),
            "Should NOT find Site.Person group - parts are not contiguous in most files"
        );
    }

    #[test]
    fn contiguity_dotted_vs_concatenated_large_group() {
        // Large group where some files use dots and some use concatenated names
        // All should be grouped together because contiguity is maintained
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // Dotted forms: "Photo.Lab" is contiguous
        std::fs::write(root.join("Photo.Lab.Project.Alpha.1080p.mp4"), "").unwrap();
        std::fs::write(root.join("Photo.Lab.Project.Beta.1080p.mp4"), "").unwrap();
        std::fs::write(root.join("Photo.Lab.Project.Gamma.720p.mp4"), "").unwrap();

        // Concatenated forms: "PhotoLab" as single part
        std::fs::write(root.join("PhotoLab.Project.Delta.1080p.mp4"), "").unwrap();
        std::fs::write(root.join("PhotoLab.Project.Epsilon.720p.mp4"), "").unwrap();
        std::fs::write(root.join("PHOTOLAB.Project.Zeta.1080p.mp4"), "").unwrap();

        // Mixed case variations
        std::fs::write(root.join("photolab.Project.Eta.720p.mp4"), "").unwrap();
        std::fs::write(root.join("Photolab.Project.Theta.1080p.mp4"), "").unwrap();

        let dirmove = DirMove::new(root, make_grouping_config(3));
        let files_with_names = dirmove.collect_files_with_names().unwrap();
        let groups = dirmove.collect_all_prefix_groups(&files_with_names);

        // Should find a group containing all 8 files (PhotoLab = Photo.Lab)
        let photolab_group = groups
            .iter()
            .find(|(k, _)| k.to_lowercase().replace('.', "") == "photolab");
        assert!(photolab_group.is_some(), "Should find PhotoLab group");
        assert_eq!(
            photolab_group.unwrap().1.0.len(),
            8,
            "All 8 files should be in PhotoLab group"
        );
    }

    #[test]
    fn contiguity_three_part_prefix_various_forms() {
        // Three-part prefix in various forms: dotted, concatenated, partially concatenated
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // Fully dotted: "Dark.Star.Media"
        std::fs::write(root.join("Dark.Star.Media.Video.01.mp4"), "").unwrap();
        std::fs::write(root.join("Dark.Star.Media.Video.02.mp4"), "").unwrap();
        std::fs::write(root.join("Dark.Star.Media.Video.03.mp4"), "").unwrap();

        // Fully concatenated: "DarkStarMedia"
        std::fs::write(root.join("DarkStarMedia.Video.04.mp4"), "").unwrap();
        std::fs::write(root.join("DarkStarMedia.Video.05.mp4"), "").unwrap();

        // Partially concatenated: "DarkStar.Media"
        std::fs::write(root.join("DarkStar.Media.Video.06.mp4"), "").unwrap();
        std::fs::write(root.join("DarkStar.Media.Video.07.mp4"), "").unwrap();

        // Other partial: "Dark.StarMedia"
        std::fs::write(root.join("Dark.StarMedia.Video.08.mp4"), "").unwrap();

        let dirmove = DirMove::new(root, make_grouping_config(3));
        let files_with_names = dirmove.collect_files_with_names().unwrap();
        let groups = dirmove.collect_all_prefix_groups(&files_with_names);

        // All 8 files should be grouped together
        let dark_star_media_group = groups
            .iter()
            .find(|(k, _)| k.to_lowercase().replace('.', "") == "darkstarmedia");
        assert!(
            dark_star_media_group.is_some(),
            "Should find DarkStarMedia group. Found: {:?}",
            groups.keys().collect::<Vec<_>>()
        );
        assert_eq!(
            dark_star_media_group.unwrap().1.0.len(),
            8,
            "All 8 files should be in group"
        );
    }

    #[test]
    fn contiguity_numbers_break_adjacency() {
        // Numbers between parts should break contiguity
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // These have numbers between "Creator" and "Name" - NOT contiguous
        std::fs::write(root.join("Creator.2021.Name.Video.01.mp4"), "").unwrap();
        std::fs::write(root.join("Creator.2022.Name.Video.02.mp4"), "").unwrap();
        std::fs::write(root.join("Creator.2023.Name.Video.03.mp4"), "").unwrap();
        std::fs::write(root.join("Creator.2024.Name.Video.04.mp4"), "").unwrap();

        // These have "Creator.Name" contiguous
        std::fs::write(root.join("Creator.Name.Video.05.mp4"), "").unwrap();
        std::fs::write(root.join("Creator.Name.Video.06.mp4"), "").unwrap();

        let dirmove = DirMove::new(root, make_grouping_config(3));
        let files_with_names = dirmove.collect_files_with_names().unwrap();
        let groups = dirmove.collect_all_prefix_groups(&files_with_names);

        // Should find "Creator" with 6 files
        assert!(groups.contains_key("Creator"), "Should find Creator group");
        assert_eq!(groups.get("Creator").unwrap().0.len(), 6, "Creator should have 6 files");

        // Should NOT find "Creator.Name" with 6 files because only 2 have contiguous parts
        // If it exists, it should only have 2 files
        if let Some((files, _, _)) = groups.get("Creator.Name") {
            assert_eq!(
                files.len(),
                2,
                "Creator.Name should only have 2 files (the contiguous ones)"
            );
        }
    }

    #[test]
    fn contiguity_glue_words_break_adjacency() {
        // Glue words (and, the, of, etc.) between parts should break contiguity
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // "Beauty" and "Beast" separated by glue word "and" - NOT contiguous after filtering
        std::fs::write(root.join("Beauty.and.Beast.Episode.01.mp4"), "").unwrap();
        std::fs::write(root.join("Beauty.and.Beast.Episode.02.mp4"), "").unwrap();
        std::fs::write(root.join("Beauty.and.Beast.Episode.03.mp4"), "").unwrap();

        // "Beauty.Beast" contiguous (no glue word)
        std::fs::write(root.join("Beauty.Beast.Special.01.mp4"), "").unwrap();

        let dirmove = DirMove::new(root, make_grouping_config(3));
        let files_with_names = dirmove.collect_files_with_names().unwrap();
        let groups = dirmove.collect_all_prefix_groups(&files_with_names);

        // Should find "Beauty" with 4 files
        assert!(groups.contains_key("Beauty"), "Should find Beauty group");
        assert_eq!(groups.get("Beauty").unwrap().0.len(), 4, "Beauty should have 4 files");

        // "Beauty.Beast" should NOT have 4 files - only 1 has them contiguous in original
        assert!(
            !groups.contains_key("Beauty.Beast"),
            "Beauty.Beast should not exist as a group (only 1 contiguous file)"
        );
    }

    #[test]
    fn contiguity_mixed_separators_large_dataset() {
        // Large dataset with various separator patterns
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // Group 1: "Alpha.Beta" contiguous in various forms (should all group together)
        // Note: "AlphaBeta" is a single part, different from "Alpha.Beta" (two parts)
        std::fs::write(root.join("Alpha.Beta.Content.01.mp4"), "").unwrap();
        std::fs::write(root.join("Alpha.Beta.Content.02.mp4"), "").unwrap();
        std::fs::write(root.join("AlphaBeta.Content.03.mp4"), "").unwrap();
        std::fs::write(root.join("AlphaBeta.Content.04.mp4"), "").unwrap();
        std::fs::write(root.join("ALPHA.BETA.Content.05.mp4"), "").unwrap();

        // Group 2: "Alpha" only, different second parts (matches single-part "Alpha")
        std::fs::write(root.join("Alpha.Gamma.Content.06.mp4"), "").unwrap();
        std::fs::write(root.join("Alpha.Delta.Content.07.mp4"), "").unwrap();
        std::fs::write(root.join("Alpha.Epsilon.Content.08.mp4"), "").unwrap();

        // Group 3: "Alpha" with numbers breaking contiguity to second meaningful part
        std::fs::write(root.join("Alpha.2023.Beta.Content.09.mp4"), "").unwrap();
        std::fs::write(root.join("Alpha.2024.Beta.Content.10.mp4"), "").unwrap();

        let dirmove = DirMove::new(root, make_grouping_config(3));
        let files_with_names = dirmove.collect_files_with_names().unwrap();
        let groups = dirmove.collect_all_prefix_groups(&files_with_names);

        // "Alpha.Beta" or "AlphaBeta" should have 5 files (the contiguous ones)
        // All 5 files normalize to "alphabeta" and have contiguous parts
        let alpha_beta_group = groups
            .iter()
            .find(|(k, _)| k.to_lowercase().replace('.', "") == "alphabeta");
        assert!(
            alpha_beta_group.is_some(),
            "Should find AlphaBeta group. Found: {:?}",
            groups.keys().collect::<Vec<_>>()
        );
        assert_eq!(alpha_beta_group.unwrap().1.0.len(), 5, "AlphaBeta should have 5 files");

        // "Alpha" should have ALL 10 files because:
        // - Files 01, 02, 05 start with "Alpha." (dotted)
        // - Files 03, 04 start with "AlphaBeta" which starts with "Alpha"
        // - Files 06, 07, 08 start with "Alpha." (dotted)
        // - Files 09, 10 start with "Alpha." (dotted)
        // The starts_with behavior allows broad grouping
        assert!(groups.contains_key("Alpha"), "Should find Alpha group");
        assert_eq!(
            groups.get("Alpha").unwrap().0.len(),
            10,
            "Alpha should have all 10 files (including AlphaBeta files via starts_with)"
        );
    }

    #[test]
    fn contiguity_long_date_sequences() {
        // Files with long date sequences between parts
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // Long date sequences break contiguity
        std::fs::write(root.join("Studio.2023.01.15.10.30.45.Actor.Scene.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.2023.02.20.11.45.30.Actor.Scene.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.2023.03.25.12.00.15.Actor.Scene.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.2024.04.30.13.15.00.Actor.Scene.mp4"), "").unwrap();

        // Contiguous version
        std::fs::write(root.join("Studio.Actor.Scene.Direct.mp4"), "").unwrap();

        let dirmove = DirMove::new(root, make_grouping_config(3));
        let files_with_names = dirmove.collect_files_with_names().unwrap();
        let groups = dirmove.collect_all_prefix_groups(&files_with_names);

        // Should find "Studio" with 5 files
        assert!(groups.contains_key("Studio"), "Should find Studio group");
        assert_eq!(groups.get("Studio").unwrap().0.len(), 5, "Studio should have 5 files");

        // "Studio.Actor" should NOT have 5 files
        assert!(
            !groups.contains_key("Studio.Actor"),
            "Studio.Actor should not exist (only 1 contiguous)"
        );
    }

    #[test]
    fn contiguity_extended_prefix_groups() {
        // Names that start with the same prefix should all be in the broad group,
        // while also having their own specific groups.
        // PhotoLab, PhotoLabs, PhotoLabPro all start with "PhotoLab"
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // "PhotoLab" variations
        std::fs::write(root.join("PhotoLab.Image.01.jpg"), "").unwrap();
        std::fs::write(root.join("PhotoLab.Image.02.jpg"), "").unwrap();
        std::fs::write(root.join("Photo.Lab.Image.03.jpg"), "").unwrap();

        // "PhotoLabs" (starts with PhotoLab)
        std::fs::write(root.join("PhotoLabs.Image.01.jpg"), "").unwrap();
        std::fs::write(root.join("PhotoLabs.Image.02.jpg"), "").unwrap();
        std::fs::write(root.join("PhotoLabs.Image.03.jpg"), "").unwrap();

        // "PhotoLabPro" (starts with PhotoLab)
        std::fs::write(root.join("PhotoLabPro.Image.01.jpg"), "").unwrap();
        std::fs::write(root.join("PhotoLabPro.Image.02.jpg"), "").unwrap();
        std::fs::write(root.join("PhotoLabPro.Image.03.jpg"), "").unwrap();

        let dirmove = DirMove::new(root, make_grouping_config(3));
        let files_with_names = dirmove.collect_files_with_names().unwrap();
        let groups = dirmove.collect_all_prefix_groups(&files_with_names);

        // "PhotoLab" should have ALL 9 files (broad group including extended prefixes)
        let photolab_group = groups
            .iter()
            .find(|(k, _)| k.to_lowercase().replace('.', "") == "photolab");
        assert!(photolab_group.is_some(), "Should find PhotoLab group");
        assert_eq!(
            photolab_group.unwrap().1.0.len(),
            9,
            "PhotoLab should have all 9 files (including PhotoLabs and PhotoLabPro)"
        );

        // "PhotoLabs" should have exactly 3 files (specific group)
        assert!(groups.contains_key("PhotoLabs"), "Should find PhotoLabs group");
        assert_eq!(
            groups.get("PhotoLabs").unwrap().0.len(),
            3,
            "PhotoLabs should have exactly 3 files"
        );

        // "PhotoLabPro" should have exactly 3 files (specific group)
        assert!(groups.contains_key("PhotoLabPro"), "Should find PhotoLabPro group");
        assert_eq!(
            groups.get("PhotoLabPro").unwrap().0.len(),
            3,
            "PhotoLabPro should have exactly 3 files"
        );
    }

    #[test]
    fn contiguity_resolution_in_middle_does_not_break() {
        // Resolution patterns in middle should be filtered but not break logical contiguity
        // if the meaningful parts are still adjacent in the original
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // "Studio.Production" is contiguous, resolution comes after
        std::fs::write(root.join("Studio.Production.1080p.Episode.01.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.Production.720p.Episode.02.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.Production.2160p.Episode.03.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.Production.480p.Episode.04.mp4"), "").unwrap();

        let dirmove = DirMove::new(root, make_grouping_config(3));
        let files_with_names = dirmove.collect_files_with_names().unwrap();
        let groups = dirmove.collect_all_prefix_groups(&files_with_names);

        // "Studio.Production" should have all 4 files
        assert!(
            groups.contains_key("Studio.Production"),
            "Should find Studio.Production group"
        );
        assert_eq!(
            groups.get("Studio.Production").unwrap().0.len(),
            4,
            "Studio.Production should have 4 files"
        );
    }

    #[test]
    fn contiguity_prefix_appears_multiple_times() {
        // Prefix parts appear multiple times in filename
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // "Alpha.Beta" appears at start AND later in name
        std::fs::write(root.join("Alpha.Beta.Content.Alpha.Beta.Repeat.mp4"), "").unwrap();
        std::fs::write(root.join("Alpha.Beta.Another.File.mp4"), "").unwrap();
        std::fs::write(root.join("Alpha.Beta.Third.Entry.mp4"), "").unwrap();

        let dirmove = DirMove::new(root, make_grouping_config(3));
        let files_with_names = dirmove.collect_files_with_names().unwrap();
        let groups = dirmove.collect_all_prefix_groups(&files_with_names);

        // Should find "Alpha.Beta" with all 3 files
        assert!(
            groups.contains_key("Alpha.Beta") || groups.contains_key("AlphaBeta"),
            "Should find Alpha.Beta group"
        );
        let alpha_beta_group = groups
            .iter()
            .find(|(k, _)| k.to_lowercase().replace('.', "") == "alphabeta");
        assert_eq!(alpha_beta_group.unwrap().1.0.len(), 3, "Alpha.Beta should have 3 files");
    }
}

/// Tests for files with varied prefixes before actual group names.
/// These tests ensure that:
/// 1. Files with ignored prefixes are properly grouped
/// 2. "Close" but non-matching files are excluded
/// 3. Completely different files are never incorrectly included
#[cfg(test)]
mod test_varied_prefix_grouping {
    use super::*;

    fn make_config_with_ignores(min_group_size: usize, ignores: Vec<String>) -> Config {
        Config {
            auto: false,
            create: true,
            debug: false,
            dryrun: true,
            include: Vec::new(),
            exclude: Vec::new(),
            ignored_group_names: Vec::new(),
            ignored_group_parts: Vec::new(),
            min_group_size,
            min_prefix_chars: 1,
            overwrite: false,
            prefix_ignores: ignores,
            prefix_overrides: Vec::new(),
            recurse: false,
            verbose: false,
            unpack_directory_names: Vec::new(),
        }
    }

    fn make_grouping_config(min_group_size: usize) -> Config {
        make_config_with_ignores(min_group_size, Vec::new())
    }

    // ===== Tests for prefix ignore functionality =====

    #[test]
    fn prefix_ignore_groups_files_with_and_without_prefix() {
        // Files with "Site" prefix should group with files without it when "Site" is ignored
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // Files WITH the ignored prefix
        std::fs::write(root.join("Site.StudioAlpha.Scene.001.mp4"), "").unwrap();
        std::fs::write(root.join("Site.StudioAlpha.Scene.002.mp4"), "").unwrap();
        std::fs::write(root.join("Site.StudioAlpha.Scene.003.mp4"), "").unwrap();

        // Files WITHOUT the prefix (same studio)
        std::fs::write(root.join("StudioAlpha.Scene.004.mp4"), "").unwrap();
        std::fs::write(root.join("StudioAlpha.Scene.005.mp4"), "").unwrap();

        // With position-agnostic matching, this file also matches StudioAlpha
        std::fs::write(root.join("SiteX.StudioAlpha.Scene.006.mp4"), "").unwrap();

        // Completely different files
        std::fs::write(root.join("RandomVideo.Episode.01.mp4"), "").unwrap();
        std::fs::write(root.join("UnrelatedContent.File.mp4"), "").unwrap();

        let dirmove = DirMove::new(root, make_config_with_ignores(3, vec!["Site".to_string()]));
        let files_with_names = dirmove.collect_files_with_names().unwrap();
        let groups = dirmove.collect_all_prefix_groups(&files_with_names);

        // StudioAlpha should have 6 files:
        // - 3 with Site prefix stripped
        // - 2 without prefix
        // - 1 with SiteX prefix (position-agnostic matching finds StudioAlpha anywhere)
        assert!(groups.contains_key("StudioAlpha"), "Should find StudioAlpha group");
        assert_eq!(
            groups.get("StudioAlpha").unwrap().0.len(),
            6,
            "StudioAlpha should have 6 files (position-agnostic matching)"
        );

        // RandomVideo and UnrelatedContent should NOT be in StudioAlpha
        let studio_files = &groups.get("StudioAlpha").unwrap().0;
        assert!(
            !studio_files.iter().any(|p| p.to_string_lossy().contains("RandomVideo")),
            "RandomVideo should not be in StudioAlpha group"
        );
        assert!(
            !studio_files
                .iter()
                .any(|p| p.to_string_lossy().contains("UnrelatedContent")),
            "UnrelatedContent should not be in StudioAlpha group"
        );
    }

    #[test]
    fn prefix_ignore_multiple_ignored_prefixes() {
        // Test with multiple ignored prefixes
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // Various prefix combinations - all should group together
        std::fs::write(root.join("Premium.HD.ContentCreator.Video.001.mp4"), "").unwrap();
        std::fs::write(root.join("Premium.ContentCreator.Video.002.mp4"), "").unwrap();
        std::fs::write(root.join("HD.ContentCreator.Video.003.mp4"), "").unwrap();
        std::fs::write(root.join("ContentCreator.Video.004.mp4"), "").unwrap();
        std::fs::write(root.join("ContentCreator.Video.005.mp4"), "").unwrap();

        // Different content that should NOT match - totally different first parts
        std::fs::write(root.join("Premium.OtherStudio.File.001.mp4"), "").unwrap();
        std::fs::write(root.join("MyContentMaker.Video.001.mp4"), "").unwrap(); // Different name entirely

        let dirmove = DirMove::new(
            root,
            make_config_with_ignores(3, vec!["Premium".to_string(), "HD".to_string()]),
        );
        let files_with_names = dirmove.collect_files_with_names().unwrap();
        let groups = dirmove.collect_all_prefix_groups(&files_with_names);

        // ContentCreator should have all 5 matching files
        assert!(
            groups.contains_key("ContentCreator"),
            "Should find ContentCreator group"
        );
        let cc_files = &groups.get("ContentCreator").unwrap().0;

        // All 5 ContentCreator files should be present
        let content_creator_count = cc_files
            .iter()
            .filter(|p| p.to_string_lossy().contains("ContentCreator.Video"))
            .count();
        assert_eq!(
            content_creator_count, 5,
            "ContentCreator should have all 5 ContentCreator.Video files"
        );

        // MyContentMaker should NOT be included (different name)
        assert!(
            !cc_files.iter().any(|p| p.to_string_lossy().contains("MyContentMaker")),
            "MyContentMaker should not be in ContentCreator group"
        );
    }

    #[test]
    fn prefix_ignore_case_insensitive() {
        // Ignored prefixes should work case-insensitively
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("PREMIUM.StudioBeta.Film.01.mp4"), "").unwrap();
        std::fs::write(root.join("Premium.StudioBeta.Film.02.mp4"), "").unwrap();
        std::fs::write(root.join("premium.StudioBeta.Film.03.mp4"), "").unwrap();
        std::fs::write(root.join("StudioBeta.Film.04.mp4"), "").unwrap();

        // Different studio
        std::fs::write(root.join("Premium.StudioGamma.Movie.01.mp4"), "").unwrap();

        let dirmove = DirMove::new(root, make_config_with_ignores(3, vec!["PREMIUM".to_string()]));
        let files_with_names = dirmove.collect_files_with_names().unwrap();
        let groups = dirmove.collect_all_prefix_groups(&files_with_names);

        assert!(groups.contains_key("StudioBeta"), "Should find StudioBeta group");
        assert_eq!(
            groups.get("StudioBeta").unwrap().0.len(),
            4,
            "StudioBeta should have 4 files (case variations stripped)"
        );
    }

    #[test]
    fn prefix_ignore_does_not_strip_from_middle() {
        // Ignored prefixes should only be stripped from the START, not middle
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("Site.MainChannel.Content.01.mp4"), "").unwrap();
        std::fs::write(root.join("Site.MainChannel.Content.02.mp4"), "").unwrap();
        std::fs::write(root.join("Site.MainChannel.Content.03.mp4"), "").unwrap();

        // Completely different
        std::fs::write(root.join("OtherChannel.Video.mp4"), "").unwrap();

        let dirmove = DirMove::new(root, make_config_with_ignores(3, vec!["Site".to_string()]));
        let files_with_names = dirmove.collect_files_with_names().unwrap();
        let groups = dirmove.collect_all_prefix_groups(&files_with_names);

        // MainChannel should have exactly 3 files (the ones with Site prefix stripped)
        assert!(groups.contains_key("MainChannel"), "Should find MainChannel group");
        let mc_files = &groups.get("MainChannel").unwrap().0;
        assert_eq!(mc_files.len(), 3, "MainChannel should have exactly 3 files");
    }

    // ===== Tests for near-matches - verifying grouping boundaries =====

    #[test]
    fn distinct_studio_names_form_separate_groups() {
        // Different studio names should form their own groups
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // StudioAlpha files
        std::fs::write(root.join("StudioAlpha.Movie.001.mp4"), "").unwrap();
        std::fs::write(root.join("StudioAlpha.Movie.002.mp4"), "").unwrap();
        std::fs::write(root.join("StudioAlpha.Movie.003.mp4"), "").unwrap();

        // StudioBeta files - completely different
        std::fs::write(root.join("StudioBeta.Film.001.mp4"), "").unwrap();
        std::fs::write(root.join("StudioBeta.Film.002.mp4"), "").unwrap();
        std::fs::write(root.join("StudioBeta.Film.003.mp4"), "").unwrap();

        // Completely different
        std::fs::write(root.join("OtherProduction.Episode.01.mp4"), "").unwrap();
        std::fs::write(root.join("RandomFile.txt"), "").unwrap();

        let dirmove = DirMove::new(root, make_grouping_config(3));
        let files_with_names = dirmove.collect_files_with_names().unwrap();
        let groups = dirmove.collect_all_prefix_groups(&files_with_names);

        // StudioAlpha should have exactly 3 files
        assert!(groups.contains_key("StudioAlpha"), "Should find StudioAlpha group");
        let studio_a_files = &groups.get("StudioAlpha").unwrap().0;
        assert_eq!(studio_a_files.len(), 3, "StudioAlpha should have exactly 3 files");

        // StudioBeta should have exactly 3 files
        assert!(groups.contains_key("StudioBeta"), "Should find StudioBeta group");
        assert_eq!(groups.get("StudioBeta").unwrap().0.len(), 3);

        // Verify StudioBeta files are NOT in StudioAlpha group
        for file in studio_a_files {
            let name = file.to_string_lossy();
            assert!(
                !name.contains("StudioBeta"),
                "StudioBeta should not be in StudioAlpha group"
            );
        }
    }

    #[test]
    fn near_match_extra_prefix_not_grouped() {
        // "MyStudio" should NOT be grouped with "Studio" files
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // "Network" as first part
        std::fs::write(root.join("Network.Show.Episode.01.mp4"), "").unwrap();
        std::fs::write(root.join("Network.Show.Episode.02.mp4"), "").unwrap();
        std::fs::write(root.join("Network.Show.Episode.03.mp4"), "").unwrap();

        // Files with extra prefix before "Network" - should NOT match
        std::fs::write(root.join("OldNetwork.Show.Episode.01.mp4"), "").unwrap();
        std::fs::write(root.join("NewNetwork.Show.Episode.01.mp4"), "").unwrap();
        std::fs::write(root.join("TheNetwork.Show.Episode.01.mp4"), "").unwrap();

        // Completely unrelated
        std::fs::write(root.join("Documentary.Nature.mp4"), "").unwrap();

        let dirmove = DirMove::new(root, make_grouping_config(2));
        let files_with_names = dirmove.collect_files_with_names().unwrap();
        let groups = dirmove.collect_all_prefix_groups(&files_with_names);

        // Network.Show should exist with exactly 3 files
        let network_group = groups
            .iter()
            .find(|(k, _)| k.contains("Network") && !k.contains("Old") && !k.contains("New") && !k.contains("The"));
        assert!(network_group.is_some(), "Should find Network group");
        assert_eq!(
            network_group.unwrap().1.0.len(),
            3,
            "Network group should have exactly 3 files"
        );
    }

    #[test]
    fn near_match_similar_names_different_groups() {
        // Similar but distinct names should form separate groups
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // First group: "DarkStar"
        std::fs::write(root.join("DarkStar.Episode.01.mp4"), "").unwrap();
        std::fs::write(root.join("DarkStar.Episode.02.mp4"), "").unwrap();
        std::fs::write(root.join("DarkStar.Episode.03.mp4"), "").unwrap();

        // Second group: "DarkStorm" (similar prefix)
        std::fs::write(root.join("DarkStorm.Video.01.mp4"), "").unwrap();
        std::fs::write(root.join("DarkStorm.Video.02.mp4"), "").unwrap();
        std::fs::write(root.join("DarkStorm.Video.03.mp4"), "").unwrap();

        // Third group: "StarDark" (reversed, completely different)
        std::fs::write(root.join("StarDark.Film.01.mp4"), "").unwrap();
        std::fs::write(root.join("StarDark.Film.02.mp4"), "").unwrap();

        // Unrelated
        std::fs::write(root.join("LightStar.Content.mp4"), "").unwrap();
        std::fs::write(root.join("BrightStorm.Content.mp4"), "").unwrap();

        let dirmove = DirMove::new(root, make_grouping_config(2));
        let files_with_names = dirmove.collect_files_with_names().unwrap();
        let groups = dirmove.collect_all_prefix_groups(&files_with_names);

        // DarkStar should have exactly 3 files
        assert!(groups.contains_key("DarkStar"), "Should find DarkStar group");
        assert_eq!(groups.get("DarkStar").unwrap().0.len(), 3);

        // DarkStorm should have exactly 3 files
        assert!(groups.contains_key("DarkStorm"), "Should find DarkStorm group");
        assert_eq!(groups.get("DarkStorm").unwrap().0.len(), 3);

        // StarDark should have exactly 2 files (not mixed with DarkStar)
        assert!(groups.contains_key("StarDark"), "Should find StarDark group");
        assert_eq!(groups.get("StarDark").unwrap().0.len(), 2);
    }

    #[test]
    fn substring_in_middle_not_matched() {
        // A prefix appearing in the middle of another name should not cause grouping
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // "Alpha" as standalone first part
        std::fs::write(root.join("Alpha.Project.File.01.mp4"), "").unwrap();
        std::fs::write(root.join("Alpha.Project.File.02.mp4"), "").unwrap();
        std::fs::write(root.join("Alpha.Project.File.03.mp4"), "").unwrap();

        // "Alpha" appears in middle of concatenated name - should NOT group with Alpha
        std::fs::write(root.join("BetaAlpha.Content.01.mp4"), "").unwrap();
        std::fs::write(root.join("GammaAlphaOmega.Content.01.mp4"), "").unwrap();

        // "Alpha" as suffix - should NOT group
        std::fs::write(root.join("TeamAlpha.Mission.01.mp4"), "").unwrap();
        std::fs::write(root.join("TeamAlpha.Mission.02.mp4"), "").unwrap();

        // Completely different
        std::fs::write(root.join("Beta.Different.File.mp4"), "").unwrap();
        std::fs::write(root.join("Gamma.Another.File.mp4"), "").unwrap();

        let dirmove = DirMove::new(root, make_grouping_config(2));
        let files_with_names = dirmove.collect_files_with_names().unwrap();
        let groups = dirmove.collect_all_prefix_groups(&files_with_names);

        // Alpha.Project should have exactly 3 files
        let alpha_group = groups.iter().find(|(k, _)| {
            k.as_str() == "Alpha"
                || k.as_str() == "Alpha.Project"
                || k.as_str() == "AlphaProject"
                || k.as_str() == "Alpha.Project.File"
        });
        assert!(alpha_group.is_some(), "Should find Alpha group");
        let alpha_files = &alpha_group.unwrap().1.0;
        assert_eq!(alpha_files.len(), 3, "Alpha group should have exactly 3 files");

        // Verify BetaAlpha and TeamAlpha are not in Alpha group
        for file in alpha_files {
            let name = file.to_string_lossy();
            assert!(!name.contains("BetaAlpha"), "BetaAlpha should not be in Alpha group");
            assert!(!name.contains("TeamAlpha"), "TeamAlpha should not be in Alpha group");
            assert!(
                !name.contains("GammaAlpha"),
                "GammaAlphaOmega should not be in Alpha group"
            );
        }

        // TeamAlpha should be its own group
        assert!(groups.contains_key("TeamAlpha"), "Should find TeamAlpha group");
        assert_eq!(groups.get("TeamAlpha").unwrap().0.len(), 2);
    }

    // ===== Tests for completely different files not being grouped =====

    #[test]
    fn completely_different_files_isolated() {
        // Ensure completely unrelated files are never grouped together
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // Group A: "Phoenix"
        std::fs::write(root.join("Phoenix.Series.S01E01.mp4"), "").unwrap();
        std::fs::write(root.join("Phoenix.Series.S01E02.mp4"), "").unwrap();
        std::fs::write(root.join("Phoenix.Series.S01E03.mp4"), "").unwrap();

        // Group B: "Dragon"
        std::fs::write(root.join("Dragon.Movie.Part1.mp4"), "").unwrap();
        std::fs::write(root.join("Dragon.Movie.Part2.mp4"), "").unwrap();
        std::fs::write(root.join("Dragon.Movie.Part3.mp4"), "").unwrap();

        // Completely different individual files
        std::fs::write(root.join("vacation_photo.jpg"), "").unwrap();
        std::fs::write(root.join("document.pdf"), "").unwrap();
        std::fs::write(root.join("readme.txt"), "").unwrap();
        std::fs::write(root.join("config.json"), "").unwrap();
        std::fs::write(root.join("SingleFile.mp4"), "").unwrap();

        let dirmove = DirMove::new(root, make_grouping_config(3));
        let files_with_names = dirmove.collect_files_with_names().unwrap();
        let groups = dirmove.collect_all_prefix_groups(&files_with_names);

        // Phoenix should have exactly 3 files
        let phoenix_group = groups.iter().find(|(k, _)| k.contains("Phoenix"));
        assert!(phoenix_group.is_some(), "Should find Phoenix group");
        assert_eq!(phoenix_group.unwrap().1.0.len(), 3);

        // Dragon should have exactly 3 files
        let dragon_group = groups.iter().find(|(k, _)| k.contains("Dragon"));
        assert!(dragon_group.is_some(), "Should find Dragon group");
        assert_eq!(dragon_group.unwrap().1.0.len(), 3);

        // Verify random files are not in any main group
        for (files, _, _) in groups.values() {
            for file in files {
                let name = file.to_string_lossy();
                assert!(
                    !name.contains("vacation_photo"),
                    "vacation_photo should not be in any group"
                );
                assert!(
                    !name.contains("document.pdf"),
                    "document.pdf should not be in any group"
                );
                assert!(!name.contains("readme.txt"), "readme.txt should not be in any group");
            }
        }
    }

    #[test]
    fn mixed_content_with_noise_files() {
        // Real-world scenario with target files mixed among noise
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // Target group: "StudioPremium"
        std::fs::write(root.join("StudioPremium.Show.Episode.01.1080p.mp4"), "").unwrap();
        std::fs::write(root.join("StudioPremium.Show.Episode.02.1080p.mp4"), "").unwrap();
        std::fs::write(root.join("StudioPremium.Show.Episode.03.1080p.mp4"), "").unwrap();
        std::fs::write(root.join("StudioPremium.Show.Episode.04.1080p.mp4"), "").unwrap();

        // Noise: similar-ish names
        std::fs::write(root.join("Studio.Basic.Content.mp4"), "").unwrap();
        std::fs::write(root.join("PremiumContent.Video.mp4"), "").unwrap();
        std::fs::write(root.join("MyStudioPremium.Fake.mp4"), "").unwrap();

        // Noise: completely random
        std::fs::write(root.join("IMG_20240101_001.jpg"), "").unwrap();
        std::fs::write(root.join("IMG_20240101_002.jpg"), "").unwrap();
        std::fs::write(root.join("screenshot_2024.png"), "").unwrap();
        std::fs::write(root.join("notes.txt"), "").unwrap();
        std::fs::write(root.join("backup.zip"), "").unwrap();

        let dirmove = DirMove::new(root, make_grouping_config(3));
        let files_with_names = dirmove.collect_files_with_names().unwrap();
        let groups = dirmove.collect_all_prefix_groups(&files_with_names);

        // StudioPremium should have exactly 4 files
        assert!(groups.contains_key("StudioPremium"), "Should find StudioPremium group");
        let sp_files = &groups.get("StudioPremium").unwrap().0;
        assert_eq!(sp_files.len(), 4, "StudioPremium should have exactly 4 files");

        // Verify no noise is included
        for file in sp_files {
            let name = file.to_string_lossy();
            assert!(
                name.contains("StudioPremium.Show"),
                "Only StudioPremium.Show files should be in group, got: {name}"
            );
        }
    }

    // ===== Complex scenarios combining prefix ignores and near-matches =====

    #[test]
    fn prefix_ignore_with_similar_non_matching_files() {
        // Test prefix ignore with close but non-matching files present
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // Files with ignored prefix "Web"
        std::fs::write(root.join("Web.ChannelX.Video.001.mp4"), "").unwrap();
        std::fs::write(root.join("Web.ChannelX.Video.002.mp4"), "").unwrap();
        std::fs::write(root.join("Web.ChannelX.Video.003.mp4"), "").unwrap();

        // Files without prefix (same channel)
        std::fs::write(root.join("ChannelX.Video.004.mp4"), "").unwrap();
        std::fs::write(root.join("ChannelX.Video.005.mp4"), "").unwrap();

        // Close names that should NOT match - different channel names
        std::fs::write(root.join("Web.ChannelY.Different.001.mp4"), "").unwrap();
        std::fs::write(root.join("ChannelZ.Video.001.mp4"), "").unwrap();

        // Completely different
        std::fs::write(root.join("Unrelated.Random.File.mp4"), "").unwrap();
        std::fs::write(root.join("Another.Completely.Different.File.mp4"), "").unwrap();

        let dirmove = DirMove::new(root, make_config_with_ignores(3, vec!["Web".to_string()]));
        let files_with_names = dirmove.collect_files_with_names().unwrap();
        let groups = dirmove.collect_all_prefix_groups(&files_with_names);

        // ChannelX should have 5 files (3 with Web stripped + 2 without prefix)
        assert!(groups.contains_key("ChannelX"), "Should find ChannelX group");
        let channel_files = &groups.get("ChannelX").unwrap().0;
        assert_eq!(channel_files.len(), 5, "ChannelX should have exactly 5 files");

        // Verify close-but-different files are NOT included
        for file in channel_files {
            let name = file.to_string_lossy();
            assert!(!name.contains("ChannelY"), "ChannelY should not be in ChannelX group");
            assert!(!name.contains("ChannelZ"), "ChannelZ should not be in ChannelX group");
        }
    }

    #[test]
    fn multiple_prefix_ignores_comprehensive() {
        // Test multiple ignored prefixes with various edge cases
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // Target files with various ignored prefixes
        std::fs::write(root.join("AAA.BBB.TargetStudio.Content.01.mp4"), "").unwrap();
        std::fs::write(root.join("AAA.TargetStudio.Content.02.mp4"), "").unwrap();
        std::fs::write(root.join("BBB.TargetStudio.Content.03.mp4"), "").unwrap();
        std::fs::write(root.join("TargetStudio.Content.04.mp4"), "").unwrap();
        std::fs::write(root.join("TargetStudio.Content.05.mp4"), "").unwrap();

        // Files that look similar but should not match - totally different studios
        std::fs::write(root.join("OtherStudio.Content.01.mp4"), "").unwrap();
        std::fs::write(root.join("DifferentStudio.Content.01.mp4"), "").unwrap();

        // Completely unrelated noise
        std::fs::write(root.join("SomethingElse.Entirely.mp4"), "").unwrap();
        std::fs::write(root.join("NoMatch.AtAll.mp4"), "").unwrap();
        std::fs::write(root.join("random_file_123.mp4"), "").unwrap();

        let dirmove = DirMove::new(
            root,
            make_config_with_ignores(3, vec!["AAA".to_string(), "BBB".to_string()]),
        );
        let files_with_names = dirmove.collect_files_with_names().unwrap();
        let groups = dirmove.collect_all_prefix_groups(&files_with_names);

        // TargetStudio should have exactly 5 files
        assert!(groups.contains_key("TargetStudio"), "Should find TargetStudio group");
        let ts_files = &groups.get("TargetStudio").unwrap().0;
        assert_eq!(ts_files.len(), 5, "TargetStudio should have exactly 5 files");

        // Verify no wrong files included
        for file in ts_files {
            let name = file.to_string_lossy();
            assert!(
                name.contains("TargetStudio.Content"),
                "Only TargetStudio.Content files should be in group, got: {name}"
            );
        }
    }

    #[test]
    fn dotted_vs_concatenated_with_varied_prefixes() {
        // Test that dotted and concatenated forms group correctly even with prefixes
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // Concatenated form: "GoldenEagle"
        std::fs::write(root.join("GoldenEagle.Documentary.01.mp4"), "").unwrap();
        std::fs::write(root.join("GoldenEagle.Documentary.02.mp4"), "").unwrap();

        // Dotted form: "Golden.Eagle"
        std::fs::write(root.join("Golden.Eagle.Documentary.03.mp4"), "").unwrap();
        std::fs::write(root.join("Golden.Eagle.Documentary.04.mp4"), "").unwrap();

        // With ignored prefix
        std::fs::write(root.join("HD.GoldenEagle.Documentary.05.mp4"), "").unwrap();
        std::fs::write(root.join("HD.Golden.Eagle.Documentary.06.mp4"), "").unwrap();

        // Completely different content - different prefixes
        std::fs::write(root.join("SilverEagle.Other.01.mp4"), "").unwrap();
        std::fs::write(root.join("GoldenHawk.Similar.01.mp4"), "").unwrap();

        let dirmove = DirMove::new(root, make_config_with_ignores(3, vec!["HD".to_string()]));
        let files_with_names = dirmove.collect_files_with_names().unwrap();
        let groups = dirmove.collect_all_prefix_groups(&files_with_names);

        // Should find GoldenEagle (or Golden.Eagle) group with 6 files
        let golden_eagle = groups
            .iter()
            .find(|(k, _)| k.to_lowercase().replace('.', "") == "goldeneagle");
        assert!(golden_eagle.is_some(), "Should find GoldenEagle group");
        assert_eq!(
            golden_eagle.unwrap().1.0.len(),
            6,
            "GoldenEagle should have 6 files (both forms + with prefix)"
        );
    }

    #[test]
    fn three_part_prefix_with_ignores_and_noise() {
        // Test three-part prefixes with ignored prefixes and noise files
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // Three-part prefix group
        std::fs::write(root.join("Big.Red.Studio.Film.001.mp4"), "").unwrap();
        std::fs::write(root.join("Big.Red.Studio.Film.002.mp4"), "").unwrap();
        std::fs::write(root.join("Big.Red.Studio.Film.003.mp4"), "").unwrap();
        std::fs::write(root.join("BigRedStudio.Film.004.mp4"), "").unwrap();

        // With ignored prefix
        std::fs::write(root.join("Premium.Big.Red.Studio.Film.005.mp4"), "").unwrap();
        std::fs::write(root.join("Premium.BigRedStudio.Film.006.mp4"), "").unwrap();

        // Close but not matching - different words entirely
        std::fs::write(root.join("Big.Blue.House.Film.001.mp4"), "").unwrap(); // Different second and third
        std::fs::write(root.join("Small.Red.Barn.Film.001.mp4"), "").unwrap(); // Different first and third

        // Noise
        std::fs::write(root.join("Unrelated.Content.File.mp4"), "").unwrap();
        std::fs::write(root.join("Random.Video.Here.mp4"), "").unwrap();

        let dirmove = DirMove::new(root, make_config_with_ignores(3, vec!["Premium".to_string()]));
        let files_with_names = dirmove.collect_files_with_names().unwrap();
        let groups = dirmove.collect_all_prefix_groups(&files_with_names);

        // Big.Red.Studio (or BigRedStudio) should have 6 files
        let brs_group = groups
            .iter()
            .find(|(k, _)| k.to_lowercase().replace('.', "") == "bigredstudio");
        assert!(brs_group.is_some(), "Should find BigRedStudio group");
        assert_eq!(brs_group.unwrap().1.0.len(), 6, "BigRedStudio should have 6 files");

        // Verify close-but-different files are excluded
        let brs_files = &brs_group.unwrap().1.0;
        for file in brs_files {
            let name = file.to_string_lossy();
            assert!(!name.contains("Big.Blue"), "Big.Blue should not match");
            assert!(!name.contains("Small.Red"), "Small.Red should not match");
        }
    }

    #[test]
    fn single_letter_prefix_ignore() {
        // Test that single-letter prefix ignores work correctly
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // Files with single-letter prefix to ignore
        std::fs::write(root.join("X.MainContent.Video.001.mp4"), "").unwrap();
        std::fs::write(root.join("X.MainContent.Video.002.mp4"), "").unwrap();
        std::fs::write(root.join("X.MainContent.Video.003.mp4"), "").unwrap();

        // Files without prefix
        std::fs::write(root.join("MainContent.Video.004.mp4"), "").unwrap();
        std::fs::write(root.join("MainContent.Video.005.mp4"), "").unwrap();

        // Files where X is part of the name (should NOT be affected)
        std::fs::write(root.join("XMainContent.Wrong.001.mp4"), "").unwrap();
        std::fs::write(root.join("MainXContent.Other.001.mp4"), "").unwrap();

        // Noise
        std::fs::write(root.join("OtherStuff.File.mp4"), "").unwrap();

        let dirmove = DirMove::new(root, make_config_with_ignores(3, vec!["X".to_string()]));
        let files_with_names = dirmove.collect_files_with_names().unwrap();
        let groups = dirmove.collect_all_prefix_groups(&files_with_names);

        // MainContent should have 5 files
        assert!(groups.contains_key("MainContent"), "Should find MainContent group");
        let mc_files = &groups.get("MainContent").unwrap().0;
        assert_eq!(mc_files.len(), 5, "MainContent should have 5 files");

        // XMainContent and MainXContent should NOT be included
        for file in mc_files {
            let name = file.to_string_lossy();
            assert!(!name.contains("XMainContent"), "XMainContent should not be in group");
            assert!(!name.contains("MainXContent"), "MainXContent should not be in group");
        }
    }

    #[test]
    fn numeric_looking_prefix_ignore() {
        // Test prefix ignore that looks like it could be a year or number
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // Note: numeric parts are filtered differently, this tests a text prefix
        std::fs::write(root.join("Ver2.StudioName.Content.01.mp4"), "").unwrap();
        std::fs::write(root.join("Ver2.StudioName.Content.02.mp4"), "").unwrap();
        std::fs::write(root.join("Ver2.StudioName.Content.03.mp4"), "").unwrap();
        std::fs::write(root.join("StudioName.Content.04.mp4"), "").unwrap();
        std::fs::write(root.join("StudioName.Content.05.mp4"), "").unwrap();

        // Different version prefix - with position-agnostic matching, this also matches StudioName
        std::fs::write(root.join("Ver3.StudioName.Different.01.mp4"), "").unwrap();

        // Noise
        std::fs::write(root.join("Ver2StudioName.Wrong.01.mp4"), "").unwrap(); // No dot
        std::fs::write(root.join("Random.Other.File.mp4"), "").unwrap();

        let dirmove = DirMove::new(root, make_config_with_ignores(3, vec!["Ver2".to_string()]));
        let files_with_names = dirmove.collect_files_with_names().unwrap();
        let groups = dirmove.collect_all_prefix_groups(&files_with_names);

        // StudioName should have 6 files:
        // - 3 with Ver2 stripped
        // - 2 without prefix
        // - 1 with Ver3 prefix (position-agnostic matching finds StudioName anywhere)
        assert!(groups.contains_key("StudioName"), "Should find StudioName group");
        let sn_files = &groups.get("StudioName").unwrap().0;
        assert_eq!(sn_files.len(), 6, "StudioName should have 6 files");
    }

    #[test]
    fn empty_prefix_ignore_list_no_effect() {
        // Verify empty prefix ignore list doesn't affect grouping
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("Prefix.Target.File.01.mp4"), "").unwrap();
        std::fs::write(root.join("Prefix.Target.File.02.mp4"), "").unwrap();
        std::fs::write(root.join("Prefix.Target.File.03.mp4"), "").unwrap();
        std::fs::write(root.join("Target.File.04.mp4"), "").unwrap();

        // With empty ignore list, "Prefix.Target" files should group separately
        let dirmove = DirMove::new(root, make_config_with_ignores(3, Vec::new()));
        let files_with_names = dirmove.collect_files_with_names().unwrap();
        let groups = dirmove.collect_all_prefix_groups(&files_with_names);

        // Prefix.Target should have 3 files (no prefix stripping)
        let pt_group = groups
            .iter()
            .find(|(k, _)| k.contains("Prefix") && k.contains("Target"));
        assert!(pt_group.is_some(), "Should find Prefix.Target group");
        assert_eq!(
            pt_group.unwrap().1.0.len(),
            3,
            "Prefix.Target should have 3 files (no stripping)"
        );
    }

    /// Tests for the `min_prefix_chars` configuration option.
    /// This option sets the minimum character count for single-word prefixes
    /// to be considered valid group names. Default is 5 to avoid false matches
    /// with short names like "alex", "name", etc.
    #[cfg(test)]
    mod test_min_prefix_chars {
        use super::test_helpers::*;
        use super::*;

        fn make_config_with_min_chars(min_group_size: usize, min_prefix_chars: usize) -> Config {
            Config {
                auto: false,
                create: true,
                debug: false,
                dryrun: true,
                include: Vec::new(),
                exclude: Vec::new(),
                ignored_group_names: Vec::new(),
                ignored_group_parts: Vec::new(),
                min_group_size,
                min_prefix_chars,
                overwrite: false,
                prefix_ignores: Vec::new(),
                prefix_overrides: Vec::new(),
                recurse: false,
                verbose: false,
                unpack_directory_names: Vec::new(),
            }
        }

        // ===== Unit tests for find_prefix_candidates =====

        #[test]
        fn short_prefix_excluded_with_default_min_chars() {
            // "Alex" has 4 chars, should be excluded with min_prefix_chars=5
            let files = make_test_files(&["Alex.Video.001.mp4", "Alex.Video.002.mp4", "Alex.Video.003.mp4"]);
            let candidates = utils::find_prefix_candidates("Alex.Video.001.mp4", &files, 2, 5);

            // "Alex" (4 chars) should be excluded, but "Video" (5 chars) is found with position-agnostic matching
            assert!(
                !candidates.iter().any(|c| c.prefix == "Alex"),
                "Single-part prefix 'Alex' (4 chars) should be excluded with min_prefix_chars=5"
            );
            assert!(
                candidates.iter().any(|c| c.prefix == "Video"),
                "Single-part prefix 'Video' (5 chars) should be included with position-agnostic matching"
            );
            assert!(
                candidates.iter().any(|c| c.part_count == 2 && c.prefix == "Alex.Video"),
                "Two-part prefix 'Alex.Video' should still be found"
            );
        }

        #[test]
        fn short_prefix_included_with_low_min_chars() {
            // "Alex" has 4 chars, should be included with min_prefix_chars=4
            let files = make_test_files(&["Alex.Video.001.mp4", "Alex.Video.002.mp4", "Alex.Video.003.mp4"]);
            let candidates = utils::find_prefix_candidates("Alex.Video.001.mp4", &files, 2, 4);

            // Should find single-part prefix "Alex"
            let single_part = candidates.iter().find(|c| c.part_count == 1);
            assert!(
                single_part.is_some(),
                "Single-part prefix 'Alex' should be included with min_prefix_chars=4"
            );
            assert_eq!(single_part.unwrap().prefix, "Alex");
        }

        #[test]
        fn exact_threshold_includes_prefix() {
            // "Names" has exactly 5 chars, should be included with min_prefix_chars=5
            let files = make_test_files(&["Names.List.001.txt", "Names.List.002.txt", "Names.List.003.txt"]);
            let candidates = utils::find_prefix_candidates("Names.List.001.txt", &files, 2, 5);

            let single_part = candidates.iter().find(|c| c.part_count == 1);
            assert!(
                single_part.is_some(),
                "Single-part prefix 'Names' (5 chars) should be included"
            );
            assert_eq!(single_part.unwrap().prefix, "Names");
        }

        #[test]
        fn long_prefix_always_included() {
            // "Alexander" has 9 chars, should always be included
            let files = make_test_files(&[
                "Alexander.Movie.001.mp4",
                "Alexander.Movie.002.mp4",
                "Alexander.Movie.003.mp4",
            ]);
            let candidates = utils::find_prefix_candidates("Alexander.Movie.001.mp4", &files, 2, 5);

            let single_part = candidates.iter().find(|c| c.part_count == 1);
            assert!(
                single_part.is_some(),
                "Single-part prefix 'Alexander' (9 chars) should be included"
            );
            assert_eq!(single_part.unwrap().prefix, "Alexander");
        }

        #[test]
        fn two_part_prefix_affected_by_min_chars() {
            // Two-part prefixes ARE affected by min_prefix_chars (counts chars excluding dots)
            let files = make_test_files(&["AB.CD.File.001.mp4", "AB.CD.File.002.mp4", "AB.CD.File.003.mp4"]);
            // With high min_prefix_chars=10, "AB.CD" (4 chars) should be excluded
            let candidates = utils::find_prefix_candidates("AB.CD.File.001.mp4", &files, 2, 10);

            let two_part = candidates.iter().find(|c| c.part_count == 2);
            assert!(
                two_part.is_none(),
                "Two-part prefix 'AB.CD' (4 chars) should be excluded with min_prefix_chars=10"
            );

            // Single-part "AB" (2 chars) should also be excluded
            let single_part = candidates.iter().find(|c| c.part_count == 1);
            assert!(
                single_part.is_none(),
                "Single-part prefix 'AB' should be excluded with min_prefix_chars=10"
            );

            // But with lower threshold, it should be included
            let candidates = utils::find_prefix_candidates("AB.CD.File.001.mp4", &files, 2, 4);
            let two_part = candidates.iter().find(|c| c.part_count == 2);
            assert!(
                two_part.is_some(),
                "Two-part prefix 'AB.CD' (4 chars) should be included with min_prefix_chars=4"
            );
        }

        #[test]
        fn unicode_chars_counted_correctly() {
            // Unicode characters should be counted as single chars, not bytes
            // "日本語" has 3 chars but 9 bytes in UTF-8
            let files = make_test_files(&["日本語.Video.001.mp4", "日本語.Video.002.mp4", "日本語.Video.003.mp4"]);

            // With min_prefix_chars=3, "日本語" (3 chars) should be included
            let candidates = utils::find_prefix_candidates("日本語.Video.001.mp4", &files, 2, 3);
            let unicode_prefix = candidates.iter().find(|c| c.prefix == "日本語");
            assert!(
                unicode_prefix.is_some(),
                "Unicode prefix '日本語' (3 chars) should be included with min=3"
            );

            // With min_prefix_chars=4, "日本語" (3 chars) should be excluded
            // but "Video" (5 chars) is still found with position-agnostic matching
            let candidates = utils::find_prefix_candidates("日本語.Video.001.mp4", &files, 2, 4);
            let unicode_prefix = candidates.iter().find(|c| c.prefix == "日本語");
            assert!(
                unicode_prefix.is_none(),
                "Unicode prefix '日本語' (3 chars) should be excluded with min=4"
            );
            // Video (5 chars) is still found
            let video_prefix = candidates.iter().find(|c| c.prefix == "Video");
            assert!(video_prefix.is_some(), "Video (5 chars) should be included with min=4");
        }

        #[test]
        fn min_chars_zero_allows_all() {
            // With min_prefix_chars=0, even single-char prefixes should work
            let files = make_test_files(&["A.Video.001.mp4", "A.Video.002.mp4", "A.Video.003.mp4"]);
            let candidates = utils::find_prefix_candidates("A.Video.001.mp4", &files, 2, 0);

            let single_part = candidates.iter().find(|c| c.part_count == 1);
            assert!(
                single_part.is_some(),
                "Single-char prefix 'A' should be included with min_prefix_chars=0"
            );
        }

        #[test]
        fn min_chars_one_allows_single_char() {
            // With min_prefix_chars=1, single-char prefixes should work
            let files = make_test_files(&["X.Files.S01E01.mp4", "X.Files.S01E02.mp4", "X.Files.S01E03.mp4"]);
            let candidates = utils::find_prefix_candidates("X.Files.S01E01.mp4", &files, 2, 1);

            let single_part = candidates.iter().find(|c| c.part_count == 1);
            assert!(
                single_part.is_some(),
                "Single-char prefix 'X' should be included with min_prefix_chars=1"
            );
        }

        // ===== Integration tests with collect_all_prefix_groups =====

        #[test]
        fn short_names_not_grouped_with_default_config() {
            let tmp = tempfile::TempDir::new().unwrap();
            let root = tmp.path().to_path_buf();

            // Short prefix "Alex" (4 chars) - should not form single-word group
            std::fs::write(root.join("Alex.Scene.001.mp4"), "").unwrap();
            std::fs::write(root.join("Alex.Scene.002.mp4"), "").unwrap();
            std::fs::write(root.join("Alex.Scene.003.mp4"), "").unwrap();

            // Long prefix "Alexander" (9 chars) - should form group
            std::fs::write(root.join("Alexander.Movie.001.mp4"), "").unwrap();
            std::fs::write(root.join("Alexander.Movie.002.mp4"), "").unwrap();
            std::fs::write(root.join("Alexander.Movie.003.mp4"), "").unwrap();

            let dirmove = DirMove::new(root, make_config_with_min_chars(3, 5));
            let files_with_names = dirmove.collect_files_with_names().unwrap();
            let groups = dirmove.collect_all_prefix_groups(&files_with_names);

            // "Alex" alone should NOT be a group (4 chars < 5)
            assert!(
                !groups.contains_key("Alex"),
                "Short prefix 'Alex' should not form a single-word group"
            );

            // But "Alex.Scene" should still be a valid 2-part group
            assert!(
                groups.contains_key("Alex.Scene") || groups.contains_key("AlexScene"),
                "Two-part prefix 'Alex.Scene' should still form a group"
            );

            // "Alexander" should be a valid group (9 chars >= 5)
            assert!(
                groups.contains_key("Alexander"),
                "Long prefix 'Alexander' should form a group"
            );
        }

        #[test]
        fn common_short_words_excluded() {
            let tmp = tempfile::TempDir::new().unwrap();
            let root = tmp.path().to_path_buf();

            // Common short words that could cause false groupings
            std::fs::write(root.join("Name.File.001.txt"), "").unwrap();
            std::fs::write(root.join("Name.File.002.txt"), "").unwrap();
            std::fs::write(root.join("Name.File.003.txt"), "").unwrap();

            std::fs::write(root.join("Data.Report.001.csv"), "").unwrap();
            std::fs::write(root.join("Data.Report.002.csv"), "").unwrap();
            std::fs::write(root.join("Data.Report.003.csv"), "").unwrap();

            std::fs::write(root.join("Test.Case.001.rs"), "").unwrap();
            std::fs::write(root.join("Test.Case.002.rs"), "").unwrap();
            std::fs::write(root.join("Test.Case.003.rs"), "").unwrap();

            let dirmove = DirMove::new(root, make_config_with_min_chars(3, 5));
            let files_with_names = dirmove.collect_files_with_names().unwrap();
            let groups = dirmove.collect_all_prefix_groups(&files_with_names);

            // "Name" (4 chars), "Data" (4 chars), "Test" (4 chars) should NOT be groups
            assert!(!groups.contains_key("Name"), "Short word 'Name' should not form group");
            assert!(!groups.contains_key("Data"), "Short word 'Data' should not form group");
            assert!(!groups.contains_key("Test"), "Short word 'Test' should not form group");

            // But two-part prefixes should work
            assert!(
                groups.contains_key("Name.File") || groups.contains_key("NameFile"),
                "Two-part 'Name.File' should form group"
            );
        }

        #[test]
        fn config_override_allows_short_prefixes() {
            let tmp = tempfile::TempDir::new().unwrap();
            let root = tmp.path().to_path_buf();

            // Short prefix files
            std::fs::write(root.join("ABC.Video.001.mp4"), "").unwrap();
            std::fs::write(root.join("ABC.Video.002.mp4"), "").unwrap();
            std::fs::write(root.join("ABC.Video.003.mp4"), "").unwrap();

            // With min_prefix_chars=3, "ABC" should be allowed
            let dirmove = DirMove::new(root, make_config_with_min_chars(3, 3));
            let files_with_names = dirmove.collect_files_with_names().unwrap();
            let groups = dirmove.collect_all_prefix_groups(&files_with_names);

            assert!(
                groups.contains_key("ABC"),
                "Short prefix 'ABC' should form group with min_prefix_chars=3"
            );
        }

        #[test]
        fn high_threshold_excludes_most_single_words() {
            let tmp = tempfile::TempDir::new().unwrap();
            let root = tmp.path().to_path_buf();

            // Various length prefixes
            std::fs::write(root.join("Short.File.001.mp4"), "").unwrap();
            std::fs::write(root.join("Short.File.002.mp4"), "").unwrap();
            std::fs::write(root.join("Short.File.003.mp4"), "").unwrap();

            std::fs::write(root.join("Medium.Content.001.mp4"), "").unwrap();
            std::fs::write(root.join("Medium.Content.002.mp4"), "").unwrap();
            std::fs::write(root.join("Medium.Content.003.mp4"), "").unwrap();

            std::fs::write(root.join("VeryLongPrefix.Video.001.mp4"), "").unwrap();
            std::fs::write(root.join("VeryLongPrefix.Video.002.mp4"), "").unwrap();
            std::fs::write(root.join("VeryLongPrefix.Video.003.mp4"), "").unwrap();

            // With min_prefix_chars=10, only "VeryLongPrefix" (14 chars) qualifies
            let dirmove = DirMove::new(root, make_config_with_min_chars(3, 10));
            let files_with_names = dirmove.collect_files_with_names().unwrap();
            let groups = dirmove.collect_all_prefix_groups(&files_with_names);

            // "Short" (5 chars) and "Medium" (6 chars) should NOT be single-word groups
            assert!(!groups.contains_key("Short"), "'Short' should be excluded with min=10");
            assert!(
                !groups.contains_key("Medium"),
                "'Medium' should be excluded with min=10"
            );

            // "VeryLongPrefix" (14 chars) should be a group
            assert!(
                groups.contains_key("VeryLongPrefix"),
                "'VeryLongPrefix' should form group with min=10"
            );
        }

        #[test]
        fn mixed_lengths_correct_grouping() {
            let tmp = tempfile::TempDir::new().unwrap();
            let root = tmp.path().to_path_buf();

            // 4-char prefix
            std::fs::write(root.join("Film.Classic.001.mp4"), "").unwrap();
            std::fs::write(root.join("Film.Classic.002.mp4"), "").unwrap();
            std::fs::write(root.join("Film.Classic.003.mp4"), "").unwrap();

            // 5-char prefix (exactly at threshold)
            std::fs::write(root.join("Movie.Action.001.mp4"), "").unwrap();
            std::fs::write(root.join("Movie.Action.002.mp4"), "").unwrap();
            std::fs::write(root.join("Movie.Action.003.mp4"), "").unwrap();

            // 6-char prefix (above threshold)
            std::fs::write(root.join("Series.Drama.001.mp4"), "").unwrap();
            std::fs::write(root.join("Series.Drama.002.mp4"), "").unwrap();
            std::fs::write(root.join("Series.Drama.003.mp4"), "").unwrap();

            let dirmove = DirMove::new(root, make_config_with_min_chars(3, 5));
            let files_with_names = dirmove.collect_files_with_names().unwrap();
            let groups = dirmove.collect_all_prefix_groups(&files_with_names);

            // "Film" (4 chars) should NOT be a single-word group
            assert!(!groups.contains_key("Film"), "'Film' (4 chars) should be excluded");

            // "Movie" (5 chars) SHOULD be a single-word group
            assert!(groups.contains_key("Movie"), "'Movie' (5 chars) should be included");

            // "Series" (6 chars) SHOULD be a single-word group
            assert!(groups.contains_key("Series"), "'Series' (6 chars) should be included");
        }

        #[test]
        fn emoji_prefix_counted_as_chars() {
            let tmp = tempfile::TempDir::new().unwrap();
            let root = tmp.path().to_path_buf();

            // Emoji prefix - each emoji is typically 1-2 chars
            std::fs::write(root.join("🎬🎥.Video.001.mp4"), "").unwrap();
            std::fs::write(root.join("🎬🎥.Video.002.mp4"), "").unwrap();
            std::fs::write(root.join("🎬🎥.Video.003.mp4"), "").unwrap();

            // With min=2, emoji prefix should work
            let dirmove = DirMove::new(root, make_config_with_min_chars(3, 2));
            let files_with_names = dirmove.collect_files_with_names().unwrap();
            let groups = dirmove.collect_all_prefix_groups(&files_with_names);

            // Should find the emoji prefix group
            let has_emoji_group = groups.keys().any(|k| k.contains('🎬'));
            assert!(
                has_emoji_group,
                "Emoji prefix should form group with appropriate min_chars"
            );
        }

        // ===== Tests for dotted prefixes with min_prefix_chars =====

        #[test]
        fn dotted_prefix_char_count_excludes_dots() {
            // "A.B" has 2 chars (excluding dot), should be excluded with min=5
            let files = make_test_files(&["A.B.File.001.mp4", "A.B.File.002.mp4", "A.B.File.003.mp4"]);
            let candidates = utils::find_prefix_candidates("A.B.File.001.mp4", &files, 2, 5);

            // Should NOT find "A.B" (2 chars) as a valid prefix
            let two_part = candidates.iter().find(|c| c.prefix == "A.B");
            assert!(
                two_part.is_none(),
                "Two-part prefix 'A.B' (2 chars) should be excluded with min=5"
            );
        }

        #[test]
        fn dotted_prefix_included_when_chars_meet_threshold() {
            // "Ab.Cd" has 4 chars (excluding dot), should be included with min=4
            let files = make_test_files(&["Ab.Cd.File.001.mp4", "Ab.Cd.File.002.mp4", "Ab.Cd.File.003.mp4"]);
            let candidates = utils::find_prefix_candidates("Ab.Cd.File.001.mp4", &files, 2, 4);

            let two_part = candidates.iter().find(|c| c.prefix == "Ab.Cd");
            assert!(
                two_part.is_some(),
                "Two-part prefix 'Ab.Cd' (4 chars) should be included with min=4"
            );
        }

        #[test]
        fn three_part_dotted_prefix_char_count() {
            // "A.B.C" has 3 chars (excluding dots), should be excluded with min=5
            let files = make_test_files(&["A.B.C.File.001.mp4", "A.B.C.File.002.mp4", "A.B.C.File.003.mp4"]);
            let candidates = utils::find_prefix_candidates("A.B.C.File.001.mp4", &files, 2, 5);

            let three_part = candidates.iter().find(|c| c.prefix == "A.B.C");
            assert!(
                three_part.is_none(),
                "Three-part prefix 'A.B.C' (3 chars) should be excluded with min=5"
            );
        }

        #[test]
        fn three_part_dotted_prefix_included_when_long_enough() {
            // "Alpha.Beta.Gamma" has 14 chars (excluding dots)
            let files = make_test_files(&[
                "Alpha.Beta.Gamma.File.001.mp4",
                "Alpha.Beta.Gamma.File.002.mp4",
                "Alpha.Beta.Gamma.File.003.mp4",
            ]);
            let candidates = utils::find_prefix_candidates("Alpha.Beta.Gamma.File.001.mp4", &files, 2, 10);

            let three_part = candidates.iter().find(|c| c.prefix == "Alpha.Beta.Gamma");
            assert!(
                three_part.is_some(),
                "Three-part prefix 'Alpha.Beta.Gamma' (14 chars) should be included with min=10"
            );
        }

        #[test]
        fn count_prefix_chars_helper_works_correctly() {
            assert_eq!(utils::count_prefix_chars("A"), 1);
            assert_eq!(utils::count_prefix_chars("AB"), 2);
            assert_eq!(utils::count_prefix_chars("A.B"), 2);
            assert_eq!(utils::count_prefix_chars("A.B.C"), 3);
            assert_eq!(utils::count_prefix_chars("Alpha.Beta"), 9);
            assert_eq!(utils::count_prefix_chars("Alpha.Beta.Gamma"), 14);
            assert_eq!(utils::count_prefix_chars("..."), 0);
            assert_eq!(utils::count_prefix_chars("A...B"), 2);
            // Unicode
            assert_eq!(utils::count_prefix_chars("日本語"), 3);
            assert_eq!(utils::count_prefix_chars("日.本.語"), 3);
        }

        #[test]
        fn integration_dotted_short_prefix_excluded() {
            let tmp = tempfile::TempDir::new().unwrap();
            let root = tmp.path().to_path_buf();

            // "AB.CD" has 4 chars - should be excluded with min=5
            std::fs::write(root.join("AB.CD.Video.001.mp4"), "").unwrap();
            std::fs::write(root.join("AB.CD.Video.002.mp4"), "").unwrap();
            std::fs::write(root.join("AB.CD.Video.003.mp4"), "").unwrap();

            let dirmove = DirMove::new(root, make_config_with_min_chars(3, 5));
            let files_with_names = dirmove.collect_files_with_names().unwrap();
            let groups = dirmove.collect_all_prefix_groups(&files_with_names);

            // "AB.CD" (4 chars) and "AB" (2 chars) should NOT be groups
            assert!(!groups.contains_key("AB.CD"), "'AB.CD' (4 chars) should be excluded");
            assert!(!groups.contains_key("ABCD"), "'ABCD' (4 chars) should be excluded");
            assert!(!groups.contains_key("AB"), "'AB' (2 chars) should be excluded");
        }

        #[test]
        fn integration_dotted_long_prefix_included() {
            let tmp = tempfile::TempDir::new().unwrap();
            let root = tmp.path().to_path_buf();

            // "Alpha.Beta" has 9 chars - should be included with min=5
            std::fs::write(root.join("Alpha.Beta.Video.001.mp4"), "").unwrap();
            std::fs::write(root.join("Alpha.Beta.Video.002.mp4"), "").unwrap();
            std::fs::write(root.join("Alpha.Beta.Video.003.mp4"), "").unwrap();

            let dirmove = DirMove::new(root, make_config_with_min_chars(3, 5));
            let files_with_names = dirmove.collect_files_with_names().unwrap();
            let groups = dirmove.collect_all_prefix_groups(&files_with_names);

            // Should find Alpha.Beta or AlphaBeta group
            let has_alpha_beta = groups.contains_key("Alpha.Beta") || groups.contains_key("AlphaBeta");
            assert!(has_alpha_beta, "'Alpha.Beta' (9 chars) should form a group");
        }

        #[test]
        fn mixed_dotted_and_single_word_with_threshold() {
            let tmp = tempfile::TempDir::new().unwrap();
            let root = tmp.path().to_path_buf();

            // Short dotted: "X.Y" (2 chars) - excluded
            std::fs::write(root.join("X.Y.Content.001.mp4"), "").unwrap();
            std::fs::write(root.join("X.Y.Content.002.mp4"), "").unwrap();
            std::fs::write(root.join("X.Y.Content.003.mp4"), "").unwrap();

            // Short single: "Test" (4 chars) - excluded
            std::fs::write(root.join("Test.Video.001.mp4"), "").unwrap();
            std::fs::write(root.join("Test.Video.002.mp4"), "").unwrap();
            std::fs::write(root.join("Test.Video.003.mp4"), "").unwrap();

            // Long dotted: "Long.Name" (8 chars) - included
            std::fs::write(root.join("Long.Name.File.001.mp4"), "").unwrap();
            std::fs::write(root.join("Long.Name.File.002.mp4"), "").unwrap();
            std::fs::write(root.join("Long.Name.File.003.mp4"), "").unwrap();

            // Long single: "Studio" (6 chars) - included
            std::fs::write(root.join("Studio.Movie.001.mp4"), "").unwrap();
            std::fs::write(root.join("Studio.Movie.002.mp4"), "").unwrap();
            std::fs::write(root.join("Studio.Movie.003.mp4"), "").unwrap();

            let dirmove = DirMove::new(root, make_config_with_min_chars(3, 5));
            let files_with_names = dirmove.collect_files_with_names().unwrap();
            let groups = dirmove.collect_all_prefix_groups(&files_with_names);

            // Short ones excluded
            assert!(!groups.contains_key("X.Y"), "'X.Y' (2 chars) should be excluded");
            assert!(!groups.contains_key("XY"), "'XY' (2 chars) should be excluded");
            assert!(!groups.contains_key("X"), "'X' (1 char) should be excluded");
            assert!(!groups.contains_key("Test"), "'Test' (4 chars) should be excluded");

            // Long ones included
            let has_long_name = groups.contains_key("Long.Name") || groups.contains_key("LongName");
            assert!(has_long_name, "'Long.Name' (8 chars) should be included");
            assert!(groups.contains_key("Studio"), "'Studio' (6 chars) should be included");
        }

        #[test]
        fn threshold_zero_allows_all_dotted_prefixes() {
            let tmp = tempfile::TempDir::new().unwrap();
            let root = tmp.path().to_path_buf();

            // Short dotted prefix - "X.Y" has 2 chars, but with "Content" it becomes more
            std::fs::write(root.join("X.Y.Content.001.mp4"), "").unwrap();
            std::fs::write(root.join("X.Y.Content.002.mp4"), "").unwrap();
            std::fs::write(root.join("X.Y.Content.003.mp4"), "").unwrap();

            let dirmove = DirMove::new(root, make_config_with_min_chars(3, 0));
            let files_with_names = dirmove.collect_files_with_names().unwrap();
            let groups = dirmove.collect_all_prefix_groups(&files_with_names);

            // With min=0, even short prefixes like "X" (1 char) should form a group
            let has_short_prefix = groups.keys().any(|k| k.starts_with('X'));
            assert!(
                has_short_prefix,
                "With min=0, short prefixes should be allowed. Groups: {:?}",
                groups.keys().collect::<Vec<_>>()
            );
        }

        #[test]
        fn threshold_exact_boundary_dotted() {
            let tmp = tempfile::TempDir::new().unwrap();
            let root = tmp.path().to_path_buf();

            // "Ab.Cde" has exactly 5 chars - should be included with min=5
            std::fs::write(root.join("Ab.Cde.Video.001.mp4"), "").unwrap();
            std::fs::write(root.join("Ab.Cde.Video.002.mp4"), "").unwrap();
            std::fs::write(root.join("Ab.Cde.Video.003.mp4"), "").unwrap();

            // "Ab.Cd" has 4 chars - should be excluded with min=5
            std::fs::write(root.join("Ab.Cd.Other.001.mp4"), "").unwrap();
            std::fs::write(root.join("Ab.Cd.Other.002.mp4"), "").unwrap();
            std::fs::write(root.join("Ab.Cd.Other.003.mp4"), "").unwrap();

            let dirmove = DirMove::new(root, make_config_with_min_chars(3, 5));
            let files_with_names = dirmove.collect_files_with_names().unwrap();
            let groups = dirmove.collect_all_prefix_groups(&files_with_names);

            // Exactly 5 chars should be included
            let has_ab_cde = groups.contains_key("Ab.Cde") || groups.contains_key("AbCde");
            assert!(has_ab_cde, "'Ab.Cde' (5 chars) should be included at exact threshold");

            // 4 chars should be excluded
            assert!(
                !groups.contains_key("Ab.Cd") && !groups.contains_key("AbCd"),
                "'Ab.Cd' (4 chars) should be excluded"
            );
        }

        #[test]
        fn high_threshold_excludes_all_short_dotted() {
            let tmp = tempfile::TempDir::new().unwrap();
            let root = tmp.path().to_path_buf();

            // Various dotted prefixes all below threshold of 15
            std::fs::write(root.join("One.Two.File.001.mp4"), "").unwrap(); // 6 chars
            std::fs::write(root.join("One.Two.File.002.mp4"), "").unwrap();
            std::fs::write(root.join("One.Two.File.003.mp4"), "").unwrap();

            std::fs::write(root.join("Alpha.Beta.Video.001.mp4"), "").unwrap(); // 9 chars
            std::fs::write(root.join("Alpha.Beta.Video.002.mp4"), "").unwrap();
            std::fs::write(root.join("Alpha.Beta.Video.003.mp4"), "").unwrap();

            std::fs::write(root.join("Super.Long.Prefix.Content.001.mp4"), "").unwrap(); // 15 chars
            std::fs::write(root.join("Super.Long.Prefix.Content.002.mp4"), "").unwrap();
            std::fs::write(root.join("Super.Long.Prefix.Content.003.mp4"), "").unwrap();

            let dirmove = DirMove::new(root, make_config_with_min_chars(3, 15));
            let files_with_names = dirmove.collect_files_with_names().unwrap();
            let groups = dirmove.collect_all_prefix_groups(&files_with_names);

            // Short ones excluded
            assert!(
                !groups.contains_key("One.Two") && !groups.contains_key("OneTwo"),
                "'One.Two' (6 chars) should be excluded with min=15"
            );
            assert!(
                !groups.contains_key("Alpha.Beta") && !groups.contains_key("AlphaBeta"),
                "'Alpha.Beta' (9 chars) should be excluded with min=15"
            );

            // Exactly 15 chars should be included
            let has_super_long = groups.contains_key("Super.Long.Prefix") || groups.contains_key("SuperLongPrefix");
            assert!(
                has_super_long,
                "'Super.Long.Prefix' (15 chars) should be included at threshold"
            );
        }

        #[test]
        fn unicode_dotted_prefix_counted_correctly() {
            let tmp = tempfile::TempDir::new().unwrap();
            let root = tmp.path().to_path_buf();

            // "日.本" has 2 chars (excluding dot) - should be excluded with min=3
            std::fs::write(root.join("日.本.Video.001.mp4"), "").unwrap();
            std::fs::write(root.join("日.本.Video.002.mp4"), "").unwrap();
            std::fs::write(root.join("日.本.Video.003.mp4"), "").unwrap();

            // "日本語.映画" has 5 chars - should be included with min=3
            std::fs::write(root.join("日本語.映画.Content.001.mp4"), "").unwrap();
            std::fs::write(root.join("日本語.映画.Content.002.mp4"), "").unwrap();
            std::fs::write(root.join("日本語.映画.Content.003.mp4"), "").unwrap();

            let dirmove = DirMove::new(root, make_config_with_min_chars(3, 3));
            let files_with_names = dirmove.collect_files_with_names().unwrap();
            let groups = dirmove.collect_all_prefix_groups(&files_with_names);

            // "日.本" (2 chars) should be excluded
            assert!(
                !groups.contains_key("日.本") && !groups.contains_key("日本") && !groups.contains_key("日"),
                "'日.本' (2 chars) should be excluded with min=3"
            );

            // "日本語.映画" (5 chars) should be included
            let has_japanese = groups.keys().any(|k| k.contains("日本語"));
            assert!(has_japanese, "'日本語.映画' (5 chars) should be included with min=3");
        }
    }

    // ===== Tests for broader grouping behavior (starts_with) =====

    #[test]
    fn broader_group_includes_extended_names() {
        // Test that broader groups CAN include files with extended prefixes
        // e.g., "Studio" group includes "StudioPro", "StudioBasic" files
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // Base "Studio" files
        std::fs::write(root.join("Studio.Video.001.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.Video.002.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.Video.003.mp4"), "").unwrap();

        // Extended names that start with "Studio"
        std::fs::write(root.join("StudioPro.Premium.001.mp4"), "").unwrap();
        std::fs::write(root.join("StudioPro.Premium.002.mp4"), "").unwrap();
        std::fs::write(root.join("StudioBasic.Free.001.mp4"), "").unwrap();

        // Completely different
        std::fs::write(root.join("OtherContent.File.mp4"), "").unwrap();
        std::fs::write(root.join("MyStudio.Different.mp4"), "").unwrap(); // "Studio" not at start

        let dirmove = DirMove::new(root, make_grouping_config(2));
        let files_with_names = dirmove.collect_files_with_names().unwrap();
        let groups = dirmove.collect_all_prefix_groups(&files_with_names);

        // "Studio" group exists and includes base files
        assert!(groups.contains_key("Studio"), "Should find Studio group");
        let studio_files = &groups.get("Studio").unwrap().0;
        assert!(studio_files.len() >= 3, "Studio should have at least base 3 files");

        // StudioPro should also exist as its own more specific group
        assert!(groups.contains_key("StudioPro"), "Should find StudioPro group");
        assert_eq!(groups.get("StudioPro").unwrap().0.len(), 2);

        // MyStudio should NOT be in Studio group (Studio not at prefix position)
        for file in studio_files {
            let name = file.to_string_lossy();
            assert!(!name.contains("MyStudio"), "MyStudio should not be in Studio group");
        }
    }

    #[test]
    fn specific_groups_separate_from_unrelated_names() {
        // Test that specific groups contain their matches and unrelated names are separate
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // Specific group: "AlphaStudio"
        std::fs::write(root.join("AlphaStudio.Movie.001.mp4"), "").unwrap();
        std::fs::write(root.join("AlphaStudio.Movie.002.mp4"), "").unwrap();
        std::fs::write(root.join("AlphaStudio.Movie.003.mp4"), "").unwrap();

        // Different: "BetaStudio" - completely unrelated
        std::fs::write(root.join("BetaStudio.Other.001.mp4"), "").unwrap();
        std::fs::write(root.join("BetaStudio.Other.002.mp4"), "").unwrap();
        std::fs::write(root.join("BetaStudio.Other.003.mp4"), "").unwrap();

        // Different: "GammaProduction"
        std::fs::write(root.join("GammaProduction.Film.001.mp4"), "").unwrap();
        std::fs::write(root.join("GammaProduction.Film.002.mp4"), "").unwrap();

        let dirmove = DirMove::new(root, make_grouping_config(2));
        let files_with_names = dirmove.collect_files_with_names().unwrap();
        let groups = dirmove.collect_all_prefix_groups(&files_with_names);

        // AlphaStudio should have exactly 3 files
        assert!(groups.contains_key("AlphaStudio"), "Should find AlphaStudio group");
        let alpha_files = &groups.get("AlphaStudio").unwrap().0;
        assert_eq!(alpha_files.len(), 3, "AlphaStudio should have exactly 3 files");

        // Verify BetaStudio is NOT in AlphaStudio group
        for file in alpha_files {
            let name = file.to_string_lossy();
            assert!(
                !name.contains("BetaStudio"),
                "BetaStudio should not be in AlphaStudio group"
            );
            assert!(
                !name.contains("GammaProduction"),
                "GammaProduction should not be in AlphaStudio group"
            );
        }

        // BetaStudio should have its own group
        assert!(groups.contains_key("BetaStudio"), "Should find BetaStudio group");
        assert_eq!(groups.get("BetaStudio").unwrap().0.len(), 3);

        // GammaProduction should have its own group
        assert!(
            groups.contains_key("GammaProduction"),
            "Should find GammaProduction group"
        );
        assert_eq!(groups.get("GammaProduction").unwrap().0.len(), 2);
    }

    #[test]
    fn files_without_dots_form_single_groups() {
        // Files without dots should not cause issues
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // Files with dots
        std::fs::write(root.join("Target.Series.Episode.01.mp4"), "").unwrap();
        std::fs::write(root.join("Target.Series.Episode.02.mp4"), "").unwrap();
        std::fs::write(root.join("Target.Series.Episode.03.mp4"), "").unwrap();

        // Files without dots (just extension)
        std::fs::write(root.join("standalone.mp4"), "").unwrap();
        std::fs::write(root.join("another_file.txt"), "").unwrap();
        std::fs::write(root.join("nodots.jpg"), "").unwrap();

        // Completely different
        std::fs::write(root.join("Unrelated.Other.File.mp4"), "").unwrap();

        let dirmove = DirMove::new(root, make_grouping_config(3));
        let files_with_names = dirmove.collect_files_with_names().unwrap();
        let groups = dirmove.collect_all_prefix_groups(&files_with_names);

        // Target.Series should have 3 files
        let target_group = groups.iter().find(|(k, _)| k.contains("Target"));
        assert!(target_group.is_some(), "Should find Target group");
        assert_eq!(target_group.unwrap().1.0.len(), 3);

        // standalone, another_file, nodots should NOT be in Target group
        let target_files = &target_group.unwrap().1.0;
        for file in target_files {
            let name = file.to_string_lossy();
            assert!(!name.contains("standalone"), "standalone should not be in Target group");
            assert!(
                !name.contains("another_file"),
                "another_file should not be in Target group"
            );
            assert!(!name.contains("nodots"), "nodots should not be in Target group");
        }
    }
}

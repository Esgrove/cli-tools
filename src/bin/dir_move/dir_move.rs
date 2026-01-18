use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use colored::Colorize;
use itertools::Itertools;
use regex::Regex;
use walkdir::WalkDir;

use cli_tools::{
    get_relative_path_or_filename, path_to_filename_string, path_to_string_relative, print_bold, print_error,
    print_magenta, print_warning,
};

use crate::DirMoveArgs;
use crate::config::Config;

/// Regex to match video resolutions like 1080p, 2160p, or 1920x1080.
static RE_RESOLUTION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(\d{3,4}p|\d{3,4}x\d{3,4})\b").expect("Invalid resolution regex"));

/// Common glue words to filter out from grouping names.
const GLUE_WORDS: &[&str] = &[
    "a", "an", "and", "at", "by", "for", "in", "of", "on", "or", "the", "to", "with",
];

/// Directory names that should be deleted when encountered.
const UNWANTED_DIRECTORIES: &[&str] = &[".unwanted"];

#[derive(Debug)]
pub struct DirMove {
    root: PathBuf,
    config: Config,
}

/// Information about a directory used for matching files to move.
#[derive(Debug)]
struct DirectoryInfo {
    /// Absolute path to the directory.
    path: PathBuf,
    /// Normalized directory name (lowercase, dots replaced with spaces).
    name: String,
}

#[derive(Debug)]
struct MoveInfo {
    source: PathBuf,
    target: PathBuf,
}

/// Information about what needs to be moved during an unpack operation.
#[derive(Debug, Default)]
struct UnpackInfo {
    /// Files to move.
    file_moves: Vec<MoveInfo>,
    /// Directories to move directly.
    directory_moves: Vec<MoveInfo>,
}

impl MoveInfo {
    const fn new(source: PathBuf, target: PathBuf) -> Self {
        Self { source, target }
    }
}

impl DirectoryInfo {
    fn new(path: PathBuf) -> Self {
        let name = path_to_filename_string(&path).to_lowercase().replace('.', " ");
        Self { path, name }
    }
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

            // Move directories that don't match unpack names directly (more efficient).
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
                e.file_name().to_str().is_some_and(Self::is_unwanted_directory) || !cli_tools::should_skip_entry(e)
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

            if Self::is_unwanted_directory(name) {
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
            Self::copy_dir_recursive(source, target)?;
            std::fs::remove_dir_all(source)?;
        }

        Ok(())
    }

    /// Recursively copy a directory and its contents.
    fn copy_dir_recursive(source: &Path, target: &Path) -> anyhow::Result<()> {
        std::fs::create_dir_all(target)?;

        for entry in std::fs::read_dir(source)?.filter_map(Result::ok) {
            let src_path = entry.path();
            let dst_path = target.join(entry.file_name());

            if src_path.is_dir() {
                Self::copy_dir_recursive(&src_path, &dst_path)?;
            } else {
                std::fs::copy(&src_path, &dst_path)?;
            }
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
                if Self::is_unwanted_directory(&dir_name) {
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
            let file_name_normalized = file_name.replace('.', " ").to_lowercase();

            // Apply prefix ignores: strip ignored prefixes from the normalized filename
            let file_name_stripped = self.strip_ignored_prefixes(&file_name_normalized);

            for &idx in &dir_indices {
                // dir.name is already lowercase
                // Also strip ignored prefixes from directory name for matching
                let dir_name_stripped = self.strip_ignored_prefixes(&dirs[idx].name);

                // Skip directories whose name is exactly an ignored prefix
                // (after stripping, the directory name would be empty or unchanged if it's just the prefix)
                if self.is_ignored_prefix(&dirs[idx].name) {
                    continue;
                }

                // Check if:
                // 1. Stripped filename contains stripped directory name (both have prefix removed)
                // 2. Original filename contains stripped directory name (dir has prefix, file doesn't)
                // 3. Stripped filename contains original directory name (file has prefix, dir doesn't)
                let is_match = file_name_stripped.contains(&*dir_name_stripped)
                    || file_name_normalized.contains(&*dir_name_stripped)
                    || file_name_stripped.contains(&dirs[idx].name);

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

    /// Filter out dot-separated parts that contain only numeric digits, resolution patterns, or glue words.
    /// For example, "Show.2024.S01E01.mkv" becomes "Show.S01E01.mkv".
    /// For example, "Show.1080p.S01E01.mkv" becomes "Show.S01E01.mkv".
    /// For example, "Show.and.Tell.mkv" becomes "Show.Tell.mkv".
    fn filter_numeric_resolution_and_glue_parts(filename: &str) -> String {
        filename
            .split('.')
            .filter(|part| {
                if part.is_empty() {
                    return true;
                }
                // Filter purely numeric parts
                if part.chars().all(|c| c.is_ascii_digit()) {
                    return false;
                }
                // Filter resolution patterns
                if RE_RESOLUTION.is_match(part) {
                    return false;
                }
                // Filter glue words (case-insensitive)
                !GLUE_WORDS.contains(&part.to_lowercase().as_str())
            })
            .collect::<Vec<_>>()
            .join(".")
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
    /// Returns a list of (`file_path`, `processed_name`) pairs.
    fn collect_files_with_names(&self) -> anyhow::Result<Vec<(PathBuf, String)>> {
        let mut files_with_names: Vec<(PathBuf, String)> = Vec::new();

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
            let file_name_for_grouping = self.strip_ignored_dot_prefixes(&file_name);
            let file_name_for_grouping = Self::filter_numeric_resolution_and_glue_parts(&file_name_for_grouping);
            files_with_names.push((file_path, file_name_for_grouping));
        }

        // Debug: print unique processed names
        if self.config.debug {
            let unique_names: HashSet<_> = files_with_names.iter().map(|(_, name)| name.as_str()).collect();
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
    fn collect_all_prefix_groups(&self, files_with_names: &[(PathBuf, String)]) -> HashMap<String, Vec<PathBuf>> {
        // Use lowercase keys for grouping, but store original prefix for display
        let mut prefix_groups: HashMap<String, (String, Vec<PathBuf>)> = HashMap::new();

        for (file_path, file_name) in files_with_names {
            let prefix_candidates =
                Self::find_prefix_candidates(file_name, files_with_names, self.config.min_group_size);

            // Add file to ALL matching prefix groups, not just the best one
            // Use fully normalized key (no dots, lowercase) for grouping to handle
            // both case variations and dot-separated vs concatenated prefixes
            for (prefix, _count) in prefix_candidates {
                let key = Self::normalize_prefix(&prefix);
                if let Some((_, files)) = prefix_groups.get_mut(&key) {
                    files.push(file_path.clone());
                } else {
                    // Store the original prefix for display purposes
                    prefix_groups.insert(key, (prefix.into_owned(), vec![file_path.clone()]));
                }
            }
        }

        // Convert to final format: display_prefix -> files
        let display_groups: HashMap<String, Vec<PathBuf>> = prefix_groups.into_values().collect();

        // Apply prefix overrides: if a group's prefix starts with an override, use the override
        self.apply_prefix_overrides(display_groups)
    }

    /// Create directories for files with matching prefixes and move files into them.
    /// Only considers files directly in the base path (not recursive).
    /// Files can match multiple groups - they remain available until actually moved.
    fn create_dirs_and_move_files(&self) -> anyhow::Result<()> {
        let files_with_names = self.collect_files_with_names()?;
        let prefix_groups = self.collect_all_prefix_groups(&files_with_names);

        // Sort groups by prefix length (longest first) for better grouping, then alphabetically
        let mut groups_to_process: Vec<_> = prefix_groups
            .into_iter()
            .filter(|(_, files)| files.len() >= self.config.min_group_size)
            .sorted_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.0.cmp(&b.0)))
            .collect();

        if groups_to_process.is_empty() {
            if self.config.verbose {
                println!(
                    "No file groups with {} or more matching prefixes found.",
                    self.config.min_group_size
                );
            }
            return Ok(());
        }

        // Track files that have been moved to avoid offering them again
        let mut moved_files: HashSet<PathBuf> = HashSet::new();

        // Count initial groups for display (before filtering by moved files)
        let initial_group_count = groups_to_process.len();
        print_bold!(
            "Found {} group(s) with {}+ files sharing the same prefix:\n",
            initial_group_count,
            self.config.min_group_size
        );

        while !groups_to_process.is_empty() {
            let (prefix, files) = groups_to_process.remove(0);

            // Filter out already moved files
            let available_files: Vec<_> = files.into_iter().filter(|f| !moved_files.contains(f)).collect();

            if available_files.len() < self.config.min_group_size {
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
    fn apply_prefix_overrides(&self, groups: HashMap<String, Vec<PathBuf>>) -> HashMap<String, Vec<PathBuf>> {
        if self.config.prefix_overrides.is_empty() {
            return groups;
        }

        let mut result: HashMap<String, Vec<PathBuf>> = HashMap::new();

        for (prefix, files) in groups {
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

            result.entry(target_prefix).or_default().extend(files);
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

    /// Find prefix candidates for a file, prioritizing earlier positions in the filename.
    /// Returns a list of (prefix, `match_count`) pairs in priority order:
    /// 3-part prefix, 2-part prefix, 1-part prefix.
    /// Longer prefixes are preferred as they provide more specific grouping.
    /// Also handles case variations and dot-separated vs concatenated forms.
    fn find_prefix_candidates<'a>(
        file_name: &'a str,
        all_files: &[(PathBuf, String)],
        min_group_size: usize,
    ) -> Vec<(Cow<'a, str>, usize)> {
        let Some(first_part) = file_name.split('.').next().filter(|p| !p.is_empty()) else {
            return Vec::new();
        };

        let mut candidates: Vec<(Cow<'a, str>, usize)> = Vec::new();

        // Count matches for 3-part prefix (highest priority)
        if let Some(three_part) = Self::get_n_part_prefix(file_name, 3) {
            let three_part_normalized = Self::normalize_prefix(three_part);
            let count = all_files
                .iter()
                .filter(|(_, name)| Self::prefix_matches_normalized(name, &three_part_normalized))
                .count();
            if count >= min_group_size {
                candidates.push((Cow::Borrowed(three_part), count));
            }
        }

        // Count matches for 2-part prefix (second priority)
        if let Some(two_part) = Self::get_n_part_prefix(file_name, 2) {
            let two_part_normalized = Self::normalize_prefix(two_part);
            let count = all_files
                .iter()
                .filter(|(_, name)| Self::prefix_matches_normalized(name, &two_part_normalized))
                .count();
            if count >= min_group_size {
                candidates.push((Cow::Borrowed(two_part), count));
            }
        }

        // Count matches for 1-part prefix (lowest priority)
        let first_part_normalized = first_part.to_lowercase();
        let count = all_files
            .iter()
            .filter(|(_, name)| Self::prefix_matches_normalized(name, &first_part_normalized))
            .count();
        if count >= min_group_size {
            candidates.push((Cow::Borrowed(first_part), count));
        }

        candidates
    }

    /// Check if a filename's prefix matches the given normalized target.
    /// Checks 1-part, 2-part, and 3-part prefixes to handle cases like:
    /// - "PhotoLab.Image" matching "photolab" (1-part)
    /// - "Photo.Lab.Image" matching "photolab" (2-part combined)
    fn prefix_matches_normalized(file_name: &str, target_normalized: &str) -> bool {
        let parts: Vec<&str> = file_name.split('.').collect();

        // Check 1-part prefix
        if let Some(&first) = parts.first()
            && first.to_lowercase() == *target_normalized
        {
            return true;
        }

        // Check 2-part prefix combined
        if parts.len() >= 2 {
            let two_combined = format!("{}{}", parts[0], parts[1]).to_lowercase();
            if two_combined == *target_normalized {
                return true;
            }
        }

        // Check 3-part prefix combined
        if parts.len() >= 3 {
            let three_combined = format!("{}{}{}", parts[0], parts[1], parts[2]).to_lowercase();
            if three_combined == *target_normalized {
                return true;
            }
        }

        false
    }

    /// Normalize a prefix for comparison by removing dots and lowercasing.
    /// This allows "Show.TV" and "`ShowTV`" to be treated as equivalent.
    fn normalize_prefix(prefix: &str) -> String {
        prefix.replace('.', "").to_lowercase()
    }

    /// Extract a prefix consisting of the first n dot-separated parts.
    /// Returns None if there aren't enough parts.
    fn get_n_part_prefix(file_name: &str, n: usize) -> Option<&str> {
        let mut dots_found = 0;
        let mut nth_dot_pos = 0;

        for (i, c) in file_name.bytes().enumerate() {
            if c == b'.' {
                dots_found += 1;
                if dots_found == n {
                    nth_dot_pos = i;
                } else if dots_found > n {
                    // Found more than n dots, return prefix up to nth dot
                    return Some(&file_name[..nth_dot_pos]);
                }
            }
        }

        // If we found exactly n dots, that's n+1 parts which is enough
        if dots_found >= n && nth_dot_pos > 0 {
            return Some(&file_name[..nth_dot_pos]);
        }

        // Not enough parts
        None
    }

    /// Check if a directory name is in the unwanted list.
    fn is_unwanted_directory(name: &str) -> bool {
        UNWANTED_DIRECTORIES.iter().any(|u| name.eq_ignore_ascii_case(u))
    }
}

#[cfg(test)]
mod test_helpers {
    use super::*;

    pub fn make_test_files(names: &[&str]) -> Vec<(PathBuf, String)> {
        names.iter().map(|n| (PathBuf::from(*n), (*n).to_string())).collect()
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
            min_group_size: 3,
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
            min_group_size: 3,
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
mod test_prefix_extraction {
    use super::*;

    #[test]
    fn three_parts_from_long_name() {
        assert_eq!(
            DirMove::get_n_part_prefix("Some.Name.Thing.v1.mp4", 3),
            Some("Some.Name.Thing")
        );
    }

    #[test]
    fn two_parts_from_name() {
        assert_eq!(DirMove::get_n_part_prefix("Some.Name.Thing.mp4", 2), Some("Some.Name"));
    }

    #[test]
    fn not_enough_parts_for_three() {
        assert_eq!(DirMove::get_n_part_prefix("Some.Name.mp4", 3), None);
    }

    #[test]
    fn not_enough_parts_for_two() {
        assert_eq!(DirMove::get_n_part_prefix("Some.mp4", 2), None);
    }

    #[test]
    fn exact_parts_for_two() {
        assert_eq!(DirMove::get_n_part_prefix("Some.Name.mp4", 2), Some("Some.Name"));
    }

    #[test]
    fn single_part_name() {
        assert_eq!(DirMove::get_n_part_prefix("file.mp4", 1), Some("file"));
    }

    #[test]
    fn empty_string() {
        assert_eq!(DirMove::get_n_part_prefix("", 1), None);
    }

    #[test]
    fn no_extension() {
        assert_eq!(DirMove::get_n_part_prefix("Some.Name", 1), Some("Some"));
    }

    #[test]
    fn many_parts() {
        assert_eq!(DirMove::get_n_part_prefix("A.B.C.D.E.F.mp4", 3), Some("A.B.C"));
    }

    #[test]
    fn with_numbers_in_name() {
        assert_eq!(DirMove::get_n_part_prefix("Show.2024.S01E01.mp4", 2), Some("Show.2024"));
    }

    #[test]
    fn with_special_characters() {
        assert_eq!(
            DirMove::get_n_part_prefix("Show-Name.Part.One.mp4", 2),
            Some("Show-Name.Part")
        );
    }

    #[test]
    fn with_underscores() {
        assert_eq!(
            DirMove::get_n_part_prefix("Show_Name.Part_One.Episode.mp4", 2),
            Some("Show_Name.Part_One")
        );
    }
}

#[cfg(test)]
mod test_prefix_candidates {
    use super::test_helpers::*;
    use super::*;

    #[test]
    fn single_file_no_match() {
        let files = make_test_files(&["LongName.v1.mp4", "Other.v2.mp4"]);
        let candidates = DirMove::find_prefix_candidates("LongName.v1.mp4", &files, 2);
        assert!(candidates.is_empty());
    }

    #[test]
    fn simple_prefix_multiple_files() {
        let files = make_test_files(&["LongName.v1.mp4", "LongName.v2.mp4", "Other.v2.mp4"]);
        let candidates = DirMove::find_prefix_candidates("LongName.v1.mp4", &files, 2);
        assert_eq!(candidates, vec![(Cow::Borrowed("LongName"), 2)]);
    }

    #[test]
    fn prioritizes_longer_prefix() {
        let files = make_test_files(&[
            "Some.Name.Thing.v1.mp4",
            "Some.Name.Thing.v2.mp4",
            "Some.Name.Thing.v3.mp4",
        ]);
        let candidates = DirMove::find_prefix_candidates("Some.Name.Thing.v1.mp4", &files, 2);
        assert_eq!(candidates.len(), 3);
        assert_eq!(candidates[0], (Cow::Borrowed("Some.Name.Thing"), 3));
        assert_eq!(candidates[1], (Cow::Borrowed("Some.Name"), 3));
        assert_eq!(candidates[2], (Cow::Borrowed("Some"), 3));
    }

    #[test]
    fn mixed_prefixes_different_third_parts() {
        let files = make_test_files(&[
            "Some.Name.Thing.v1.mp4",
            "Some.Name.Thing.v2.mp4",
            "Some.Name.Other.v1.mp4",
        ]);
        let candidates = DirMove::find_prefix_candidates("Some.Name.Thing.v1.mp4", &files, 2);
        assert_eq!(candidates.len(), 3);
        assert_eq!(candidates[0], (Cow::Borrowed("Some.Name.Thing"), 2));
        assert_eq!(candidates[1], (Cow::Borrowed("Some.Name"), 3));
        assert_eq!(candidates[2], (Cow::Borrowed("Some"), 3));
    }

    #[test]
    fn fallback_to_two_part_when_no_three_part_matches() {
        let files = make_test_files(&["Some.Name.Thing.mp4", "Some.Name.Other.mp4", "Some.Name.More.mp4"]);
        let candidates = DirMove::find_prefix_candidates("Some.Name.Thing.mp4", &files, 2);
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0], (Cow::Borrowed("Some.Name"), 3));
        assert_eq!(candidates[1], (Cow::Borrowed("Some"), 3));
    }

    #[test]
    fn single_word_fallback() {
        let files = make_test_files(&["ABC.2023.Thing.mp4", "ABC.2024.Other.mp4", "ABC.2025.More.mp4"]);
        let candidates = DirMove::find_prefix_candidates("ABC.2023.Thing.mp4", &files, 3);
        assert_eq!(candidates, vec![(Cow::Borrowed("ABC"), 3)]);
    }

    #[test]
    fn respects_min_group_size() {
        let files = make_test_files(&[
            "Some.Name.Thing.v1.mp4",
            "Some.Name.Thing.v2.mp4",
            "Some.Name.Other.v1.mp4",
        ]);
        let candidates = DirMove::find_prefix_candidates("Some.Name.Thing.v1.mp4", &files, 3);
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0], (Cow::Borrowed("Some.Name"), 3));
        assert_eq!(candidates[1], (Cow::Borrowed("Some"), 3));
    }

    #[test]
    fn no_matches_below_threshold() {
        let files = make_test_files(&["ABC.random.mp4", "XYZ.other.mp4"]);
        let candidates = DirMove::find_prefix_candidates("ABC.random.mp4", &files, 2);
        assert!(candidates.is_empty());
    }

    #[test]
    fn returns_all_viable_options_for_alternatives() {
        let files = make_test_files(&[
            "Show.Name.S01E01.mp4",
            "Show.Name.S01E02.mp4",
            "Show.Name.S01E03.mp4",
            "Show.Other.S01E01.mp4",
            "Show.Other.S01E02.mp4",
        ]);
        let candidates = DirMove::find_prefix_candidates("Show.Name.S01E01.mp4", &files, 2);
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0], (Cow::Borrowed("Show.Name"), 3));
        assert_eq!(candidates[1], (Cow::Borrowed("Show"), 5));
    }

    #[test]
    fn empty_file_list() {
        let files: Vec<(PathBuf, String)> = Vec::new();
        let candidates = DirMove::find_prefix_candidates("Some.Name.mp4", &files, 2);
        assert!(candidates.is_empty());
    }

    #[test]
    fn file_not_in_list() {
        let files = make_test_files(&["Other.Name.mp4", "Different.File.mp4"]);
        let candidates = DirMove::find_prefix_candidates("Some.Name.mp4", &files, 2);
        assert!(candidates.is_empty());
    }

    #[test]
    fn min_group_size_one() {
        let files = make_test_files(&["Unique.Name.v1.mp4", "Other.v2.mp4"]);
        let candidates = DirMove::find_prefix_candidates("Unique.Name.v1.mp4", &files, 1);
        assert_eq!(candidates.len(), 3);
        assert_eq!(candidates[0], (Cow::Borrowed("Unique.Name.v1"), 1));
        assert_eq!(candidates[1], (Cow::Borrowed("Unique.Name"), 1));
        assert_eq!(candidates[2], (Cow::Borrowed("Unique"), 1));
    }

    #[test]
    fn many_files_same_prefix() {
        let files = make_test_files(&[
            "Series.Episode.01.mp4",
            "Series.Episode.02.mp4",
            "Series.Episode.03.mp4",
            "Series.Episode.04.mp4",
            "Series.Episode.05.mp4",
            "Series.Episode.06.mp4",
            "Series.Episode.07.mp4",
            "Series.Episode.08.mp4",
            "Series.Episode.09.mp4",
            "Series.Episode.10.mp4",
        ]);
        let candidates = DirMove::find_prefix_candidates("Series.Episode.01.mp4", &files, 5);
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0], (Cow::Borrowed("Series.Episode"), 10));
        assert_eq!(candidates[1], (Cow::Borrowed("Series"), 10));
    }

    #[test]
    fn case_insensitive_prefix_matching() {
        let files = make_test_files(&["Show.Name.v1.mp4", "show.name.v2.mp4", "SHOW.NAME.v3.mp4"]);
        let candidates = DirMove::find_prefix_candidates("Show.Name.v1.mp4", &files, 2);
        // Case-insensitive matching should group all three files
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0], (Cow::Borrowed("Show.Name"), 3));
        assert_eq!(candidates[1], (Cow::Borrowed("Show"), 3));
    }

    #[test]
    fn dot_separated_matches_concatenated() {
        // "Photo.Lab" and "PhotoLab" should be treated as equivalent
        let files = make_test_files(&[
            "Photo.Lab.Image.One.jpg",
            "PhotoLab.Image.Two.jpg",
            "PhotoLab.Image.Three.jpg",
            "Photolab.Image.Four.jpg",
        ]);
        let candidates = DirMove::find_prefix_candidates("PhotoLab.Image.Two.jpg", &files, 2);
        // All files should match - PhotoLab = Photo.Lab = Photolab
        assert!(!candidates.is_empty());
        // The 1-part prefix "PhotoLab" should match all 4 files
        let photolab = candidates.iter().find(|(p, _)| p.to_lowercase() == "photolab");
        assert!(photolab.is_some());
        assert_eq!(photolab.unwrap().1, 4);

        // "Studio.TV" and "StudioTV" should be treated as equivalent
        let files = make_test_files(&[
            "Studio.TV.First.Episode.mp4",
            "StudioTV.Second.Episode.mp4",
            "StudioTV.Third.Episode.mp4",
            "Studiotv.Fourth.Episode.mp4",
        ]);
        let candidates = DirMove::find_prefix_candidates("StudioTV.Second.Episode.mp4", &files, 2);
        // All files should match on the single-part prefix (StudioTV = Studio.TV = Studiotv)
        assert!(!candidates.is_empty());
        // The 1-part prefix should match all 4 files
        let studiotv = candidates.iter().find(|(p, _)| p.to_lowercase() == "studiotv");
        assert!(studiotv.is_some());
        assert_eq!(studiotv.unwrap().1, 4);
    }

    #[test]
    fn dot_separated_three_parts_matches_concatenated() {
        // "Sun.Set.HD" and "SunSetHD" should be treated as equivalent
        let files = make_test_files(&[
            "Sun.Set.HD.Image.One.jpg",
            "SunSetHD.Image.Two.jpg",
            "Sunsethd.Image.Three.jpg",
        ]);
        let candidates = DirMove::find_prefix_candidates("SunSetHD.Image.Two.jpg", &files, 2);
        assert!(!candidates.is_empty());
        // The 1-part prefix "SunSetHD" should match all 3 files
        let sunsethd = candidates.iter().find(|(p, _)| p.to_lowercase() == "sunsethd");
        assert!(sunsethd.is_some());
        assert_eq!(sunsethd.unwrap().1, 3);

        // "Show.T.V" and "ShowTV" should be treated as equivalent
        let files = make_test_files(&[
            "Show.T.V.First.Episode.mp4",
            "ShowTV.Second.Episode.mp4",
            "Showtv.Third.Episode.mp4",
        ]);
        let candidates = DirMove::find_prefix_candidates("ShowTV.Second.Episode.mp4", &files, 2);
        assert!(!candidates.is_empty());
        let showtv = candidates.iter().find(|(p, _)| p.to_lowercase() == "showtv");
        assert!(showtv.is_some());
        assert_eq!(showtv.unwrap().1, 3);
    }

    #[test]
    fn normalize_prefix_removes_dots_and_lowercases() {
        assert_eq!(DirMove::normalize_prefix("PhotoLab"), "photolab");
        assert_eq!(DirMove::normalize_prefix("Photo.Lab"), "photolab");
        assert_eq!(DirMove::normalize_prefix("photo.lab"), "photolab");
        assert_eq!(DirMove::normalize_prefix("Album.Name.Here"), "albumnamehere");
        assert_eq!(DirMove::normalize_prefix("StudioTV"), "studiotv");
        assert_eq!(DirMove::normalize_prefix("Studio.TV"), "studiotv");
        assert_eq!(DirMove::normalize_prefix("studio.tv"), "studiotv");
        assert_eq!(DirMove::normalize_prefix("Show.Name.Here"), "shownamehere");
    }

    #[test]
    fn prefix_matches_normalized_single_part() {
        assert!(DirMove::prefix_matches_normalized("PhotoLab.Image.jpg", "photolab"));
        assert!(DirMove::prefix_matches_normalized("PHOTOLAB.Image.jpg", "photolab"));
        assert!(DirMove::prefix_matches_normalized("ShowTV.Episode.mp4", "showtv"));
        assert!(DirMove::prefix_matches_normalized("SHOWTV.Episode.mp4", "showtv"));
    }

    #[test]
    fn prefix_matches_normalized_two_parts() {
        assert!(DirMove::prefix_matches_normalized("Photo.Lab.Image.jpg", "photolab"));
        assert!(DirMove::prefix_matches_normalized("photo.lab.Image.jpg", "photolab"));
        assert!(DirMove::prefix_matches_normalized("Show.TV.Episode.mp4", "showtv"));
        assert!(DirMove::prefix_matches_normalized("show.tv.Episode.mp4", "showtv"));
    }

    #[test]
    fn prefix_matches_normalized_three_parts() {
        assert!(DirMove::prefix_matches_normalized("Ph.oto.Lab.Image.jpg", "photolab"));
        assert!(DirMove::prefix_matches_normalized("Sh.ow.TV.Episode.mp4", "showtv"));
    }

    #[test]
    fn prefix_matches_normalized_no_match() {
        assert!(!DirMove::prefix_matches_normalized("Other.Album.jpg", "photolab"));
        assert!(!DirMove::prefix_matches_normalized("PhotoLabX.Image.jpg", "photolab"));
        assert!(!DirMove::prefix_matches_normalized("Other.Show.mp4", "showtv"));
        assert!(!DirMove::prefix_matches_normalized("ShowTVX.Episode.mp4", "showtv"));
    }

    #[test]
    fn min_group_size_filters_small_groups() {
        let files = make_test_files(&[
            "Vacation.Photos.Image1.jpg",
            "Vacation.Photos.Image2.jpg",
            "Other.Album.Image1.jpg",
        ]);
        // With min_group_size=3, Vacation.Photos group (2 files) should not appear
        let candidates = DirMove::find_prefix_candidates("Vacation.Photos.Image1.jpg", &files, 3);
        assert!(candidates.is_empty());

        // With min_group_size=2, Vacation.Photos group should appear
        let candidates = DirMove::find_prefix_candidates("Vacation.Photos.Image1.jpg", &files, 2);
        assert!(!candidates.is_empty());
        assert_eq!(candidates[0].1, 2);
    }

    #[test]
    fn min_group_size_at_exact_threshold() {
        let files = make_test_files(&[
            "Beach.Summer.Photo1.jpg",
            "Beach.Summer.Photo2.jpg",
            "Beach.Summer.Photo3.jpg",
        ]);
        // Exactly 3 files, min_group_size=3 should match
        let candidates = DirMove::find_prefix_candidates("Beach.Summer.Photo1.jpg", &files, 3);
        assert!(!candidates.is_empty());
        assert!(candidates.iter().any(|(_, count)| *count == 3));

        // min_group_size=4 should not match
        let candidates = DirMove::find_prefix_candidates("Beach.Summer.Photo1.jpg", &files, 4);
        assert!(candidates.is_empty());
    }

    #[test]
    fn mixed_case_variations_all_group_together() {
        let files = make_test_files(&[
            "MyAlbum.Photo.One.jpg",
            "MYALBUM.Photo.Two.jpg",
            "myalbum.Photo.Three.jpg",
            "Myalbum.Photo.Four.jpg",
            "myAlbum.Photo.Five.jpg",
        ]);
        let candidates = DirMove::find_prefix_candidates("MyAlbum.Photo.One.jpg", &files, 2);
        assert!(!candidates.is_empty());
        // All 5 should be grouped together regardless of case
        let one_part = candidates.iter().find(|(p, _)| !p.contains('.'));
        assert!(one_part.is_some());
        assert_eq!(one_part.unwrap().1, 5);
    }

    #[test]
    fn dot_separated_with_mixed_case() {
        // Combining both dot-separation and case variations
        let files = make_test_files(&[
            "My.Album.Photo.One.jpg",
            "MyAlbum.Photo.Two.jpg",
            "MYALBUM.Photo.Three.jpg",
            "my.album.Photo.Four.jpg",
            "MY.ALBUM.Photo.Five.jpg",
        ]);
        let candidates = DirMove::find_prefix_candidates("MyAlbum.Photo.Two.jpg", &files, 2);
        assert!(!candidates.is_empty());
        // All should match: My.Album = MyAlbum = MYALBUM = my.album = MY.ALBUM
        let myalbum = candidates.iter().find(|(p, _)| p.to_lowercase() == "myalbum");
        assert!(myalbum.is_some());
        assert_eq!(myalbum.unwrap().1, 5);
    }

    #[test]
    fn dot_separated_two_parts_match_single_word() {
        // Two dot-separated parts should match single concatenated word
        let files = make_test_files(&[
            "Photo.Lab.Image1.jpg",
            "PhotoLab.Image2.jpg",
            "Photolab.Image3.jpg",
            "PHOTOLAB.Image4.jpg",
            "Photo.LAB.Image5.jpg",
        ]);
        let candidates = DirMove::find_prefix_candidates("PhotoLab.Image2.jpg", &files, 3);
        assert!(!candidates.is_empty());
        let photolab = candidates.iter().find(|(p, _)| p.to_lowercase() == "photolab");
        assert!(photolab.is_some());
        assert_eq!(photolab.unwrap().1, 5);
    }

    #[test]
    fn three_part_dot_separated_match_single_word() {
        let files = make_test_files(&[
            "Sun.Set.HD.Image1.jpg",
            "SunSetHD.Image2.jpg",
            "Sunsethd.Image3.jpg",
            "SUN.SET.HD.Image4.jpg",
        ]);
        let candidates = DirMove::find_prefix_candidates("SunSetHD.Image2.jpg", &files, 2);
        assert!(!candidates.is_empty());
        let sunsethd = candidates.iter().find(|(p, _)| p.to_lowercase() == "sunsethd");
        assert!(sunsethd.is_some());
        assert_eq!(sunsethd.unwrap().1, 4);
    }

    #[test]
    fn prefers_longer_prefix_over_short_with_more_matches() {
        let files = make_test_files(&[
            "Album.Name.Set1.Photo1.jpg",
            "Album.Name.Set1.Photo2.jpg",
            "Album.Name.Set2.Photo1.jpg",
            "Album.Other.Set1.Photo1.jpg",
        ]);
        let candidates = DirMove::find_prefix_candidates("Album.Name.Set1.Photo1.jpg", &files, 2);
        // Should offer longer prefixes first: 3-part, then 2-part, then 1-part
        assert_eq!(candidates.len(), 3);
        assert_eq!(candidates[0], (Cow::Borrowed("Album.Name.Set1"), 2));
        assert_eq!(candidates[1], (Cow::Borrowed("Album.Name"), 3));
        assert_eq!(candidates[2], (Cow::Borrowed("Album"), 4));
    }

    #[test]
    fn min_group_size_one_matches_single_file() {
        let files = make_test_files(&["Unique.Name.File.jpg", "Other.Name.File.jpg"]);
        let candidates = DirMove::find_prefix_candidates("Unique.Name.File.jpg", &files, 1);
        assert!(!candidates.is_empty());
        assert_eq!(candidates[0], (Cow::Borrowed("Unique.Name.File"), 1));
        assert_eq!(candidates[1], (Cow::Borrowed("Unique.Name"), 1));
        assert_eq!(candidates[2], (Cow::Borrowed("Unique"), 1));
    }

    #[test]
    fn high_min_group_size_filters_all() {
        let files = make_test_files(&[
            "Gallery.Photos.Img1.jpg",
            "Gallery.Photos.Img2.jpg",
            "Gallery.Photos.Img3.jpg",
            "Gallery.Photos.Img4.jpg",
            "Gallery.Photos.Img5.jpg",
        ]);
        // min_group_size=10 should filter everything (only 5 files)
        let candidates = DirMove::find_prefix_candidates("Gallery.Photos.Img1.jpg", &files, 10);
        assert!(candidates.is_empty());
    }

    #[test]
    fn identical_prefix_files_are_grouped() {
        // Files with exact same prefix should always be grouped
        let files = make_test_files(&[
            "Wedding.Photos.IMG001.jpg",
            "Wedding.Photos.IMG002.jpg",
            "Wedding.Photos.IMG003.jpg",
            "Wedding.Photos.IMG004.jpg",
        ]);
        let candidates = DirMove::find_prefix_candidates("Wedding.Photos.IMG001.jpg", &files, 2);
        assert!(!candidates.is_empty());
        // Should find 2-part prefix with all 4 files
        let two_part = candidates.iter().find(|(p, _)| *p == "Wedding.Photos");
        assert!(two_part.is_some());
        assert_eq!(two_part.unwrap().1, 4);
    }

    #[test]
    fn identical_single_word_prefix_grouped() {
        let files = make_test_files(&["Concert.Image1.jpg", "Concert.Image2.jpg", "Concert.Image3.jpg"]);
        let candidates = DirMove::find_prefix_candidates("Concert.Image1.jpg", &files, 2);
        assert!(!candidates.is_empty());
        let one_part = candidates.iter().find(|(p, _)| *p == "Concert");
        assert!(one_part.is_some());
        assert_eq!(one_part.unwrap().1, 3);
    }

    #[test]
    fn tv_show_season_episodes() {
        let files = make_test_files(&[
            "Drama.Series.S01E01.mp4",
            "Drama.Series.S01E02.mp4",
            "Drama.Series.S01E03.mp4",
            "Drama.Series.S02E01.mp4",
            "Drama.Series.S02E02.mp4",
        ]);
        let candidates = DirMove::find_prefix_candidates("Drama.Series.S01E01.mp4", &files, 2);
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0], (Cow::Borrowed("Drama.Series"), 5));
        assert_eq!(candidates[1], (Cow::Borrowed("Drama"), 5));
    }

    #[test]
    fn movie_series_with_years() {
        let files = make_test_files(&[
            "Studio.Action.2012.BluRay.mp4",
            "Studio.Action.2015.BluRay.mp4",
            "Studio.Comedy.2014.BluRay.mp4",
            "Studio.Comedy.2017.BluRay.mp4",
        ]);
        let candidates = DirMove::find_prefix_candidates("Studio.Action.2012.BluRay.mp4", &files, 2);
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0], (Cow::Borrowed("Studio.Action"), 2));
        assert_eq!(candidates[1], (Cow::Borrowed("Studio"), 4));
    }

    #[test]
    fn long_name_with_year_after_prefix() {
        let files = make_test_files(&[
            "Drama.Series.Name.2020.S01E01.Pilot.1080p.mp4",
            "Drama.Series.Name.2020.S01E02.Awakening.1080p.mp4",
            "Drama.Series.Name.2020.S01E03.Revelation.1080p.mp4",
            "Drama.Series.Name.2021.S02E01.Return.1080p.mp4",
        ]);
        let candidates = DirMove::find_prefix_candidates("Drama.Series.Name.2020.S01E01.Pilot.1080p.mp4", &files, 2);
        assert_eq!(candidates.len(), 3);
        assert_eq!(candidates[0], (Cow::Borrowed("Drama.Series.Name"), 4));
        assert_eq!(candidates[1], (Cow::Borrowed("Drama.Series"), 4));
        assert_eq!(candidates[2], (Cow::Borrowed("Drama"), 4));
    }

    #[test]
    fn long_name_with_only_year_after_prefix() {
        let files = make_test_files(&[
            "Action.Movie.Title.2019.Directors.Cut.mp4",
            "Action.Movie.Title.2020.Extended.Edition.mp4",
            "Action.Movie.Title.2021.Remastered.mp4",
        ]);
        let candidates = DirMove::find_prefix_candidates("Action.Movie.Title.2019.Directors.Cut.mp4", &files, 2);
        assert_eq!(candidates.len(), 3);
        assert_eq!(candidates[0], (Cow::Borrowed("Action.Movie.Title"), 3));
        assert_eq!(candidates[1], (Cow::Borrowed("Action.Movie"), 3));
        assert_eq!(candidates[2], (Cow::Borrowed("Action"), 3));
    }

    #[test]
    fn long_name_with_date_after_prefix() {
        let files = make_test_files(&[
            "Daily.News.Show.2024.01.15.Morning.Report.mp4",
            "Daily.News.Show.2024.01.16.Evening.Edition.mp4",
            "Daily.News.Show.2024.01.17.Special.Coverage.mp4",
        ]);
        let candidates = DirMove::find_prefix_candidates("Daily.News.Show.2024.01.15.Morning.Report.mp4", &files, 3);
        assert_eq!(candidates.len(), 3);
        assert_eq!(candidates[0], (Cow::Borrowed("Daily.News.Show"), 3));
        assert_eq!(candidates[1], (Cow::Borrowed("Daily.News"), 3));
        assert_eq!(candidates[2], (Cow::Borrowed("Daily"), 3));
    }

    #[test]
    fn long_name_franchise_with_year_variations() {
        let files = make_test_files(&[
            "Epic.Adventure.Saga.Part.One.2018.BluRay.Remux.mp4",
            "Epic.Adventure.Saga.Part.Two.2020.BluRay.Remux.mp4",
            "Epic.Adventure.Saga.Part.Three.2022.BluRay.Remux.mp4",
            "Epic.Adventure.Origins.Prequel.2015.BluRay.mp4",
        ]);
        let candidates =
            DirMove::find_prefix_candidates("Epic.Adventure.Saga.Part.One.2018.BluRay.Remux.mp4", &files, 2);
        assert_eq!(candidates.len(), 3);
        assert_eq!(candidates[0], (Cow::Borrowed("Epic.Adventure.Saga"), 3));
        assert_eq!(candidates[1], (Cow::Borrowed("Epic.Adventure"), 4));
        assert_eq!(candidates[2], (Cow::Borrowed("Epic"), 4));
    }

    #[test]
    fn long_name_season_with_year_in_name() {
        let files = make_test_files(&[
            "Anthology.Series.Collection.S01E01.Genesis.1080p.WEB.mp4",
            "Anthology.Series.Collection.S01E02.Exodus.1080p.WEB.mp4",
            "Anthology.Series.Collection.S02E01.Revival.1080p.WEB.mp4",
            "Anthology.Series.Collection.S02E02.Finale.1080p.WEB.mp4",
        ]);
        let candidates =
            DirMove::find_prefix_candidates("Anthology.Series.Collection.S01E01.Genesis.1080p.WEB.mp4", &files, 2);
        assert_eq!(candidates.len(), 3);
        assert_eq!(candidates[0], (Cow::Borrowed("Anthology.Series.Collection"), 4));
        assert_eq!(candidates[1], (Cow::Borrowed("Anthology.Series"), 4));
        assert_eq!(candidates[2], (Cow::Borrowed("Anthology"), 4));
    }

    #[test]
    fn long_name_documentary_with_regions() {
        let files = make_test_files(&[
            "Nature.Wildlife.Documentary.Africa.Savanna.2019.4K.mp4",
            "Nature.Wildlife.Documentary.Asia.Jungle.2020.4K.mp4",
            "Nature.Wildlife.Documentary.Europe.Alps.2021.4K.mp4",
        ]);
        let candidates =
            DirMove::find_prefix_candidates("Nature.Wildlife.Documentary.Africa.Savanna.2019.4K.mp4", &files, 3);
        assert_eq!(candidates.len(), 3);
        assert_eq!(candidates[0], (Cow::Borrowed("Nature.Wildlife.Documentary"), 3));
        assert_eq!(candidates[1], (Cow::Borrowed("Nature.Wildlife"), 3));
        assert_eq!(candidates[2], (Cow::Borrowed("Nature"), 3));
    }

    #[test]
    fn long_name_with_version_and_year() {
        let files = make_test_files(&[
            "Software.Tutorial.Guide.v2.2023.Intro.Basics.mp4",
            "Software.Tutorial.Guide.v2.2023.Advanced.Topics.mp4",
            "Software.Tutorial.Guide.v2.2023.Expert.Masterclass.mp4",
        ]);
        let candidates = DirMove::find_prefix_candidates("Software.Tutorial.Guide.v2.2023.Intro.Basics.mp4", &files, 3);
        assert_eq!(candidates.len(), 3);
        assert_eq!(candidates[0], (Cow::Borrowed("Software.Tutorial.Guide"), 3));
        assert_eq!(candidates[1], (Cow::Borrowed("Software.Tutorial"), 3));
        assert_eq!(candidates[2], (Cow::Borrowed("Software"), 3));
    }

    #[test]
    fn short_names_with_extensions() {
        let files = make_test_files(&["A.B.mp4", "A.C.mp4", "A.D.mp4"]);
        let candidates = DirMove::find_prefix_candidates("A.B.mp4", &files, 2);
        let a_candidate = candidates.iter().find(|(p, _)| p.to_lowercase() == "a");
        assert!(a_candidate.is_some());
        assert_eq!(a_candidate.unwrap().1, 3);
    }

    #[test]
    fn with_filtered_numeric_parts() {
        let files = make_test_files(&["Show.Name.S01.mp4", "Show.Name.S02.mp4", "Show.Name.S03.mp4"]);
        let candidates = DirMove::find_prefix_candidates("Show.Name.S01.mp4", &files, 2);
        let show_name = candidates.iter().find(|(p, _)| p.to_lowercase() == "show.name");
        assert!(show_name.is_some());
        assert_eq!(show_name.unwrap().1, 3);
    }

    #[test]
    fn numeric_filtering_groups_correctly() {
        let filtered_files = make_test_files(&["ABC.Thing.v1.mp4", "ABC.Thing.v2.mp4", "ABC.Thing.v3.mp4"]);
        let candidates = DirMove::find_prefix_candidates("ABC.Thing.v1.mp4", &filtered_files, 3);
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0], (Cow::Borrowed("ABC.Thing"), 3));
        assert_eq!(candidates[1], (Cow::Borrowed("ABC"), 3));
    }

    #[test]
    fn mixed_years_without_filtering() {
        let unfiltered_files = make_test_files(&["ABC.2023.Thing.mp4", "ABC.2024.Other.mp4", "ABC.2025.More.mp4"]);
        let candidates = DirMove::find_prefix_candidates("ABC.2023.Thing.mp4", &unfiltered_files, 3);
        assert_eq!(candidates, vec![(Cow::Borrowed("ABC"), 3)]);
    }

    #[test]
    fn tv_show_generic_scenario() {
        let filtered_files = make_test_files(&[
            "Series.Name.S01E01.1080p.mp4",
            "Series.Name.S01E02.1080p.mp4",
            "Series.Name.S01E03.1080p.mp4",
        ]);
        let candidates = DirMove::find_prefix_candidates("Series.Name.S01E01.1080p.mp4", &filtered_files, 3);
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0], (Cow::Borrowed("Series.Name"), 3));
        assert_eq!(candidates[1], (Cow::Borrowed("Series"), 3));
    }

    #[test]
    fn long_name_mixed_year_positions() {
        let files = make_test_files(&[
            "Studio.Franchise.Title.Original.2020.Remastered.2023.HDR.mp4",
            "Studio.Franchise.Title.Original.2020.Remastered.2024.HDR.mp4",
            "Studio.Franchise.Other.Sequel.2021.Remastered.2023.HDR.mp4",
        ]);
        let candidates = DirMove::find_prefix_candidates(
            "Studio.Franchise.Title.Original.2020.Remastered.2023.HDR.mp4",
            &files,
            2,
        );
        assert_eq!(candidates.len(), 3);
        assert_eq!(candidates[0], (Cow::Borrowed("Studio.Franchise.Title"), 2));
        assert_eq!(candidates[1], (Cow::Borrowed("Studio.Franchise"), 3));
        assert_eq!(candidates[2], (Cow::Borrowed("Studio"), 3));
    }

    #[test]
    fn long_name_decade_in_title() {
        let files = make_test_files(&[
            "Retro.Eighties.Collection.Vol1.Greatest.Hits.mp4",
            "Retro.Eighties.Collection.Vol2.Classic.Cuts.mp4",
            "Retro.Eighties.Collection.Vol3.Deep.Tracks.mp4",
        ]);
        let candidates = DirMove::find_prefix_candidates("Retro.Eighties.Collection.Vol1.Greatest.Hits.mp4", &files, 3);
        assert_eq!(candidates.len(), 3);
        assert_eq!(candidates[0], (Cow::Borrowed("Retro.Eighties.Collection"), 3));
        assert_eq!(candidates[1], (Cow::Borrowed("Retro.Eighties"), 3));
        assert_eq!(candidates[2], (Cow::Borrowed("Retro"), 3));
    }

    #[test]
    fn three_part_prefix_with_mixed_fourth_parts() {
        let files = make_test_files(&[
            "Alpha.Beta.Gamma.One.mp4",
            "Alpha.Beta.Gamma.Two.mp4",
            "Alpha.Beta.Gamma.Three.mp4",
            "Alpha.Beta.Delta.One.mp4",
        ]);
        let candidates = DirMove::find_prefix_candidates("Alpha.Beta.Gamma.One.mp4", &files, 2);
        assert_eq!(candidates.len(), 3);
        assert_eq!(candidates[0], (Cow::Borrowed("Alpha.Beta.Gamma"), 3));
        assert_eq!(candidates[1], (Cow::Borrowed("Alpha.Beta"), 4));
        assert_eq!(candidates[2], (Cow::Borrowed("Alpha"), 4));
    }

    #[test]
    fn only_two_files_high_threshold() {
        let files = make_test_files(&["Show.Name.v1.mp4", "Show.Name.v2.mp4"]);
        let candidates = DirMove::find_prefix_candidates("Show.Name.v1.mp4", &files, 5);
        // With min_group_size=5 and only 2 files, no candidates should qualify
        assert!(candidates.is_empty());
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
            min_group_size: 2,
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
        assert_eq!(groups.get("Show.Name").unwrap().len(), 3);
        assert_eq!(groups.get("Show.Other").unwrap().len(), 2);
        assert_eq!(groups.get("Show").unwrap().len(), 5);
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
            min_group_size: 2,
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
        assert_eq!(groups.get("Alpha.Beta.Gamma").unwrap().len(), 2);
        assert_eq!(groups.get("Alpha.Beta.Delta").unwrap().len(), 2);
        assert_eq!(groups.get("Alpha.Beta").unwrap().len(), 4);
        assert_eq!(groups.get("Alpha").unwrap().len(), 4);
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
            min_group_size: 2,
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
            min_group_size: 2,
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
            min_group_size: 2,
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
            min_group_size: 3,
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
        assert_eq!(groups.get("SeriesA.Season1").unwrap().len(), 10);
        assert_eq!(groups.get("SeriesA.Season2").unwrap().len(), 8);
        assert_eq!(groups.get("SeriesA").unwrap().len(), 18);
        assert_eq!(groups.get("SeriesB.Season1").unwrap().len(), 5);
        assert_eq!(groups.get("SeriesB").unwrap().len(), 5);
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
            min_group_size: 2,
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
            min_group_size: 5,
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
            min_group_size: 3,
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
        let max_group_size = groups.values().map(Vec::len).max().unwrap_or(0);
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
            min_group_size: 2,
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
        let max_group_size = groups.values().map(Vec::len).max().unwrap_or(0);
        assert_eq!(max_group_size, 4, "Should have a group with all 4 files");
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
            min_group_size: 2,
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
        assert_eq!(groups.get("Summer.Vacation").unwrap().len(), 4);
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

        // With min_group_size=6, should find no groups
        let config_6 = Config {
            auto: false,
            create: true,
            debug: false,
            dryrun: true,
            include: Vec::new(),
            exclude: Vec::new(),
            min_group_size: 6,
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
        assert!(
            groups_6.is_empty(),
            "min_group_size=6 should find no groups with only 5 files"
        );

        // With min_group_size=5, should find the group
        let config_5 = Config {
            auto: false,
            create: true,
            debug: false,
            dryrun: true,
            include: Vec::new(),
            exclude: Vec::new(),
            min_group_size: 5,
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
        assert_eq!(groups_5.get("Gallery.Photos").unwrap().len(), 5);

        // With min_group_size=3, should still find the same group
        let config_3 = Config {
            auto: false,
            create: true,
            debug: false,
            dryrun: true,
            include: Vec::new(),
            exclude: Vec::new(),
            min_group_size: 3,
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
        assert_eq!(groups_3.get("Gallery.Photos").unwrap().len(), 5);
    }
}

#[cfg(test)]
mod test_filtering {
    use super::*;

    #[test]
    fn removes_year() {
        let result = DirMove::filter_numeric_resolution_and_glue_parts("Show.2024.Episode.mp4");
        assert_eq!(result, "Show.Episode.mp4");
    }

    #[test]
    fn removes_multiple_numeric() {
        let result = DirMove::filter_numeric_resolution_and_glue_parts("Show.2024.01.Episode.mp4");
        assert_eq!(result, "Show.Episode.mp4");
    }

    #[test]
    fn keeps_mixed_alphanumeric() {
        let result = DirMove::filter_numeric_resolution_and_glue_parts("Show.S01E02.Episode.mp4");
        assert_eq!(result, "Show.S01E02.Episode.mp4");
    }

    #[test]
    fn no_numeric_parts() {
        let result = DirMove::filter_numeric_resolution_and_glue_parts("Show.Name.Episode.mp4");
        assert_eq!(result, "Show.Name.Episode.mp4");
    }

    #[test]
    fn all_numeric_except_extension() {
        let result = DirMove::filter_numeric_resolution_and_glue_parts("2024.01.15.mp4");
        assert_eq!(result, "mp4");
    }

    #[test]
    fn empty_string() {
        let result = DirMove::filter_numeric_resolution_and_glue_parts("");
        assert_eq!(result, "");
    }

    #[test]
    fn single_part() {
        let result = DirMove::filter_numeric_resolution_and_glue_parts("file.mp4");
        assert_eq!(result, "file.mp4");
    }

    #[test]
    fn removes_1080p() {
        let result = DirMove::filter_numeric_resolution_and_glue_parts("Movie.Name.1080p.BluRay.mp4");
        assert_eq!(result, "Movie.Name.BluRay.mp4");
    }

    #[test]
    fn removes_2160p() {
        let result = DirMove::filter_numeric_resolution_and_glue_parts("Movie.Name.2160p.UHD.mp4");
        assert_eq!(result, "Movie.Name.UHD.mp4");
    }

    #[test]
    fn removes_720p() {
        let result = DirMove::filter_numeric_resolution_and_glue_parts("Movie.Name.720p.WEB.mp4");
        assert_eq!(result, "Movie.Name.WEB.mp4");
    }

    #[test]
    fn removes_dimension_format() {
        let result = DirMove::filter_numeric_resolution_and_glue_parts("Video.1920x1080.Sample.mp4");
        assert_eq!(result, "Video.Sample.mp4");
    }

    #[test]
    fn removes_smaller_dimension() {
        let result = DirMove::filter_numeric_resolution_and_glue_parts("Video.640x480.Old.mp4");
        assert_eq!(result, "Video.Old.mp4");
    }

    #[test]
    fn case_insensitive_resolution() {
        let result = DirMove::filter_numeric_resolution_and_glue_parts("Movie.Name.1080P.BluRay.mp4");
        assert_eq!(result, "Movie.Name.BluRay.mp4");
    }

    #[test]
    fn removes_and_glue_word() {
        let result = DirMove::filter_numeric_resolution_and_glue_parts("Show.and.Tell.mp4");
        assert_eq!(result, "Show.Tell.mp4");
    }

    #[test]
    fn removes_the_glue_word() {
        let result = DirMove::filter_numeric_resolution_and_glue_parts("The.Movie.Name.mp4");
        assert_eq!(result, "Movie.Name.mp4");
    }

    #[test]
    fn removes_multiple_glue_words() {
        let result = DirMove::filter_numeric_resolution_and_glue_parts("The.Show.and.The.Tell.mp4");
        assert_eq!(result, "Show.Tell.mp4");
    }

    #[test]
    fn glue_words_case_insensitive() {
        let result = DirMove::filter_numeric_resolution_and_glue_parts("THE.Show.AND.Tell.mp4");
        assert_eq!(result, "Show.Tell.mp4");
    }

    #[test]
    fn removes_all_glue_words() {
        let result = DirMove::filter_numeric_resolution_and_glue_parts("a.an.the.and.of.mp4");
        assert_eq!(result, "mp4");
    }

    #[test]
    fn complex_filtering_year_resolution_glue() {
        let result = DirMove::filter_numeric_resolution_and_glue_parts("The.Movie.2024.1080p.and.More.mp4");
        assert_eq!(result, "Movie.More.mp4");
    }

    #[test]
    fn preserves_episode_codes() {
        let result = DirMove::filter_numeric_resolution_and_glue_parts("Show.S01E01.2024.1080p.mp4");
        assert_eq!(result, "Show.S01E01.mp4");
    }

    #[test]
    fn keeps_4k_not_matched_by_resolution_regex() {
        let result = DirMove::filter_numeric_resolution_and_glue_parts("Movie.4K.HDR.mp4");
        assert_eq!(result, "Movie.4K.HDR.mp4");
    }

    #[test]
    fn multiple_resolutions() {
        let result = DirMove::filter_numeric_resolution_and_glue_parts("Movie.1080p.2160p.720p.mp4");
        assert_eq!(result, "Movie.mp4");
    }

    #[test]
    fn only_extension_left() {
        let result = DirMove::filter_numeric_resolution_and_glue_parts("2024.1080p.the.mp4");
        assert_eq!(result, "mp4");
    }

    #[test]
    fn no_dots_in_name() {
        let result = DirMove::filter_numeric_resolution_and_glue_parts("filename");
        assert_eq!(result, "filename");
    }

    #[test]
    fn consecutive_dots() {
        let result = DirMove::filter_numeric_resolution_and_glue_parts("Show..Name..mp4");
        assert_eq!(result, "Show..Name..mp4");
    }

    #[test]
    fn generic_movie_name() {
        let result = DirMove::filter_numeric_resolution_and_glue_parts(
            "The.Action.Film.1999.Remastered.2160p.UHD.BluRay.x265.mp4",
        );
        assert_eq!(result, "Action.Film.Remastered.UHD.BluRay.x265.mp4");
    }

    #[test]
    fn generic_tv_show() {
        let result = DirMove::filter_numeric_resolution_and_glue_parts("Drama.Series.S05E16.2013.1080p.BluRay.mp4");
        assert_eq!(result, "Drama.Series.S05E16.BluRay.mp4");
    }
}

#[cfg(test)]
mod test_prefix_overrides {
    use super::test_helpers::*;
    use super::*;

    #[test]
    fn no_overrides() {
        let dirmove = make_test_dirmove(Vec::new());
        let mut groups: HashMap<String, Vec<PathBuf>> = HashMap::new();
        groups.insert("Some.Name.Thing".to_string(), vec![PathBuf::from("file1.mp4")]);

        let result = dirmove.apply_prefix_overrides(groups);
        assert!(result.contains_key("Some.Name.Thing"));
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn matching_override() {
        let dirmove = make_test_dirmove(vec!["longer.prefix".to_string()]);
        let mut groups: HashMap<String, Vec<PathBuf>> = HashMap::new();
        groups.insert(
            "longer.prefix.name".to_string(),
            vec![PathBuf::from("longer.prefix.name.file.mp4")],
        );

        let result = dirmove.apply_prefix_overrides(groups);
        assert!(result.contains_key("longer.prefix"));
        assert!(!result.contains_key("longer.prefix.name"));
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn merges_groups() {
        let dirmove = make_test_dirmove(vec!["Some.Name".to_string()]);
        let mut groups: HashMap<String, Vec<PathBuf>> = HashMap::new();
        groups.insert("Some.Name.Thing".to_string(), vec![PathBuf::from("file1.mp4")]);
        groups.insert("Some.Name.Other".to_string(), vec![PathBuf::from("file2.mp4")]);

        let result = dirmove.apply_prefix_overrides(groups);
        assert!(result.contains_key("Some.Name"));
        assert_eq!(result.len(), 1);
        assert_eq!(result.get("Some.Name").map(Vec::len), Some(2));
    }

    #[test]
    fn non_matching() {
        let dirmove = make_test_dirmove(vec!["Other.Prefix".to_string()]);
        let mut groups: HashMap<String, Vec<PathBuf>> = HashMap::new();
        groups.insert("Some.Name.Thing".to_string(), vec![PathBuf::from("file1.mp4")]);

        let result = dirmove.apply_prefix_overrides(groups);
        assert!(result.contains_key("Some.Name.Thing"));
        assert!(!result.contains_key("Other.Prefix"));
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn partial_match_only() {
        let dirmove = make_test_dirmove(vec!["Some".to_string()]);
        let mut groups: HashMap<String, Vec<PathBuf>> = HashMap::new();
        groups.insert("Something.Else".to_string(), vec![PathBuf::from("file1.mp4")]);
        groups.insert("Some.Name".to_string(), vec![PathBuf::from("file2.mp4")]);

        let result = dirmove.apply_prefix_overrides(groups);
        assert!(result.contains_key("Some"));
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn override_more_specific_than_prefix() {
        let dirmove = make_test_dirmove(vec!["Example.Name".to_string()]);
        let mut groups: HashMap<String, Vec<PathBuf>> = HashMap::new();
        groups.insert(
            "Example".to_string(),
            vec![
                PathBuf::from("Example.Name.Video1.mp4"),
                PathBuf::from("Example.Name.Video2.mp4"),
                PathBuf::from("Example.Name.Video3.mp4"),
            ],
        );

        let result = dirmove.apply_prefix_overrides(groups);
        assert!(result.contains_key("Example.Name"));
        assert!(!result.contains_key("Example"));
        assert_eq!(result.len(), 1);
        assert_eq!(result.get("Example.Name").map(Vec::len), Some(3));
    }

    #[test]
    fn multiple_overrides() {
        let dirmove = make_test_dirmove(vec!["Show.A".to_string(), "Show.B".to_string()]);
        let mut groups: HashMap<String, Vec<PathBuf>> = HashMap::new();
        groups.insert("Show.A.Season1".to_string(), vec![PathBuf::from("file1.mp4")]);
        groups.insert("Show.B.Season1".to_string(), vec![PathBuf::from("file2.mp4")]);
        groups.insert("Show.C.Season1".to_string(), vec![PathBuf::from("file3.mp4")]);

        let result = dirmove.apply_prefix_overrides(groups);
        assert!(result.contains_key("Show.A"));
        assert!(result.contains_key("Show.B"));
        assert!(result.contains_key("Show.C.Season1"));
        assert_eq!(result.len(), 3);
    }

    #[test]
    fn empty_groups() {
        let dirmove = make_test_dirmove(vec!["Some".to_string()]);
        let groups: HashMap<String, Vec<PathBuf>> = HashMap::new();

        let result = dirmove.apply_prefix_overrides(groups);
        assert!(result.is_empty());
    }

    #[test]
    fn override_with_case_sensitivity() {
        let dirmove = make_test_dirmove(vec!["show.name".to_string()]);
        let mut groups: HashMap<String, Vec<PathBuf>> = HashMap::new();
        groups.insert("Show.Name.Season1".to_string(), vec![PathBuf::from("file1.mp4")]);

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

        let filtered: Vec<(PathBuf, String)> = original_filenames
            .iter()
            .map(|name| {
                let filtered_name = DirMove::filter_numeric_resolution_and_glue_parts(name);
                (PathBuf::from(*name), filtered_name)
            })
            .collect();

        assert_eq!(filtered[0].1, "ShowName.S01E01.mp4");
        assert_eq!(filtered[1].1, "ShowName.S01E02.mp4");
        assert_eq!(filtered[2].1, "ShowName.S01E03.mp4");
        assert_eq!(filtered[3].1, "OtherShow.Special.mp4");
        assert_eq!(filtered[4].1, "OtherShow.Episode.mp4");

        let show_candidates = DirMove::find_prefix_candidates(&filtered[0].1, &filtered, 3);
        assert_eq!(show_candidates, vec![(Cow::Borrowed("ShowName"), 3)]);

        let other_candidates = DirMove::find_prefix_candidates(&filtered[3].1, &filtered, 2);
        assert_eq!(other_candidates, vec![(Cow::Borrowed("OtherShow"), 2)]);
    }

    #[test]
    fn with_resolution_numbers() {
        let original_filenames = [
            "MovieName.2024.720.rip.mp4",
            "MovieName.2024.720.other.mp4",
            "MovieName.2024.720.more.mp4",
        ];

        let filtered: Vec<(PathBuf, String)> = original_filenames
            .iter()
            .map(|name| {
                let filtered_name = DirMove::filter_numeric_resolution_and_glue_parts(name);
                (PathBuf::from(*name), filtered_name)
            })
            .collect();

        assert_eq!(filtered[0].1, "MovieName.rip.mp4");
        assert_eq!(filtered[1].1, "MovieName.other.mp4");
        assert_eq!(filtered[2].1, "MovieName.more.mp4");

        let candidates = DirMove::find_prefix_candidates(&filtered[0].1, &filtered, 3);
        assert_eq!(candidates, vec![(Cow::Borrowed("MovieName"), 3)]);
    }

    #[test]
    fn with_resolution_pattern() {
        let original_filenames = [
            "MovieName.2024.1080p.rip.mp4",
            "MovieName.2024.1080p.other.mp4",
            "MovieName.2024.1080p.more.mp4",
        ];

        let filtered: Vec<(PathBuf, String)> = original_filenames
            .iter()
            .map(|name| {
                let filtered_name = DirMove::filter_numeric_resolution_and_glue_parts(name);
                (PathBuf::from(*name), filtered_name)
            })
            .collect();

        assert_eq!(filtered[0].1, "MovieName.rip.mp4");
        assert_eq!(filtered[1].1, "MovieName.other.mp4");
        assert_eq!(filtered[2].1, "MovieName.more.mp4");

        let candidates = DirMove::find_prefix_candidates(&filtered[0].1, &filtered, 3);
        assert_eq!(candidates, vec![(Cow::Borrowed("MovieName"), 3)]);
    }

    #[test]
    fn with_glue_words() {
        let original_filenames = [
            "Show.and.Tell.part1.mp4",
            "Show.and.Tell.part2.mp4",
            "Show.and.Tell.part3.mp4",
        ];

        let filtered: Vec<(PathBuf, String)> = original_filenames
            .iter()
            .map(|name| {
                let filtered_name = DirMove::filter_numeric_resolution_and_glue_parts(name);
                (PathBuf::from(*name), filtered_name)
            })
            .collect();

        assert_eq!(filtered[0].1, "Show.Tell.part1.mp4");
        assert_eq!(filtered[1].1, "Show.Tell.part2.mp4");
        assert_eq!(filtered[2].1, "Show.Tell.part3.mp4");

        let candidates = DirMove::find_prefix_candidates(&filtered[0].1, &filtered, 3);
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0], (Cow::Borrowed("Show.Tell"), 3));
        assert_eq!(candidates[1], (Cow::Borrowed("Show"), 3));
    }

    #[test]
    fn short_prefix_with_shared_parts() {
        let original_filenames = [
            "ABC.2023.Thing.v1.mp4",
            "ABC.2024.Thing.v2.mp4",
            "ABC.2025.Thing.v3.mp4",
        ];

        let unfiltered: Vec<(PathBuf, String)> = original_filenames
            .iter()
            .map(|name| (PathBuf::from(*name), (*name).to_string()))
            .collect();

        let candidates_unfiltered = DirMove::find_prefix_candidates(&unfiltered[0].1, &unfiltered, 3);
        assert_eq!(candidates_unfiltered, vec![(Cow::Borrowed("ABC"), 3)]);

        let filtered: Vec<(PathBuf, String)> = original_filenames
            .iter()
            .map(|name| {
                let filtered_name = DirMove::filter_numeric_resolution_and_glue_parts(name);
                (PathBuf::from(*name), filtered_name)
            })
            .collect();

        assert_eq!(filtered[0].1, "ABC.Thing.v1.mp4");
        assert_eq!(filtered[1].1, "ABC.Thing.v2.mp4");
        assert_eq!(filtered[2].1, "ABC.Thing.v3.mp4");

        let candidates = DirMove::find_prefix_candidates(&filtered[0].1, &filtered, 3);
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0], (Cow::Borrowed("ABC.Thing"), 3));
        assert_eq!(candidates[1], (Cow::Borrowed("ABC"), 3));
    }

    #[test]
    fn files_with_resolution_grouped_correctly() {
        let original_filenames = [
            "Some.Video.1080p.part1.mp4",
            "Some.Video.1080p.part2.mp4",
            "Some.Video.1080p.part3.mp4",
        ];

        let filtered: Vec<(PathBuf, String)> = original_filenames
            .iter()
            .map(|name| {
                let filtered_name = DirMove::filter_numeric_resolution_and_glue_parts(name);
                (PathBuf::from(*name), filtered_name)
            })
            .collect();

        assert_eq!(filtered[0].1, "Some.Video.part1.mp4");

        let candidates = DirMove::find_prefix_candidates(&filtered[0].1, &filtered, 3);
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0], (Cow::Borrowed("Some.Video"), 3));

        let dir_name = candidates[0].0.replace('.', " ");
        assert_eq!(dir_name, "Some Video");
    }

    #[test]
    fn files_with_2160p_resolution() {
        let original_filenames = [
            "Movie.Name.2160p.file1.mp4",
            "Movie.Name.2160p.file2.mp4",
            "Movie.Name.2160p.file3.mp4",
        ];

        let filtered: Vec<(PathBuf, String)> = original_filenames
            .iter()
            .map(|name| {
                let filtered_name = DirMove::filter_numeric_resolution_and_glue_parts(name);
                (PathBuf::from(*name), filtered_name)
            })
            .collect();

        assert_eq!(filtered[0].1, "Movie.Name.file1.mp4");

        let candidates = DirMove::find_prefix_candidates(&filtered[0].1, &filtered, 3);
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0], (Cow::Borrowed("Movie.Name"), 3));

        let dir_name = candidates[0].0.replace('.', " ");
        assert_eq!(dir_name, "Movie Name");
    }

    #[test]
    fn files_with_dimension_resolution() {
        let original_filenames = [
            "Cool.Stuff.1920x1080.part1.mp4",
            "Cool.Stuff.1920x1080.part2.mp4",
            "Cool.Stuff.1920x1080.part3.mp4",
        ];

        let filtered: Vec<(PathBuf, String)> = original_filenames
            .iter()
            .map(|name| {
                let filtered_name = DirMove::filter_numeric_resolution_and_glue_parts(name);
                (PathBuf::from(*name), filtered_name)
            })
            .collect();

        assert_eq!(filtered[0].1, "Cool.Stuff.part1.mp4");

        let candidates = DirMove::find_prefix_candidates(&filtered[0].1, &filtered, 3);
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0], (Cow::Borrowed("Cool.Stuff"), 3));

        let dir_name = candidates[0].0.replace('.', " ");
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

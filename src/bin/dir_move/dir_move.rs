use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use colored::Colorize;
use itertools::Itertools;
use regex::Regex;
use walkdir::WalkDir;

/// Regex to match video resolutions like 1080p, 2160p, or 1920x1080.
static RE_RESOLUTION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(\d{3,4}p|\d{3,4}x\d{3,4})\b").expect("Invalid resolution regex"));

/// Common glue words to filter out from grouping names.
const GLUE_WORDS: &[&str] = &[
    "a", "an", "and", "at", "by", "for", "in", "of", "on", "or", "the", "to", "with",
];

use cli_tools::{
    get_relative_path_or_filename, path_to_filename_string, path_to_string_relative, print_bold, print_error,
    print_magenta, print_warning,
};

use crate::DirMoveArgs;
use crate::config::Config;

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

/// Information about what needs to be moved during an unpack operation.
#[derive(Debug, Default)]
struct UnpackInfo {
    /// Files to move: (source, destination).
    file_moves: Vec<(PathBuf, PathBuf)>,
    /// Directories to move directly: (source, destination).
    direct_dir_moves: Vec<(PathBuf, PathBuf)>,
}

impl DirectoryInfo {
    fn new(path: PathBuf) -> Self {
        let name = path_to_filename_string(&path).to_lowercase().replace('.', " ");
        Self { path, name }
    }
}

impl DirMove {
    pub fn new(args: DirMoveArgs) -> anyhow::Result<Self> {
        let root = cli_tools::resolve_input_path(args.path.as_deref())?;
        let config = Config::from_args(args);
        if config.debug {
            eprintln!("Config: {config:#?}");
            eprintln!("Root: {}", root.display());
        }
        Ok(Self { root, config })
    }

    pub fn run(&self) -> anyhow::Result<()> {
        // Optional: unpack configured directory names by moving their contents up one level.
        // - If recurse is enabled, this searches recursively.
        // - Otherwise, it only checks directories directly under root.
        if !self.config.unpack_directory_names.is_empty() {
            self.unpack_directories()?;
        }

        // Normal directory matching/moving always runs first.
        self.move_files_to_dir()?;

        // Optional: after the normal move (and optional unpack), create dirs by prefix and move.
        if self.config.create {
            self.create_dirs_and_move_files()?;
        }

        Ok(())
    }

    /// Unpack directories with names matching config.
    ///
    /// For each matching directory `.../<match>/...`, move its entire contents to the parent directory,
    /// preserving the structure below `<match>`. For example:
    ///
    /// `Example/Videos/Name/file2.txt` -> `Example/Name/file2.txt`
    ///
    /// Prunes empty directories that were touched by this unpack operation or already empty directories that match.
    fn unpack_directories(&self) -> anyhow::Result<()> {
        if self.config.unpack_directory_names.is_empty() {
            return Ok(());
        }

        let candidates = self.collect_unpack_candidates();

        // Track directories we touched so we can prune empties safely.
        let mut touched_dirs: HashSet<PathBuf> = HashSet::new();

        for vdir in candidates {
            if !vdir.exists() {
                continue;
            }

            let Some(parent) = vdir.parent().map(Path::to_path_buf) else {
                continue;
            };

            let unpack_info = self.collect_unpack_info(&vdir, &parent);
            self.print_unpack_summary(&parent, &unpack_info);

            if self.config.dryrun {
                continue;
            }

            // Move directories that don't match unpack names directly (more efficient).
            for dir_move in &unpack_info.direct_dir_moves {
                self.move_directory(&dir_move.0, &dir_move.1, &mut touched_dirs)?;
            }

            // Move individual files.
            for (src, dst) in &unpack_info.file_moves {
                self.unpack_move_one_file(src, dst, &mut touched_dirs)?;
            }

            touched_dirs.insert(vdir.clone());
            touched_dirs.insert(parent);

            self.prune_empty_dirs_under(&vdir, &mut touched_dirs)?;
        }

        Ok(())
    }

    fn collect_unpack_candidates(&self) -> Vec<PathBuf> {
        let mut candidates = Vec::new();

        let walker = if self.config.recurse {
            WalkDir::new(&self.root)
        } else {
            WalkDir::new(&self.root).max_depth(1)
        };

        for entry in walker
            .into_iter()
            .filter_entry(|e| !cli_tools::should_skip_entry(e))
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

            if self.config.unpack_directory_names.contains(&name.to_lowercase()) {
                candidates.push(entry.path().to_path_buf());
            }
        }

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

        root_candidates
    }

    /// Information about what needs to be moved during an unpack operation.
    fn collect_unpack_info(&self, vdir: &Path, parent: &Path) -> UnpackInfo {
        let mut info = UnpackInfo::default();

        let Ok(entries) = std::fs::read_dir(vdir) else {
            return info;
        };

        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };

            let dst = parent.join(name);

            if path.is_file() {
                info.file_moves.push((path, dst));
            } else if path.is_dir() {
                // If subdirectory name matches an unpack directory name, recurse into it.
                // Otherwise, check if it contains nested unpack directories.
                if self.config.unpack_directory_names.contains(&name.to_lowercase()) {
                    // Recursively collect from nested matching directories.
                    let nested_info = self.collect_unpack_info(&path, parent);
                    info.file_moves.extend(nested_info.file_moves);
                    info.direct_dir_moves.extend(nested_info.direct_dir_moves);
                } else if self.contains_unpack_directory(&path) {
                    // Non-matching directory contains nested unpack dirs, recurse into it
                    // with this directory as the new parent.
                    let nested_info = self.collect_unpack_info(&path, &dst);
                    info.file_moves.extend(nested_info.file_moves);
                    info.direct_dir_moves.extend(nested_info.direct_dir_moves);
                } else {
                    // No nested unpack dirs, move the entire directory directly (more efficient).
                    info.direct_dir_moves.push((path, dst));
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
        let dir_count = info.direct_dir_moves.len();

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

        for (input, output) in &info.direct_dir_moves {
            let src_display = get_relative_path_or_filename(input, target_dir);
            let dst_display = get_relative_path_or_filename(output, target_dir);
            println!("  {src_display} -> {dst_display}");
        }
        if self.config.verbose {
            for (input, output) in &info.file_moves {
                let src_display = get_relative_path_or_filename(input, target_dir);
                let dst_display = get_relative_path_or_filename(output, target_dir);
                println!("  {src_display} -> {dst_display}");
            }
        }
    }

    /// Move an entire directory to a new location.
    fn move_directory(&self, src: &Path, dst: &Path, touched_dirs: &mut HashSet<PathBuf>) -> anyhow::Result<()> {
        if dst.exists() && !self.config.overwrite {
            print_warning!("Skipping existing directory: {}", dst.display());
            return Ok(());
        }

        if let Some(src_parent) = src.parent() {
            touched_dirs.insert(src_parent.to_path_buf());
        }

        // Try rename first (fast, same filesystem).
        // Fall back to recursive copy + remove for cross-device moves.
        if std::fs::rename(src, dst).is_err() {
            Self::copy_dir_recursive(src, dst)?;
            std::fs::remove_dir_all(src)?;
        }

        Ok(())
    }

    /// Recursively copy a directory and its contents.
    fn copy_dir_recursive(src: &Path, dst: &Path) -> anyhow::Result<()> {
        std::fs::create_dir_all(dst)?;

        for entry in std::fs::read_dir(src)?.filter_map(Result::ok) {
            let src_path = entry.path();
            let dst_path = dst.join(entry.file_name());

            if src_path.is_dir() {
                Self::copy_dir_recursive(&src_path, &dst_path)?;
            } else {
                std::fs::copy(&src_path, &dst_path)?;
            }
        }

        Ok(())
    }

    fn unpack_move_one_file(&self, src: &Path, dst: &Path, touched_dirs: &mut HashSet<PathBuf>) -> anyhow::Result<()> {
        if dst.exists() && !self.config.overwrite {
            print_warning!("Skipping existing file: {}", dst.display());
            return Ok(());
        }

        if let Some(dst_parent) = dst.parent() {
            if !dst_parent.exists() {
                std::fs::create_dir_all(dst_parent)?;
            }
            touched_dirs.insert(dst_parent.to_path_buf());
        }

        if let Some(src_parent) = src.parent() {
            touched_dirs.insert(src_parent.to_path_buf());
        }

        // Rename is preferred; if it fails (e.g. cross-device), fall back to copy+remove.
        if std::fs::rename(src, dst).is_err() {
            std::fs::copy(src, dst)?;
            std::fs::remove_file(src)?;
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
                // Skip system directories like $RECYCLE.BIN
                if cli_tools::is_system_directory_path(&path) {
                    continue;
                }
                let dir_name = entry.file_name().to_string_lossy().to_lowercase();
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
            let file_name_normalized = self.strip_ignored_prefixes(&file_name_normalized);

            for &idx in &dir_indices {
                // dir.name is already lowercase
                // Check if the normalized filename contains the directory name
                if file_name_normalized.contains(&dirs[idx].name) {
                    matches.entry(idx).or_default().push(file_path.clone());
                    // Only match to first directory found
                    break;
                }
            }
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

    /// Collect files from base path and group them by prefix.
    fn collect_files_by_prefix(&self) -> anyhow::Result<HashMap<String, Vec<PathBuf>>> {
        // First pass: collect all files with their filename (with ignored prefixes stripped)
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

        // Second pass: determine best prefix for each file
        let mut prefix_groups: HashMap<String, Vec<PathBuf>> = HashMap::new();

        for (file_path, file_name) in &files_with_names {
            if let Some(prefix) = Self::find_best_prefix(file_name, &files_with_names) {
                prefix_groups
                    .entry(prefix.into_owned())
                    .or_default()
                    .push(file_path.clone());
            }
        }

        // Apply prefix overrides: if a group's prefix starts with an override, use the override
        let prefix_groups = self.apply_prefix_overrides(prefix_groups);

        Ok(prefix_groups)
    }

    /// Create directories for files with matching prefixes and move files into them.
    /// Only considers files directly in the base path (not recursive).
    fn create_dirs_and_move_files(&self) -> anyhow::Result<()> {
        let prefix_groups = self.collect_files_by_prefix()?;

        // Filter to only groups with 3+ files
        let groups_to_process: Vec<_> = prefix_groups
            .into_iter()
            .filter(|(_, files)| files.len() >= self.config.min_group_size)
            .sorted_by(|a, b| a.0.cmp(&b.0))
            .collect();

        if groups_to_process.is_empty() {
            if self.config.verbose {
                println!("No file groups with 3 or more matching prefixes found.");
            }
            return Ok(());
        }

        print_bold!(
            "Found {} group(s) with {}+ files sharing the same prefix:\n",
            groups_to_process.len(),
            self.config.min_group_size
        );

        for (prefix, files) in groups_to_process {
            let dir_name = prefix.replace('.', " ");
            let dir_path = self.root.join(&dir_name);
            let dir_exists = dir_path.exists();

            println!("{}: {} files", dir_name.cyan().bold(), files.len());
            for file_path in &files {
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
                    if let Err(e) = self.move_files_to_target_dir(&dir_path, &files) {
                        print_error!("Failed to process {}: {e}", dir_name);
                    }
                } else {
                    println!("  Skipped");
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

    /// Find the best prefix for a file by checking if other files share the same prefix.
    /// For short simple prefixes (≤4 chars), tries longer prefixes first.
    /// Returns None if only a short prefix exists with no shared longer prefix.
    fn find_best_prefix<'a>(file_name: &'a str, all_files: &[(PathBuf, String)]) -> Option<Cow<'a, str>> {
        let simple_prefix = file_name.split('.').next().filter(|p| !p.is_empty())?;

        // Find all files that share the same simple prefix
        let files_with_same_prefix: Vec<_> = all_files
            .iter()
            .filter(|(_, name)| name.split('.').next() == Some(simple_prefix))
            .collect();

        // Try to find a longer shared prefix (up to 3 parts) that ALL files with this simple prefix share
        // First try 3-part prefix
        if let Some(three_part) = Self::get_n_part_prefix(file_name, 3) {
            let all_share_three_part = files_with_same_prefix
                .iter()
                .all(|(_, name)| Self::get_n_part_prefix(name, 3) == Some(three_part));
            if all_share_three_part && files_with_same_prefix.len() > 1 {
                return Some(Cow::Borrowed(three_part));
            }
        }

        // Then try 2-part prefix
        if let Some(two_part) = Self::get_n_part_prefix(file_name, 2) {
            let all_share_two_part = files_with_same_prefix
                .iter()
                .all(|(_, name)| Self::get_n_part_prefix(name, 2) == Some(two_part));
            if all_share_two_part && files_with_same_prefix.len() > 1 {
                return Some(Cow::Borrowed(two_part));
            }
        }

        // For long simple prefixes (> 4 chars), use the simple prefix
        if simple_prefix.len() > 4 {
            return Some(Cow::Borrowed(simple_prefix));
        }

        // No shared longer prefix found for short simple prefix, skip this file
        None
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::borrow::Cow;
    use std::collections::HashMap;
    use tempfile::TempDir;

    fn make_test_files(names: &[&str]) -> Vec<(PathBuf, String)> {
        names.iter().map(|n| (PathBuf::from(*n), (*n).to_string())).collect()
    }

    fn write_file(path: &Path, contents: &str) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, contents)?;
        Ok(())
    }

    fn assert_exists(path: &Path) {
        assert!(path.exists(), "Expected path to exist: {}", path.display());
    }

    fn assert_not_exists(path: &Path) {
        assert!(!path.exists(), "Expected path to NOT exist: {}", path.display());
    }

    #[test]
    fn test_get_n_part_prefix_three_parts() {
        assert_eq!(
            DirMove::get_n_part_prefix("Some.Name.Thing.v1.mp4", 3),
            Some("Some.Name.Thing")
        );
    }

    #[test]
    fn test_get_n_part_prefix_two_parts() {
        assert_eq!(DirMove::get_n_part_prefix("Some.Name.Thing.mp4", 2), Some("Some.Name"));
    }

    #[test]
    fn test_get_n_part_prefix_not_enough_parts() {
        // Need n+1 parts minimum (n for prefix, 1 for extension)
        assert_eq!(DirMove::get_n_part_prefix("Some.Name.mp4", 3), None);
        assert_eq!(DirMove::get_n_part_prefix("Some.mp4", 2), None);
    }

    #[test]
    fn test_get_n_part_prefix_exact_parts() {
        // 3 parts total, asking for 2-part prefix should work
        assert_eq!(DirMove::get_n_part_prefix("Some.Name.mp4", 2), Some("Some.Name"));
    }

    #[test]
    fn test_find_best_prefix_long_simple_prefix_single_file() {
        // Simple prefix > 4 chars with only one file matching should be used directly
        let files = make_test_files(&["LongName.v1.mp4", "Other.v2.mp4"]);
        assert_eq!(
            DirMove::find_best_prefix("LongName.v1.mp4", &files),
            Some(Cow::Borrowed("LongName"))
        );
    }

    #[test]
    fn test_find_best_prefix_short_prefix_no_matches() {
        // Short prefix with no shared longer prefix should return None
        let files = make_test_files(&["ABC.random.mp4", "XYZ.other.mp4"]);
        assert_eq!(DirMove::find_best_prefix("ABC.random.mp4", &files), None);
    }

    #[test]
    fn test_find_best_prefix_short_prefix_with_three_part_match() {
        // All files with same simple prefix share 3-part prefix, so use 3-part
        let files = make_test_files(&[
            "Some.Name.Thing.v1.mp4",
            "Some.Name.Thing.v2.mp4",
            "Some.Name.Thing.v3.mp4",
        ]);
        assert_eq!(
            DirMove::find_best_prefix("Some.Name.Thing.v1.mp4", &files),
            Some(Cow::Borrowed("Some.Name.Thing"))
        );
    }

    #[test]
    fn test_find_best_prefix_short_prefix_mixed_three_part_uses_two_part() {
        // Files share simple prefix "Some" but have different 3-part prefixes
        // Should fall back to shared 2-part prefix "Some.Name"
        let files = make_test_files(&[
            "Some.Name.Thing.v1.mp4",
            "Some.Name.Thing.v2.mp4",
            "Some.Name.Other.v1.mp4",
        ]);
        assert_eq!(
            DirMove::find_best_prefix("Some.Name.Thing.v1.mp4", &files),
            Some(Cow::Borrowed("Some.Name"))
        );
    }

    #[test]
    fn test_find_best_prefix_short_prefix_fallback_to_two_part() {
        // No 3-part matches, but 2-part matches exist
        let files = make_test_files(&["Some.Name.Thing.mp4", "Some.Name.Other.mp4", "Some.Name.More.mp4"]);
        assert_eq!(
            DirMove::find_best_prefix("Some.Name.Thing.mp4", &files),
            Some(Cow::Borrowed("Some.Name"))
        );
    }

    #[test]
    fn test_find_best_prefix_mixed_three_part_falls_back_to_two_part() {
        // Files share simple prefix "Some" but have different 3-part prefixes
        // Should fall back to shared 2-part prefix "Some.Name"
        let files = make_test_files(&[
            "Some.Name.Thing.v1.mp4",
            "Some.Name.Thing.v2.mp4",
            "Some.Name.Other.v1.mp4",
            "Some.Name.Other.v2.mp4",
        ]);
        // All share 2-part "Some.Name", so use that
        assert_eq!(
            DirMove::find_best_prefix("Some.Name.Thing.v1.mp4", &files),
            Some(Cow::Borrowed("Some.Name"))
        );
    }

    #[test]
    fn test_find_best_prefix_exactly_four_char_prefix() {
        // 4-char prefix is still "short", needs longer match
        let files = make_test_files(&["ABCD.Name.Thing.mp4", "ABCD.Name.Other.mp4"]);
        assert_eq!(
            DirMove::find_best_prefix("ABCD.Name.Thing.mp4", &files),
            Some(Cow::Borrowed("ABCD.Name"))
        );
    }

    #[test]
    fn test_find_best_prefix_five_char_prefix_with_shared_two_part() {
        // 5-char prefix is "long", but all files share 2-part prefix "ABCDE.Name"
        let files = make_test_files(&["ABCDE.Name.Thing.mp4", "ABCDE.Name.Other.mp4"]);
        assert_eq!(
            DirMove::find_best_prefix("ABCDE.Name.Thing.mp4", &files),
            Some(Cow::Borrowed("ABCDE.Name"))
        );
    }

    #[test]
    fn test_find_best_prefix_five_char_prefix_no_shared_longer() {
        // 5-char prefix with different 2-part prefixes falls back to simple prefix
        let files = make_test_files(&["ABCDE.Name.Thing.mp4", "ABCDE.Other.Thing.mp4"]);
        assert_eq!(
            DirMove::find_best_prefix("ABCDE.Name.Thing.mp4", &files),
            Some(Cow::Borrowed("ABCDE"))
        );
    }

    fn make_test_config_with_ignores(prefix_overrides: Vec<String>, prefix_ignores: Vec<String>) -> Config {
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

    /// Helper to create a config for unpack tests.
    fn make_unpack_config(unpack_names: Vec<&str>, recurse: bool, dryrun: bool, overwrite: bool) -> Config {
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

    #[test]
    fn test_unpack_basic_preserves_structure_and_removes_matched_dir() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Example");
        std::fs::create_dir_all(&root)?;

        // Example/Videos/Name/file2.txt
        // Example/Videos/file1.txt
        let videos = root.join("Videos");
        write_file(&videos.join("Name").join("file2.txt"), "file2")?;
        write_file(&videos.join("file1.txt"), "file1")?;

        let root_for_asserts = root.clone();

        let dirmove = DirMove {
            root,
            config: make_unpack_config(vec!["videos"], true, false, false),
        };

        dirmove.unpack_directories()?;

        // End result should be:
        // Example/Name/file2.txt
        // Example/file1.txt
        assert_exists(&root_for_asserts.join("Name").join("file2.txt"));
        assert_exists(&root_for_asserts.join("file1.txt"));

        // Videos dir should be removed when empty
        assert_not_exists(&root_for_asserts.join("Videos"));

        Ok(())
    }

    #[test]
    fn test_unpack_case_insensitive_dirname_match() -> anyhow::Result<()> {
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
    fn test_unpack_does_not_prune_unrelated_empty_dirs() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Example");
        std::fs::create_dir_all(&root)?;

        // An unrelated empty directory should not be removed just because it is empty.
        let unrelated_empty = root.join("EmptyUnrelated");
        std::fs::create_dir_all(&unrelated_empty)?;

        // A matched directory that is already empty should be removed.
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
    fn test_unpack_moves_non_matching_dirs_directly() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Example");
        std::fs::create_dir_all(&root)?;

        // Create: Example/Videos/SubDir/nested/deep.txt
        // SubDir does NOT match "videos", so it should be moved directly.
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

        // SubDir should be moved directly to Example/SubDir with its contents intact.
        assert_exists(&root_for_asserts.join("SubDir").join("nested").join("deep.txt"));
        assert_exists(&root_for_asserts.join("SubDir").join("file.txt"));

        // Videos dir should be removed.
        assert_not_exists(&root_for_asserts.join("Videos"));

        Ok(())
    }

    #[test]
    fn test_unpack_dryrun_does_not_modify_files() -> anyhow::Result<()> {
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

        // Files should NOT have been moved in dryrun mode.
        assert_exists(&root_for_asserts.join("Videos").join("file1.txt"));
        assert_exists(&root_for_asserts.join("Videos").join("SubDir").join("file2.txt"));

        // Destination files should NOT exist.
        assert_not_exists(&root_for_asserts.join("file1.txt"));
        assert_not_exists(&root_for_asserts.join("SubDir"));

        Ok(())
    }

    #[test]
    fn test_unpack_nested_matching_dirs_are_flattened() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Example");
        std::fs::create_dir_all(&root)?;

        // Create: Example/Videos/Videos/file.txt
        // Both "Videos" directories match, so contents should be moved to Example.
        let outer_videos = root.join("Videos");
        let inner_videos = outer_videos.join("Videos");
        write_file(&inner_videos.join("file.txt"), "nested content")?;

        let root_for_asserts = root.clone();

        let dirmove = DirMove {
            root,
            config: make_unpack_config(vec!["videos"], true, false, false),
        };

        dirmove.unpack_directories()?;

        // File should end up at Example/file.txt since both Videos dirs are flattened.
        assert_exists(&root_for_asserts.join("file.txt"));
        assert_not_exists(&root_for_asserts.join("Videos"));

        Ok(())
    }

    #[test]
    fn test_unpack_multiple_unpack_names() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Example");
        std::fs::create_dir_all(&root)?;

        // Create directories matching different unpack names.
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

        // Both directories should be unpacked.
        assert_exists(&root_for_asserts.join("video.mp4"));
        assert_exists(&root_for_asserts.join("extra.txt"));
        assert_not_exists(&root_for_asserts.join("Videos"));
        assert_not_exists(&root_for_asserts.join("Extras"));

        Ok(())
    }

    #[test]
    fn test_unpack_skips_existing_file_without_overwrite() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Example");
        std::fs::create_dir_all(&root)?;

        // Create a file in Videos and a conflicting file in root.
        let videos = root.join("Videos");
        write_file(&videos.join("conflict.txt"), "from videos")?;
        write_file(&root.join("conflict.txt"), "original")?;

        let root_for_asserts = root.clone();

        let dirmove = DirMove {
            root,
            config: make_unpack_config(vec!["videos"], true, false, false),
        };

        dirmove.unpack_directories()?;

        // Original file should be preserved.
        let content = std::fs::read_to_string(root_for_asserts.join("conflict.txt"))?;
        assert_eq!(content, "original");

        Ok(())
    }

    #[test]
    fn test_unpack_overwrites_existing_file_with_overwrite() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Example");
        std::fs::create_dir_all(&root)?;

        // Create a file in Videos and a conflicting file in root.
        let videos = root.join("Videos");
        write_file(&videos.join("conflict.txt"), "from videos")?;
        write_file(&root.join("conflict.txt"), "original")?;

        let root_for_asserts = root.clone();

        let dirmove = DirMove {
            root,
            config: make_unpack_config(vec!["videos"], true, false, true),
        };

        dirmove.unpack_directories()?;

        // File should be overwritten.
        let content = std::fs::read_to_string(root_for_asserts.join("conflict.txt"))?;
        assert_eq!(content, "from videos");

        Ok(())
    }

    #[test]
    fn test_unpack_skips_existing_directory_without_overwrite() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Example");
        std::fs::create_dir_all(&root)?;

        // Create a subdir in Videos and a conflicting directory in root.
        let videos = root.join("Videos");
        write_file(&videos.join("SubDir").join("new.txt"), "new content")?;
        write_file(&root.join("SubDir").join("existing.txt"), "existing content")?;

        let root_for_asserts = root.clone();

        let dirmove = DirMove {
            root,
            config: make_unpack_config(vec!["videos"], true, false, false),
        };

        dirmove.unpack_directories()?;

        // Original directory should be preserved with its content.
        assert_exists(&root_for_asserts.join("SubDir").join("existing.txt"));
        // New file should NOT have been added (directory move was skipped).
        assert_not_exists(&root_for_asserts.join("SubDir").join("new.txt"));

        Ok(())
    }

    #[test]
    fn test_unpack_non_recursive_only_checks_root_level() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Example");
        std::fs::create_dir_all(&root)?;

        // Create nested Videos directory (should NOT be unpacked with recurse=false).
        let nested = root.join("Parent").join("Videos");
        write_file(&nested.join("file.txt"), "nested")?;

        // Create root-level Videos (should be unpacked).
        let root_videos = root.join("Videos");
        write_file(&root_videos.join("root_file.txt"), "root")?;

        let root_for_asserts = root.clone();

        let dirmove = DirMove {
            root,
            config: make_unpack_config(vec!["videos"], false, false, false),
        };

        dirmove.unpack_directories()?;

        // Root-level Videos should be unpacked.
        assert_exists(&root_for_asserts.join("root_file.txt"));
        assert_not_exists(&root_for_asserts.join("Videos"));

        // Nested Videos should NOT be unpacked.
        assert_exists(&root_for_asserts.join("Parent").join("Videos").join("file.txt"));

        Ok(())
    }

    #[test]
    fn test_unpack_deeply_nested_structure() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Example");
        std::fs::create_dir_all(&root)?;

        // Create: Example/Videos/A/B/C/deep.txt
        let videos = root.join("Videos");
        write_file(&videos.join("A").join("B").join("C").join("deep.txt"), "deep")?;
        write_file(&videos.join("A").join("shallow.txt"), "shallow")?;

        let root_for_asserts = root.clone();

        let dirmove = DirMove {
            root,
            config: make_unpack_config(vec!["videos"], true, false, false),
        };

        dirmove.unpack_directories()?;

        // Directory A should be moved directly with all its nested contents.
        assert_exists(&root_for_asserts.join("A").join("B").join("C").join("deep.txt"));
        assert_exists(&root_for_asserts.join("A").join("shallow.txt"));
        assert_not_exists(&root_for_asserts.join("Videos"));

        Ok(())
    }

    #[test]
    fn test_unpack_mixed_files_and_directories() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Example");
        std::fs::create_dir_all(&root)?;

        // Create a mix of files and directories in Videos.
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

        // Files should be moved individually.
        assert_exists(&root_for_asserts.join("file1.txt"));
        assert_exists(&root_for_asserts.join("file2.txt"));
        // Directories should be moved directly.
        assert_exists(&root_for_asserts.join("Dir1").join("nested1.txt"));
        assert_exists(&root_for_asserts.join("Dir2").join("nested2.txt"));
        assert_not_exists(&root_for_asserts.join("Videos"));

        Ok(())
    }

    #[test]
    fn test_unpack_empty_unpack_names_does_nothing() -> anyhow::Result<()> {
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

        // Nothing should be unpacked.
        assert_exists(&root_for_asserts.join("Videos").join("file.txt"));
        assert_not_exists(&root_for_asserts.join("file.txt"));

        Ok(())
    }

    #[test]
    fn test_unpack_no_matching_directories() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Example");
        std::fs::create_dir_all(&root)?;

        // Create directories that don't match the unpack names.
        let other = root.join("Other");
        write_file(&other.join("file.txt"), "content")?;

        let root_for_asserts = root.clone();

        let dirmove = DirMove {
            root,
            config: make_unpack_config(vec!["videos"], true, false, false),
        };

        dirmove.unpack_directories()?;

        // Nothing should be unpacked.
        assert_exists(&root_for_asserts.join("Other").join("file.txt"));

        Ok(())
    }

    #[test]
    fn test_unpack_multiple_matching_dirs_at_different_levels() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Example");
        std::fs::create_dir_all(&root)?;

        // Create: Example/Videos/file1.txt
        // Create: Example/Parent/Videos/file2.txt
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

        // Both should be unpacked to their respective parents.
        assert_exists(&root_for_asserts.join("file1.txt"));
        assert_exists(&root_for_asserts.join("Parent").join("file2.txt"));
        assert_not_exists(&root_for_asserts.join("Videos"));
        assert_not_exists(&root_for_asserts.join("Parent").join("Videos"));

        Ok(())
    }

    #[test]
    fn test_unpack_preserves_file_content() -> anyhow::Result<()> {
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

        // File content should be preserved exactly.
        let moved_content = std::fs::read_to_string(root_for_asserts.join("content.txt"))?;
        assert_eq!(moved_content, original_content);

        Ok(())
    }

    #[test]
    fn test_unpack_handles_special_characters_in_names() -> anyhow::Result<()> {
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
    fn test_unpack_alternating_match_non_match_dirs() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Example");
        std::fs::create_dir_all(&root)?;

        // Create: Example/Videos/Other/Videos/file.txt
        // Outer Videos matches, Other doesn't, inner Videos matches.
        // Processing order (deepest first):
        // 1. Inner Videos is unpacked: Videos/Other/Videos/file.txt -> Videos/Other/file.txt
        // 2. Outer Videos is unpacked: Videos/Other (non-matching) moved directly -> Other
        let path = root.join("Videos").join("Other").join("Videos");
        write_file(&path.join("file.txt"), "deep")?;

        let root_for_asserts = root.clone();

        let dirmove = DirMove {
            root,
            config: make_unpack_config(vec!["videos"], true, false, false),
        };

        dirmove.unpack_directories()?;

        // Inner Videos was unpacked first, then Other was moved directly.
        // Final result: Example/Other/file.txt
        assert_exists(&root_for_asserts.join("Other").join("file.txt"));
        assert_not_exists(&root_for_asserts.join("Videos"));
        assert_not_exists(&root_for_asserts.join("Other").join("Videos"));

        Ok(())
    }

    #[test]
    fn test_unpack_collect_info_counts_correctly() -> anyhow::Result<()> {
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

        let parent = root;
        let info = dirmove.collect_unpack_info(&videos, &parent);

        // Should have 3 files and 2 directories.
        assert_eq!(info.file_moves.len(), 3);
        assert_eq!(info.direct_dir_moves.len(), 2);

        Ok(())
    }

    #[test]
    fn test_unpack_dryrun_preserves_empty_matched_directory() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Example");
        std::fs::create_dir_all(&root)?;

        // Create an empty matched directory.
        let videos = root.join("Videos");
        std::fs::create_dir_all(&videos)?;

        let root_for_asserts = root.clone();

        let dirmove = DirMove {
            root,
            config: make_unpack_config(vec!["videos"], true, true, false),
        };

        dirmove.unpack_directories()?;

        // In dryrun mode, even empty matched directories should NOT be removed.
        assert_exists(&root_for_asserts.join("Videos"));

        Ok(())
    }

    fn make_test_dirmove_with_ignores(prefix_overrides: Vec<String>, prefix_ignores: Vec<String>) -> DirMove {
        DirMove {
            root: PathBuf::from("."),
            config: make_test_config_with_ignores(prefix_overrides, prefix_ignores),
        }
    }

    fn make_test_dirmove(prefix_overrides: Vec<String>) -> DirMove {
        make_test_dirmove_with_ignores(prefix_overrides, Vec::new())
    }

    #[test]
    fn test_apply_prefix_overrides_no_overrides() {
        let dirmove = make_test_dirmove(Vec::new());
        let mut groups: HashMap<String, Vec<PathBuf>> = HashMap::new();
        groups.insert("Some.Name.Thing".to_string(), vec![PathBuf::from("file1.mp4")]);

        let result = dirmove.apply_prefix_overrides(groups);
        assert!(result.contains_key("Some.Name.Thing"));
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn test_apply_prefix_overrides_matching_override() {
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
    fn test_apply_prefix_overrides_merges_groups() {
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
    fn test_apply_prefix_overrides_non_matching() {
        let dirmove = make_test_dirmove(vec!["Other.Prefix".to_string()]);
        let mut groups: HashMap<String, Vec<PathBuf>> = HashMap::new();
        groups.insert("Some.Name.Thing".to_string(), vec![PathBuf::from("file1.mp4")]);

        let result = dirmove.apply_prefix_overrides(groups);
        assert!(result.contains_key("Some.Name.Thing"));
        assert!(!result.contains_key("Other.Prefix"));
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn test_apply_prefix_overrides_partial_match_only() {
        let dirmove = make_test_dirmove(vec!["Some".to_string()]);
        let mut groups: HashMap<String, Vec<PathBuf>> = HashMap::new();
        groups.insert("Something.Else".to_string(), vec![PathBuf::from("file1.mp4")]);
        groups.insert("Some.Name".to_string(), vec![PathBuf::from("file2.mp4")]);

        let result = dirmove.apply_prefix_overrides(groups);
        // "Something.Else" starts with "Some" so it gets merged
        // "Some.Name" also starts with "Some" so it gets merged
        assert!(result.contains_key("Some"));
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn test_apply_prefix_overrides_override_more_specific_than_prefix() {
        // Override "Example.Name" is more specific than computed prefix "Example"
        // Files start with "Example.Name" so override should apply
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

    fn make_test_dirs(names: &[&str]) -> Vec<DirectoryInfo> {
        names
            .iter()
            .map(|n| DirectoryInfo {
                path: PathBuf::from(*n),
                name: n.to_lowercase(),
            })
            .collect()
    }

    fn make_file_paths(names: &[&str]) -> Vec<PathBuf> {
        names.iter().map(|n| PathBuf::from(*n)).collect()
    }

    #[test]
    fn test_match_files_to_directories_basic_match() {
        // Directory: "Certain Name", files with "Certain.Name" should match
        let dirmove = make_test_dirmove(Vec::new());
        let dirs = make_test_dirs(&["Certain Name"]);
        let files = make_file_paths(&[
            "Something.else.Certain.Name.video.1.mp4",
            "Certain.Name.Example.video.2.mp4",
            "Another.Certain.Name.Example.video.3.mp4",
            "Another.Name.Example.video.3.mp4",
            "Cert.Name.Example.video.3.mp4",
            "Certain.Not.Example.video.mp4",
        ]);

        let result = dirmove.match_files_to_directories(&files, &dirs);

        assert_eq!(result.len(), 1);
        assert!(result.contains_key(&0));
        assert_eq!(result.get(&0).map(Vec::len), Some(3));
    }

    #[test]
    fn test_match_files_to_directories_no_matches() {
        let dirmove = make_test_dirmove(Vec::new());
        let dirs = make_test_dirs(&["Some Directory"]);
        let files = make_file_paths(&["unrelated.file.mp4", "another.file.txt"]);

        let result = dirmove.match_files_to_directories(&files, &dirs);

        assert!(result.is_empty());
    }

    #[test]
    fn test_match_files_to_directories_multiple_dirs() {
        let dirmove = make_test_dirmove(Vec::new());
        let dirs = make_test_dirs(&["First Dir", "Second Dir"]);
        let files = make_file_paths(&["First.Dir.file1.mp4", "Second.Dir.file2.mp4", "First.Dir.file3.mp4"]);

        let result = dirmove.match_files_to_directories(&files, &dirs);

        assert_eq!(result.len(), 2);
        assert_eq!(result.get(&0).map(Vec::len), Some(2));
        assert_eq!(result.get(&1).map(Vec::len), Some(1));
    }

    #[test]
    fn test_match_files_to_directories_case_insensitive() {
        let dirmove = make_test_dirmove(Vec::new());
        let dirs = make_test_dirs(&["My Directory"]);
        let files = make_file_paths(&[
            "MY.DIRECTORY.file1.mp4",
            "my.directory.file2.mp4",
            "My.Directory.file3.mp4",
        ]);

        let result = dirmove.match_files_to_directories(&files, &dirs);

        assert_eq!(result.len(), 1);
        assert_eq!(result.get(&0).map(Vec::len), Some(3));
    }

    #[test]
    fn test_match_files_to_directories_partial_match() {
        // Directory "Test Name" should NOT match "Testing.Name.file.mp4"
        // because "testing name file mp4" does not contain "test name" as substring
        // ("testing" != "test ")
        let dirmove = make_test_dirmove(Vec::new());
        let dirs = make_test_dirs(&["Test Name"]);
        let files = make_file_paths(&["Testing.Name.file.mp4", "Test.Name.file.mp4"]);

        let result = dirmove.match_files_to_directories(&files, &dirs);

        assert_eq!(result.len(), 1);
        // Only "Test.Name.file.mp4" matches because normalized is "test name file mp4"
        // which contains "test name"
        // "Testing.Name.file.mp4" normalized is "testing name file mp4" which does NOT
        // contain "test name" (it has "testing name" not "test name")
        assert_eq!(result.get(&0).map(Vec::len), Some(1));
    }

    #[test]
    fn test_match_files_to_directories_longer_match_wins() {
        // If a file could match multiple directories, longer/more specific name wins
        // e.g., "ProjectNew" should match before "Project"
        let dirmove = make_test_dirmove(Vec::new());
        let dirs = make_test_dirs(&["Project", "ProjectNew"]);
        let files = make_file_paths(&["ProjectNew.2025.10.12.file.mp4", "Project.2025.10.05.file.mp4"]);

        let result = dirmove.match_files_to_directories(&files, &dirs);

        // Should have matches for both directories
        assert_eq!(result.len(), 2);
        // "ProjectNew" file should match "ProjectNew" directory (index 1), not "Project"
        assert!(result.contains_key(&1));
        assert_eq!(result.get(&1).map(Vec::len), Some(1));
        // "Project" file should match "Project" directory (index 0)
        assert!(result.contains_key(&0));
        assert_eq!(result.get(&0).map(Vec::len), Some(1));
    }

    #[test]
    fn test_match_files_to_directories_empty_files() {
        let dirmove = make_test_dirmove(Vec::new());
        let dirs = make_test_dirs(&["Some Dir"]);
        let files: Vec<PathBuf> = Vec::new();

        let result = dirmove.match_files_to_directories(&files, &dirs);

        assert!(result.is_empty());
    }

    #[test]
    fn test_match_files_to_directories_empty_dirs() {
        let dirmove = make_test_dirmove(Vec::new());
        let dirs: Vec<DirectoryInfo> = Vec::new();
        let files = make_file_paths(&["some.file.mp4"]);

        let result = dirmove.match_files_to_directories(&files, &dirs);

        assert!(result.is_empty());
    }

    #[test]
    fn test_match_files_to_directories_dots_replaced_with_spaces() {
        // Verify that dots in filenames are replaced with spaces for matching
        // Directory "My Show" should match "My.Show.S01E01.mp4"
        let dirmove = make_test_dirmove(Vec::new());
        let dirs = make_test_dirs(&["My Show"]);
        let files = make_file_paths(&["My.Show.S01E01.mp4", "My.Show.S01E02.mp4"]);

        let result = dirmove.match_files_to_directories(&files, &dirs);

        assert_eq!(result.len(), 1);
        assert_eq!(result.get(&0).map(Vec::len), Some(2));
    }

    #[test]
    fn test_match_files_to_directories_mixed_separators() {
        // Files with various separators in names
        let dirmove = make_test_dirmove(Vec::new());
        let dirs = make_test_dirs(&["Show Name"]);
        let files = make_file_paths(&[
            "Show.Name.episode.mp4",
            "Other.Show.Name.here.mp4",
            "prefix.Show.Name.suffix.mp4",
        ]);

        let result = dirmove.match_files_to_directories(&files, &dirs);

        assert_eq!(result.len(), 1);
        assert_eq!(result.get(&0).map(Vec::len), Some(3));
    }

    #[test]
    fn test_strip_ignored_prefixes_no_ignores() {
        let dirmove = make_test_dirmove(Vec::new());
        let result = dirmove.strip_ignored_prefixes("something other matching");
        assert_eq!(result.as_ref(), "something other matching");
    }

    #[test]
    fn test_strip_ignored_prefixes_single_ignore() {
        let dirmove = make_test_dirmove_with_ignores(Vec::new(), vec!["Something".to_string()]);
        let result = dirmove.strip_ignored_prefixes("something other matching");
        assert_eq!(result.as_ref(), "other matching");
    }

    #[test]
    fn test_strip_ignored_prefixes_multiple_ignores() {
        let dirmove = make_test_dirmove_with_ignores(Vec::new(), vec!["Something".to_string(), "Other".to_string()]);
        let result = dirmove.strip_ignored_prefixes("something other matching");
        assert_eq!(result.as_ref(), "matching");
    }

    #[test]
    fn test_strip_ignored_prefixes_no_match() {
        let dirmove = make_test_dirmove_with_ignores(Vec::new(), vec!["Prefix".to_string()]);
        let result = dirmove.strip_ignored_prefixes("something other matching");
        assert_eq!(result.as_ref(), "something other matching");
    }

    #[test]
    fn test_strip_ignored_dot_prefixes_single_ignore() {
        let dirmove = make_test_dirmove_with_ignores(Vec::new(), vec!["Something".to_string()]);
        let result = dirmove.strip_ignored_dot_prefixes("Something.other.matching.mp4");
        assert_eq!(result, "other.matching.mp4");
    }

    #[test]
    fn test_strip_ignored_dot_prefixes_multiple_ignores() {
        let dirmove = make_test_dirmove_with_ignores(Vec::new(), vec!["Something".to_string(), "other".to_string()]);
        let result = dirmove.strip_ignored_dot_prefixes("Something.other.matching.mp4");
        assert_eq!(result, "matching.mp4");
    }

    #[test]
    fn test_match_files_to_directories_with_prefix_ignore() {
        // File "Something.other.matching.mp4" should match "matching" dir when "Something" is ignored
        let dirmove = make_test_dirmove_with_ignores(Vec::new(), vec!["Something".to_string()]);
        let dirs = make_test_dirs(&["matching", "Something"]);
        let files = make_file_paths(&["Something.other.matching.mp4"]);

        let result = dirmove.match_files_to_directories(&files, &dirs);

        // Should match "matching" (index 0), not "Something" (index 1)
        assert_eq!(result.len(), 1);
        assert!(result.contains_key(&0));
        assert!(!result.contains_key(&1));
    }

    #[test]
    fn test_match_files_to_directories_with_repeated_prefix_ignore() {
        // File has the ignored prefix multiple times in the name, should only strip from start
        let dirmove = make_test_dirmove_with_ignores(Vec::new(), vec!["Something".to_string()]);
        let dirs = make_test_dirs(&["other Something", "Something"]);
        let files = make_file_paths(&["Something.other.Something.matching.mp4"]);

        let result = dirmove.match_files_to_directories(&files, &dirs);

        // Should match "other Something" (index 0) after stripping leading "Something"
        // The second "Something" in the middle of the name should remain for matching
        assert_eq!(result.len(), 1);
        assert!(result.contains_key(&0));
        assert!(!result.contains_key(&1));
    }

    #[test]
    fn test_match_files_to_directories_with_multiple_prefix_ignores() {
        // File should match after stripping multiple ignored prefixes
        let dirmove = make_test_dirmove_with_ignores(Vec::new(), vec!["Prefix1".to_string(), "Prefix2".to_string()]);
        let dirs = make_test_dirs(&["Target Dir"]);
        let files = make_file_paths(&["Prefix1.Prefix2.Target.Dir.file.mp4"]);

        let result = dirmove.match_files_to_directories(&files, &dirs);

        assert_eq!(result.len(), 1);
        assert!(result.contains_key(&0));
    }

    #[test]
    fn test_filter_parts_removes_year() {
        assert_eq!(
            DirMove::filter_numeric_resolution_and_glue_parts("Show.2024.S01E01.mkv"),
            "Show.S01E01.mkv"
        );
    }

    #[test]
    fn test_filter_parts_removes_multiple_numeric() {
        assert_eq!(
            DirMove::filter_numeric_resolution_and_glue_parts("Show.2024.720.thing.mp4"),
            "Show.thing.mp4"
        );
    }

    #[test]
    fn test_filter_parts_keeps_mixed_alphanumeric() {
        assert_eq!(
            DirMove::filter_numeric_resolution_and_glue_parts("Show.S01E02.2024.mp4"),
            "Show.S01E02.mp4"
        );
    }

    #[test]
    fn test_filter_parts_no_numeric() {
        assert_eq!(
            DirMove::filter_numeric_resolution_and_glue_parts("Show.Episode.Title.mp4"),
            "Show.Episode.Title.mp4"
        );
    }

    #[test]
    fn test_filter_parts_all_numeric_except_extension() {
        assert_eq!(DirMove::filter_numeric_resolution_and_glue_parts("2024.720.mp4"), "mp4");
    }

    #[test]
    fn test_filter_parts_empty_string() {
        assert_eq!(DirMove::filter_numeric_resolution_and_glue_parts(""), "");
    }

    #[test]
    fn test_filter_parts_single_part() {
        assert_eq!(
            DirMove::filter_numeric_resolution_and_glue_parts("Show.mp4"),
            "Show.mp4"
        );
    }

    #[test]
    fn test_filter_parts_removes_1080p() {
        assert_eq!(
            DirMove::filter_numeric_resolution_and_glue_parts("Show.1080p.S01E01.mkv"),
            "Show.S01E01.mkv"
        );
    }

    #[test]
    fn test_filter_parts_removes_2160p() {
        assert_eq!(
            DirMove::filter_numeric_resolution_and_glue_parts("Some.Video.2160p.part1.mp4"),
            "Some.Video.part1.mp4"
        );
    }

    #[test]
    fn test_filter_parts_removes_720p() {
        assert_eq!(
            DirMove::filter_numeric_resolution_and_glue_parts("Movie.720p.rip.mp4"),
            "Movie.rip.mp4"
        );
    }

    #[test]
    fn test_filter_parts_removes_dimension_format() {
        assert_eq!(
            DirMove::filter_numeric_resolution_and_glue_parts("Video.1920x1080.stuff.mp4"),
            "Video.stuff.mp4"
        );
    }

    #[test]
    fn test_filter_parts_removes_smaller_dimension() {
        assert_eq!(
            DirMove::filter_numeric_resolution_and_glue_parts("Video.640x480.stuff.mp4"),
            "Video.stuff.mp4"
        );
    }

    #[test]
    fn test_filter_parts_case_insensitive_resolution() {
        assert_eq!(
            DirMove::filter_numeric_resolution_and_glue_parts("Show.1080P.episode.mkv"),
            "Show.episode.mkv"
        );
    }

    #[test]
    fn test_filter_parts_removes_and() {
        assert_eq!(
            DirMove::filter_numeric_resolution_and_glue_parts("Show.and.Tell.mp4"),
            "Show.Tell.mp4"
        );
    }

    #[test]
    fn test_filter_parts_removes_the() {
        assert_eq!(
            DirMove::filter_numeric_resolution_and_glue_parts("The.Big.Show.mp4"),
            "Big.Show.mp4"
        );
    }

    #[test]
    fn test_filter_parts_removes_multiple_glue_words() {
        assert_eq!(
            DirMove::filter_numeric_resolution_and_glue_parts("Show.of.the.Year.mp4"),
            "Show.Year.mp4"
        );
    }

    #[test]
    fn test_filter_parts_glue_words_case_insensitive() {
        assert_eq!(
            DirMove::filter_numeric_resolution_and_glue_parts("Show.AND.Tell.mp4"),
            "Show.Tell.mp4"
        );
    }

    #[test]
    fn test_filter_parts_removes_all_glue_words() {
        assert_eq!(
            DirMove::filter_numeric_resolution_and_glue_parts("A.Day.in.the.Life.of.Bob.mp4"),
            "Day.Life.Bob.mp4"
        );
    }

    #[test]
    fn test_find_best_prefix_with_filtered_numeric_parts() {
        // Simulate the full flow: files are filtered before being passed to find_best_prefix
        // Original filenames: ShowName.2024.S01E01.mp4, ShowName.2024.S01E02.mp4, ShowName.2024.S01E03.mp4
        // After filtering: ShowName.S01E01.mp4, ShowName.S01E02.mp4, ShowName.S01E03.mp4
        let filtered_files = make_test_files(&["ShowName.S01E01.mp4", "ShowName.S01E02.mp4", "ShowName.S01E03.mp4"]);

        // All files should group under "ShowName" (8 chars > 4, so uses simple prefix)
        assert_eq!(
            DirMove::find_best_prefix("ShowName.S01E01.mp4", &filtered_files),
            Some(Cow::Borrowed("ShowName"))
        );
    }

    #[test]
    fn test_find_best_prefix_numeric_filtering_groups_correctly() {
        // Files with years should group together after numeric filtering
        // Original: ABC.2023.Thing.v1.mp4, ABC.2024.Thing.v2.mp4, ABC.2025.Thing.v3.mp4
        // After filtering: ABC.Thing.v1.mp4, ABC.Thing.v2.mp4, ABC.Thing.v3.mp4
        let filtered_files = make_test_files(&["ABC.Thing.v1.mp4", "ABC.Thing.v2.mp4", "ABC.Thing.v3.mp4"]);

        // Short prefix "ABC" (3 chars) should find shared 2-part prefix "ABC.Thing"
        assert_eq!(
            DirMove::find_best_prefix("ABC.Thing.v1.mp4", &filtered_files),
            Some(Cow::Borrowed("ABC.Thing"))
        );
    }

    #[test]
    fn test_find_best_prefix_mixed_years_without_filtering_no_group() {
        // Without filtering, files with different years wouldn't group on short prefix
        // These files have short prefix "ABC" but different 2-part prefixes
        let unfiltered_files = make_test_files(&["ABC.2023.Thing.mp4", "ABC.2024.Other.mp4", "ABC.2025.More.mp4"]);

        // No shared 3-part or 2-part prefix, so returns None for short prefix
        assert_eq!(DirMove::find_best_prefix("ABC.2023.Thing.mp4", &unfiltered_files), None);
    }

    #[test]
    fn test_find_best_prefix_after_filtering_groups_by_show_name() {
        // Real-world scenario: TV show episodes with year in name
        // Original: Series.Name.2024.S01E01.1080p.mp4, Series.Name.2024.S01E02.1080p.mp4, etc.
        // After filtering: Series.Name.S01E01.1080p.mp4, Series.Name.S01E02.1080p.mp4, etc.
        let filtered_files = make_test_files(&[
            "Series.Name.S01E01.1080p.mp4",
            "Series.Name.S01E02.1080p.mp4",
            "Series.Name.S01E03.1080p.mp4",
        ]);

        // All files share "Series.Name" 2-part prefix, so use that instead of just "Series"
        assert_eq!(
            DirMove::find_best_prefix("Series.Name.S01E01.1080p.mp4", &filtered_files),
            Some(Cow::Borrowed("Series.Name"))
        );
    }

    #[test]
    fn test_full_filtering_flow_simulation() {
        // Simulate the full flow from collect_files_by_prefix
        let original_filenames = [
            "ShowName.2023.S01E01.720p.mp4",
            "ShowName.2024.S01E02.720p.mp4",
            "ShowName.2025.S01E03.720p.mp4",
            "OtherShow.2024.Special.mp4",
        ];

        // Step 1: filter_numeric_resolution_and_glue_parts (simulating what collect_files_by_prefix does)
        let filtered: Vec<(PathBuf, String)> = original_filenames
            .iter()
            .map(|name| {
                let filtered_name = DirMove::filter_numeric_resolution_and_glue_parts(name);
                (PathBuf::from(*name), filtered_name)
            })
            .collect();

        // Verify filtering worked - both years and resolutions are removed
        assert_eq!(filtered[0].1, "ShowName.S01E01.mp4");
        assert_eq!(filtered[1].1, "ShowName.S01E02.mp4");
        assert_eq!(filtered[2].1, "ShowName.S01E03.mp4");
        assert_eq!(filtered[3].1, "OtherShow.Special.mp4");

        // Now find_best_prefix should group the "ShowName" files together
        let show_prefix = DirMove::find_best_prefix(&filtered[0].1, &filtered);
        assert_eq!(show_prefix, Some(Cow::Borrowed("ShowName")));

        // The "OtherShow" file uses simple prefix (9 chars > 4) but has no other matches
        // Since it's a long prefix, it returns the prefix even without matches
        let other_prefix = DirMove::find_best_prefix(&filtered[3].1, &filtered);
        assert_eq!(other_prefix, Some(Cow::Borrowed("OtherShow")));
    }

    #[test]
    fn test_full_filtering_flow_with_resolution_numbers() {
        // Files with resolution numbers that are purely numeric
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

        // All numeric parts (2024, 720) should be removed
        assert_eq!(filtered[0].1, "MovieName.rip.mp4");
        assert_eq!(filtered[1].1, "MovieName.other.mp4");
        assert_eq!(filtered[2].1, "MovieName.more.mp4");

        // Should group under "MovieName" (9 chars > 4, uses simple prefix)
        let prefix = DirMove::find_best_prefix(&filtered[0].1, &filtered);
        assert_eq!(prefix, Some(Cow::Borrowed("MovieName")));
    }

    #[test]
    fn test_full_filtering_flow_with_resolution_pattern() {
        // Files with resolution patterns like 1080p, 2160p
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

        // Both year (2024) and resolution (1080p) should be removed
        assert_eq!(filtered[0].1, "MovieName.rip.mp4");
        assert_eq!(filtered[1].1, "MovieName.other.mp4");
        assert_eq!(filtered[2].1, "MovieName.more.mp4");

        // Should group under "MovieName"
        let prefix = DirMove::find_best_prefix(&filtered[0].1, &filtered);
        assert_eq!(prefix, Some(Cow::Borrowed("MovieName")));
    }

    #[test]
    fn test_full_filtering_flow_with_glue_words() {
        // Files with glue words that should be filtered
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

        // Glue word "and" should be removed
        assert_eq!(filtered[0].1, "Show.Tell.part1.mp4");
        assert_eq!(filtered[1].1, "Show.Tell.part2.mp4");
        assert_eq!(filtered[2].1, "Show.Tell.part3.mp4");

        // Should group under "Show.Tell"
        let prefix = DirMove::find_best_prefix(&filtered[0].1, &filtered);
        assert_eq!(prefix, Some(Cow::Borrowed("Show.Tell")));
    }

    #[test]
    fn test_full_filtering_flow_short_prefix_with_shared_parts() {
        // Test with short prefix (≤4 chars) that requires shared multi-part prefix
        // Without filtering, these files have different 2-part prefixes due to years
        let original_filenames = [
            "ABC.2023.Thing.v1.mp4",
            "ABC.2024.Thing.v2.mp4",
            "ABC.2025.Thing.v3.mp4",
        ];

        // Without filtering - different years mean no shared 2-part prefix
        let unfiltered: Vec<(PathBuf, String)> = original_filenames
            .iter()
            .map(|name| (PathBuf::from(*name), (*name).to_string()))
            .collect();

        // No match because 2-part prefixes are ABC.2023, ABC.2024, ABC.2025 (all different)
        let prefix_unfiltered = DirMove::find_best_prefix(&unfiltered[0].1, &unfiltered);
        assert_eq!(prefix_unfiltered, None);

        // With filtering - years removed, shared 2-part prefix "ABC.Thing"
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

        // Now they share 2-part prefix "ABC.Thing"
        let prefix_filtered = DirMove::find_best_prefix(&filtered[0].1, &filtered);
        assert_eq!(prefix_filtered, Some(Cow::Borrowed("ABC.Thing")));
    }

    #[test]
    fn test_full_flow_files_with_resolution_grouped_correctly() {
        // Files with resolution - resolution is filtered during prefix finding
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

        // Resolution is filtered out before prefix finding
        assert_eq!(filtered[0].1, "Some.Video.part1.mp4");

        // Find prefix - resolution already stripped
        let prefix = DirMove::find_best_prefix(&filtered[0].1, &filtered);
        assert_eq!(prefix, Some(Cow::Borrowed("Some.Video")));

        // Directory name is just prefix with dots replaced by spaces
        let dir_name = prefix.unwrap().replace('.', " ");
        assert_eq!(dir_name, "Some Video");
    }

    #[test]
    fn test_full_flow_files_with_2160p_resolution() {
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

        // Resolution is filtered out
        assert_eq!(filtered[0].1, "Movie.Name.file1.mp4");

        // Prefix without resolution
        let prefix = DirMove::find_best_prefix(&filtered[0].1, &filtered);
        assert_eq!(prefix, Some(Cow::Borrowed("Movie.Name")));

        // Directory name
        let dir_name = prefix.unwrap().replace('.', " ");
        assert_eq!(dir_name, "Movie Name");
    }

    #[test]
    fn test_full_flow_files_with_dimension_resolution() {
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

        // Resolution is filtered out
        assert_eq!(filtered[0].1, "Cool.Stuff.part1.mp4");

        // Prefix without resolution
        let prefix = DirMove::find_best_prefix(&filtered[0].1, &filtered);
        assert_eq!(prefix, Some(Cow::Borrowed("Cool.Stuff")));

        // Directory name
        let dir_name = prefix.unwrap().replace('.', " ");
        assert_eq!(dir_name, "Cool Stuff");
    }

    #[test]
    fn test_unpack_nested_chain_single_summary() -> anyhow::Result<()> {
        // Test that deeply nested unpack directories (Project\updates\1\videos) produce
        // a single consolidated move chain, not separate summaries for each level.
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Project");
        std::fs::create_dir_all(&root)?;

        // Create: MYM/updates/1/videos/file.txt
        // All of "updates", "1", and "videos" are unpack names.
        let deep_path = root.join("updates").join("1").join("videos");
        write_file(&deep_path.join("file.txt"), "content")?;
        write_file(&deep_path.join("another.txt"), "more content")?;

        let root_for_asserts = root.clone();

        let dirmove = DirMove {
            root,
            config: make_unpack_config(vec!["updates", "1", "videos"], true, false, false),
        };

        dirmove.unpack_directories()?;

        // Files should end up directly in Project (all unpack dirs flattened).
        assert_exists(&root_for_asserts.join("file.txt"));
        assert_exists(&root_for_asserts.join("another.txt"));
        assert_not_exists(&root_for_asserts.join("updates"));

        Ok(())
    }

    #[test]
    fn test_unpack_nested_chain_with_non_matching_dir_between() -> anyhow::Result<()> {
        // Test: Project/updates/KeepThis/videos/file.txt
        // "updates" and "videos" match, but "KeepThis" doesn't.
        // KeepThis should be preserved in the output structure.
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

        // KeepThis should be preserved, file unpacked from videos into it.
        assert_exists(&root_for_asserts.join("KeepThis").join("file.txt"));
        assert_not_exists(&root_for_asserts.join("updates"));
        assert_not_exists(&root_for_asserts.join("KeepThis").join("videos"));

        Ok(())
    }

    #[test]
    fn test_unpack_nested_chain_multiple_non_matching_dirs() -> anyhow::Result<()> {
        // Test: Root/updates/A/videos/B/downloads/file.txt
        // "updates", "videos", "downloads" match; "A" and "B" don't.
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

        // A and B should be preserved in the hierarchy.
        assert_exists(&root_for_asserts.join("A").join("B").join("file.txt"));
        assert_not_exists(&root_for_asserts.join("updates"));

        Ok(())
    }

    #[test]
    fn test_unpack_collect_candidates_filters_nested() -> anyhow::Result<()> {
        // Verify that collect_unpack_candidates only returns root unpack dirs.
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Test");
        std::fs::create_dir_all(&root)?;

        // Create nested structure where all dirs match.
        let path = root.join("videos").join("updates").join("downloads");
        std::fs::create_dir_all(&path)?;
        write_file(&path.join("file.txt"), "x")?;

        let dirmove = DirMove {
            root: root.clone(),
            config: make_unpack_config(vec!["videos", "updates", "downloads"], true, false, false),
        };

        let candidates = dirmove.collect_unpack_candidates();

        // Should only return the topmost "videos" directory, not the nested ones.
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0], root.join("videos"));

        Ok(())
    }

    #[test]
    fn test_unpack_separate_trees_get_separate_processing() -> anyhow::Result<()> {
        // Two separate unpack directory trees should each be processed.
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Root");
        std::fs::create_dir_all(&root)?;

        // Tree 1: Root/DirA/videos/file1.txt
        write_file(&root.join("DirA").join("videos").join("file1.txt"), "1")?;

        // Tree 2: Root/DirB/videos/file2.txt
        write_file(&root.join("DirB").join("videos").join("file2.txt"), "2")?;

        let root_for_asserts = root.clone();

        let dirmove = DirMove {
            root,
            config: make_unpack_config(vec!["videos"], true, false, false),
        };

        dirmove.unpack_directories()?;

        // Each tree should be unpacked independently.
        assert_exists(&root_for_asserts.join("DirA").join("file1.txt"));
        assert_exists(&root_for_asserts.join("DirB").join("file2.txt"));
        assert_not_exists(&root_for_asserts.join("DirA").join("videos"));
        assert_not_exists(&root_for_asserts.join("DirB").join("videos"));

        Ok(())
    }

    #[test]
    fn test_unpack_chain_with_files_at_multiple_levels() -> anyhow::Result<()> {
        // Files at different levels of the unpack chain should all end up in the target.
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Root");
        std::fs::create_dir_all(&root)?;

        // Create: Root/updates/file1.txt
        //         Root/updates/videos/file2.txt
        //         Root/updates/videos/downloads/file3.txt
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

        // All files should end up directly in Root.
        assert_exists(&root_for_asserts.join("file1.txt"));
        assert_exists(&root_for_asserts.join("file2.txt"));
        assert_exists(&root_for_asserts.join("file3.txt"));
        assert_not_exists(&root_for_asserts.join("updates"));

        Ok(())
    }

    #[test]
    fn test_unpack_chain_with_non_matching_subdirs_moved_directly() -> anyhow::Result<()> {
        // Non-matching directories without nested unpack dirs should be moved directly.
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Root");
        std::fs::create_dir_all(&root)?;

        // Create: Root/updates/KeepMe/deep/file.txt (no unpack dirs inside KeepMe)
        let keep_me = root.join("updates").join("KeepMe");
        write_file(&keep_me.join("deep").join("file.txt"), "content")?;

        let root_for_asserts = root.clone();

        let dirmove = DirMove {
            root,
            config: make_unpack_config(vec!["updates"], true, false, false),
        };

        dirmove.unpack_directories()?;

        // KeepMe should be moved directly with its structure intact.
        assert_exists(&root_for_asserts.join("KeepMe").join("deep").join("file.txt"));
        assert_not_exists(&root_for_asserts.join("updates"));

        Ok(())
    }

    #[test]
    fn test_contains_unpack_directory_helper() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Root");
        std::fs::create_dir_all(&root)?;

        // Create: Root/A/B/videos/file.txt
        let videos = root.join("A").join("B").join("videos");
        write_file(&videos.join("file.txt"), "x")?;

        // Create: Root/C/nothing_special/file.txt
        write_file(&root.join("C").join("nothing_special").join("file.txt"), "y")?;

        let dirmove = DirMove {
            root: root.clone(),
            config: make_unpack_config(vec!["videos"], true, false, false),
        };

        // A contains videos (nested), should return true.
        assert!(dirmove.contains_unpack_directory(&root.join("A")));

        // B contains videos directly, should return true.
        assert!(dirmove.contains_unpack_directory(&root.join("A").join("B")));

        // C does not contain any unpack directory, should return false.
        assert!(!dirmove.contains_unpack_directory(&root.join("C")));

        Ok(())
    }
}

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

use colored::Colorize;
#[cfg(not(test))]
use indicatif::ProgressStyle;
use indicatif::{ParallelProgressIterator, ProgressBar};
use itertools::Itertools;
use rayon::iter::{IntoParallelRefIterator, ParallelIterator};
use regex::Regex;
use walkdir::WalkDir;

use cli_tools::{print_error, print_warning};

use crate::Args;
use crate::config::{Config, DupeConfig};

/// Regex to match resolution patterns like 720p, 1080p, or 1234x5678
static RE_RESOLUTION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(\d{3,4}p|\d{3,4}x\d{3,4})\b").expect("Invalid resolution regex"));

/// Regex to match codec patterns
static RE_CODEC: LazyLock<Regex> = LazyLock::new(|| {
    let pattern = format!(r"(?i)\b({})\b", CODEC_PATTERNS.join("|"));
    Regex::new(&pattern).expect("Invalid codec regex")
});

/// Regex to match two or more consecutive dots
static RE_MULTI_DOTS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\.{2,}").expect("Invalid dots regex"));

/// Regex to match two or more consecutive whitespace characters
static RE_MULTI_SPACES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s{2,}").expect("Invalid spaces regex"));

/// Common codec patterns to remove when normalizing
const CODEC_PATTERNS: &[&str] = &["x264", "x265", "h264", "h265"];
#[cfg(not(test))]
const PROGRESS_BAR_CHARS: &str = "=>-";
#[cfg(not(test))]
const PROGRESS_BAR_TEMPLATE: &str = "[{elapsed_precise}] {bar:80.magenta/blue} {pos}/{len} {percent}%";
/// All video extensions
pub const FILE_EXTENSIONS: &[&str] = &["mp4", "mkv", "wmv", "flv", "m4v", "ts", "mpg", "avi", "mov", "webm"];

/// Information about a found file
#[derive(Debug, Clone)]
pub struct FileInfo {
    pub(crate) path: PathBuf,
    pub(crate) filename: String,
    pub(crate) stem: String,
    pub(crate) extension: String,
    /// Pattern match range (start, end) if matched by a pattern
    pattern_match: Option<(usize, usize)>,
}

pub struct DupeFind {
    config: Config,
    roots: Vec<PathBuf>,
}

impl FileInfo {
    fn new(path: PathBuf, extension: String) -> Self {
        let filename = cli_tools::path_to_filename_string(&path);
        let stem = cli_tools::path_to_file_stem_string(&path);
        Self {
            path,
            filename,
            stem,
            extension,
            pattern_match: None,
        }
    }
}

impl DupeFind {
    pub fn new(args: Args) -> anyhow::Result<Self> {
        let user_config = DupeConfig::get_user_config();

        // Resolve all input paths:
        // - If default flag is set, use default_paths from config
        // - CLI args take priority, then config file, then current directory
        let roots = if args.default && !user_config.default_paths.is_empty() {
            user_config
                .default_paths
                .iter()
                .map(|p| cli_tools::resolve_required_input_path(p))
                .collect::<anyhow::Result<Vec<_>>>()?
        } else if !args.paths.is_empty() {
            args.paths
                .iter()
                .map(|p| cli_tools::resolve_required_input_path(p))
                .collect::<anyhow::Result<Vec<_>>>()?
        } else if !user_config.paths.is_empty() {
            user_config
                .paths
                .iter()
                .map(|p| cli_tools::resolve_required_input_path(p))
                .collect::<anyhow::Result<Vec<_>>>()?
        } else {
            vec![cli_tools::resolve_input_path(None)?]
        };

        let config = Config::from_args(args)?;

        Ok(Self { config, roots })
    }

    pub fn run(&self) -> anyhow::Result<()> {
        if self.config.verbose {
            let paths_display = self
                .roots
                .iter()
                .map(|path| cli_tools::path_to_string(path))
                .collect::<Vec<_>>()
                .join(", ");
            println!("Scanning paths: {}", paths_display.magenta());
            println!("Extensions: {:?}", self.config.extensions);
            if !self.config.patterns.is_empty() {
                println!(
                    "Patterns: {:?}",
                    self.config.patterns.iter().map(Regex::as_str).collect::<Vec<_>>()
                );
            }
        }

        let files = self.gather_files();
        let duplicates = self.find_all_duplicates(&files);

        if duplicates.is_empty() {
            println!("{}", "No duplicates found".green());
            return Ok(());
        }

        // Interactive mode when not in print/dryrun mode
        if !self.config.dryrun {
            return crate::tui::run_interactive(&duplicates);
        }

        // Print-only mode
        println!(
            "{}",
            format!("Found {} duplicate groups:", duplicates.len()).yellow().bold()
        );

        for (key, files) in &duplicates {
            println!("\n{}:", key.cyan());
            for file in files.iter().sorted_by_key(|f| &f.path) {
                let display_name = Self::format_filename_with_highlight(&file.filename, file.pattern_match);
                println!("  {display_name}");
            }
        }

        if self.config.move_files {
            self.move_duplicates(&duplicates)?;
        }

        Ok(())
    }

    /// Collect all video files from all root directories in parallel
    fn gather_files(&self) -> Vec<FileInfo> {
        let files: Mutex<Vec<FileInfo>> = Mutex::new(Vec::new());

        // Process each root directory in parallel
        self.roots.par_iter().for_each(|root| {
            let collected_files = self.collect_video_files_from_root(root);
            if let Ok(mut all_files) = files.lock() {
                all_files.extend(collected_files);
            }
        });

        files.into_inner().unwrap_or_default()
    }

    /// Collect video files from a single root directory
    fn collect_video_files_from_root(&self, root: &Path) -> Vec<FileInfo> {
        let walker = if self.config.recurse {
            WalkDir::new(root)
        } else {
            WalkDir::new(root).max_depth(1)
        };

        walker
            .into_iter()
            .filter_entry(|e| !cli_tools::should_skip_entry(e))
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_file())
            .filter_map(|entry| {
                let path = entry.path();
                let extension = cli_tools::path_to_file_extension_string(path);
                if self.config.extensions.contains(&extension) {
                    Some(FileInfo::new(path.to_path_buf(), extension))
                } else {
                    None
                }
            })
            .collect()
    }

    /// Find all duplicates in a single pass using multiple detection methods.
    /// Files are grouped together if they match any of the criteria:
    /// - Same filename in different directories
    /// - Match the same identifier pattern
    /// - Same normalized name (different resolution / codec / extension)
    fn find_all_duplicates(&self, files: &[FileInfo]) -> Vec<(String, Vec<FileInfo>)> {
        if self.config.verbose {
            println!("Checking {} files for duplicates...", files.len());
        }

        // Compute normalized filenames in parallel with progress bar
        #[cfg(test)]
        let progress_bar = ProgressBar::hidden();
        #[cfg(not(test))]
        let progress_bar = {
            let pb = ProgressBar::new(files.len() as u64);
            pb.set_style(
                ProgressStyle::default_bar()
                    .template(PROGRESS_BAR_TEMPLATE)
                    .expect("Failed to set progress bar template")
                    .progress_chars(PROGRESS_BAR_CHARS),
            );
            pb
        };
        let normalized_keys: Vec<String> = files
            .par_iter()
            .progress_with(progress_bar)
            .map(|file| Self::normalize_stem(&file.stem))
            .collect();

        // Use a union-find approach:
        // Map each file to a canonical group key.
        // Merge groups when files match multiple criteria
        let mut file_to_group: HashMap<usize, String> = HashMap::new();
        let mut groups: HashMap<String, Vec<usize>> = HashMap::new();

        // First pass: assign initial groups based on normalized filename
        for (idx, key) in normalized_keys.into_iter().enumerate() {
            file_to_group.insert(idx, key.clone());
            groups.entry(key).or_default().push(idx);
        }

        // Merge groups based on exact filename matches
        let mut filename_to_indices: HashMap<String, Vec<usize>> = HashMap::new();
        for (idx, file) in files.iter().enumerate() {
            filename_to_indices
                .entry(file.filename.to_lowercase())
                .or_default()
                .push(idx);
        }

        for indices in filename_to_indices.values() {
            if indices.len() > 1 {
                Self::merge_indices_into_groups(indices, &mut file_to_group, &mut groups);
            }
        }

        // Merge groups based on pattern matches and store match positions
        let mut pattern_matches: HashMap<usize, (usize, usize)> = HashMap::new();
        if !self.config.patterns.is_empty() {
            let mut pattern_to_indices: HashMap<String, Vec<usize>> = HashMap::new();
            for (idx, file) in files.iter().enumerate() {
                for pattern in &self.config.patterns {
                    if let Some(m) = pattern.find(&file.filename) {
                        pattern_to_indices.entry(m.as_str().to_string()).or_default().push(idx);
                        pattern_matches.insert(idx, (m.start(), m.end()));
                        break; // Only match first pattern
                    }
                }
            }

            for indices in pattern_to_indices.values() {
                if indices.len() > 1 {
                    Self::merge_indices_into_groups(indices, &mut file_to_group, &mut groups);
                }
            }
        }

        // Convert to final output format, filtering to groups with multiple files
        groups
            .into_iter()
            .filter(|(_, indices)| indices.len() > 1)
            .map(|(key, indices)| {
                let file_refs: Vec<FileInfo> = indices
                    .iter()
                    .map(|&idx| {
                        let mut file = files[idx].clone();
                        file.pattern_match = pattern_matches.get(&idx).copied();
                        file
                    })
                    .collect();
                (key, file_refs)
            })
            .sorted_by(|a, b| a.0.cmp(&b.0))
            .collect()
    }

    /// Merge file indices into the same group
    fn merge_indices_into_groups(
        indices: &[usize],
        file_to_group: &mut HashMap<usize, String>,
        groups: &mut HashMap<String, Vec<usize>>,
    ) {
        if indices.len() < 2 {
            return;
        }

        // Find the canonical group (use the first one as canonical)
        let canonical_group = file_to_group[&indices[0]].clone();

        for &idx in &indices[1..] {
            let current_group = file_to_group[&idx].clone();
            if current_group != canonical_group {
                // Move all files from current_group to canonical_group
                if let Some(to_move) = groups.remove(&current_group) {
                    for moved_idx in &to_move {
                        file_to_group.insert(*moved_idx, canonical_group.clone());
                    }
                    groups.entry(canonical_group.clone()).or_default().extend(to_move);
                }
            }
        }
    }

    /// Format filename with optional pattern match highlighting
    fn format_filename_with_highlight(filename: &str, pattern_match: Option<(usize, usize)>) -> String {
        if let Some((start, end)) = pattern_match {
            let before = &filename[..start];
            let matched = filename[start..end].green().to_string();
            let after = &filename[end..];
            format!("{before}{matched}{after}")
        } else {
            filename.to_string()
        }
    }

    /// Normalize a file stem by removing resolution and codec patterns
    fn normalize_stem(stem: &str) -> String {
        let mut normalized = stem.to_lowercase();

        // Remove resolutions
        normalized = RE_RESOLUTION.replace_all(&normalized, "").to_string();

        // Remove codec patterns
        normalized = RE_CODEC.replace_all(&normalized, "").to_string();

        // Clean up multiple dots and spaces
        normalized = RE_MULTI_DOTS.replace_all(&normalized, ".").to_string();
        normalized = RE_MULTI_SPACES.replace_all(&normalized, " ").to_string();

        let result = normalized
            .trim_matches(|c| c == '.' || c == ' ' || c == '_' || c == '-')
            .to_string();

        // Fallback to lowercase stem if normalization removed everything
        if result.is_empty() { stem.to_lowercase() } else { result }
    }

    /// Move duplicate files to a Duplicates directory
    fn move_duplicates(&self, duplicates: &[(String, Vec<FileInfo>)]) -> anyhow::Result<()> {
        // Use the first root as the duplicates directory location
        let duplicates_dir = self.roots.first().map_or_else(
            || {
                std::env::current_dir()
                    .unwrap_or_else(|_| PathBuf::from("."))
                    .join("Duplicates")
            },
            |r| r.join("Duplicates"),
        );

        println!(
            "{}",
            format!("\nMoving duplicates to {}", duplicates_dir.display())
                .magenta()
                .bold()
        );

        if self.config.dryrun {
            // Create the duplicates directory if it doesn't exist
            std::fs::create_dir_all(&duplicates_dir)?;
        }

        for (identifier, files) in duplicates {
            // Use pattern match text as directory name if available, otherwise use identifier
            let group_name = files
                .first()
                .and_then(|f| f.pattern_match.map(|(start, end)| f.filename[start..end].to_string()))
                .unwrap_or_else(|| identifier.clone());

            for file in files {
                let target_dir = duplicates_dir.join(&group_name);
                let target_path = cli_tools::get_unique_path(&target_dir, &file.filename, &file.stem, &file.extension);

                println!(
                    "{}: {}",
                    if self.config.dryrun {
                        "[DRYRUN] Move".magenta()
                    } else {
                        "Move".magenta()
                    },
                    cli_tools::path_to_string_relative(&target_path)
                );

                if self.config.dryrun {
                    continue;
                }

                print!("{}", "Move file? (y/n): ".magenta());
                std::io::stdout().flush()?;

                let mut input = String::new();
                std::io::stdin().read_line(&mut input)?;

                if input.trim().eq_ignore_ascii_case("y") {
                    // Create directory if needed
                    if let Err(e) = std::fs::create_dir_all(&target_dir) {
                        print_warning!("Failed to create directory {}: {e}", target_dir.display());
                        continue;
                    }

                    match std::fs::rename(&file.path, &target_path) {
                        Ok(()) => println!("{}", "Moved".green()),
                        Err(e) => print_error!("Failed to move file: {e}"),
                    }
                } else {
                    println!("Skipped");
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::config::Config;
    use crate::dupe_find::{DupeFind, FileInfo};
    use cli_tools::get_unique_path;
    use regex::Regex;
    use std::path::PathBuf;

    /// Helper to create a `FileInfo` for testing
    fn make_file(path: &str, ext: &str) -> FileInfo {
        FileInfo::new(PathBuf::from(path), ext.to_string())
    }

    /// Helper to create a `DupeFind` with specific patterns for testing
    fn make_dupe_finder(patterns: Vec<&str>) -> DupeFind {
        let patterns = patterns
            .into_iter()
            .map(|p| Regex::new(p).expect("Invalid test pattern"))
            .collect();
        DupeFind {
            roots: vec![],
            config: Config {
                dryrun: true,
                extensions: vec!["mp4".to_string(), "mkv".to_string()],
                move_files: false,
                patterns,
                recurse: false,
                verbose: false,
            },
        }
    }

    #[test]
    fn test_find_duplicates_by_normalized_name() {
        let finder = make_dupe_finder(vec![]);
        let files = vec![
            make_file("/path1/movie.1080p.mp4", "mp4"),
            make_file("/path2/movie.720p.mkv", "mkv"),
            make_file("/path3/other.mp4", "mp4"),
        ];

        let duplicates = finder.find_all_duplicates(&files);

        // movie.1080p and movie.720p should be grouped (both normalize to "movie")
        assert_eq!(duplicates.len(), 1);
        assert_eq!(duplicates[0].0, "movie");
        assert_eq!(duplicates[0].1.len(), 2);
    }

    #[test]
    fn test_find_duplicates_by_exact_filename() {
        let finder = make_dupe_finder(vec![]);
        let files = vec![
            make_file("/path1/video.mp4", "mp4"),
            make_file("/path2/video.mp4", "mp4"),
            make_file("/path3/other.mp4", "mp4"),
        ];

        let duplicates = finder.find_all_duplicates(&files);

        assert_eq!(duplicates.len(), 1);
        assert_eq!(duplicates[0].1.len(), 2);
    }

    #[test]
    fn test_find_duplicates_by_pattern() {
        let finder = make_dupe_finder(vec![r"ABC\d+"]);
        let files = vec![
            make_file("/path1/video.ABC123.mp4", "mp4"),
            make_file("/path2/movie.ABC123.mkv", "mkv"),
            make_file("/path3/other.XYZ999.mp4", "mp4"),
        ];

        let duplicates = finder.find_all_duplicates(&files);

        // video.ABC123 and movie.ABC123 should be grouped by pattern match
        assert_eq!(duplicates.len(), 1);
        assert_eq!(duplicates[0].1.len(), 2);
        // Verify pattern match positions are stored
        assert!(duplicates[0].1.iter().all(|f| f.pattern_match.is_some()));
    }

    #[test]
    fn test_find_duplicates_pattern_same_root() {
        // Files with same pattern in the same root directory
        let finder = make_dupe_finder(vec![r"[A-Z]{2}\d{4}"]);
        let files = vec![
            make_file("/videos/Holiday.AB1234.mp4", "mp4"),
            make_file("/videos/Vacation.AB1234.mkv", "mkv"),
            make_file("/videos/Trip.AB1234.1080p.mp4", "mp4"),
            make_file("/videos/Trip.ABC1234.1080p.mp4", "mp4"),
            make_file("/videos/Trip.DDD123.1080p.mp4", "mp4"),
            make_file("/videos/Trip.1234.1080p.mp4", "mp4"),
        ];

        let duplicates = finder.find_all_duplicates(&files);

        assert_eq!(duplicates.len(), 1);
        assert_eq!(duplicates[0].1.len(), 3);
        assert!(duplicates[0].1.iter().all(|f| f.pattern_match.is_some()));
    }

    #[test]
    fn test_find_duplicates_pattern_nested_subdirs() {
        // Files with same pattern scattered in nested subdirectories
        let finder = make_dupe_finder(vec![r"ID-[a-z0-9]+"]);
        let files = vec![
            make_file("/root/2024/january/clip.ID-abc123.mp4", "mp4"),
            make_file("/root/2024/february/video.ID-abc123.mkv", "mkv"),
            make_file("/root/2023/archive/old.ID-abc123.mp4", "mp4"),
            make_file("/root/downloads/new.ID-abc123.720p.mp4", "mp4"),
            make_file("/other/backup/copy.ID-abc123.mp4", "mp4"),
        ];

        let duplicates = finder.find_all_duplicates(&files);

        assert_eq!(duplicates.len(), 1);
        assert_eq!(duplicates[0].1.len(), 5);
    }

    #[test]
    fn test_find_duplicates_pattern_varied_naming_styles() {
        // Different naming conventions but same pattern identifier
        let finder = make_dupe_finder(vec![r"tt\d{7}"]);
        let files = vec![
            // Various naming styles used in media files
            make_file("/movies/The.Movie.2024.tt1234567.1080p.mp4", "mp4"),
            make_file("/movies/The Movie (2024) tt1234567.mkv", "mkv"),
            make_file("/movies/the-movie-2024-tt1234567-720p.mp4", "mp4"),
            make_file("/downloads/The_Movie_2024_tt1234567.mp4", "mp4"),
            make_file("/backup/movie.tt1234567.x265.mp4", "mp4"),
            // Different identifier - should not be grouped
            make_file("/movies/Other.Film.tt9999999.mp4", "mp4"),
        ];

        let duplicates = finder.find_all_duplicates(&files);

        // Should have one group for tt1234567 (5 files)
        // tt9999999 is alone, so no group for it
        let tt1234567_group = duplicates
            .iter()
            .find(|(_, files)| files.iter().any(|f| f.filename.contains("tt1234567")));
        assert!(tt1234567_group.is_some());
        assert_eq!(tt1234567_group.unwrap().1.len(), 5);
    }

    #[test]
    fn test_find_duplicates_pattern_multiple_roots() {
        // Same pattern appearing across completely different root directories
        let finder = make_dupe_finder(vec![r"S\d{2}E\d{2}"]);
        let files = vec![
            make_file("/nas/tv/Show/Season1/show.S01E05.mp4", "mp4"),
            make_file("/local/downloads/show.S01E05.720p.mkv", "mkv"),
            make_file("/external/backup/tv/show.S01E05.1080p.mp4", "mp4"),
            make_file("/cloud/media/show.S01E05.x265.mp4", "mp4"),
            // Different episode
            make_file("/nas/tv/Show/Season1/show.S01E06.mp4", "mp4"),
            make_file("/local/downloads/show.S01E06.mkv", "mkv"),
        ];

        let duplicates = finder.find_all_duplicates(&files);

        // Should have 2 groups: S01E05 (4 files) and S01E06 (2 files)
        assert_eq!(duplicates.len(), 2);

        let e05_group = duplicates
            .iter()
            .find(|(_, files)| files.iter().any(|f| f.filename.contains("S01E05")));
        assert!(e05_group.is_some());
        assert_eq!(e05_group.unwrap().1.len(), 4);

        let e06_group = duplicates
            .iter()
            .find(|(_, files)| files.iter().any(|f| f.filename.contains("S01E06")));
        assert!(e06_group.is_some());
        assert_eq!(e06_group.unwrap().1.len(), 2);
    }

    #[test]
    fn test_find_duplicates_pattern_with_special_chars_in_path() {
        // Paths with spaces, unicode, and special characters
        let finder = make_dupe_finder(vec![r"REF\d+"]);
        let files = vec![
            make_file("/My Videos/2024 Clips/video.REF001.mp4", "mp4"),
            make_file("/Media Library/Downloads (New)/clip.REF001.mkv", "mkv"),
            make_file("/Données/Vidéos/fichier.REF001.mp4", "mp4"),
            make_file("/path/with spaces/and-dashes/file.REF001.mp4", "mp4"),
        ];

        let duplicates = finder.find_all_duplicates(&files);

        assert_eq!(duplicates.len(), 1);
        assert_eq!(duplicates[0].1.len(), 4);
    }

    #[test]
    fn test_find_duplicates_pattern_same_identifier_different_case_in_name() {
        // Files with same pattern identifier but different casing elsewhere in filename
        // Pattern groups by the matched string itself, so same identifier = same group
        let finder = make_dupe_finder(vec![r"ID\d+"]);
        let files = vec![
            make_file("/path1/VIDEO.ID123.mp4", "mp4"),
            make_file("/path2/video.ID123.mkv", "mkv"),
            make_file("/path3/Video.ID123.mp4", "mp4"),
            make_file("/path4/CLIP.ID123.mp4", "mp4"),
            make_file("/path4/CLIP.CD123.mp4", "mp4"),
            make_file("/path4/CLIP.123.mp4", "mp4"),
            make_file("/path4/CLIP.ID.mp4", "mp4"),
        ];

        let duplicates = finder.find_all_duplicates(&files);

        // All have same pattern match "ID123", so grouped together
        assert_eq!(duplicates.len(), 1);
        assert_eq!(duplicates[0].1.len(), 4);
    }

    #[test]
    fn test_find_duplicates_pattern_at_different_positions() {
        // Pattern appearing at start, middle, and end of filename
        let finder = make_dupe_finder(vec![r"KEY\d{3}"]);
        let files = vec![
            make_file("/videos/KEY001.video.mp4", "mp4"),
            make_file("/videos/video.KEY001.mp4", "mp4"),
            make_file("/videos/video.KEY001.1080p.mp4", "mp4"),
            make_file("/videos/some.long.name.KEY001.mp4", "mp4"),
        ];

        let duplicates = finder.find_all_duplicates(&files);

        assert_eq!(duplicates.len(), 1);
        assert_eq!(duplicates[0].1.len(), 4);
        // Verify all have pattern matches recorded
        assert!(duplicates[0].1.iter().all(|f| f.pattern_match.is_some()));
    }

    #[test]
    fn test_find_duplicates_pattern_overlapping_with_resolution() {
        // Pattern that could be confused with resolution patterns
        let finder = make_dupe_finder(vec![r"V\d{4}"]);
        let files = vec![
            make_file("/videos/movie.V1080.mp4", "mp4"),
            make_file("/videos/movie.V1080.720p.mkv", "mkv"),
            make_file("/videos/clip.V1080.1080p.x265.mp4", "mp4"),
        ];

        let duplicates = finder.find_all_duplicates(&files);

        // Should group by pattern V1080, resolution should be stripped separately
        assert_eq!(duplicates.len(), 1);
        assert_eq!(duplicates[0].1.len(), 3);
    }

    #[test]
    #[allow(clippy::similar_names)]
    fn test_find_duplicates_many_patterns_many_files() {
        // Stress test with multiple patterns and many files
        let finder = make_dupe_finder(vec![r"GRP[A-Z]", r"NUM\d+", r"TAG_\w+"]);
        let files = vec![
            // GRPA group
            make_file("/dir1/a.GRPA.mp4", "mp4"),
            make_file("/dir2/b.GRPA.mkv", "mkv"),
            make_file("/dir3/c.GRPA.mp4", "mp4"),
            // GRPB group
            make_file("/dir1/x.GRPB.mp4", "mp4"),
            make_file("/dir2/y.GRPB.mkv", "mkv"),
            // NUM001 group
            make_file("/dir1/video.NUM001.mp4", "mp4"),
            make_file("/dir2/movie.NUM001.mkv", "mkv"),
            make_file("/dir3/clip.NUM001.mp4", "mp4"),
            make_file("/dir4/film.NUM001.mp4", "mp4"),
            // TAG_test group
            make_file("/dir1/file.TAG_test.mp4", "mp4"),
            make_file("/dir2/other.TAG_test.mkv", "mkv"),
            // Unique files (no duplicates)
            make_file("/dir1/unique1.mp4", "mp4"),
            make_file("/dir2/unique2.mkv", "mkv"),
        ];

        let duplicates = finder.find_all_duplicates(&files);

        // Should have 4 groups: GRPA(3), GRPB(2), NUM001(4), TAG_test(2)
        assert_eq!(duplicates.len(), 4);

        let grpa = duplicates
            .iter()
            .find(|(_, f)| f.iter().any(|x| x.filename.contains("GRPA")));
        assert_eq!(grpa.unwrap().1.len(), 3);

        let grpb = duplicates
            .iter()
            .find(|(_, f)| f.iter().any(|x| x.filename.contains("GRPB")));
        assert_eq!(grpb.unwrap().1.len(), 2);

        let num001 = duplicates
            .iter()
            .find(|(_, f)| f.iter().any(|x| x.filename.contains("NUM001")));
        assert_eq!(num001.unwrap().1.len(), 4);

        let tag_test = duplicates
            .iter()
            .find(|(_, f)| f.iter().any(|x| x.filename.contains("TAG_test")));
        assert_eq!(tag_test.unwrap().1.len(), 2);
    }

    #[test]
    fn test_find_duplicates_no_duplicates() {
        let finder = make_dupe_finder(vec![]);
        let files = vec![
            make_file("/path1/video1.mp4", "mp4"),
            make_file("/path2/video2.mp4", "mp4"),
            make_file("/path3/video3.mp4", "mp4"),
        ];

        let duplicates = finder.find_all_duplicates(&files);

        assert!(duplicates.is_empty());
    }

    #[test]
    fn test_find_duplicates_merges_groups() {
        // Test that files matching by pattern are merged into one group
        let finder = make_dupe_finder(vec![r"ID\d+"]);
        let files = vec![
            // These two match by normalized name (both -> "movie")
            make_file("/path1/movie.1080p.mp4", "mp4"),
            make_file("/path2/movie.720p.mkv", "mkv"),
            // These two match by pattern ID123
            make_file("/path3/video.ID123.mp4", "mp4"),
            make_file("/path4/other.ID123.mp4", "mp4"),
        ];

        let duplicates = finder.find_all_duplicates(&files);

        // Should have two groups:
        // - "movie" group (movie.1080p and movie.720p normalize to same name)
        // - merged ID123 group (video.ID123 and other.ID123 match by pattern)
        assert_eq!(duplicates.len(), 2);

        // Find the movie group and verify it has 2 files
        let movie_group = duplicates.iter().find(|(k, _)| k == "movie");
        assert!(movie_group.is_some());
        assert_eq!(movie_group.unwrap().1.len(), 2);

        // Find the pattern-matched group (will be keyed by first file's normalized name)
        let pattern_group = duplicates.iter().find(|(k, _)| k != "movie");
        assert!(pattern_group.is_some());
        assert_eq!(pattern_group.unwrap().1.len(), 2);
        // Verify pattern match positions are stored
        assert!(pattern_group.unwrap().1.iter().all(|f| f.pattern_match.is_some()));
    }

    #[test]
    fn test_find_duplicates_with_resolution_variants() {
        let finder = make_dupe_finder(vec![]);
        let files = vec![
            make_file("/videos/show.1080p.x265.mp4", "mp4"),
            make_file("/videos/show.720p.x264.mkv", "mkv"),
            make_file("/videos/show.1920x1080.mp4", "mp4"),
            make_file("/videos/show.2160p.mp4", "mp4"),
        ];

        let duplicates = finder.find_all_duplicates(&files);

        // All should normalize to "show"
        assert_eq!(duplicates.len(), 1);
        assert_eq!(duplicates[0].0, "show");
        assert_eq!(duplicates[0].1.len(), 4);
    }

    #[test]
    fn test_find_duplicates_multiple_patterns() {
        let finder = make_dupe_finder(vec![r"ABC\d+", r"XYZ\d+"]);
        let files = vec![
            make_file("/path1/video.ABC123.mp4", "mp4"),
            make_file("/path2/movie.ABC123.mkv", "mkv"),
            make_file("/path3/clip.XYZ456.mp4", "mp4"),
            make_file("/path4/film.XYZ456.mkv", "mkv"),
        ];

        let duplicates = finder.find_all_duplicates(&files);

        // Should have two groups: ABC123 and XYZ456
        assert_eq!(duplicates.len(), 2);
    }

    #[test]
    fn test_find_duplicates_first_pattern_takes_priority() {
        // When a file matches multiple patterns, first pattern wins
        let finder = make_dupe_finder(vec![r"FIRST\d+", r"SECOND\d+"]);
        let files = vec![
            // These have FIRST pattern
            make_file("/path1/video.FIRST001.mp4", "mp4"),
            make_file("/path2/movie.FIRST001.mkv", "mkv"),
            // This has both patterns - FIRST should win
            make_file("/path3/clip.FIRST001.SECOND002.mp4", "mp4"),
            // These only have SECOND
            make_file("/path4/other.SECOND002.mp4", "mp4"),
            make_file("/path5/another.SECOND002.mkv", "mkv"),
        ];

        let duplicates = finder.find_all_duplicates(&files);

        // FIRST001 group should have 3 files (including the one with both patterns)
        let first_group = duplicates
            .iter()
            .find(|(_, f)| f.iter().any(|x| x.filename.contains("FIRST001")));
        assert!(first_group.is_some());
        assert_eq!(first_group.unwrap().1.len(), 3);

        // SECOND002 group should have 2 files (excluding the one matched by FIRST)
        let second_group = duplicates.iter().find(|(_, f)| {
            f.iter().all(|x| !x.filename.contains("FIRST001")) && f.iter().any(|x| x.filename.contains("SECOND002"))
        });
        assert!(second_group.is_some());
        assert_eq!(second_group.unwrap().1.len(), 2);
    }

    #[test]
    fn test_find_duplicates_empty_input() {
        let finder = make_dupe_finder(vec![]);
        let files: Vec<FileInfo> = vec![];

        let duplicates = finder.find_all_duplicates(&files);

        assert!(duplicates.is_empty());
    }

    #[test]
    fn test_find_duplicates_single_file() {
        let finder = make_dupe_finder(vec![]);
        let files = vec![make_file("/path/video.mp4", "mp4")];

        let duplicates = finder.find_all_duplicates(&files);

        assert!(duplicates.is_empty());
    }

    #[test]
    fn test_normalize_stem_removes_resolution() {
        assert_eq!(DupeFind::normalize_stem("video.1080p"), "video");
        assert_eq!(DupeFind::normalize_stem("video.1280x720"), "video");
        assert_eq!(DupeFind::normalize_stem("video.1440p"), "video");
        assert_eq!(DupeFind::normalize_stem("video.1920x1080"), "video");
        assert_eq!(DupeFind::normalize_stem("video.2160p"), "video");
        assert_eq!(DupeFind::normalize_stem("video.3840x2160"), "video");
        assert_eq!(DupeFind::normalize_stem("video.720p"), "video");
    }

    #[test]
    fn test_normalize_stem_removes_codec() {
        assert_eq!(DupeFind::normalize_stem("video.h264"), "video");
        assert_eq!(DupeFind::normalize_stem("video.H264"), "video");
        assert_eq!(DupeFind::normalize_stem("video.h265"), "video");
        assert_eq!(DupeFind::normalize_stem("video.x264"), "video");
        assert_eq!(DupeFind::normalize_stem("video.x265"), "video");
        assert_eq!(DupeFind::normalize_stem("video.X265"), "video");
    }

    #[test]
    fn test_normalize_stem_removes_both() {
        assert_eq!(DupeFind::normalize_stem("video.1080p.x265"), "video");
        assert_eq!(DupeFind::normalize_stem("video.720p.x264"), "video");
        assert_eq!(
            DupeFind::normalize_stem("Movie.Title.2024.1080p.x265"),
            "movie.title.2024"
        );
        assert_eq!(
            DupeFind::normalize_stem("Movie.Title.2024.1920x1080.h265"),
            "movie.title.2024"
        );
    }

    #[test]
    fn test_normalize_stem_same_base() {
        // These should all normalize to the same base name
        let name1 = DupeFind::normalize_stem("Movie.Title.1080p");
        let name2 = DupeFind::normalize_stem("Movie.Title.720p");
        let name3 = DupeFind::normalize_stem("Movie.Title.1920x1080");
        let name4 = DupeFind::normalize_stem("Movie.Title.1080p.x265");
        let name5 = DupeFind::normalize_stem("Movie.Title.720p.x264");

        assert_eq!(name1, name2);
        assert_eq!(name2, name3);
        assert_eq!(name3, name4);
        assert_eq!(name4, name5);
    }

    #[test]
    fn test_normalize_stem_preserves_content() {
        let normalized = DupeFind::normalize_stem("Some.Movie.2024.1080p.x265");
        assert!(!normalized.contains("1080p"));
        assert!(!normalized.contains("x265"));
        assert!(normalized.contains("2024"));
        assert!(normalized.contains("movie"));
        assert!(normalized.contains("some"));
    }

    #[test]
    fn test_normalize_stem_cleans_multiple_dots() {
        assert_eq!(DupeFind::normalize_stem("movie....name"), "movie.name");
        assert_eq!(DupeFind::normalize_stem("video...title"), "video.title");
        assert_eq!(DupeFind::normalize_stem("video..1080p"), "video");
    }

    #[test]
    fn test_normalize_stem_cleans_multiple_spaces() {
        assert_eq!(DupeFind::normalize_stem("a    b     c"), "a b c");
        assert_eq!(DupeFind::normalize_stem("movie   name"), "movie name");
        assert_eq!(DupeFind::normalize_stem("video  title"), "video title");
    }

    #[test]
    fn test_normalize_stem_trims_separators() {
        assert_eq!(DupeFind::normalize_stem(" video title "), "video title");
        assert_eq!(DupeFind::normalize_stem("-video-title-"), "video-title");
        assert_eq!(DupeFind::normalize_stem(".video.title."), "video.title");
        assert_eq!(DupeFind::normalize_stem("_video_title_"), "video_title");
    }

    #[test]
    fn test_normalize_stem_fallback_to_original() {
        // When normalization removes everything, fallback to lowercase original
        assert_eq!(DupeFind::normalize_stem("..."), "...");
        assert_eq!(DupeFind::normalize_stem("1080p"), "1080p");
        assert_eq!(DupeFind::normalize_stem("X265"), "x265");
    }

    #[test]
    fn test_normalize_stem_case_insensitive() {
        assert_eq!(DupeFind::normalize_stem("Movie.TITLE"), "movie.title");
        assert_eq!(DupeFind::normalize_stem("VIDEO.1080P"), "video");
    }

    #[test]
    fn test_get_unique_path_no_conflict() {
        let dir = PathBuf::from("/nonexistent/path");
        let result = get_unique_path(&dir, "video.mp4", "video", "mp4");
        assert_eq!(result, dir.join("video.mp4"));
    }

    #[test]
    fn test_get_unique_path_no_extension() {
        let dir = PathBuf::from("/nonexistent/path");
        let result = get_unique_path(&dir, "video", "video", "");
        assert_eq!(result, dir.join("video"));
    }

    #[test]
    fn test_format_filename_with_highlight_no_match() {
        let result = DupeFind::format_filename_with_highlight("video.mp4", None);
        assert_eq!(result, "video.mp4");
    }
}

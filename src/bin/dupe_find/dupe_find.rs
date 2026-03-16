use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use colored::Colorize;
#[cfg(not(test))]
use indicatif::ProgressStyle;
use indicatif::{ParallelProgressIterator, ProgressBar};
use itertools::Itertools;
use rayon::iter::IntoParallelRefIterator;
use rayon::iter::ParallelIterator;
use walkdir::WalkDir;

use cli_tools::dupe_find::{
    DupeFileInfo, DuplicateGroup, MatchRange, format_filename_with_highlight, merge_indices_into_groups, normalize_stem,
};
use cli_tools::scan_cache::ScanCache;
use cli_tools::video_info::VideoInfo;
use cli_tools::{create_semaphore_for_io_bound, print_error, print_yellow};

use crate::Args;
use crate::config::{Config, DupeConfig};

#[cfg(not(test))]
const PROGRESS_BAR_CHARS: &str = "=>-";
#[cfg(not(test))]
const PROGRESS_BAR_TEMPLATE: &str = "[{elapsed_precise}] {bar:80.magenta/blue} {pos}/{len} {percent}%";
#[cfg(not(test))]
const SPINNER_TEMPLATE: &str = "[{elapsed_precise}] {spinner:.magenta} {msg} ({pos} files found)";

/// Duplicate file finder that scans directories for duplicate video files.
pub struct DupeFind {
    config: Config,
    roots: Vec<PathBuf>,
}

impl DupeFind {
    pub fn new(args: Args) -> anyhow::Result<Self> {
        let user_config = DupeConfig::get_user_config()?;

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
        }

        if self.config.debug {
            println!("Extensions: {:?}", self.config.extensions);
            if !self.config.patterns.is_empty() {
                println!("Patterns:");
                for pattern in &self.config.patterns {
                    println!("  {}", pattern.as_str());
                }
            }
            if !self.config.ignore_matches.is_empty() {
                println!("Ignore matches: {:?}", self.config.ignore_matches);
            }
        }

        let files = self.gather_files();
        let duplicates = self.find_all_duplicates(&files);
        let duplicates = self.filter_ignored_groups(duplicates);

        if duplicates.is_empty() {
            println!("{}", "No duplicates found".green());
            return Ok(());
        }

        // Interactive mode when not in print/dryrun mode
        if !self.config.dryrun {
            let metadata = Self::collect_metadata_for_groups(&duplicates);
            return crate::tui::run_interactive(&duplicates, &metadata);
        }

        // Print-only mode
        println!(
            "{}",
            format!("Found {} duplicate groups:", duplicates.len()).yellow().bold()
        );

        for group in &duplicates {
            println!("\n{}:", group.key.cyan());
            for file in group.files.iter().sorted_by_key(|f| &f.path) {
                let display_name = format_filename_with_highlight(&file.filename, file.pattern_match);
                println!("  {display_name}");
            }
        }

        if self.config.move_files {
            self.move_duplicates(&duplicates)?;
        }

        Ok(())
    }

    /// Collect all video files from all root directories in parallel.
    /// Shows a spinner progress bar while scanning.
    fn gather_files(&self) -> Vec<DupeFileInfo> {
        #[cfg(not(test))]
        let progress_bar = {
            let pb = ProgressBar::new_spinner();
            pb.set_style(
                ProgressStyle::default_spinner()
                    .template(SPINNER_TEMPLATE)
                    .expect("Failed to set spinner template"),
            );
            pb.set_message("Scanning directories");
            pb
        };

        let files: Mutex<Vec<DupeFileInfo>> = Mutex::new(Vec::new());

        // Process each root directory in parallel
        self.roots.par_iter().for_each(|root| {
            let collected_files = self.collect_video_files_from_root(
                root,
                #[cfg(not(test))]
                &progress_bar,
            );
            if let Ok(mut all_files) = files.lock() {
                all_files.extend(collected_files);
            }
        });

        #[cfg(not(test))]
        progress_bar.finish_and_clear();

        files.into_inner().unwrap_or_default()
    }

    /// Collect video files from a single root directory.
    fn collect_video_files_from_root(
        &self,
        root: &Path,
        #[cfg(not(test))] progress_bar: &ProgressBar,
    ) -> Vec<DupeFileInfo> {
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
                    #[cfg(not(test))]
                    progress_bar.inc(1);
                    Some(DupeFileInfo::new(path.to_path_buf(), extension))
                } else {
                    None
                }
            })
            .collect()
    }

    /// Collect metadata for all files in duplicate groups using ffprobe.
    ///
    /// Checks the shared scan cache first so that files already analysed by
    /// `vconvert` (or a previous `dupefind` run) are not re-probed.
    /// Newly probed results are written back to the cache.
    fn collect_metadata_for_groups(groups: &[DuplicateGroup]) -> HashMap<PathBuf, VideoInfo> {
        // Collect all unique file paths from duplicate groups
        let all_files: Vec<PathBuf> = groups
            .iter()
            .flat_map(|group| group.files.iter().map(|f| f.path.clone()))
            .collect();

        if all_files.is_empty() {
            return HashMap::new();
        }

        // Try to load the scan cache; if it fails, just probe everything
        let scan_cache = match ScanCache::open() {
            Ok(cache) => Some(cache),
            Err(error) => {
                print_yellow!("Could not open scan cache: {error}");
                None
            }
        };

        let cached_entries = scan_cache
            .as_ref()
            .and_then(|cache| cache.get_all().ok())
            .unwrap_or_default();

        // Split files into cache hits and misses (check path + size match)
        let mut metadata: HashMap<PathBuf, VideoInfo> = HashMap::new();
        let mut cache_misses: Vec<PathBuf> = Vec::new();

        for path in &all_files {
            let path_key = path.to_string_lossy();
            let file_size = std::fs::metadata(path).map(|m| m.len()).ok();

            if let Some(cached) = cached_entries.get(path_key.as_ref())
                && file_size == Some(cached.size_bytes)
            {
                metadata.insert(path.clone(), cached.to_video_info());
            } else {
                cache_misses.push(path.clone());
            }
        }

        let cache_hit_count = metadata.len();
        if cache_hit_count > 0 {
            println!(
                "Scan cache: {} hit(s), {} miss(es)",
                cache_hit_count,
                cache_misses.len()
            );
        }

        // Probe remaining files with ffprobe
        if !cache_misses.is_empty() {
            let runtime = tokio::runtime::Runtime::new().expect("Failed to create tokio runtime");
            let probed = runtime.block_on(collect_metadata_async(cache_misses));

            // Write newly probed results back to the cache
            if let Some(mut cache) = scan_cache {
                let entries: Vec<(&Path, &VideoInfo)> =
                    probed.iter().map(|(path, info)| (path.as_path(), info)).collect();
                if let Err(error) = cache.batch_upsert(&entries) {
                    print_yellow!("Failed to write scan cache: {error}");
                }
            }

            metadata.extend(probed);
        }

        metadata
    }

    /// Find all duplicates in a single pass using multiple detection methods.
    /// Filter out duplicate groups whose display name matches any of the configured ignore strings.
    /// Comparison is case-insensitive.
    fn filter_ignored_groups(&self, groups: Vec<DuplicateGroup>) -> Vec<DuplicateGroup> {
        if self.config.ignore_matches.is_empty() {
            return groups;
        }

        groups
            .into_iter()
            .filter(|group| {
                let name = group.display_name().to_lowercase();
                let contains = self.config.ignore_matches.contains(&name);
                if contains && self.config.verbose {
                    println!("Ignoring group: {}", group.display_name());
                }
                !contains
            })
            .collect()
    }

    /// Files are grouped together if they match any of the criteria:
    /// - Same filename in different directories
    /// - Match the same identifier pattern
    /// - Same normalized name (different resolution / codec / extension)
    fn find_all_duplicates(&self, files: &[DupeFileInfo]) -> Vec<DuplicateGroup> {
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
            .map(|file| normalize_stem(&file.stem))
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
                merge_indices_into_groups(indices, &mut file_to_group, &mut groups);
            }
        }

        // Merge groups based on pattern matches and store match positions
        let mut pattern_matches: HashMap<usize, MatchRange> = HashMap::new();
        if !self.config.patterns.is_empty() {
            let mut pattern_to_indices: HashMap<String, Vec<usize>> = HashMap::new();
            for (idx, file) in files.iter().enumerate() {
                for pattern in &self.config.patterns {
                    if let Some(m) = pattern.find(&file.filename) {
                        pattern_to_indices.entry(m.as_str().to_string()).or_default().push(idx);
                        pattern_matches.insert(
                            idx,
                            MatchRange {
                                start: m.start(),
                                end: m.end(),
                            },
                        );
                        break; // Only match first pattern
                    }
                }
            }

            for indices in pattern_to_indices.values() {
                if indices.len() > 1 {
                    merge_indices_into_groups(indices, &mut file_to_group, &mut groups);
                }
            }
        }

        // Convert to final output format, filtering to groups with multiple files
        groups
            .into_iter()
            .filter(|(_, indices)| indices.len() > 1)
            .map(|(key, indices)| {
                let file_refs: Vec<DupeFileInfo> = indices
                    .iter()
                    .map(|&idx| {
                        let mut file = files[idx].clone();
                        file.pattern_match = pattern_matches.get(&idx).copied();
                        file
                    })
                    .collect();
                DuplicateGroup::new(key, file_refs)
            })
            .sorted_by(|a, b| a.key.cmp(&b.key))
            .collect()
    }

    /// Move duplicate files to a Duplicates directory
    fn move_duplicates(&self, duplicates: &[DuplicateGroup]) -> anyhow::Result<()> {
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

        for group in duplicates {
            let group_name = group.display_name();

            for file in &group.files {
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
                        print_yellow!("Failed to create directory {}: {e}", target_dir.display());
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

/// Collect video metadata concurrently using semaphore-limited async tasks.
///
/// Each ffprobe call runs in a blocking task with concurrency controlled
/// by a semaphore sized for I/O-bound work (`num_cpus * 2`).
async fn collect_metadata_async(files: Vec<PathBuf>) -> HashMap<PathBuf, VideoInfo> {
    let semaphore = create_semaphore_for_io_bound();

    #[cfg(not(test))]
    let progress_bar = {
        let pb = ProgressBar::new(files.len() as u64);
        pb.set_style(
            ProgressStyle::default_bar()
                .template(PROGRESS_BAR_TEMPLATE)
                .expect("Failed to set progress bar template")
                .progress_chars(PROGRESS_BAR_CHARS),
        );
        Arc::new(pb)
    };
    #[cfg(test)]
    let progress_bar = Arc::new(ProgressBar::hidden());

    let tasks: Vec<_> = files
        .into_iter()
        .map(|path| {
            let semaphore = Arc::clone(&semaphore);
            let progress = Arc::clone(&progress_bar);
            tokio::spawn(async move {
                let permit = semaphore.acquire().await.expect("Failed to acquire semaphore");
                let result = tokio::task::spawn_blocking({
                    let path = path.clone();
                    move || VideoInfo::from_path(&path)
                })
                .await
                .expect("spawn_blocking task failed");
                drop(permit);
                progress.inc(1);
                (path, result)
            })
        })
        .collect();

    let metadata: HashMap<PathBuf, VideoInfo> = futures::future::join_all(tasks)
        .await
        .into_iter()
        .filter_map(std::result::Result::ok)
        .filter_map(|(path, result)| match result {
            Ok(info) => Some((path, info)),
            Err(err) => {
                eprintln!("Error: {err}");
                None
            }
        })
        .collect();

    progress_bar.finish_and_clear();

    metadata
}

#[cfg(test)]
mod tests_dupe_find {
    use crate::config::Config;
    use crate::dupe_find::DupeFind;
    use cli_tools::dupe_find::{DupeFileInfo, format_filename_with_highlight, normalize_stem};
    use cli_tools::get_unique_path;
    use regex::Regex;
    use std::path::PathBuf;

    /// Helper to create a `DupeFileInfo` for testing
    fn make_file(path: &str, ext: &str) -> DupeFileInfo {
        DupeFileInfo::new(PathBuf::from(path), ext.to_string())
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
                debug: false,
                dryrun: true,
                extensions: vec!["mp4".to_string(), "mkv".to_string()],
                ignore_matches: vec![],
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
        assert_eq!(duplicates[0].key, "movie");
        assert_eq!(duplicates[0].files.len(), 2);
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
        assert_eq!(duplicates[0].files.len(), 2);
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
        assert_eq!(duplicates[0].files.len(), 2);
        // Verify pattern match positions are stored
        assert!(duplicates[0].files.iter().all(|f| f.pattern_match.is_some()));
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
        assert_eq!(duplicates[0].files.len(), 3);
        assert!(duplicates[0].files.iter().all(|f| f.pattern_match.is_some()));
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
        assert_eq!(duplicates[0].files.len(), 5);
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
            .find(|group| group.files.iter().any(|f| f.filename.contains("tt1234567")));
        assert!(tt1234567_group.is_some());
        assert_eq!(tt1234567_group.unwrap().files.len(), 5);
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
            .find(|group| group.files.iter().any(|f| f.filename.contains("S01E05")));
        assert!(e05_group.is_some());
        assert_eq!(e05_group.unwrap().files.len(), 4);

        let e06_group = duplicates
            .iter()
            .find(|group| group.files.iter().any(|f| f.filename.contains("S01E06")));
        assert!(e06_group.is_some());
        assert_eq!(e06_group.unwrap().files.len(), 2);
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
        assert_eq!(duplicates[0].files.len(), 4);
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
        assert_eq!(duplicates[0].files.len(), 4);
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
        assert_eq!(duplicates[0].files.len(), 4);
        // Verify all have pattern matches recorded
        assert!(duplicates[0].files.iter().all(|f| f.pattern_match.is_some()));
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
        assert_eq!(duplicates[0].files.len(), 3);
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
            .find(|group| group.files.iter().any(|x| x.filename.contains("GRPA")));
        assert_eq!(grpa.unwrap().files.len(), 3);

        let grpb = duplicates
            .iter()
            .find(|group| group.files.iter().any(|x| x.filename.contains("GRPB")));
        assert_eq!(grpb.unwrap().files.len(), 2);

        let num001 = duplicates
            .iter()
            .find(|group| group.files.iter().any(|x| x.filename.contains("NUM001")));
        assert_eq!(num001.unwrap().files.len(), 4);

        let tag_test = duplicates
            .iter()
            .find(|group| group.files.iter().any(|x| x.filename.contains("TAG_test")));
        assert_eq!(tag_test.unwrap().files.len(), 2);
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
        let movie_group = duplicates.iter().find(|group| group.key == "movie");
        assert!(movie_group.is_some());
        assert_eq!(movie_group.unwrap().files.len(), 2);

        // Find the pattern-matched group (will be keyed by first file's normalized name)
        let pattern_group = duplicates.iter().find(|group| group.key != "movie");
        assert!(pattern_group.is_some());
        assert_eq!(pattern_group.unwrap().files.len(), 2);
        // Verify pattern match positions are stored
        assert!(pattern_group.unwrap().files.iter().all(|f| f.pattern_match.is_some()));
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
        assert_eq!(duplicates[0].key, "show");
        assert_eq!(duplicates[0].files.len(), 4);
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
            .find(|group| group.files.iter().any(|x| x.filename.contains("FIRST001")));
        assert!(first_group.is_some());
        assert_eq!(first_group.unwrap().files.len(), 3);

        // SECOND002 group should have 2 files (excluding the one matched by FIRST)
        let second_group = duplicates.iter().find(|group| {
            group.files.iter().all(|x| !x.filename.contains("FIRST001"))
                && group.files.iter().any(|x| x.filename.contains("SECOND002"))
        });
        assert!(second_group.is_some());
        assert_eq!(second_group.unwrap().files.len(), 2);
    }

    #[test]
    fn test_find_duplicates_empty_input() {
        let finder = make_dupe_finder(vec![]);
        let files: Vec<DupeFileInfo> = vec![];

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
        assert_eq!(normalize_stem("video.1080p"), "video");
        assert_eq!(normalize_stem("video.1280x720"), "video");
        assert_eq!(normalize_stem("video.1440p"), "video");
        assert_eq!(normalize_stem("video.1920x1080"), "video");
        assert_eq!(normalize_stem("video.2160p"), "video");
        assert_eq!(normalize_stem("video.3840x2160"), "video");
        assert_eq!(normalize_stem("video.720p"), "video");
    }

    #[test]
    fn test_normalize_stem_removes_codec() {
        assert_eq!(normalize_stem("video.h264"), "video");
        assert_eq!(normalize_stem("video.H264"), "video");
        assert_eq!(normalize_stem("video.h265"), "video");
        assert_eq!(normalize_stem("video.x264"), "video");
        assert_eq!(normalize_stem("video.x265"), "video");
        assert_eq!(normalize_stem("video.X265"), "video");
    }

    #[test]
    fn test_normalize_stem_removes_both() {
        assert_eq!(normalize_stem("video.1080p.x265"), "video");
        assert_eq!(normalize_stem("video.720p.x264"), "video");
        assert_eq!(normalize_stem("Movie.Title.2024.1080p.x265"), "movie.title.2024");
        assert_eq!(normalize_stem("Movie.Title.2024.1920x1080.h265"), "movie.title.2024");
    }

    #[test]
    fn test_normalize_stem_same_base() {
        // These should all normalize to the same base name
        let name1 = normalize_stem("Movie.Title.1080p");
        let name2 = normalize_stem("Movie.Title.720p");
        let name3 = normalize_stem("Movie.Title.1920x1080");
        let name4 = normalize_stem("Movie.Title.1080p.x265");
        let name5 = normalize_stem("Movie.Title.720p.x264");

        assert_eq!(name1, name2);
        assert_eq!(name2, name3);
        assert_eq!(name3, name4);
        assert_eq!(name4, name5);
    }

    #[test]
    fn test_normalize_stem_preserves_content() {
        let normalized = normalize_stem("Some.Movie.2024.1080p.x265");
        assert!(!normalized.contains("1080p"));
        assert!(!normalized.contains("x265"));
        assert!(normalized.contains("2024"));
        assert!(normalized.contains("movie"));
        assert!(normalized.contains("some"));
    }

    #[test]
    fn test_normalize_stem_cleans_multiple_dots() {
        assert_eq!(normalize_stem("movie....name"), "movie.name");
        assert_eq!(normalize_stem("video...title"), "video.title");
        assert_eq!(normalize_stem("video..1080p"), "video");
    }

    #[test]
    fn test_normalize_stem_cleans_multiple_spaces() {
        assert_eq!(normalize_stem("a    b     c"), "a b c");
        assert_eq!(normalize_stem("movie   name"), "movie name");
        assert_eq!(normalize_stem("video  title"), "video title");
    }

    #[test]
    fn test_normalize_stem_trims_separators() {
        assert_eq!(normalize_stem(" video title "), "video title");
        assert_eq!(normalize_stem("-video-title-"), "video-title");
        assert_eq!(normalize_stem(".video.title."), "video.title");
        assert_eq!(normalize_stem("_video_title_"), "video_title");
    }

    #[test]
    fn test_normalize_stem_fallback_to_original() {
        // When normalization removes everything, fallback to lowercase original
        assert_eq!(normalize_stem("..."), "...");
        assert_eq!(normalize_stem("1080p"), "1080p");
        assert_eq!(normalize_stem("X265"), "x265");
    }

    #[test]
    fn test_normalize_stem_case_insensitive() {
        assert_eq!(normalize_stem("Movie.TITLE"), "movie.title");
        assert_eq!(normalize_stem("VIDEO.1080P"), "video");
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
        let result = format_filename_with_highlight("video.mp4", None);
        assert_eq!(result, "video.mp4");
    }
}

#[cfg(test)]
mod test_video_info_resolution_string {
    #![allow(unused_imports)]
    use super::*;
    use cli_tools::video_info::Resolution;

    #[test]
    fn formats_resolution() {
        let info = VideoInfo {
            size_bytes: Some(1000),
            duration: Some(60.0),
            resolution: Some(Resolution::new(1920, 1080)),
            codec: Some("h264".to_string()),
            ..Default::default()
        };
        assert_eq!(info.resolution_string(), Some("1920x1080".to_string()));
    }

    #[test]
    fn returns_none_when_resolution_missing() {
        let info = VideoInfo {
            size_bytes: Some(1000),
            duration: Some(60.0),
            codec: Some("h264".to_string()),
            ..Default::default()
        };
        assert_eq!(info.resolution_string(), None);
    }
}

#[cfg(test)]
mod test_display_name {
    use cli_tools::dupe_find::{DupeFileInfo, DuplicateGroup, MatchRange};
    use std::path::PathBuf;

    /// Helper to create a `DupeFileInfo` with an optional pattern match.
    fn make_file_with_match(filename: &str, ext: &str, pattern_match: Option<MatchRange>) -> DupeFileInfo {
        let path = PathBuf::from(filename);
        let mut file = DupeFileInfo::new(path, ext.to_string());
        file.pattern_match = pattern_match;
        file
    }

    #[test]
    fn falls_back_to_key_when_no_pattern_matches() {
        let group = DuplicateGroup::new(
            "some.movie".to_string(),
            vec![
                make_file_with_match("some.movie.mkv", "mkv", None),
                make_file_with_match("some.movie.mp4", "mp4", None),
            ],
        );
        assert_eq!(group.display_name(), "some.movie");
    }

    #[test]
    fn uses_common_pattern_text_when_all_files_match() {
        // Files: "Show.S01E05.720p.mkv" and "Show.S01E05.1080p.mp4"
        // Pattern matched "S01E05" in both
        let group = DuplicateGroup::new(
            "show.s01e05".to_string(),
            vec![
                make_file_with_match("Show.S01E05.720p.mkv", "mkv", Some(MatchRange { start: 5, end: 11 })),
                make_file_with_match("Show.S01E05.1080p.mp4", "mp4", Some(MatchRange { start: 5, end: 11 })),
            ],
        );
        assert_eq!(group.display_name(), "S01E05");
    }

    #[test]
    fn uses_pattern_text_with_different_casing() {
        // Same pattern matched with different casing across files
        let group = DuplicateGroup::new(
            "show.s01e05".to_string(),
            vec![
                make_file_with_match("Show.S01E05.720p.mkv", "mkv", Some(MatchRange { start: 5, end: 11 })),
                make_file_with_match("show.s01e05.1080p.mp4", "mp4", Some(MatchRange { start: 5, end: 11 })),
            ],
        );
        // "S01E05" and "s01e05" match case-insensitively, uses first file's text
        assert_eq!(group.display_name(), "S01E05");
    }

    #[test]
    fn falls_back_to_key_when_pattern_texts_differ() {
        // Merged group where files matched different pattern texts
        let group = DuplicateGroup::new(
            "normalized.key".to_string(),
            vec![
                make_file_with_match("Show.S01E05.mkv", "mkv", Some(MatchRange { start: 5, end: 11 })),
                make_file_with_match("Show.S02E10.mkv", "mkv", Some(MatchRange { start: 5, end: 11 })),
            ],
        );
        // "S01E05" != "S02E10", so falls back to key
        assert_eq!(group.display_name(), "normalized.key");
    }

    #[test]
    fn falls_back_to_key_when_only_some_files_match() {
        // Group merged from pattern match + normalized name match
        let group = DuplicateGroup::new(
            "some.show".to_string(),
            vec![
                make_file_with_match("Some.Show.S01E05.mkv", "mkv", Some(MatchRange { start: 10, end: 16 })),
                make_file_with_match("Some.Show.mkv", "mkv", None),
            ],
        );
        // Not all files have a pattern match, only one matches
        // The iterator skips the None, finds "S01E05", then has no more items
        // So it returns the single pattern text
        assert_eq!(group.display_name(), "S01E05");
    }

    #[test]
    fn uses_pattern_text_for_single_file_with_match() {
        let group = DuplicateGroup::new(
            "key".to_string(),
            vec![make_file_with_match(
                "Show.S01E05.mkv",
                "mkv",
                Some(MatchRange { start: 5, end: 11 }),
            )],
        );
        assert_eq!(group.display_name(), "S01E05");
    }

    #[test]
    fn falls_back_to_key_for_empty_group() {
        let group = DuplicateGroup::new("empty.key".to_string(), vec![]);
        assert_eq!(group.display_name(), "empty.key");
    }

    #[test]
    fn matches_episode_pattern_with_letters_and_numbers() {
        // Pattern like (?i)S\d+E\d+ matching "S01E05" in filenames
        let group = DuplicateGroup::new(
            "show.s01e05".to_string(),
            vec![
                make_file_with_match("Show.S01E05.720p.mkv", "mkv", Some(MatchRange { start: 5, end: 11 })),
                make_file_with_match(
                    "Show.S01E05.REPACK.1080p.mp4",
                    "mp4",
                    Some(MatchRange { start: 5, end: 11 }),
                ),
                make_file_with_match("show.s01e05.web.mkv", "mkv", Some(MatchRange { start: 5, end: 11 })),
            ],
        );
        assert_eq!(group.display_name(), "S01E05");
    }

    #[test]
    fn matches_numeric_only_identifier() {
        // Pattern like \d{6} matching a numeric ID in filenames
        let group = DuplicateGroup::new(
            "clip.483621".to_string(),
            vec![
                make_file_with_match("clip.483621.720p.mkv", "mkv", Some(MatchRange { start: 5, end: 11 })),
                make_file_with_match("clip_483621_1080p.mp4", "mp4", Some(MatchRange { start: 5, end: 11 })),
            ],
        );
        assert_eq!(group.display_name(), "483621");
    }

    #[test]
    fn matches_alphabetic_only_identifier() {
        // Pattern like [A-Za-z]+ matching a word identifier
        let group = DuplicateGroup::new(
            "documentary.wildlife".to_string(),
            vec![
                make_file_with_match(
                    "Documentary.Wildlife.720p.mkv",
                    "mkv",
                    Some(MatchRange { start: 12, end: 20 }),
                ),
                make_file_with_match(
                    "documentary.wildlife.1080p.mp4",
                    "mp4",
                    Some(MatchRange { start: 12, end: 20 }),
                ),
            ],
        );
        // "Wildlife" and "wildlife" match case-insensitively
        assert_eq!(group.display_name(), "Wildlife");
    }

    #[test]
    fn matches_pattern_at_different_positions_in_filenames() {
        // Same matched text but at different character positions in each filename
        // Pattern like (?i)ABC-\d+ matching "ABC-123"
        let group = DuplicateGroup::new(
            "normalized.key".to_string(),
            vec![
                make_file_with_match("ABC-123.720p.mkv", "mkv", Some(MatchRange { start: 0, end: 7 })),
                make_file_with_match(
                    "prefix.ABC-123.1080p.mp4",
                    "mp4",
                    Some(MatchRange { start: 7, end: 14 }),
                ),
            ],
        );
        assert_eq!(group.display_name(), "ABC-123");
    }

    #[test]
    fn matches_pattern_with_mixed_separators() {
        // Pattern matching an identifier like "2024.05.01" across files with different formatting
        let group = DuplicateGroup::new(
            "show.2024.05.01".to_string(),
            vec![
                make_file_with_match(
                    "Show.2024.05.01.Episode.mkv",
                    "mkv",
                    Some(MatchRange { start: 5, end: 15 }),
                ),
                make_file_with_match(
                    "show.2024.05.01.rerun.mp4",
                    "mp4",
                    Some(MatchRange { start: 5, end: 15 }),
                ),
            ],
        );
        assert_eq!(group.display_name(), "2024.05.01");
    }

    #[test]
    fn matches_short_alphanumeric_code() {
        // Pattern like [A-Z]\d{3} matching codes like "A001"
        let group = DuplicateGroup::new(
            "batch.a001".to_string(),
            vec![
                make_file_with_match("Batch.A001.v1.mkv", "mkv", Some(MatchRange { start: 6, end: 10 })),
                make_file_with_match("batch.a001.v2.mp4", "mp4", Some(MatchRange { start: 6, end: 10 })),
            ],
        );
        assert_eq!(group.display_name(), "A001");
    }
}

#[cfg(test)]
mod test_filter_ignored_groups {
    use crate::config::Config;
    use crate::dupe_find::DupeFind;
    use cli_tools::dupe_find::{DupeFileInfo, DuplicateGroup, MatchRange};

    use std::path::PathBuf;

    /// Helper to create a `DupeFileInfo` with an optional pattern match.
    fn make_file_with_match(filename: &str, ext: &str, pattern_match: Option<MatchRange>) -> DupeFileInfo {
        let path = PathBuf::from(filename);
        let mut file = DupeFileInfo::new(path, ext.to_string());
        file.pattern_match = pattern_match;
        file
    }

    /// Helper to create a `DupeFind` with specific ignore matches.
    fn make_dupe_finder(ignore_matches: Vec<&str>) -> DupeFind {
        DupeFind {
            roots: vec![],
            config: Config {
                debug: false,
                dryrun: false,
                extensions: vec![],
                ignore_matches: ignore_matches.into_iter().map(str::to_lowercase).collect(),
                move_files: false,
                patterns: vec![],
                recurse: false,
                verbose: false,
            },
        }
    }

    #[test]
    fn keeps_all_groups_when_no_ignores_configured() {
        let finder = make_dupe_finder(vec![]);
        let groups = vec![
            DuplicateGroup::new(
                "movie.one".to_string(),
                vec![
                    make_file_with_match("movie.one.mkv", "mkv", None),
                    make_file_with_match("movie.one.mp4", "mp4", None),
                ],
            ),
            DuplicateGroup::new(
                "movie.two".to_string(),
                vec![
                    make_file_with_match("movie.two.mkv", "mkv", None),
                    make_file_with_match("movie.two.mp4", "mp4", None),
                ],
            ),
        ];
        let filtered = finder.filter_ignored_groups(groups);
        assert_eq!(filtered.len(), 2);
    }

    #[test]
    fn filters_group_by_key_match() {
        let finder = make_dupe_finder(vec!["movie.one"]);
        let groups = vec![
            DuplicateGroup::new(
                "movie.one".to_string(),
                vec![
                    make_file_with_match("movie.one.mkv", "mkv", None),
                    make_file_with_match("movie.one.mp4", "mp4", None),
                ],
            ),
            DuplicateGroup::new(
                "movie.two".to_string(),
                vec![
                    make_file_with_match("movie.two.mkv", "mkv", None),
                    make_file_with_match("movie.two.mp4", "mp4", None),
                ],
            ),
        ];
        let filtered = finder.filter_ignored_groups(groups);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].key, "movie.two");
    }

    #[test]
    fn filters_group_by_pattern_match_display_name() {
        let finder = make_dupe_finder(vec!["S01E05"]);
        let groups = vec![DuplicateGroup::new(
            "show.s01e05".to_string(),
            vec![
                make_file_with_match("Show.S01E05.720p.mkv", "mkv", Some(MatchRange { start: 5, end: 11 })),
                make_file_with_match("Show.S01E05.1080p.mp4", "mp4", Some(MatchRange { start: 5, end: 11 })),
            ],
        )];
        let filtered = finder.filter_ignored_groups(groups);
        assert_eq!(filtered.len(), 0);
    }

    #[test]
    fn comparison_is_case_insensitive() {
        let finder = make_dupe_finder(vec!["ABC-123"]);
        let groups = vec![DuplicateGroup::new(
            "normalized.key".to_string(),
            vec![
                make_file_with_match("prefix.Abc-123.mkv", "mkv", Some(MatchRange { start: 7, end: 14 })),
                make_file_with_match("prefix.abc-123.mp4", "mp4", Some(MatchRange { start: 7, end: 14 })),
            ],
        )];
        let filtered = finder.filter_ignored_groups(groups);
        assert_eq!(filtered.len(), 0);
    }

    #[test]
    fn does_not_filter_partial_matches() {
        let finder = make_dupe_finder(vec!["ABC"]);
        let groups = vec![DuplicateGroup::new(
            "abc-123".to_string(),
            vec![
                make_file_with_match("ABC-123.mkv", "mkv", None),
                make_file_with_match("ABC-123.mp4", "mp4", None),
            ],
        )];
        // "abc-123" != "abc", so the group should be kept
        let filtered = finder.filter_ignored_groups(groups);
        assert_eq!(filtered.len(), 1);
    }

    #[test]
    fn filters_multiple_ignored_patterns() {
        let finder = make_dupe_finder(vec!["movie.one", "movie.three"]);
        let groups = vec![
            DuplicateGroup::new(
                "movie.one".to_string(),
                vec![
                    make_file_with_match("movie.one.mkv", "mkv", None),
                    make_file_with_match("movie.one.mp4", "mp4", None),
                ],
            ),
            DuplicateGroup::new(
                "movie.two".to_string(),
                vec![
                    make_file_with_match("movie.two.mkv", "mkv", None),
                    make_file_with_match("movie.two.mp4", "mp4", None),
                ],
            ),
            DuplicateGroup::new(
                "movie.three".to_string(),
                vec![
                    make_file_with_match("movie.three.mkv", "mkv", None),
                    make_file_with_match("movie.three.mp4", "mp4", None),
                ],
            ),
        ];
        let filtered = finder.filter_ignored_groups(groups);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].key, "movie.two");
    }

    #[test]
    fn filters_by_numeric_pattern_display_name() {
        let finder = make_dupe_finder(vec!["483621"]);
        let groups = vec![DuplicateGroup::new(
            "clip.483621".to_string(),
            vec![
                make_file_with_match("clip.483621.720p.mkv", "mkv", Some(MatchRange { start: 5, end: 11 })),
                make_file_with_match("clip_483621_1080p.mp4", "mp4", Some(MatchRange { start: 5, end: 11 })),
            ],
        )];
        let filtered = finder.filter_ignored_groups(groups);
        assert_eq!(filtered.len(), 0);
    }
}

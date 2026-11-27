//! Core duplicate finder orchestration, grouping, and file operations.
//!
//! This module defines `DupeFind`, coordinates directory scanning and duplicate detection,
//! and handles duplicate filtering, display, and moves.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use colored::Colorize;
#[cfg(not(test))]
use indicatif::ProgressBar;
#[cfg(not(test))]
use indicatif::ProgressStyle;
use itertools::Itertools;
use rayon::iter::IntoParallelRefIterator;
use rayon::iter::ParallelIterator;
use walkdir::WalkDir;

use cli_tools_common::{MoveResult, OverwritePolicy, TargetReservations, print_error, print_yellow};
use cli_tools_db::locks::ActiveLockStore;
use cli_tools_dupefind::{
    DupeFileInfo, DuplicateGroup, DuplicateMatchOptions, filter_ignored_groups, find_duplicates,
    find_hash_only_duplicates, format_filename_with_highlight, group_matches_ignore,
};
use cli_tools_media::file_hash::hash_file;
use cli_tools_media::scan_cache::ScanCache;
use cli_tools_media::video_info::VideoInfo;

use crate::config::{Config, DupeConfig};
#[cfg(not(test))]
use crate::helpers::SPINNER_TEMPLATE;
use crate::{Args, helpers};

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
                .map(|p| cli_tools_common::resolve_required_input_path(p))
                .collect::<anyhow::Result<Vec<_>>>()?
        } else if !args.paths.is_empty() {
            args.paths
                .iter()
                .map(|p| cli_tools_common::resolve_required_input_path(p))
                .collect::<anyhow::Result<Vec<_>>>()?
        } else if !user_config.paths.is_empty() {
            user_config
                .paths
                .iter()
                .map(|p| cli_tools_common::resolve_required_input_path(p))
                .collect::<anyhow::Result<Vec<_>>>()?
        } else {
            vec![cli_tools_common::resolve_input_path(None)?]
        };

        let config = Config::from_args_and_user_config(args, user_config)?;
        let roots = deduplicate_roots(roots, config.recurse);

        Ok(Self { config, roots })
    }

    pub fn run(&self) -> anyhow::Result<()> {
        if self.config.verbose {
            let paths_display = self
                .roots
                .iter()
                .map(|path| cli_tools_common::path_to_string(path))
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
            if !self.config.prefix_ignores.is_empty() {
                println!("Prefix ignores: {:?}", self.config.prefix_ignores);
            }
            println!("Hash comparison: {}", self.config.hash_compare);
            println!("Hash only: {}", self.config.hash_only);
        }

        let files = self.gather_files();
        if self.config.verbose {
            println!("Checking {} files for duplicates...", files.len());
        }
        let hash_matches = helpers::find_hash_matches(
            &files,
            self.config.hash_compare,
            !self.config.no_cache,
            self.config.verbose,
        );
        let duplicates = if self.config.hash_only {
            find_hash_only_duplicates(&files, &hash_matches)
        } else {
            find_duplicates(
                &files,
                &DuplicateMatchOptions {
                    patterns: &self.config.patterns,
                    prefix_ignores: &self.config.prefix_ignores,
                    additional_match_groups: &hash_matches,
                },
            )
        };
        if self.config.verbose {
            for group in duplicates
                .iter()
                .filter(|group| group_matches_ignore(group, &self.config.ignore_matches))
            {
                println!("Ignoring group: {}", group.display_name());
            }
        }
        let duplicates = filter_ignored_groups(duplicates, &self.config.ignore_matches);

        if duplicates.is_empty() {
            println!("{}", "No duplicates found".green());
            return Ok(());
        }

        // Interactive mode when not moving and not in print/dryrun mode
        if !self.config.dryrun && !self.config.move_files {
            let metadata = helpers::collect_metadata_for_groups(&duplicates, !self.config.no_cache);
            return crate::tui::run_interactive(&duplicates, &metadata);
        }

        // Print-only or move mode:
        // print group display names and full relative paths so directories are distinguishable.
        println!(
            "{}",
            format!("Found {} duplicate groups:", duplicates.len()).yellow().bold()
        );

        for group in &duplicates {
            println!("\n{}:", group.display_name().cyan());
            for file in group.files.iter().sorted_by_key(|f| &f.path) {
                let relative = cli_tools_common::path_to_string_relative(&file.path);
                let highlighted_filename = format_filename_with_highlight(&file.filename, file.pattern_match);
                let display = match Path::new(&relative).parent() {
                    Some(parent) if !parent.as_os_str().is_empty() => {
                        format!("{}/{highlighted_filename}", parent.display())
                    }
                    _ => highlighted_filename,
                };
                println!("  {display}");
            }
        }

        if self.config.move_files {
            let metadata = if cfg!(test) {
                std::collections::HashMap::new()
            } else {
                helpers::collect_metadata_for_groups(&duplicates, !self.config.no_cache)
            };
            self.move_duplicates(&duplicates, &metadata)?;
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

        // A file reachable from several roots must only be listed once,
        // otherwise it would form a duplicate group with itself.
        let mut files = files.into_inner().unwrap_or_default();
        files.sort_by(|left, right| left.path.cmp(&right.path));
        files.dedup_by(|left, right| left.path == right.path);
        files
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
            .filter_entry(|entry| {
                if cli_tools_common::should_skip_entry(entry) {
                    return false;
                }
                !(entry.depth() > 0
                    && entry.file_type().is_dir()
                    && entry.file_name().eq_ignore_ascii_case("Duplicates"))
            })
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_file())
            .filter_map(|entry| {
                let path = entry.path();
                let extension = cli_tools_common::path_to_file_extension_string(path);
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

    /// Move duplicate files to a `Duplicates` directory, keeping the best-ranked file in place.
    fn move_duplicates(
        &self,
        duplicates: &[DuplicateGroup],
        metadata: &std::collections::HashMap<PathBuf, VideoInfo>,
    ) -> anyhow::Result<()> {
        let duplicates_dir = self.duplicates_dir();
        println!(
            "{}",
            format!("\nMoving duplicates to {}", duplicates_dir.display())
                .magenta()
                .bold()
        );

        let lock_store = if self.config.dryrun || cfg!(test) {
            None
        } else {
            ActiveLockStore::open_default("dupefind").ok()
        };
        let mut scan_cache = if self.config.dryrun || self.config.no_cache || cfg!(test) {
            None
        } else {
            ScanCache::open().ok()
        };

        let mut reservations = TargetReservations::new();
        let mut failed_count = 0usize;
        for group in duplicates {
            let group_name = sanitize_group_dir_name(&group.display_name());
            let target_dir = duplicates_dir.join(&group_name);
            let sorted_files: Vec<&DupeFileInfo> = group.files.iter().sorted_by_key(|file| &file.path).collect();
            let best_index = crate::tui::find_best_file_index_with_metadata(&sorted_files, metadata);
            if let Some(best) = sorted_files.get(best_index) {
                println!(
                    "{}: {}",
                    "Keep".green(),
                    cli_tools_common::path_to_string_relative(&best.path)
                );
            }

            for (index, file) in sorted_files.iter().enumerate() {
                if index == best_index || file.path.parent() == Some(target_dir.as_path()) {
                    continue;
                }
                let target_path = cli_tools_common::get_unique_path_with_reservations(
                    &target_dir,
                    &file.filename,
                    &file.stem,
                    &file.extension,
                    &reservations,
                );

                println!(
                    "{}: {}",
                    if self.config.dryrun {
                        "[DRYRUN] Move".magenta()
                    } else {
                        "Move".magenta()
                    },
                    cli_tools_common::path_to_string_relative(&target_path)
                );

                if self.config.dryrun {
                    reservations.reserve(&target_path);
                    continue;
                }

                if let Some(ref store) = lock_store
                    && let Ok(Some(holder)) = store.is_any_locked_by_other(&[file.path.clone(), target_path.clone()])
                {
                    print_yellow!(
                        "Skipping locked file (held by {} pid {}): {}",
                        holder.tool,
                        holder.pid,
                        file.path.display()
                    );
                    continue;
                }

                if let Err(e) = std::fs::create_dir_all(&target_dir) {
                    print_yellow!("Failed to create directory {}: {e}", target_dir.display());
                    failed_count += 1;
                    continue;
                }

                match move_file_with_verification(&file.path, &target_path) {
                    Ok(MoveResult::SkippedExisting) => {
                        print_yellow!("Skipping move, file already exists: {}", target_path.display());
                    }
                    Ok(_) => {
                        reservations.reserve(&target_path);
                        if let Some(ref mut cache) = scan_cache {
                            let _ = cache.move_cached_path(&file.path, &target_path);
                        }
                    }
                    Err(e) => {
                        print_error!("Failed to move file: {e}");
                        failed_count += 1;
                    }
                }
            }
        }

        anyhow::ensure!(
            failed_count == 0,
            "Failed to move {}",
            cli_tools_common::count_label(failed_count, "duplicate file", "duplicate files")
        );
        Ok(())
    }

    /// Resolve the `Duplicates` destination directory under the first configured root.
    fn duplicates_dir(&self) -> PathBuf {
        self.roots.first().map_or_else(
            || {
                std::env::current_dir()
                    .unwrap_or_else(|_| PathBuf::from("."))
                    .join("Duplicates")
            },
            |root| root.join("Duplicates"),
        )
    }
}

/// Sanitize a duplicate group display name so it is a safe directory component on Unix and Windows.
fn sanitize_group_dir_name(name: &str) -> String {
    let cleaned: String = name
        .trim()
        .chars()
        .map(|character| match character {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\0' => '_',
            other => other,
        })
        .collect();
    let trimmed = cleaned.trim_end_matches(['.', ' ']);
    if trimmed.is_empty() || trimmed == "." || trimmed == ".." {
        "_group".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Move a file to `target`, verifying BLAKE3 hashes when a cross-device copy is required.
fn move_file_with_verification(source: &Path, target: &Path) -> std::io::Result<MoveResult> {
    let verify = |src: &Path, dst: &Path| -> std::io::Result<()> {
        let src_hash = hash_file(src).map_err(std::io::Error::other)?;
        let dst_hash = hash_file(dst).map_err(std::io::Error::other)?;
        if src_hash != dst_hash {
            return Err(std::io::Error::other(format!(
                "BLAKE3 hash mismatch after cross-device copy of {}",
                src.display()
            )));
        }
        Ok(())
    };
    cli_tools_common::safe_move_file(source, target, OverwritePolicy::Skip, Some(&verify))
}

/// Remove repeated roots while preserving user CLI order, dropping roots nested under an ancestor when recursing.
fn deduplicate_roots(roots: Vec<PathBuf>, recurse: bool) -> Vec<PathBuf> {
    let mut unique: Vec<PathBuf> = Vec::with_capacity(roots.len());
    for root in roots {
        if !unique.contains(&root) {
            unique.push(root);
        }
    }
    if recurse {
        let mut kept: Vec<PathBuf> = Vec::with_capacity(unique.len());
        for root in &unique {
            let has_ancestor = unique.iter().any(|other| other != root && root.starts_with(other));
            if !has_ancestor {
                kept.push(root.clone());
            }
        }
        kept
    } else {
        unique
    }
}

#[cfg(test)]
mod test_gather_files {
    use std::fs;

    use super::*;

    fn make_finder(root: PathBuf, recurse: bool) -> DupeFind {
        DupeFind {
            roots: vec![root],
            config: Config {
                debug: false,
                dryrun: true,
                extensions: vec!["mp4".to_string()],
                hash_compare: false,
                hash_only: false,
                no_cache: true,
                ignore_matches: vec![],
                move_files: false,
                patterns: vec![],
                prefix_ignores: vec![],
                recurse,
                verbose: false,
            },
        }
    }

    #[test]
    fn includes_configured_extensions_and_skips_other_files() {
        let temp_directory = tempfile::TempDir::new().expect("should create temp directory");
        let scan_root = temp_directory.path().join("videos");
        fs::create_dir(&scan_root).expect("should create scan root");
        fs::write(scan_root.join("video.mp4"), b"video").expect("should write video file");
        fs::write(scan_root.join("notes.txt"), b"notes").expect("should write text file");
        let finder = make_finder(scan_root, false);

        let files = finder.gather_files();

        assert_eq!(files.len(), 1);
        assert_eq!(files[0].filename, "video.mp4");
    }

    #[test]
    fn respects_recursive_scanning_option() {
        let temp_directory = tempfile::TempDir::new().expect("should create temp directory");
        let scan_root = temp_directory.path().join("videos");
        let nested_directory = scan_root.join("nested");
        fs::create_dir_all(&nested_directory).expect("should create nested directory");
        fs::write(nested_directory.join("video.mp4"), b"video").expect("should write nested video file");

        let non_recursive = make_finder(scan_root.clone(), false).gather_files();
        let recursive = make_finder(scan_root, true).gather_files();

        assert!(non_recursive.is_empty());
        assert_eq!(recursive.len(), 1);
        assert_eq!(recursive[0].filename, "video.mp4");
    }

    #[test]
    fn overlapping_roots_collect_each_file_once() {
        let temp_directory = tempfile::TempDir::new().expect("should create temp directory");
        let scan_root = temp_directory.path().join("videos");
        let nested_directory = scan_root.join("nested");
        fs::create_dir_all(&nested_directory).expect("should create nested directory");
        fs::write(nested_directory.join("Only.Movie.mp4"), b"video").expect("should write video file");
        let mut finder = make_finder(scan_root.clone(), true);
        finder.roots = vec![scan_root, nested_directory.clone(), nested_directory];

        let files = finder.gather_files();

        assert_eq!(files.len(), 1, "{files:?}");
    }
}

#[cfg(test)]
mod test_move_duplicates {
    use super::*;
    use cli_tools_dupefind::DupeFileInfo;

    /// Finder rooted at the directory, always in dryrun mode so no test moves a file.
    fn dryrun_finder(root: PathBuf) -> DupeFind {
        DupeFind {
            roots: vec![root],
            config: Config {
                debug: false,
                dryrun: true,
                extensions: vec!["mp4".to_string()],
                hash_compare: false,
                hash_only: false,
                no_cache: true,
                ignore_matches: vec![],
                move_files: true,
                patterns: vec![],
                prefix_ignores: vec![],
                recurse: false,
                verbose: false,
            },
        }
    }

    /// Duplicate group of files created inside the directory.
    fn group_in(directory: &Path, names: &[&str]) -> DuplicateGroup {
        let files: Vec<DupeFileInfo> = names
            .iter()
            .map(|name| {
                let path = directory.join(name);
                std::fs::write(&path, b"video").expect("file should be written");
                DupeFileInfo::new(path, "mp4".to_string())
            })
            .collect();
        DuplicateGroup::new("movie".to_string(), files)
    }

    #[test]
    fn moving_keeps_the_best_file_and_moves_the_others() {
        let directory = tempfile::TempDir::new().expect("temporary directory");
        let group = group_in(directory.path(), &["movie.720p.mp4", "movie.1080p.mp4", "movie.mp4"]);
        let mut finder = dryrun_finder(directory.path().to_path_buf());
        finder.config.dryrun = false;

        finder
            .move_duplicates(&[group], &std::collections::HashMap::new())
            .unwrap();

        assert!(
            directory.path().join("movie.1080p.mp4").is_file(),
            "the best file should stay"
        );
        assert!(!directory.path().join("movie.720p.mp4").exists());
        assert!(!directory.path().join("movie.mp4").exists());
        let moved: usize = walkdir::WalkDir::new(directory.path().join("Duplicates"))
            .into_iter()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_file())
            .count();
        assert_eq!(moved, 2);
    }

    #[test]
    fn run_with_move_moves_without_the_interactive_view() {
        let directory = tempfile::TempDir::new().expect("temporary directory");
        let root = directory.path().join("videos");
        for subdirectory in ["first", "second"] {
            let path = root.join(subdirectory);
            std::fs::create_dir_all(&path).expect("should create subdirectory");
            std::fs::write(path.join("Movie.Name.mp4"), b"video").expect("should write video file");
        }
        let mut finder = dryrun_finder(root.clone());
        finder.config.dryrun = false;
        finder.config.recurse = true;

        finder.run().expect("the run should succeed");

        let remaining = [root.join("first/Movie.Name.mp4"), root.join("second/Movie.Name.mp4")]
            .iter()
            .filter(|path| path.is_file())
            .count();
        assert_eq!(remaining, 1, "one copy should stay in place");
        assert!(root.join("Duplicates").is_dir());
    }

    #[test]
    fn a_dryrun_moves_nothing_and_creates_no_directory() {
        let directory = tempfile::TempDir::new().expect("temporary directory");
        let group = group_in(directory.path(), &["movie.1080p.mp4", "movie.720p.mp4"]);

        dryrun_finder(directory.path().to_path_buf())
            .move_duplicates(&[group], &std::collections::HashMap::new())
            .unwrap();

        assert!(directory.path().join("movie.1080p.mp4").is_file());
        assert!(directory.path().join("movie.720p.mp4").is_file());
        assert!(
            !directory.path().join("Duplicates").exists(),
            "a dryrun should not create the duplicates directory"
        );
    }

    #[test]
    fn no_duplicate_groups_is_a_no_op() {
        let directory = tempfile::TempDir::new().expect("temporary directory");

        dryrun_finder(directory.path().to_path_buf())
            .move_duplicates(&[], &std::collections::HashMap::new())
            .unwrap();

        assert!(!directory.path().join("Duplicates").exists());
    }

    #[test]
    fn several_groups_are_all_reported() {
        let directory = tempfile::TempDir::new().expect("temporary directory");
        let first = group_in(directory.path(), &["one.1080p.mp4", "one.720p.mp4"]);
        let second = group_in(directory.path(), &["two.1080p.mp4", "two.720p.mp4"]);

        dryrun_finder(directory.path().to_path_buf())
            .move_duplicates(&[first, second], &std::collections::HashMap::new())
            .unwrap();

        assert_eq!(
            std::fs::read_dir(directory.path())
                .expect("directory should be readable")
                .count(),
            4,
            "every file should still be in place"
        );
    }

    #[test]
    fn without_a_root_the_working_directory_is_used() {
        let directory = tempfile::TempDir::new().expect("temporary directory");
        let group = group_in(directory.path(), &["movie.1080p.mp4"]);
        let finder = DupeFind {
            roots: Vec::new(),
            ..dryrun_finder(directory.path().to_path_buf())
        };

        finder
            .move_duplicates(&[group], &std::collections::HashMap::new())
            .unwrap();

        assert!(directory.path().join("movie.1080p.mp4").is_file());
    }
}

#[cfg(test)]
mod test_new {
    use super::*;
    use clap::Parser;

    #[test]
    fn a_given_path_becomes_the_only_root() {
        let directory = tempfile::TempDir::new().expect("temporary directory");
        let path = directory.path().to_string_lossy().into_owned();
        let args = Args::try_parse_from(["dupefind", &path]).expect("arguments should parse");

        let finder = DupeFind::new(args).expect("an existing path should be accepted");

        assert_eq!(finder.roots.len(), 1);
        assert!(finder.roots[0].ends_with(directory.path().file_name().expect("the directory should have a name")));
    }

    #[test]
    fn several_given_paths_all_become_roots() {
        let first = tempfile::TempDir::new().expect("temporary directory");
        let second = tempfile::TempDir::new().expect("temporary directory");
        let args = Args::try_parse_from([
            "dupefind",
            &first.path().to_string_lossy(),
            &second.path().to_string_lossy(),
        ])
        .expect("arguments should parse");

        let finder = DupeFind::new(args).expect("existing paths should be accepted");

        assert_eq!(finder.roots.len(), 2);
    }

    #[test]
    fn repeated_and_nested_paths_are_collapsed_when_recursive() {
        let directory = tempfile::TempDir::new().expect("temporary directory");
        let nested = directory.path().join("nested");
        std::fs::create_dir(&nested).expect("nested directory");
        let root = directory.path().to_string_lossy().into_owned();
        let nested = nested.to_string_lossy().into_owned();
        let args = Args::try_parse_from(["dupefind", "-r", &root, &nested, &root]).expect("arguments should parse");

        let finder = DupeFind::new(args).expect("existing paths should be accepted");

        assert_eq!(finder.roots.len(), 1, "{:?}", finder.roots);
    }

    #[test]
    fn nested_paths_are_kept_without_recursion() {
        let directory = tempfile::TempDir::new().expect("temporary directory");
        let nested = directory.path().join("nested");
        std::fs::create_dir(&nested).expect("nested directory");
        let root = directory.path().to_string_lossy().into_owned();
        let nested = nested.to_string_lossy().into_owned();
        let args = Args::try_parse_from(["dupefind", &root, &nested, &nested]).expect("arguments should parse");

        let finder = DupeFind::new(args).expect("existing paths should be accepted");

        assert_eq!(finder.roots.len(), 2, "{:?}", finder.roots);
    }

    #[test]
    fn a_missing_path_is_rejected() {
        let directory = tempfile::TempDir::new().expect("temporary directory");
        let missing = directory.path().join("missing");
        let args = Args::try_parse_from(["dupefind", &missing.to_string_lossy()]).expect("arguments should parse");

        assert!(DupeFind::new(args).is_err());
    }

    #[test]
    fn the_configured_paths_are_used_when_none_are_given() {
        let args = Args::try_parse_from(["dupefind"]).expect("arguments should parse");

        // The test config names paths that do not exist, so resolving them fails,
        // which is what proves the configured paths were the ones tried.
        let Err(error) = DupeFind::new(args) else {
            panic!("the configured paths do not exist, so this should fail");
        };

        assert!(
            error.to_string().contains("videos") || error.to_string().contains("movies"),
            "{error}"
        );
    }

    #[test]
    fn the_default_flag_uses_the_configured_default_paths() {
        let args = Args::try_parse_from(["dupefind", "--default"]).expect("arguments should parse");

        let Err(error) = DupeFind::new(args) else {
            panic!("the configured default path does not exist, so this should fail");
        };

        assert!(error.to_string().contains("default"), "{error}");
    }
}

#[cfg(test)]
mod test_run {
    use std::fs;

    use regex::Regex;

    use super::*;

    /// Dryrun finder over a recursive scan, so no test opens the interactive view or moves files.
    fn dryrun_finder(root: PathBuf) -> DupeFind {
        DupeFind {
            roots: vec![root],
            config: Config {
                debug: true,
                dryrun: true,
                extensions: vec!["mp4".to_string()],
                hash_compare: false,
                hash_only: false,
                no_cache: true,
                ignore_matches: Vec::new(),
                move_files: false,
                patterns: vec![Regex::new(r"ID\d+").expect("valid regex")],
                prefix_ignores: vec!["Studio".to_string()],
                recurse: true,
                verbose: true,
            },
        }
    }

    /// Scan root with the same file name in two subdirectories, which always forms a duplicate group.
    fn root_with_duplicate(directory: &tempfile::TempDir) -> PathBuf {
        let root = directory.path().join("videos");
        for subdirectory in ["first", "second"] {
            let path = root.join(subdirectory);
            fs::create_dir_all(&path).expect("should create subdirectory");
            fs::write(path.join("Movie.Name.mp4"), b"video").expect("should write video file");
        }
        root
    }

    #[test]
    fn reports_duplicates_without_changing_files() {
        let directory = tempfile::TempDir::new().expect("temporary directory");
        let root = root_with_duplicate(&directory);

        dryrun_finder(root.clone()).run().expect("a dryrun should succeed");

        assert!(root.join("first").join("Movie.Name.mp4").is_file());
        assert!(root.join("second").join("Movie.Name.mp4").is_file());
    }

    #[test]
    fn dryrun_move_leaves_files_in_place() {
        let directory = tempfile::TempDir::new().expect("temporary directory");
        let root = root_with_duplicate(&directory);
        let mut finder = dryrun_finder(root.clone());
        finder.config.move_files = true;

        finder.run().expect("a dryrun should succeed");

        assert!(!root.join("Duplicates").exists());
    }

    #[test]
    fn ignored_groups_are_dropped() {
        let directory = tempfile::TempDir::new().expect("temporary directory");
        let root = root_with_duplicate(&directory);
        let mut finder = dryrun_finder(root);
        finder.config.ignore_matches = vec!["movie".to_string()];

        finder.run().expect("ignoring every group should still succeed");
    }

    #[test]
    fn distinct_files_find_no_duplicates() {
        let directory = tempfile::TempDir::new().expect("temporary directory");
        let root = directory.path().join("videos");
        fs::create_dir(&root).expect("should create scan root");
        fs::write(root.join("First.Movie.mp4"), b"video").expect("should write video file");
        fs::write(root.join("Second.Film.mp4"), b"video").expect("should write video file");

        dryrun_finder(root).run().expect("no duplicates should succeed");
    }

    #[test]
    fn empty_root_finds_no_duplicates() {
        let directory = tempfile::TempDir::new().expect("temporary directory");

        dryrun_finder(directory.path().to_path_buf())
            .run()
            .expect("an empty directory should succeed");
    }
}

#[cfg(test)]
mod test_gather_nested_trees {
    use cli_tools_common::test_support::TempTree;

    use super::*;

    /// Finder over the given roots in dryrun mode.
    fn finder(roots: Vec<PathBuf>, recurse: bool) -> DupeFind {
        DupeFind {
            roots,
            config: Config {
                debug: false,
                dryrun: true,
                extensions: vec!["mp4".to_string(), "mkv".to_string()],
                hash_compare: false,
                hash_only: false,
                no_cache: true,
                ignore_matches: vec![],
                move_files: false,
                patterns: vec![],
                prefix_ignores: vec![],
                recurse,
                verbose: false,
            },
        }
    }

    /// Gathered paths relative to the tree root, sorted, with "/" separators.
    fn gathered(tree: &TempTree, roots: Vec<PathBuf>, recurse: bool) -> Vec<String> {
        let mut paths: Vec<String> = finder(roots, recurse)
            .gather_files()
            .iter()
            .map(|file| {
                file.path
                    .strip_prefix(tree.path())
                    .expect("gathered file should be inside the tree")
                    .components()
                    .map(|component| component.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("/")
            })
            .collect();
        paths.sort();
        paths
    }

    #[test]
    fn videos_at_every_depth_are_gathered_only_when_recursing() {
        let tree = TempTree::with_entries(&[
            "top.mp4",
            "a/one.mkv",
            "a/b/two.mp4",
            "a/b/c/three.mp4",
            "a/b/c/d/four.mp4",
            "a/b/c/readme.txt",
            "empty/nested/",
        ]);
        let root = vec![tree.path().to_path_buf()];

        assert_eq!(gathered(&tree, root.clone(), false), vec!["top.mp4"]);
        assert_eq!(
            gathered(&tree, root, true),
            vec![
                "a/b/c/d/four.mp4",
                "a/b/c/three.mp4",
                "a/b/two.mp4",
                "a/one.mkv",
                "top.mp4"
            ]
        );
    }

    #[test]
    fn hidden_and_system_entries_are_skipped_at_any_depth() {
        let tree = TempTree::with_entries(&[
            "movie.mp4",
            ".hidden.mp4",
            ".cache/inside.mp4",
            "a/b/.partial.mp4",
            "a/b/.thumbs/preview.mp4",
            "a/b/kept.mp4",
            "a/$RECYCLE.BIN/deleted.mp4",
            "a/b/c/_Serato_/track.mp4",
        ]);

        assert_eq!(
            gathered(&tree, vec![tree.path().to_path_buf()], true),
            vec!["a/b/kept.mp4", "movie.mp4"]
        );
    }

    #[test]
    fn symlinks_to_files_and_directories_are_not_followed() {
        let tree = TempTree::with_entries(&["library/a/b/movie.mp4"]);
        let links = [
            tree.try_symlink("library", "shortcut"),
            tree.try_symlink("library", "library/a/b/loop"),
            tree.try_symlink("library/a/b/movie.mp4", "linked.mp4"),
        ];
        if links.iter().any(Option::is_none) {
            return;
        }

        assert_eq!(
            gathered(&tree, vec![tree.path().to_path_buf()], true),
            vec!["library/a/b/movie.mp4"],
            "a link must not form a duplicate group with its own target"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_non_utf8_name_does_not_stop_the_walk() {
        let tree = TempTree::with_entries(&["a/valid.mp4", "a/b/valid.mp4"]);
        let created = tree.non_utf8_file("a/b", "broken", "mp4").is_some();

        let paths = gathered(&tree, vec![tree.path().to_path_buf()], true);

        assert!(paths.contains(&"a/b/valid.mp4".to_string()), "{paths:?}");
        assert!(paths.contains(&"a/valid.mp4".to_string()), "{paths:?}");
        assert_eq!(paths.len(), if created { 3 } else { 2 }, "{paths:?}");
    }

    #[test]
    fn nested_roots_without_recursion_scan_each_root_one_level_deep() {
        let tree = TempTree::with_entries(&["top.mp4", "a/middle.mp4", "a/b/deep.mp4"]);
        let roots = vec![tree.path().to_path_buf(), tree.join("a")];

        assert_eq!(gathered(&tree, roots, false), vec!["a/middle.mp4", "top.mp4"]);
    }

    #[test]
    fn sibling_roots_are_both_scanned_recursively() {
        let tree = TempTree::with_entries(&["first/x/one.mp4", "second/y/z/two.mp4", "outside.mp4"]);
        let roots = vec![tree.join("first"), tree.join("second")];

        assert_eq!(
            gathered(&tree, roots, true),
            vec!["first/x/one.mp4", "second/y/z/two.mp4"]
        );
    }
}

#[cfg(test)]
mod test_move_duplicates_in_trees {
    use cli_tools_common::test_support::TempTree;
    use cli_tools_dupefind::DupeFileInfo;

    use super::*;

    /// Finder rooted at the tree that moves files, recursing into subdirectories.
    fn moving_finder(tree: &TempTree, dryrun: bool) -> DupeFind {
        DupeFind {
            roots: vec![tree.path().to_path_buf()],
            config: Config {
                debug: false,
                dryrun,
                extensions: vec!["mp4".to_string(), "mkv".to_string()],
                hash_compare: false,
                hash_only: false,
                no_cache: true,
                ignore_matches: vec![],
                move_files: true,
                patterns: vec![],
                prefix_ignores: vec![],
                recurse: true,
                verbose: false,
            },
        }
    }

    /// Duplicate group of existing files at the given relative paths.
    fn group(tree: &TempTree, key: &str, relative_paths: &[&str]) -> DuplicateGroup {
        let files = relative_paths
            .iter()
            .map(|relative| {
                let path = tree.join(relative);
                let extension = cli_tools_common::path_to_file_extension_string(&path);
                DupeFileInfo::new(path, extension)
            })
            .collect();
        DuplicateGroup::new(key.to_string(), files)
    }

    /// Contents of every file in the tree, sorted, so a test can check that nothing was lost.
    fn contents(tree: &TempTree) -> Vec<String> {
        let mut contents: Vec<String> = tree.files().iter().map(|relative| tree.read(relative)).collect();
        contents.sort();
        contents
    }

    #[test]
    fn duplicates_with_the_same_name_from_different_directories_all_survive() {
        let tree = TempTree::new();
        tree.file_with("a/Movie.mp4", "first");
        tree.file_with("a/b/Movie.mp4", "second");
        tree.file_with("a/b/c/Movie.mp4", "third");
        tree.file_with("x/Movie.1080p.mp4", "best");
        let duplicates = group(
            &tree,
            "movie",
            &["a/Movie.mp4", "a/b/Movie.mp4", "a/b/c/Movie.mp4", "x/Movie.1080p.mp4"],
        );
        let before = contents(&tree);

        moving_finder(&tree, false)
            .move_duplicates(&[duplicates], &std::collections::HashMap::new())
            .unwrap();

        assert_eq!(contents(&tree), before, "no file contents may be lost");
        assert!(tree.exists("x/Movie.1080p.mp4"), "the best file stays in place");
        let moved = tree
            .files()
            .into_iter()
            .filter(|path| path.starts_with("Duplicates/"))
            .count();
        assert_eq!(moved, 3, "{:?}", tree.snapshot());
        assert!(!tree.exists("a/Movie.mp4"));
        assert!(!tree.exists("a/b/Movie.mp4"));
        assert!(!tree.exists("a/b/c/Movie.mp4"));
    }

    #[test]
    fn a_file_already_in_the_target_directory_is_not_overwritten() {
        let tree = TempTree::new();
        tree.file_with("a/Movie.mp4", "moving");
        tree.file_with("b/Movie.1080p.mp4", "best");
        let duplicates = group(&tree, "movie", &["a/Movie.mp4", "b/Movie.1080p.mp4"]);
        let target_directory = format!("Duplicates/{}", duplicates.display_name());
        tree.file_with(&format!("{target_directory}/Movie.mp4"), "already there");
        let before = contents(&tree);

        moving_finder(&tree, false)
            .move_duplicates(&[duplicates], &std::collections::HashMap::new())
            .unwrap();

        assert_eq!(contents(&tree), before, "no file contents may be lost");
        assert!(!tree.exists("a/Movie.mp4"));
        assert_eq!(tree.read(&format!("{target_directory}/Movie.mp4")), "already there");
    }

    #[test]
    fn copies_already_in_the_duplicates_directory_are_not_renamed() {
        let tree = TempTree::new();
        tree.file_with("a/Movie.1080p.mp4", "best");
        tree.file_with("b/Movie.mp4", "new copy");
        let target_directory = format!(
            "Duplicates/{}",
            group(&tree, "movie", &["a/Movie.1080p.mp4"]).display_name()
        );
        let moved_earlier = format!("{target_directory}/Movie.mp4");
        tree.file_with(&moved_earlier, "moved earlier");
        let duplicates = group(&tree, "movie", &["a/Movie.1080p.mp4", "b/Movie.mp4", &moved_earlier]);
        let before = contents(&tree);

        moving_finder(&tree, false)
            .move_duplicates(&[duplicates], &std::collections::HashMap::new())
            .unwrap();

        assert_eq!(contents(&tree), before, "no file contents may be lost");
        assert_eq!(tree.read(&moved_earlier), "moved earlier", "{:?}", tree.snapshot());
        assert!(tree.exists("a/Movie.1080p.mp4"));
        assert!(!tree.exists("b/Movie.mp4"));
    }

    #[test]
    fn a_dryrun_leaves_a_nested_tree_unchanged() {
        let tree = TempTree::new();
        tree.file_with("a/Movie.mp4", "first");
        tree.file_with("a/b/c/Movie.720p.mp4", "second");
        tree.file_with("d/Movie.1080p.mp4", "best");
        let duplicates = group(
            &tree,
            "movie",
            &["a/Movie.mp4", "a/b/c/Movie.720p.mp4", "d/Movie.1080p.mp4"],
        );
        let before = tree.snapshot();

        moving_finder(&tree, true)
            .move_duplicates(&[duplicates], &std::collections::HashMap::new())
            .unwrap();

        assert_eq!(tree.snapshot(), before);
    }

    #[test]
    fn a_full_run_on_a_nested_tree_moves_every_copy_but_one() {
        let tree = TempTree::new();
        tree.file_with("shows/season1/Some.Show.mp4", "one");
        tree.file_with("shows/season1/extra/Some.Show.mp4", "two");
        tree.file_with("archive/2020/old/Some.Show.mp4", "three");
        tree.file_with("shows/Other.Thing.mp4", "unique");
        let before = contents(&tree);

        moving_finder(&tree, false).run().expect("the run should succeed");

        assert_eq!(contents(&tree), before, "no file contents may be lost");
        let outside_duplicates = tree
            .files()
            .into_iter()
            .filter(|path| !path.starts_with("Duplicates/"))
            .count();
        assert_eq!(outside_duplicates, 2, "{:?}", tree.snapshot());
        assert!(tree.exists("shows/Other.Thing.mp4"));
    }

    #[test]
    fn a_second_run_keeps_every_file() {
        let tree = TempTree::new();
        tree.file_with("a/Some.Show.mp4", "one");
        tree.file_with("a/b/Some.Show.mp4", "two");
        tree.file_with("a/b/c/Some.Show.mp4", "three");
        let before = contents(&tree);

        moving_finder(&tree, false).run().expect("the first run should succeed");
        let kept_after_first_run: Vec<String> = tree
            .files()
            .into_iter()
            .filter(|path| !path.starts_with("Duplicates/"))
            .collect();
        moving_finder(&tree, false)
            .run()
            .expect("the second run should succeed");

        assert_eq!(contents(&tree), before, "no file contents may be lost");
        let kept_after_second_run: Vec<String> = tree
            .files()
            .into_iter()
            .filter(|path| !path.starts_with("Duplicates/"))
            .collect();
        assert_eq!(kept_after_first_run.len(), 1, "{:?}", tree.snapshot());
        assert_eq!(
            kept_after_second_run,
            kept_after_first_run,
            "the kept copy must stay in place: {:?}",
            tree.snapshot()
        );
    }

    #[test]
    fn a_second_run_keeps_library_copy_when_directory_sorts_before_duplicates() {
        let tree = TempTree::new();
        tree.file_with("Archive/Movie.1080p.mp4", "one");
        tree.file_with("Downloads/Movie.1080p.mp4", "two");
        let before = contents(&tree);

        moving_finder(&tree, false).run().expect("first run");
        let kept_after_first_run: Vec<String> = tree
            .files()
            .into_iter()
            .filter(|path| !path.starts_with("Duplicates/"))
            .collect();
        assert_eq!(kept_after_first_run.len(), 1, "{:?}", tree.snapshot());

        moving_finder(&tree, false).run().expect("second run");
        let kept_after_second_run: Vec<String> = tree
            .files()
            .into_iter()
            .filter(|path| !path.starts_with("Duplicates/"))
            .collect();

        assert_eq!(contents(&tree), before, "no file contents may be lost");
        assert_eq!(
            kept_after_second_run,
            kept_after_first_run,
            "kept library copy must not be evacuated into Duplicates: {:?}",
            tree.snapshot()
        );
    }

    #[test]
    fn copy_and_remove_fallback_moves_file_and_skips_existing_target() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let source = temp_dir.path().join("source.mp4");
        let target = temp_dir.path().join("target.mp4");
        std::fs::write(&source, b"video payload").expect("write source");

        let outcome = move_file_with_verification(&source, &target).expect("fallback move");
        assert_eq!(outcome, MoveResult::Renamed);
        assert!(!source.exists());
        assert_eq!(std::fs::read(&target).expect("read target"), b"video payload");

        let second_source = temp_dir.path().join("second.mp4");
        std::fs::write(&second_source, b"other").expect("write second");
        let skipped = move_file_with_verification(&second_source, &target).expect("skip existing");
        assert_eq!(skipped, MoveResult::SkippedExisting);
        assert!(second_source.exists());
        assert_eq!(std::fs::read(&target).expect("read target"), b"video payload");
    }

    #[test]
    fn group_named_dot_dot_stays_inside_duplicates_directory() {
        let tree = TempTree::new();
        tree.file_with("a/Studio....mp4", "one");
        tree.file_with("b/Studio....mp4", "two");

        let mut finder = moving_finder(&tree, false);
        finder.config.prefix_ignores = vec!["studio".to_string()];
        finder.run().expect("run");

        assert!(
            !tree.exists("Studio....mp4"),
            "duplicate must stay inside Duplicates/ and never escape into root: {:?}",
            tree.snapshot()
        );
        let inside_duplicates = tree
            .files()
            .into_iter()
            .filter(|path| path.starts_with("Duplicates/"))
            .count();
        assert_eq!(inside_duplicates, 1, "{:?}", tree.snapshot());
    }
}

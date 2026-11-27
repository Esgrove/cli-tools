//! Video file discovery, shared scan-cache lookup, parallel probing, and aggregate statistics collection.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use cli_tools_common::{print_error, print_yellow};
use cli_tools_media::file_hash::{FileFingerprint, fingerprint_file};
use cli_tools_media::scan_cache::{CachedVideoInfo, ScanCache};
use cli_tools_media::video_info::{VideoInfo, VideoStats};
use colored::Colorize;
use indicatif::{ProgressBar, ProgressStyle};
use rayon::prelude::*;

use crate::config::Config;

/// Progress bar template string.
const PROGRESS_BAR_TEMPLATE: &str = "[{elapsed_precise}] {bar:80.magenta/blue} {pos}/{len} {percent}%";

/// Progress bar fill characters.
const PROGRESS_BAR_CHARS: &str = "=>-";

/// A probed video file with its display name and metadata.
struct ProbedFile {
    /// Display name (relative path or filename).
    name: String,
    /// Video metadata from the scan cache or ffprobe.
    info: VideoInfo,
}

/// Result of probing one cache-miss file in parallel.
struct ProbeAttempt {
    path: PathBuf,
    name: String,
    fingerprint: Option<FileFingerprint>,
    result: Result<VideoInfo>,
}

/// Collects and displays statistics for video files.
pub struct StatsCollector {
    root: PathBuf,
    recurse: bool,
    no_cache: bool,
    verbose: bool,
}

impl StatsCollector {
    /// Create a new stats collector from the combined config.
    #[must_use]
    pub fn new(config: Config) -> Self {
        Self {
            root: config.root,
            recurse: config.recurse,
            no_cache: config.no_cache,
            verbose: config.verbose,
        }
    }

    /// Run the stats collection process.
    ///
    /// # Errors
    /// Returns an error if video files cannot be gathered or all probes fail.
    pub fn run(&self) -> Result<()> {
        let video_files = self.gather_video_files()?;

        if video_files.is_empty() {
            print_yellow!("No video files found in: {}", self.root.display());
            return Ok(());
        }

        if self.verbose {
            println!(
                "{}",
                format!(
                    "Found {}",
                    cli_tools_common::count_label(video_files.len(), "video file", "video files")
                )
                .green()
                .bold()
            );
        }

        let mut cache = if self.no_cache { None } else { ScanCache::open().ok() };
        let (mut probed_files, error_count) = probe_files_with_cache(video_files, &self.root, cache.as_mut());

        let mut stats = VideoStats::new();
        for probed in &probed_files {
            stats.add(&probed.info);
        }

        if should_list_files(self.verbose, probed_files.len()) {
            sort_for_listing(&mut probed_files);

            println!();
            for probed in &probed_files {
                Self::print_file_info(&probed.name, &probed.info);
            }
        }

        if error_count > 0 {
            println!(
                "{}",
                format!(
                    "{} could not be probed",
                    cli_tools_common::count_label(error_count, "file", "files")
                )
                .red()
            );
        }

        stats.print_summary(self.verbose);

        anyhow::ensure!(
            error_count == 0 || !probed_files.is_empty(),
            "No video files could be probed"
        );
        Ok(())
    }

    /// Print detailed info for a single video file.
    fn print_file_info(filename: &str, info: &VideoInfo) {
        let mut info_parts = Vec::new();

        if let Some(duration) = info.duration {
            info_parts.push(cli_tools_common::format_duration_seconds(duration));
        }
        if let Some(resolution) = info.resolution {
            info_parts.push(resolution.to_string());
        }
        if let Some(ref codec) = info.codec {
            info_parts.push(codec.clone());
        }
        if let Some(bitrate_kbps) = info.bitrate_kbps {
            info_parts.push(format!("{:.2} Mbps", bitrate_kbps as f64 / 1000.0));
        }
        if let Some(size_bytes) = info.size_bytes {
            info_parts.push(cli_tools_common::format_size(size_bytes));
        }

        if info_parts.is_empty() {
            println!("  {filename}");
        } else {
            println!("  {} | {}", filename.magenta(), info_parts.join(" | "));
        }
    }

    /// Gather all video files from the input path.
    fn gather_video_files(&self) -> Result<Vec<PathBuf>> {
        if self.root.is_dir() {
            if self.recurse {
                println!(
                    "{}",
                    format!("Searching recursively for video files in: {}", self.root.display()).magenta()
                );
            } else {
                println!(
                    "{}",
                    format!("Searching for video files in: {}", self.root.display()).magenta()
                );
            }
        }
        cli_tools_media::discover_video_files(&self.root, self.recurse)
    }
}

/// Whether individual file stats lines should be printed.
///
/// Aggregate summaries require at least two files, so a single probed video always prints its own line.
const fn should_list_files(verbose: bool, probed_count: usize) -> bool {
    verbose || probed_count == 1
}

/// Sort probed files by duration and then by pixel count, longest and largest first.
fn sort_for_listing(probed_files: &mut [ProbedFile]) {
    probed_files.sort_by(|a, b| {
        let duration_cmp = b
            .info
            .duration
            .unwrap_or(0.0)
            .total_cmp(&a.info.duration.unwrap_or(0.0));

        duration_cmp.then_with(|| {
            let pixels_b = b.info.resolution.map_or(0, |r| r.pixel_count());
            let pixels_a = a.info.resolution.map_or(0, |r| r.pixel_count());
            pixels_b.cmp(&pixels_a)
        })
    });
}

/// Probe video files in parallel using Rayon, reusing and populating the shared `ScanCache`.
fn probe_files_with_cache(files: Vec<PathBuf>, root: &Path, cache: Option<&mut ScanCache>) -> (Vec<ProbedFile>, usize) {
    let cached_entries: HashMap<String, CachedVideoInfo> = cache
        .as_ref()
        .and_then(|scan_cache| scan_cache.get_all().ok())
        .unwrap_or_default();

    let mut probed_files = Vec::with_capacity(files.len());
    let mut cache_misses: Vec<(PathBuf, String, Option<FileFingerprint>)> = Vec::new();

    for path in files {
        let name = cli_tools_common::get_relative_path_or_filename(&path, root);
        let fingerprint = fingerprint_file(&path).ok();
        if let (Some(path_str), Some(fingerprint)) = (path.to_str(), fingerprint)
            && let Some(cached) = cached_entries.get(path_str)
            && cached.is_current(&fingerprint)
        {
            probed_files.push(ProbedFile {
                name,
                info: cached.to_video_info(),
            });
        } else {
            cache_misses.push((path, name, fingerprint));
        }
    }

    if cache_misses.is_empty() {
        return (probed_files, 0);
    }

    let progress_bar = if cfg!(test) {
        ProgressBar::hidden()
    } else {
        let bar = ProgressBar::new(cache_misses.len() as u64);
        if let Ok(style) = ProgressStyle::default_bar().template(PROGRESS_BAR_TEMPLATE) {
            bar.set_style(style.progress_chars(PROGRESS_BAR_CHARS));
        }
        bar
    };

    let attempts: Vec<ProbeAttempt> = cache_misses
        .into_par_iter()
        .map(|(path, name, fingerprint)| {
            let result = VideoInfo::from_path(&path);
            progress_bar.inc(1);
            ProbeAttempt {
                path,
                name,
                fingerprint,
                result,
            }
        })
        .collect();
    progress_bar.finish_and_clear();

    let mut error_count: usize = 0;
    let mut newly_probed: Vec<(PathBuf, VideoInfo, FileFingerprint)> = Vec::new();

    for attempt in attempts {
        match attempt.result {
            Ok(info) => {
                if let Some(fingerprint) = attempt.fingerprint {
                    newly_probed.push((attempt.path, info.clone(), fingerprint));
                }
                probed_files.push(ProbedFile {
                    name: attempt.name,
                    info,
                });
            }
            Err(error) => {
                print_error!("Failed to probe {}: {error}", attempt.name);
                error_count += 1;
            }
        }
    }

    if let Some(scan_cache) = cache
        && !newly_probed.is_empty()
    {
        let batch: Vec<(&Path, &VideoInfo, FileFingerprint)> = newly_probed
            .iter()
            .map(|(path, info, fingerprint)| (path.as_path(), info, *fingerprint))
            .collect();
        let _ = scan_cache.batch_upsert_with_fingerprints(&batch);
    }

    (probed_files, error_count)
}

#[cfg(test)]
mod test_video_file_discovery {
    use super::*;

    fn collector(root: PathBuf, recurse: bool) -> StatsCollector {
        StatsCollector {
            root,
            recurse,
            no_cache: true,
            verbose: false,
        }
    }

    #[test]
    fn recognizes_supported_extensions_case_insensitively() {
        assert!(cli_tools_media::is_video_file(Path::new("video.mp4")));
        assert!(cli_tools_media::is_video_file(Path::new("video.MKV")));
        assert!(cli_tools_media::is_video_file(Path::new("video.webm")));
        assert!(cli_tools_media::is_video_file(Path::new("video.mpeg")));
        assert!(cli_tools_media::is_video_file(Path::new("video.ts")));
        assert!(!cli_tools_media::is_video_file(Path::new("notes.txt")));
        assert!(!cli_tools_media::is_video_file(Path::new("README")));
    }

    #[test]
    fn gathers_supported_single_file_and_rejects_unsupported_file() -> anyhow::Result<()> {
        let temp_directory = tempfile::TempDir::new()?;
        let video = temp_directory.path().join("video.mp4");
        let text = temp_directory.path().join("notes.txt");
        std::fs::write(&video, b"video")?;
        std::fs::write(&text, b"notes")?;

        assert_eq!(collector(video.clone(), false).gather_video_files()?, vec![video]);
        let error = collector(text, false)
            .gather_video_files()
            .expect_err("unsupported single file should fail");
        assert!(error.to_string().contains("not a supported video file"));
        Ok(())
    }

    #[test]
    fn non_recursive_scan_excludes_nested_videos_and_non_video_files() -> anyhow::Result<()> {
        let temp_directory = tempfile::TempDir::new()?;
        let nested = temp_directory.path().join("nested");
        std::fs::create_dir(&nested)?;
        let top_level = temp_directory.path().join("top.mp4");
        std::fs::write(&top_level, b"top")?;
        std::fs::write(temp_directory.path().join("notes.txt"), b"notes")?;
        std::fs::write(nested.join("nested.mkv"), b"nested")?;

        let files = collector(temp_directory.path().to_path_buf(), false).gather_video_files()?;

        assert_eq!(files, vec![top_level]);
        Ok(())
    }

    #[test]
    fn recursive_scan_includes_nested_videos_and_skips_hidden_directories() -> anyhow::Result<()> {
        let temp_directory = tempfile::TempDir::new()?;
        let scan_root = temp_directory.path().join("videos");
        let nested = scan_root.join("nested");
        let hidden = scan_root.join(".hidden");
        std::fs::create_dir_all(&nested)?;
        std::fs::create_dir_all(&hidden)?;
        let top_level = scan_root.join("top.mp4");
        let nested_video = nested.join("nested.mkv");
        std::fs::write(&top_level, b"top")?;
        std::fs::write(&nested_video, b"nested")?;
        std::fs::write(hidden.join("ignored.mp4"), b"hidden")?;

        let files = collector(scan_root, true).gather_video_files()?;

        assert_eq!(files.len(), 2);
        assert!(files.contains(&top_level));
        assert!(files.contains(&nested_video));
        Ok(())
    }

    #[test]
    fn missing_path_returns_contextual_error() {
        let temp_directory = tempfile::TempDir::new().expect("should create temp directory");
        let missing = temp_directory.path().join("missing");

        let error = collector(missing, false)
            .gather_video_files()
            .expect_err("missing path should fail");

        assert!(error.to_string().contains("does not exist"));
    }

    #[test]
    fn empty_directory_run_completes_without_probing() -> anyhow::Result<()> {
        let temp_directory = tempfile::TempDir::new()?;
        collector(temp_directory.path().to_path_buf(), false).run()
    }
}

#[cfg(test)]
mod test_stats_collector_new {
    use super::*;

    #[test]
    fn copies_config_values() {
        let root = PathBuf::from("videos");
        let collector = StatsCollector::new(Config {
            root: root.clone(),
            recurse: true,
            no_cache: true,
            verbose: true,
        });

        assert_eq!(collector.root, root);
        assert!(collector.recurse);
        assert!(collector.no_cache);
        assert!(collector.verbose);
    }
}

#[cfg(test)]
mod test_sort_for_listing {
    use cli_tools_media::Resolution;

    use super::*;

    fn probed(name: &str, duration: Option<f64>, resolution: Option<Resolution>) -> ProbedFile {
        ProbedFile {
            name: name.to_string(),
            info: VideoInfo {
                duration,
                resolution,
                ..Default::default()
            },
        }
    }

    #[test]
    fn longest_first_then_largest_resolution_first() {
        let mut files = vec![
            probed("short", Some(60.0), Some(Resolution::new(3840, 2160))),
            probed("long_small", Some(600.0), Some(Resolution::new(1280, 720))),
            probed("long_large", Some(600.0), Some(Resolution::new(1920, 1080))),
            probed("unknown", None, None),
        ];

        sort_for_listing(&mut files);

        let names: Vec<&str> = files.iter().map(|file| file.name.as_str()).collect();
        assert_eq!(names, vec!["long_large", "long_small", "short", "unknown"]);
    }

    #[test]
    fn missing_resolution_sorts_after_known_resolution_at_same_duration() {
        let mut files = vec![
            probed("no_resolution", Some(30.0), None),
            probed("with_resolution", Some(30.0), Some(Resolution::new(640, 480))),
        ];

        sort_for_listing(&mut files);

        assert_eq!(files[0].name, "with_resolution");
    }
}

#[cfg(test)]
mod test_run {
    use super::*;
    use cli_tools_media::Resolution;

    #[test]
    fn non_recursive_discovery_skips_hidden_files_and_symlinks() -> anyhow::Result<()> {
        let temp_directory = tempfile::TempDir::new()?;
        let visible = temp_directory.path().join("visible.mp4");
        let hidden = temp_directory.path().join(".hidden.mp4");
        std::fs::write(&visible, b"visible")?;
        std::fs::write(&hidden, b"hidden")?;
        // Without the symlink privilege on Windows, the hidden file is still checked.
        cli_tools_common::test_support::create_symlink(&visible, &temp_directory.path().join("link.mp4"));

        let found = StatsCollector::new(Config {
            root: temp_directory.path().to_path_buf(),
            recurse: false,
            no_cache: true,
            verbose: false,
        })
        .gather_video_files()?;
        assert_eq!(found, vec![visible]);
        Ok(())
    }

    #[test]
    fn single_file_prints_details_without_verbose() {
        assert!(!should_list_files(false, 0));
        assert!(should_list_files(false, 1));
        assert!(!should_list_files(false, 2));
        assert!(should_list_files(true, 2));
    }

    #[test]
    fn probing_reuses_valid_scan_cache_entries_without_running_ffprobe() -> anyhow::Result<()> {
        let temp_directory = tempfile::TempDir::new()?;
        let fake_video = temp_directory.path().join("cached.mp4");
        std::fs::write(&fake_video, b"not a real video so ffprobe would fail")?;
        let fingerprint = fingerprint_file(&fake_video)?;

        let mut cache = ScanCache::open_in_memory()?;
        let seeded = VideoInfo {
            size_bytes: Some(fingerprint.size_bytes),
            resolution: Some(Resolution::new(1920, 1080)),
            duration: Some(120.0),
            codec: Some("hevc".to_string()),
            bitrate_kbps: Some(5000),
            frames_per_second: Some(24.0),
            bit_depth: Some(10),
            ..Default::default()
        };
        cache.batch_upsert_with_fingerprints(&[(fake_video.as_path(), &seeded, fingerprint)])?;

        let (probed_files, error_count) =
            probe_files_with_cache(vec![fake_video], temp_directory.path(), Some(&mut cache));
        assert_eq!(error_count, 0);
        assert_eq!(probed_files.len(), 1);
        assert_eq!(probed_files[0].info.codec.as_deref(), Some("hevc"));
        Ok(())
    }
}

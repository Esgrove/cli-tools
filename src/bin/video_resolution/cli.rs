use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, LazyLock};

use anyhow::anyhow;
use indicatif::{ProgressBar, ProgressStyle};
use regex::Regex;
use tokio::process::Command;
use tokio::sync::{Semaphore, SemaphorePermit};
use walkdir::WalkDir;

use cli_tools::{print_bold, print_green};

use crate::config::Config;
use crate::resolution::{FFProbeResult, Resolution, print_fuzzy_resolution_ranges};

const PROGRESS_BAR_CHARS: &str = "=>-";
const PROGRESS_BAR_TEMPLATE: &str = "[{elapsed_precise}] {bar:80.magenta/blue} {pos}/{len} {percent}%";
const FILE_EXTENSIONS: [&str; 11] = [
    "mp4", "mkv", "wmv", "mov", "avi", "m4v", "flv", "webm", "webp", "ts", "mpg",
];

static RE_RESOLUTIONS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(480p|540p|544p|576p|600p|720p|1080p|1440p|2160p)")
        .expect("Failed to create regex pattern for valid resolutions")
});

static RE_HIGH_RESOLUTIONS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(720p|1080p|1440p|2160p)").expect("Failed to create regex pattern for high resolutions")
});

/// Matches full resolution patterns like `1920x1080` or `1080x1920` in filenames.
/// Used to detect files that have both a resolution label and a full resolution pattern (duplicates).
static RE_FULL_RESOLUTION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b\d{3,4}x\d{3,4}\b").expect("Failed to create regex pattern for full resolutions"));

/// Main entry point for the resolution CLI.
pub async fn run(config: Config) -> anyhow::Result<()> {
    if config.debug {
        print_fuzzy_resolution_ranges();
    }

    let files = gather_video_files(&config.path, config.recurse, config.delete_limit.is_some()).await?;

    if files.is_empty() {
        if config.verbose {
            println!("No video files to process");
        }
        return Ok(());
    }

    if config.verbose || config.debug {
        println!("Processing {} files...", files.len());
    }

    let mut results = get_resolutions(files).await?;

    // Delete low resolution files if requested
    if let Some(limit) = config.delete_limit {
        results = delete_low_resolution_files(results, limit, &config);
    }

    add_resolution_labels(&config, results);

    Ok(())
}

/// Rename video files to include their resolution label in the filename.
///
/// Files that already have the correct label are skipped.
/// Errors during renaming are printed and do not stop processing of remaining files.
fn add_resolution_labels(config: &Config, files: Vec<FFProbeResult>) {
    // Rename remaining files to add resolution labels
    let mut files_to_process: Vec<(FFProbeResult, PathBuf)> = files
        .into_iter()
        .filter_map(|result| match result.new_path_if_needed() {
            Ok(Some(new_path)) => Some((result, new_path)),
            Ok(None) => None,
            Err(err) => {
                eprintln!("Error: {err}");
                None
            }
        })
        .collect();

    files_to_process.sort_unstable_by(|a, b| {
        a.0.resolution
            .cmp(&b.0.resolution)
            .then_with(|| a.0.file.cmp(&b.0.file))
    });

    let num_files = files_to_process.len();
    if files_to_process.is_empty() {
        if config.verbose {
            println!("No video files require renaming");
        }
        return;
    } else if config.verbose {
        print_bold!("Renaming {num_files} file(s)");
    }

    print_bold!("Resolution               Label   Path");

    for (result, new_path) in files_to_process {
        if let Err(error) = result.rename(&new_path, config.overwrite, config.dryrun) {
            cli_tools::print_error!("{error}");
        }
    }

    print_green!("Renamed {num_files} file(s)");
}

/// Deletes files with resolution smaller than the given limit.
/// Returns the remaining files that were not deleted.
fn delete_low_resolution_files(results: Vec<FFProbeResult>, limit: u32, config: &Config) -> Vec<FFProbeResult> {
    let (to_delete, to_keep): (Vec<_>, Vec<_>) = results
        .into_iter()
        .partition(|result| result.resolution.is_smaller_than(limit));

    if !to_delete.is_empty() {
        let num_delete = to_delete.len();
        if config.dryrun {
            print_bold!("DRYRUN: Would delete {num_delete} file(s) smaller than {limit}:");
        } else if config.verbose {
            print_bold!("Deleting {num_delete} file(s) smaller than {limit}:");
        }

        print_bold!("Resolution            Path");
        for result in to_delete {
            if let Err(error) = result.delete(config.dryrun) {
                cli_tools::print_error!("{error}");
            }
        }

        if config.verbose {
            print_green!("Deleted {num_delete} files");
        }
    } else if config.verbose {
        println!("No files smaller than {limit}");
    }

    to_keep
}

/// Gathers video files for processing.
/// If `delete_mode` is true, excludes only files with high resolution labels (720p+).
/// Otherwise, excludes files with any resolution label.
async fn gather_video_files(path: &Path, recurse: bool, delete_mode: bool) -> anyhow::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let exclude_regex = if delete_mode {
        &*RE_HIGH_RESOLUTIONS
    } else {
        &*RE_RESOLUTIONS
    };

    if recurse {
        for entry in WalkDir::new(path)
            .into_iter()
            // ignore hidden files and system directories
            .filter_entry(|e| !cli_tools::should_skip_entry(e))
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_file())
        {
            let path = entry.path();
            if let Some(ext) = path.extension().and_then(|s| s.to_str())
                && FILE_EXTENSIONS.contains(&ext)
                && path
                    .file_name()
                    .and_then(|f| f.to_str())
                    .is_some_and(|filename| !exclude_regex.is_match(filename) || RE_FULL_RESOLUTION.is_match(filename))
            {
                files.push(path.to_path_buf());
            }
        }
    } else {
        let mut dir_entries = tokio::fs::read_dir(&path).await?;
        while let Some(ref entry) = dir_entries.next_entry().await? {
            let path = entry.path();
            if path.is_file()
                && !cli_tools::is_hidden_tokio(entry)
                && !cli_tools::is_system_directory_tokio(entry)
                && let Some(ext) = path.extension().and_then(|s| s.to_str())
                && FILE_EXTENSIONS.contains(&ext)
                && path
                    .file_name()
                    .and_then(|f| f.to_str())
                    .is_some_and(|filename| !exclude_regex.is_match(filename) || RE_FULL_RESOLUTION.is_match(filename))
            {
                files.push(path);
            }
        }
    }

    Ok(files)
}

/// Run ffprobe on all files concurrently and return successfully parsed results.
///
/// Errors from individual ffprobe calls are printed to stderr and the
/// corresponding files are excluded from the returned results.
async fn get_resolutions(files: Vec<PathBuf>) -> anyhow::Result<Vec<FFProbeResult>> {
    let semaphore = create_semaphore_for_io_bound();

    let progress_bar = Arc::new(ProgressBar::new(files.len() as u64));
    progress_bar.set_style(
        ProgressStyle::default_bar()
            .template(PROGRESS_BAR_TEMPLATE)?
            .progress_chars(PROGRESS_BAR_CHARS),
    );

    let tasks: Vec<_> = files
        .into_iter()
        .map(|path| {
            let sem = Arc::clone(&semaphore);
            let progress = Arc::clone(&progress_bar);
            tokio::spawn(async move {
                let permit: SemaphorePermit = sem.acquire().await.expect("Failed to acquire semaphore");
                let result = run_ffprobe(path).await;
                drop(permit);
                progress.inc(1);
                result
            })
        })
        .collect();

    let mut results: Vec<FFProbeResult> = futures::future::join_all(tasks)
        .await
        .into_iter()
        .map(|res| res.expect("Download future failed"))
        .filter_map(|res| match res {
            Ok(val) => Some(val),
            Err(err) => {
                eprintln!("Error: {err}");
                None
            }
        })
        .collect();

    progress_bar.finish_and_clear();

    results.sort_unstable_by(|a, b| a.resolution.cmp(&b.resolution).then_with(|| a.file.cmp(&b.file)));

    Ok(results)
}

/// Run ffprobe on a single file and parse the output into an `FFProbeResult`.
async fn run_ffprobe(file: PathBuf) -> anyhow::Result<FFProbeResult> {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v",
            "-show_entries",
            "stream=width,height",
            "-output_format",
            "default=nokey=0:noprint_wrappers=1",
        ])
        .arg(&file)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await;

    let path = file.display();
    match output {
        Ok(output) => {
            if output.status.success() {
                let resolution = parse_ffprobe_output(&output.stdout)
                    .map_err(|error| anyhow!("Failed to parse output for {path}: {error}"))?;
                Ok(FFProbeResult { file, resolution })
            } else {
                Err(anyhow!("{path}: {}", std::str::from_utf8(&output.stderr)?))
            }
        }
        Err(e) => Err(anyhow!("Command failed for {path}: {e}")),
    }
}

/// Parse ffprobe output in format "width=1920\nheight=1080\n"
#[inline]
pub fn parse_ffprobe_output(output: &[u8]) -> anyhow::Result<Resolution> {
    let mut lines = output.split(|&b| b == b'\n');

    let width = lines
        .next()
        .and_then(|line| line.get(6..)) // Skip "width="
        .map(|w| w.strip_suffix(b"\r").unwrap_or(w)) // Handle Windows CRLF
        .ok_or_else(|| anyhow!("Missing width"))?;

    let height = lines
        .next()
        .and_then(|line| line.get(7..)) // Skip "height="
        .map(|h| h.strip_suffix(b"\r").unwrap_or(h)) // Handle Windows CRLF
        .ok_or_else(|| anyhow!("Missing height"))?;

    // SAFETY: ffprobe output is always valid ASCII digits
    #[allow(unsafe_code)]
    let width_str = unsafe { std::str::from_utf8_unchecked(width) };
    #[allow(unsafe_code)]
    let height_str = unsafe { std::str::from_utf8_unchecked(height) };

    let width = width_str
        .parse()
        .map_err(|e| anyhow!("Failed to parse width '{width_str}': {e}"))?;

    let height = height_str
        .parse()
        .map_err(|e| anyhow!("Failed to parse height '{height_str}': {e}"))?;

    Ok(Resolution::new(width, height))
}

/// Create a Semaphore for I/O-bound work.
#[inline]
fn create_semaphore_for_io_bound() -> Arc<Semaphore> {
    Arc::new(Semaphore::new(num_cpus::get_physical() * 2))
}

#[cfg(test)]
mod regex_tests {
    use super::*;

    #[test]
    fn regex_full_resolution_matches_standard_patterns() {
        assert!(RE_FULL_RESOLUTION.is_match("video.1920x1080.mp4"));
        assert!(RE_FULL_RESOLUTION.is_match("video.1080x1920.mp4"));
        assert!(RE_FULL_RESOLUTION.is_match("video.960x540.mp4"));
        assert!(RE_FULL_RESOLUTION.is_match("video.3840x2160.mp4"));
        assert!(RE_FULL_RESOLUTION.is_match("video.720x480.mp4"));
    }

    #[test]
    fn regex_full_resolution_no_match_label_only() {
        assert!(!RE_FULL_RESOLUTION.is_match("video.1080p.mp4"));
        assert!(!RE_FULL_RESOLUTION.is_match("video.720p.mp4"));
        assert!(!RE_FULL_RESOLUTION.is_match("video.mp4"));
    }

    #[test]
    fn regex_full_resolution_no_match_small_numbers() {
        assert!(!RE_FULL_RESOLUTION.is_match("video.3x4.mp4"));
        assert!(!RE_FULL_RESOLUTION.is_match("video.12x34.mp4"));
    }

    #[test]
    fn regex_full_resolution_word_boundaries() {
        // \b treats digits, letters, and underscore as word characters.
        // The regex \b\d{3,4}x\d{3,4}\b requires a word boundary before the first digit
        // and after the last digit, preventing matches inside larger word-char sequences.
        // Dot-separated patterns match because dots are non-word characters.
        assert!(RE_FULL_RESOLUTION.is_match("video.1920x1080.mp4"));
        assert!(RE_FULL_RESOLUTION.is_match("video.960x540.540p.mp4"));
        // 5-digit sequences are rejected since resolutions never exceed 4 digits
        assert!(!RE_FULL_RESOLUTION.is_match("video.21920x1080.mp4"));
        assert!(!RE_FULL_RESOLUTION.is_match("video.1920x10800.mp4"));
    }

    #[test]
    fn regex_full_resolution_matches_with_label_present() {
        // Files with both full resolution and label should match
        assert!(RE_FULL_RESOLUTION.is_match("video.960x540.540p.mp4"));
        assert!(RE_FULL_RESOLUTION.is_match("video.1920x1080.1080p.mp4"));
        assert!(RE_FULL_RESOLUTION.is_match("video.1080x1920.Vertical.1080p.mp4"));
    }

    #[test]
    fn regex_resolutions_matches_standard() {
        assert!(RE_RESOLUTIONS.is_match("video.480p.mp4"));
        assert!(RE_RESOLUTIONS.is_match("video.540p.mp4"));
        assert!(RE_RESOLUTIONS.is_match("video.576p.mp4"));
        assert!(RE_RESOLUTIONS.is_match("video.720p.mp4"));
        assert!(RE_RESOLUTIONS.is_match("video.1080p.mp4"));
        assert!(RE_RESOLUTIONS.is_match("video.1440p.mp4"));
        assert!(RE_RESOLUTIONS.is_match("video.2160p.mp4"));
    }

    #[test]
    fn regex_resolutions_case_insensitive() {
        assert!(RE_RESOLUTIONS.is_match("video.1080P.mp4"));
        assert!(RE_RESOLUTIONS.is_match("video.720P.mp4"));
    }

    #[test]
    fn regex_resolutions_no_match() {
        assert!(!RE_RESOLUTIONS.is_match("video.mp4"));
        assert!(!RE_RESOLUTIONS.is_match("video.360p.mp4"));
        assert!(!RE_RESOLUTIONS.is_match("video.4k.mp4"));
        assert!(!RE_RESOLUTIONS.is_match("video.1080.mp4"));
    }

    #[test]
    fn regex_high_resolutions_matches() {
        assert!(RE_HIGH_RESOLUTIONS.is_match("video.720p.mp4"));
        assert!(RE_HIGH_RESOLUTIONS.is_match("video.1080p.mp4"));
        assert!(RE_HIGH_RESOLUTIONS.is_match("video.1440p.mp4"));
        assert!(RE_HIGH_RESOLUTIONS.is_match("video.2160p.mp4"));
    }

    #[test]
    fn regex_high_resolutions_no_match_low_res() {
        assert!(!RE_HIGH_RESOLUTIONS.is_match("video.480p.mp4"));
        assert!(!RE_HIGH_RESOLUTIONS.is_match("video.540p.mp4"));
        assert!(!RE_HIGH_RESOLUTIONS.is_match("video.576p.mp4"));
        assert!(!RE_HIGH_RESOLUTIONS.is_match("video.600p.mp4"));
    }

    #[test]
    fn regex_resolutions_matches_600p() {
        assert!(RE_RESOLUTIONS.is_match("video.600p.mp4"));
    }

    #[test]
    fn regex_resolutions_at_start_of_filename() {
        assert!(RE_RESOLUTIONS.is_match("1080p.video.mp4"));
        assert!(RE_HIGH_RESOLUTIONS.is_match("720p.video.mp4"));
    }

    #[test]
    fn regex_resolutions_at_end_of_filename() {
        assert!(RE_RESOLUTIONS.is_match("video.1080p"));
        assert!(RE_HIGH_RESOLUTIONS.is_match("video.2160p"));
    }

    #[test]
    fn regex_resolutions_in_middle_of_filename() {
        assert!(RE_RESOLUTIONS.is_match("my.video.1080p.2024.mp4"));
        assert!(RE_HIGH_RESOLUTIONS.is_match("movie.720p.bluray.mp4"));
    }

    #[test]
    fn regex_resolutions_mixed_case() {
        assert!(RE_RESOLUTIONS.is_match("video.1080P.mp4"));
        assert!(RE_RESOLUTIONS.is_match("video.720P.mp4"));
        assert!(RE_HIGH_RESOLUTIONS.is_match("video.1440P.mp4"));
    }

    #[test]
    fn regex_resolutions_no_match_8k() {
        assert!(!RE_RESOLUTIONS.is_match("video.4320p.mp4"));
        assert!(!RE_RESOLUTIONS.is_match("video.8k.mp4"));
    }

    #[test]
    fn regex_resolutions_no_match_partial() {
        // Should not match partial resolution strings
        assert!(!RE_RESOLUTIONS.is_match("video.108p.mp4"));
        assert!(!RE_RESOLUTIONS.is_match("video.72p.mp4"));
    }

    #[test]
    fn regex_resolutions_no_match_without_p_suffix() {
        assert!(!RE_RESOLUTIONS.is_match("video.1080.mp4"));
        assert!(!RE_RESOLUTIONS.is_match("video.720.mp4"));
        assert!(!RE_RESOLUTIONS.is_match("video.1920x1080.mp4"));
    }

    #[test]
    fn regex_high_resolutions_no_match_544p() {
        assert!(!RE_HIGH_RESOLUTIONS.is_match("video.544p.mp4"));
    }

    #[test]
    fn regex_resolutions_matches_544p() {
        assert!(RE_RESOLUTIONS.is_match("video.544p.mp4"));
    }

    #[test]
    fn regex_high_resolutions_case_insensitive() {
        assert!(RE_HIGH_RESOLUTIONS.is_match("video.1080P.mp4"));
        assert!(RE_HIGH_RESOLUTIONS.is_match("video.2160P.mp4"));
    }

    #[test]
    fn regex_resolutions_embedded_in_word_matches() {
        // Regex will match even if embedded - this is expected behavior
        assert!(RE_RESOLUTIONS.is_match("video1080ptest.mp4"));
    }

    #[test]
    fn regex_high_resolutions_empty_string() {
        assert!(!RE_HIGH_RESOLUTIONS.is_match(""));
        assert!(!RE_RESOLUTIONS.is_match(""));
    }

    #[test]
    fn regex_resolutions_only_resolution() {
        assert!(RE_RESOLUTIONS.is_match("1080p"));
        assert!(RE_RESOLUTIONS.is_match("720p"));
        assert!(RE_HIGH_RESOLUTIONS.is_match("2160p"));
    }
}

#[cfg(test)]
mod ffpprobe_tests {
    use super::*;

    #[test]
    fn parse_ffprobe_output_windows_crlf() {
        let output = b"width=1920\r\nheight=1080\r\n";
        let result = parse_ffprobe_output(output).unwrap();
        assert_eq!(result.width, 1920);
        assert_eq!(result.height, 1080);
    }

    #[test]
    fn parse_ffprobe_output_large_resolution() {
        let output = b"width=7680\nheight=4320\n";
        let result = parse_ffprobe_output(output).unwrap();
        assert_eq!(result.width, 7680);
        assert_eq!(result.height, 4320);
    }

    #[test]
    fn parse_ffprobe_output_missing_width() {
        let output = b"height=1080\n";
        let result = parse_ffprobe_output(output);
        assert!(result.is_err());
    }

    #[test]
    fn parse_ffprobe_output_missing_height() {
        let output = b"width=1920\n";
        let result = parse_ffprobe_output(output);
        assert!(result.is_err());
    }

    #[test]
    fn parse_ffprobe_output_empty() {
        let output = b"";
        let result = parse_ffprobe_output(output);
        assert!(result.is_err());
    }

    #[test]
    fn parse_ffprobe_output_malformed_width() {
        let output = b"width=abc\nheight=1080\n";
        let result = parse_ffprobe_output(output);
        assert!(result.is_err());
    }

    #[test]
    fn parse_ffprobe_output_malformed_height() {
        let output = b"width=1920\nheight=xyz\n";
        let result = parse_ffprobe_output(output);
        assert!(result.is_err());
    }

    #[test]
    fn parse_ffprobe_output_1080p() {
        let output = b"width=1920\nheight=1080\n";
        let result = parse_ffprobe_output(output).unwrap();
        assert_eq!(result.width, 1920);
        assert_eq!(result.height, 1080);
    }

    #[test]
    fn parse_ffprobe_output_720p() {
        let output = b"width=1280\nheight=720\n";
        let result = parse_ffprobe_output(output).unwrap();
        assert_eq!(result.width, 1280);
        assert_eq!(result.height, 720);
    }

    #[test]
    fn parse_ffprobe_output_4k() {
        let output = b"width=3840\nheight=2160\n";
        let result = parse_ffprobe_output(output).unwrap();
        assert_eq!(result.width, 3840);
        assert_eq!(result.height, 2160);
    }

    #[test]
    fn parse_ffprobe_output_vertical() {
        let output = b"width=1080\nheight=1920\n";
        let result = parse_ffprobe_output(output).unwrap();
        assert_eq!(result.width, 1080);
        assert_eq!(result.height, 1920);
    }

    #[test]
    fn parse_ffprobe_output_no_trailing_newline() {
        let output = b"width=1920\nheight=1080";
        let result = parse_ffprobe_output(output).unwrap();
        assert_eq!(result.width, 1920);
        assert_eq!(result.height, 1080);
    }

    #[test]
    fn parse_ffprobe_output_zero_dimensions() {
        let output = b"width=0\nheight=0\n";
        let result = parse_ffprobe_output(output).unwrap();
        assert_eq!(result.width, 0);
        assert_eq!(result.height, 0);
    }

    #[test]
    fn parse_ffprobe_output_square() {
        let output = b"width=1080\nheight=1080\n";
        let result = parse_ffprobe_output(output).unwrap();
        assert_eq!(result.width, 1080);
        assert_eq!(result.height, 1080);
    }

    #[test]
    fn parse_ffprobe_output_single_digit() {
        let output = b"width=1\nheight=1\n";
        let result = parse_ffprobe_output(output).unwrap();
        assert_eq!(result.width, 1);
        assert_eq!(result.height, 1);
    }

    #[test]
    fn parse_ffprobe_output_480p() {
        let output = b"width=640\nheight=480\n";
        let result = parse_ffprobe_output(output).unwrap();
        assert_eq!(result.width, 640);
        assert_eq!(result.height, 480);
    }

    #[test]
    fn parse_ffprobe_output_1440p() {
        let output = b"width=2560\nheight=1440\n";
        let result = parse_ffprobe_output(output).unwrap();
        assert_eq!(result.width, 2560);
        assert_eq!(result.height, 1440);
    }

    #[test]
    fn parse_ffprobe_output_ultrawide() {
        let output = b"width=3440\nheight=1440\n";
        let result = parse_ffprobe_output(output).unwrap();
        assert_eq!(result.width, 3440);
        assert_eq!(result.height, 1440);
    }

    #[test]
    fn parse_ffprobe_output_very_small() {
        let output = b"width=64\nheight=64\n";
        let result = parse_ffprobe_output(output).unwrap();
        assert_eq!(result.width, 64);
        assert_eq!(result.height, 64);
    }

    #[test]
    fn parse_ffprobe_output_mixed_crlf_lf() {
        // First line with CRLF, second with LF
        let output = b"width=1920\r\nheight=1080\n";
        let result = parse_ffprobe_output(output).unwrap();
        assert_eq!(result.width, 1920);
        assert_eq!(result.height, 1080);
    }

    #[test]
    fn parse_ffprobe_output_negative_width() {
        let output = b"width=-1920\nheight=1080\n";
        let result = parse_ffprobe_output(output);
        assert!(result.is_err());
    }

    #[test]
    fn parse_ffprobe_output_negative_height() {
        let output = b"width=1920\nheight=-1080\n";
        let result = parse_ffprobe_output(output);
        assert!(result.is_err());
    }

    #[test]
    fn parse_ffprobe_output_float_width() {
        let output = b"width=1920.5\nheight=1080\n";
        let result = parse_ffprobe_output(output);
        assert!(result.is_err());
    }

    #[test]
    fn parse_ffprobe_output_empty_width_value() {
        let output = b"width=\nheight=1080\n";
        let result = parse_ffprobe_output(output);
        assert!(result.is_err());
    }

    #[test]
    fn parse_ffprobe_output_empty_height_value() {
        let output = b"width=1920\nheight=\n";
        let result = parse_ffprobe_output(output);
        assert!(result.is_err());
    }

    #[test]
    fn parse_ffprobe_output_whitespace_in_width() {
        let output = b"width= 1920\nheight=1080\n";
        let result = parse_ffprobe_output(output);
        assert!(result.is_err());
    }

    #[test]
    fn parse_ffprobe_output_only_newlines() {
        let output = b"\n\n\n";
        let result = parse_ffprobe_output(output);
        assert!(result.is_err());
    }

    #[test]
    fn parse_ffprobe_output_swapped_order() {
        // ffprobe always outputs width first, but test what happens if swapped
        let output = b"height=1080\nwidth=1920\n";
        let result = parse_ffprobe_output(output);
        // This should fail because we expect width= first
        assert!(result.is_err());
    }

    #[test]
    fn parse_ffprobe_output_extra_fields_after() {
        let output = b"width=1920\nheight=1080\ncodec=h264\n";
        let result = parse_ffprobe_output(output).unwrap();
        assert_eq!(result.width, 1920);
        assert_eq!(result.height, 1080);
    }

    #[test]
    fn parse_ffprobe_output_large_values() {
        let output = b"width=15360\nheight=8640\n";
        let result = parse_ffprobe_output(output).unwrap();
        assert_eq!(result.width, 15360);
        assert_eq!(result.height, 8640);
    }

    #[test]
    fn parse_ffprobe_output_sd_resolution() {
        let output = b"width=720\nheight=576\n";
        let result = parse_ffprobe_output(output).unwrap();
        assert_eq!(result.width, 720);
        assert_eq!(result.height, 576);
    }
}

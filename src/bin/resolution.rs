use std::borrow::Cow;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, LazyLock};

use anyhow::{Error, anyhow};
use clap::Parser;
use colored::Colorize;
use indicatif::{ProgressBar, ProgressStyle};
use regex::Regex;
use tokio::process::Command;
use tokio::sync::{Semaphore, SemaphorePermit};
use walkdir::WalkDir;

use cli_tools::{print_bold, print_green};

const FILE_EXTENSIONS: [&str; 11] = [
    "mp4", "mkv", "wmv", "mov", "avi", "m4v", "flv", "webm", "webp", "ts", "mpg",
];
const PROGRESS_BAR_CHARS: &str = "=>-";
const PROGRESS_BAR_TEMPLATE: &str = "[{elapsed_precise}] {bar:80.magenta/blue} {pos}/{len} {percent}%";
const RESOLUTION_TOLERANCE: f32 = 0.025;
const KNOWN_RESOLUTIONS: &[(u32, u32)] = &[
    (640, 480),
    (720, 480),
    (720, 540),
    (720, 544),
    (720, 576),
    (800, 600),
    (1280, 720),
    (1920, 1080),
    (2560, 1440),
    (3840, 2160),
];
const FUZZY_RESOLUTIONS: [ResolutionMatch; KNOWN_RESOLUTIONS.len()] = precalculate_fuzzy_resolutions();

static RE_RESOLUTIONS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(480p|540p|544p|576p|600p|720p|1080p|1440p|2160p)")
        .expect("Failed to create regex pattern for valid resolutions")
});

static RE_HIGH_RESOLUTIONS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(720p|1080p|1440p|2160p)").expect("Failed to create regex pattern for high resolutions")
});

#[derive(Parser, Debug)]
#[command(author, version, name = env!("CARGO_BIN_NAME"), about = "Add video resolution to filenames")]
struct Args {
    /// Optional input directory or file path
    #[arg(value_hint = clap::ValueHint::AnyPath)]
    path: Option<PathBuf>,

    /// Enable debug prints
    #[arg(short = 'D', long)]
    debug: bool,

    /// Delete files with width or height smaller than limit (default: 500)
    #[arg(short = 'x', long)]
    #[allow(clippy::option_option)]
    delete: Option<Option<u32>>,

    /// Overwrite existing files
    #[arg(short, long)]
    force: bool,

    /// Only print file names without renaming or deleting
    #[arg(short, long)]
    print: bool,

    /// Recurse into subdirectories
    #[arg(short, long)]
    recurse: bool,

    /// Print verbose output
    #[arg(short, long)]
    verbose: bool,
}

#[derive(Debug, Ord, PartialOrd, Eq, PartialEq)]
struct Resolution {
    width: u32,
    height: u32,
}

#[derive(Copy, Clone, Debug)]
struct ResolutionMatch {
    label_height: u32,
    width_range: (u32, u32),
    height_range: (u32, u32),
}

#[derive(Debug, Ord, PartialOrd, Eq, PartialEq)]
struct FFProbeResult {
    file: PathBuf,
    resolution: Resolution,
}

impl fmt::Display for Resolution {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.width < self.height {
            write!(f, "Vertical.{}x{}", self.width, self.height)
        } else {
            write!(f, "{}x{}", self.width, self.height)
        }
    }
}

impl fmt::Display for ResolutionMatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}p: width {:?}, height {:?}",
            self.label_height, self.width_range, self.height_range
        )
    }
}

impl FFProbeResult {
    fn delete(&self, dryrun: bool) -> anyhow::Result<()> {
        let path_str = cli_tools::path_to_string_relative(&self.file);
        println!(
            "{:>4}x{:<4}   {}",
            self.resolution.width,
            self.resolution.height,
            path_str.red()
        );
        if !dryrun {
            std::fs::remove_file(&self.file)?;
        }
        Ok(())
    }

    fn rename(&self, new_path: &Path, overwrite: bool, dryrun: bool) -> anyhow::Result<()> {
        self.print_rename(new_path);
        if new_path.exists() && !overwrite {
            anyhow::bail!("File already exists: {}", cli_tools::path_to_string(new_path));
        }
        if !dryrun {
            std::fs::rename(&self.file, new_path)?;
        }
        Ok(())
    }

    /// Returns `Some(new_path)` if file needs renaming, `None` if already up-to-date.
    fn new_path_if_needed(&self) -> anyhow::Result<Option<PathBuf>> {
        let label = self.resolution.label();
        let (mut name, extension) = cli_tools::get_normalized_file_name_and_extension(&self.file)?;
        if name.contains(&*label) {
            Ok(None)
        } else {
            let full_resolution = self.resolution.to_string();
            if name.contains(&full_resolution) {
                name = name.replace(&full_resolution, "");
            }
            let new_file_name = format!("{name}.{label}.{extension}").replace("..", ".");
            let new_path = self.file.with_file_name(&new_file_name);
            Ok(Some(new_path))
        }
    }

    fn print_rename(&self, new_path: &Path) {
        let (_, new_path_colored) = cli_tools::color_diff(
            &cli_tools::path_to_string_relative(&self.file),
            &cli_tools::path_to_string_relative(new_path),
            false,
        );
        println!(
            "{:>4}x{:<4}   {:>18}   {}",
            self.resolution.width,
            self.resolution.height,
            self.resolution.label(),
            new_path_colored
        );
    }
}

impl Resolution {
    /// Returns true if width or height is smaller than the given limit.
    const fn is_smaller_than(&self, limit: u32) -> bool {
        self.width < limit || self.height < limit
    }

    fn label(&self) -> Cow<'static, str> {
        if self.width < self.height {
            // Vertical video
            match (self.width, self.height) {
                (480, 640 | 720) => Cow::Borrowed("Vertical.480p"),
                (540, 720) => Cow::Borrowed("Vertical.540p"),
                (544, 720) => Cow::Borrowed("Vertical.544p"),
                (576, 720) => Cow::Borrowed("Vertical.576p"),
                (600, 800) => Cow::Borrowed("Vertical.600p"),
                (720, 1280) => Cow::Borrowed("Vertical.720p"),
                (1080, 1920) => Cow::Borrowed("Vertical.1080p"),
                (1440, 2560) => Cow::Borrowed("Vertical.1440p"),
                (2160, 3840) => Cow::Borrowed("Vertical.2160p"),
                _ => self.label_fuzzy_vertical(),
            }
        } else {
            // Horizontal video
            match (self.width, self.height) {
                (640 | 720, 480) => Cow::Borrowed("480p"),
                (720, 540) => Cow::Borrowed("540p"),
                (720, 544) => Cow::Borrowed("544p"),
                (720, 576) => Cow::Borrowed("576p"),
                (800, 600) => Cow::Borrowed("600p"),
                (1280, 720) => Cow::Borrowed("720p"),
                (1920, 1080) => Cow::Borrowed("1080p"),
                (2560, 1440) => Cow::Borrowed("1440p"),
                (3840, 2160) => Cow::Borrowed("2160p"),
                _ => self.label_fuzzy_horizontal(),
            }
        }
    }

    fn label_fuzzy_vertical(&self) -> Cow<'static, str> {
        for res in &FUZZY_RESOLUTIONS {
            if self.height >= res.width_range.0
                && self.height <= res.width_range.1
                && self.width >= res.height_range.0
                && self.width <= res.height_range.1
            {
                return Cow::Owned(format!("Vertical.{}p", res.label_height));
            }
        }
        // fall back to full resolution label
        Cow::Owned(self.to_string())
    }

    fn label_fuzzy_horizontal(&self) -> Cow<'static, str> {
        for res in &FUZZY_RESOLUTIONS {
            if self.width >= res.width_range.0
                && self.width <= res.width_range.1
                && self.height >= res.height_range.0
                && self.height <= res.height_range.1
            {
                return Cow::Owned(format!("{}p", res.label_height));
            }
        }
        // fall back to full resolution label
        Cow::Owned(self.to_string())
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let absolute_input_path = cli_tools::resolve_input_path(args.path.as_deref())?;

    if args.debug {
        println!("Fuzzy resolution ranges:");
        for res in &FUZZY_RESOLUTIONS {
            println!("  {res}");
        }
    }

    if let Some(limit) = args.delete {
        if args.verbose || args.debug {
            println!("Deleting low resolution files...");
        }
        return delete_low_resolution_files(
            &absolute_input_path,
            args.recurse,
            limit.unwrap_or(500),
            args.print,
            args.verbose,
        )
        .await;
    }

    let files = gather_files_without_resolution_label(&absolute_input_path, args.recurse).await?;

    if files.is_empty() {
        if args.verbose {
            println!("No video files to process");
        }
        return Ok(());
    }

    if args.verbose || args.debug {
        println!("Processing {} files...", files.len());
    }

    // Keep successfully processed files, print errors for ffprobe command
    let mut files_to_process: Vec<(FFProbeResult, PathBuf)> = get_resolutions(files)
        .await?
        .into_iter()
        .filter_map(|res| match res {
            Ok(val) => Some(val),
            Err(err) => {
                eprintln!("Error: {err}");
                None
            }
        })
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
        if args.verbose {
            println!("No video files require renaming");
        }
        return Ok(());
    } else if args.verbose {
        print_bold!("Renaming {num_files} file(s)");
    }

    print_bold!("Resolution               Label   Path");

    for (result, new_path) in files_to_process {
        if let Err(error) = result.rename(&new_path, args.force, args.print) {
            cli_tools::print_error!("{error}");
        }
    }

    print_green!("Renamed {num_files} file(s)");

    Ok(())
}

async fn delete_low_resolution_files(
    path: &Path,
    recurse: bool,
    limit: u32,
    dryrun: bool,
    verbose: bool,
) -> anyhow::Result<()> {
    let files = gather_low_resolution_video_files(path, recurse).await?;

    if files.is_empty() {
        if verbose {
            println!("No video files to process");
        }
        return Ok(());
    }

    let results: Vec<FFProbeResult> = get_resolutions(files)
        .await?
        .into_iter()
        .filter_map(|res| match res {
            Ok(val) => Some(val),
            Err(err) => {
                eprintln!("Error: {err}");
                None
            }
        })
        .collect();

    let mut files_to_delete: Vec<FFProbeResult> = results
        .into_iter()
        .filter(|result| result.resolution.is_smaller_than(limit))
        .collect();

    files_to_delete.sort_unstable_by(|a, b| a.resolution.cmp(&b.resolution).then_with(|| a.file.cmp(&b.file)));

    if files_to_delete.is_empty() {
        if verbose {
            println!("No files smaller than {limit}");
        }
        return Ok(());
    }

    let num_files = files_to_delete.len();
    if dryrun {
        print_bold!("DRYRUN: Would delete {num_files} file(s) smaller than {limit}:");
    } else if verbose {
        print_bold!("Deleting {num_files} file(s) smaller than {limit}:");
    }

    for result in files_to_delete {
        if let Err(error) = result.delete(dryrun) {
            cli_tools::print_error!("{error}");
        }
    }

    if verbose {
        print_green!("Deleted {num_files} files");
    }

    Ok(())
}

async fn gather_low_resolution_video_files(path: &Path, recurse: bool) -> anyhow::Result<Vec<PathBuf>> {
    let mut files = Vec::new();

    if recurse {
        for entry in WalkDir::new(path)
            .into_iter()
            .filter_entry(|e| !cli_tools::is_hidden(e))
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_file())
        {
            let path = entry.path();
            if let Some(ext) = path.extension().and_then(|s| s.to_str())
                && FILE_EXTENSIONS.contains(&ext)
                && path
                    .file_name()
                    .and_then(|f| f.to_str())
                    .is_some_and(|filename| !RE_HIGH_RESOLUTIONS.is_match(filename))
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
                && let Some(ext) = path.extension().and_then(|s| s.to_str())
                && FILE_EXTENSIONS.contains(&ext)
                && path
                    .file_name()
                    .and_then(|f| f.to_str())
                    .is_some_and(|filename| !RE_HIGH_RESOLUTIONS.is_match(filename))
            {
                files.push(path);
            }
        }
    }

    Ok(files)
}

async fn gather_files_without_resolution_label(path: &Path, recurse: bool) -> anyhow::Result<Vec<PathBuf>> {
    let mut files = Vec::new();

    if recurse {
        for entry in WalkDir::new(path)
            .into_iter()
            // ignore hidden files (name starting with ".")
            .filter_entry(|e| !cli_tools::is_hidden(e))
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_file())
        {
            let path = entry.path();
            if let Some(ext) = path.extension().and_then(|s| s.to_str())
                && FILE_EXTENSIONS.contains(&ext)
                && path
                    .file_name()
                    .and_then(|f| f.to_str())
                    .is_some_and(|filename| !RE_RESOLUTIONS.is_match(filename))
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
                && let Some(ext) = path.extension().and_then(|s| s.to_str())
                && FILE_EXTENSIONS.contains(&ext)
                && path
                    .file_name()
                    .and_then(|f| f.to_str())
                    .is_some_and(|filename| !RE_RESOLUTIONS.is_match(filename))
            {
                files.push(path);
            }
        }
    }

    Ok(files)
}

async fn get_resolutions(files: Vec<PathBuf>) -> anyhow::Result<Vec<Result<FFProbeResult, Error>>> {
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

    let results = futures::future::join_all(tasks)
        .await
        .into_iter()
        .map(|res| res.expect("Download future failed"))
        .collect();

    progress_bar.finish_and_clear();

    Ok(results)
}

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
fn parse_ffprobe_output(output: &[u8]) -> anyhow::Result<Resolution> {
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

    Ok(Resolution { width, height })
}

/// Create a Semaphore sized for I/O-bound work (4x physical CPU cores).
#[inline]
fn create_semaphore_for_io_bound() -> Arc<Semaphore> {
    Arc::new(Semaphore::new(num_cpus::get_physical() * 4))
}

const fn precalculate_fuzzy_resolutions() -> [ResolutionMatch; KNOWN_RESOLUTIONS.len()] {
    let mut out = [ResolutionMatch {
        label_height: 0,
        width_range: (0, 0),
        height_range: (0, 0),
    }; KNOWN_RESOLUTIONS.len()];
    let mut i = 0;
    while i < KNOWN_RESOLUTIONS.len() {
        let (w, h) = KNOWN_RESOLUTIONS[i];
        out[i] = ResolutionMatch {
            label_height: h,
            width_range: compute_bounds(w),
            height_range: compute_bounds(h),
        };
        i += 1;
    }
    out
}

const fn compute_bounds(res: u32) -> (u32, u32) {
    let tolerance = (res as f32 * RESOLUTION_TOLERANCE) as u32;
    let min = res.saturating_sub(tolerance);
    let max = res.saturating_add(tolerance);
    (min, max)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_matches() {
        assert_eq!(
            Resolution {
                width: 1280,
                height: 720
            }
            .label(),
            "720p"
        );
        assert_eq!(
            Resolution {
                width: 1920,
                height: 1080
            }
            .label(),
            "1080p"
        );
        assert_eq!(
            Resolution {
                width: 2560,
                height: 1440
            }
            .label(),
            "1440p"
        );
        assert_eq!(
            Resolution {
                width: 3840,
                height: 2160
            }
            .label(),
            "2160p"
        );
    }

    #[test]
    fn exact_matches_vertical() {
        assert_eq!(
            Resolution {
                width: 720,
                height: 1280
            }
            .label(),
            "Vertical.720p"
        );
        assert_eq!(
            Resolution {
                width: 1080,
                height: 1920
            }
            .label(),
            "Vertical.1080p"
        );
        assert_eq!(
            Resolution {
                width: 1440,
                height: 2560
            }
            .label(),
            "Vertical.1440p"
        );
        assert_eq!(
            Resolution {
                width: 2160,
                height: 3840
            }
            .label(),
            "Vertical.2160p"
        );
    }

    #[test]
    fn approximate_matches() {
        assert_eq!(
            Resolution {
                width: 1920,
                height: 1078
            }
            .label(),
            "1080p"
        );
        assert_eq!(
            Resolution {
                width: 1278,
                height: 716
            }
            .label(),
            "720p"
        );
        assert_eq!(
            Resolution {
                width: 2540,
                height: 1442
            }
            .label(),
            "1440p"
        );
        assert_eq!(
            Resolution {
                width: 1442,
                height: 2540
            }
            .label(),
            "Vertical.1440p"
        );
        assert_eq!(
            Resolution {
                width: 3820,
                height: 2162
            }
            .label(),
            "2160p"
        );
        assert_eq!(
            Resolution {
                width: 1260,
                height: 710
            }
            .label(),
            "720p"
        );
    }

    #[test]
    fn out_of_range() {
        assert_eq!(
            Resolution {
                width: 1024,
                height: 768
            }
            .label(),
            "1024x768"
        );
        assert_eq!(
            Resolution {
                width: 3000,
                height: 2000
            }
            .label(),
            "3000x2000"
        );
    }

    #[test]
    fn lower_bound_tolerance() {
        assert_eq!(
            Resolution {
                width: 1267,
                height: 713
            }
            .label(),
            "720p"
        );
    }

    #[test]
    fn upper_bound_tolerance() {
        assert_eq!(
            Resolution {
                width: 1292,
                height: 727
            }
            .label(),
            "720p"
        );
    }

    #[test]
    fn beyond_tolerance() {
        assert_eq!(
            Resolution {
                width: 1250,
                height: 790
            }
            .label(),
            "1250x790"
        );
    }

    #[test]
    fn parse_ffprobe_output_1080p() {
        let output = b"width=1920\nheight=1080\n";
        let result = parse_ffprobe_output(output).unwrap();
        assert_eq!(result.width, 1920);
        assert_eq!(result.height, 1080);
    }

    #[test]
    fn parse_ffprobe_output_4k() {
        let output = b"width=3840\nheight=2160\n";
        let result = parse_ffprobe_output(output).unwrap();
        assert_eq!(result.width, 3840);
        assert_eq!(result.height, 2160);
    }

    #[test]
    fn parse_ffprobe_output_720p() {
        let output = b"width=1280\nheight=720\n";
        let result = parse_ffprobe_output(output).unwrap();
        assert_eq!(result.width, 1280);
        assert_eq!(result.height, 720);
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
    fn is_smaller_than_width_below_limit() {
        let res = Resolution {
            width: 400,
            height: 720,
        };
        assert!(res.is_smaller_than(480));
    }

    #[test]
    fn is_smaller_than_height_below_limit() {
        let res = Resolution {
            width: 720,
            height: 400,
        };
        assert!(res.is_smaller_than(480));
    }

    #[test]
    fn is_smaller_than_both_below_limit() {
        let res = Resolution {
            width: 320,
            height: 240,
        };
        assert!(res.is_smaller_than(480));
    }

    #[test]
    fn is_smaller_than_both_above_limit() {
        let res = Resolution {
            width: 1920,
            height: 1080,
        };
        assert!(!res.is_smaller_than(480));
    }

    #[test]
    fn is_smaller_than_at_exact_limit() {
        let res = Resolution {
            width: 480,
            height: 480,
        };
        assert!(!res.is_smaller_than(480));
    }

    #[test]
    fn is_smaller_than_one_at_limit_one_below() {
        let res = Resolution {
            width: 480,
            height: 479,
        };
        assert!(res.is_smaller_than(480));
    }
}

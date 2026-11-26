//! `FFmpeg` and ffprobe command handling for video conversion.
//!
//! Builds media processing commands, probes stream metadata,
//! runs child processes in isolation, and validates muxed output.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{LazyLock, Mutex, PoisonError};
use std::time::Duration;

use anyhow::{Context, Result};

use crate::options::{
    HardwareEncoder, MOVIE_AUDIO_LANGUAGES, MOVIE_SUBTITLE_LANGUAGES, ResolvedEncoder, TARGET_EXTENSION,
};
use crate::types::{Codec, SubtitleFile, VideoInfo};

/// Arguments included in every ffmpeg invocation.
const FFMPEG_DEFAULT_ARGS: &[&str] = &["-hide_banner", "-nostdin", "-stats", "-loglevel", "info", "-y"];

/// Number of additional hardware frames allocated for CUDA filtering.
const CUDA_EXTRA_HARDWARE_FRAMES: &str = "64";

/// Number of frames used by NVENC rate-control lookahead.
const NVENC_LOOKAHEAD_FRAMES: &str = "48";

/// NVENC quality and speed preset used for HEVC conversion.
const NVENC_PRESET: &str = "p5";

/// Minimum ratio of muxed output duration to input duration before deleting any source files.
const MIN_MUX_DURATION_RATIO: f64 = 0.99;

/// Windows API constant for creating a new process group.
#[cfg(windows)]
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

/// Flag set on a forced abort (second Ctrl+C) so any active isolated ffmpeg child is terminated.
static FORCE_ABORT: AtomicBool = AtomicBool::new(false);

/// Count of isolated child commands currently running.
static ACTIVE_COMMANDS: AtomicUsize = AtomicUsize::new(0);

/// Staged temporary output files currently alive in this process, cleaned up on forced abort.
static ACTIVE_STAGED_PATHS: LazyLock<Mutex<Vec<PathBuf>>> = LazyLock::new(|| Mutex::new(Vec::new()));

/// Active lock store owners `(db_path, owner_id)` released on forced abort.
static ACTIVE_LOCK_OWNERS: LazyLock<Mutex<Vec<(PathBuf, String)>>> = LazyLock::new(|| Mutex::new(Vec::new()));

/// Polling interval while waiting on an isolated child process.
const CHILD_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Options used to build an ffmpeg HEVC conversion command.
#[derive(Debug, Clone, Copy)]
pub struct ConversionOptions<'a> {
    input: &'a Path,
    output: &'a Path,
    quality_level: u8,
    copy_audio: bool,
    use_cuda_filters: bool,
    movie_mode: bool,
    subtitle_files: &'a [SubtitleFile],
    bit_depth: u8,
    encoder: ResolvedEncoder,
}

impl<'a> ConversionOptions<'a> {
    /// Create conversion command options using `Nvenc` with CUDA filtering enabled.
    #[cfg(test)]
    pub(crate) const fn new(
        input: &'a Path,
        output: &'a Path,
        quality_level: u8,
        copy_audio: bool,
        movie_mode: bool,
        subtitle_files: &'a [SubtitleFile],
        bit_depth: u8,
    ) -> Self {
        Self::with_encoder(
            input,
            output,
            quality_level,
            copy_audio,
            movie_mode,
            subtitle_files,
            bit_depth,
            HardwareEncoder::Nvenc,
        )
    }

    /// Create conversion command options for a specific hardware encoder selection.
    #[allow(clippy::too_many_arguments)]
    pub(crate) const fn with_encoder(
        input: &'a Path,
        output: &'a Path,
        quality_level: u8,
        copy_audio: bool,
        movie_mode: bool,
        subtitle_files: &'a [SubtitleFile],
        bit_depth: u8,
        encoder: HardwareEncoder,
    ) -> Self {
        let resolved = encoder.resolve();
        Self {
            input,
            output,
            quality_level,
            copy_audio,
            use_cuda_filters: matches!(resolved, ResolvedEncoder::Nvenc),
            movie_mode,
            subtitle_files,
            bit_depth,
            encoder: resolved,
        }
    }

    /// Return the resolved hardware encoder for these options.
    pub(crate) const fn encoder(&self) -> ResolvedEncoder {
        self.encoder
    }

    /// Return whether audio stream copy is currently enabled.
    pub(crate) const fn copy_audio(&self) -> bool {
        self.copy_audio
    }

    /// Return options configured for CPU filtering instead of CUDA filtering.
    pub(crate) const fn without_cuda_filters(mut self) -> Self {
        self.use_cuda_filters = false;
        self
    }

    /// Return options transcoding audio to AAC instead of stream-copying.
    pub(crate) const fn without_copy_audio(mut self) -> Self {
        self.copy_audio = false;
        self
    }

    /// Return options using a different encoder quality level.
    pub(crate) const fn with_quality_level(mut self, quality_level: u8) -> Self {
        self.quality_level = quality_level;
        self
    }
}

/// Build the ffmpeg command for an HEVC hardware-accelerated conversion.
///
/// Supports NVIDIA `hevc_nvenc` (with optional CUDA filtering) and Apple Silicon `hevc_videotoolbox`.
/// Movie mode maps selected internal streams and external subtitles.
///
/// # Errors
/// Returns an error if the movie mode stream languages cannot be probed.
pub fn build_conversion_command(options: &ConversionOptions<'_>) -> Result<Command> {
    let mut command = Command::new("ffmpeg");
    command
        .args(FFMPEG_DEFAULT_ARGS)
        .args(["-probesize", "50M", "-analyzeduration", "1M"]);

    let use_cuda = matches!(options.encoder, ResolvedEncoder::Nvenc) && options.use_cuda_filters;
    if use_cuda {
        command.args(["-extra_hw_frames", CUDA_EXTRA_HARDWARE_FRAMES]);
    }

    command.arg("-i").arg(safe_ffmpeg_path(options.input));
    if options.movie_mode {
        for subtitle_file in options.subtitle_files {
            command.arg("-i").arg(safe_ffmpeg_path(&subtitle_file.path));
        }
        let audio_languages = probe_stream_languages(options.input, "a")?;
        let subtitle_counts = probe_stream_language_counts(options.input, "s")?;
        let subtitle_languages = subtitle_counts.keys().cloned().collect();
        add_movie_mode_stream_maps(&mut command, &audio_languages, &subtitle_languages);
        add_external_subtitle_maps(
            &mut command,
            options.subtitle_files,
            selected_subtitle_count(&subtitle_counts),
        );
    } else {
        command.args(["-map", "0:V:0", "-map", "0:a?", "-sn"]);
    }

    match options.encoder {
        ResolvedEncoder::Nvenc => {
            if use_cuda {
                let pixel_format = if options.bit_depth > 8 { "p010le" } else { "nv12" };
                command.args(["-vf", &format!("hwupload_cuda,scale_cuda=format={pixel_format}")]);
            }
            command
                .args(["-c:v", "hevc_nvenc"])
                .args(["-rc:v", "vbr"])
                .args(["-cq:v", &options.quality_level.to_string()])
                .args(["-preset", NVENC_PRESET])
                .args(["-b:v", "0"])
                .args(["-rc-lookahead", NVENC_LOOKAHEAD_FRAMES])
                .args(["-spatial-aq", "1", "-temporal-aq", "1"]);
            if options.bit_depth > 8 {
                command.args(["-profile:v", "main10", "-pix_fmt", "p010le"]);
            }
        }
        ResolvedEncoder::VideoToolbox => {
            command
                .args(["-c:v", "hevc_videotoolbox"])
                .args(["-q:v", &videotoolbox_quality(options.quality_level).to_string()]);
            if options.bit_depth > 8 {
                command.args(["-profile:v", "main10", "-pix_fmt", "p010le"]);
            } else {
                command.args(["-pix_fmt", "yuv420p"]);
            }
        }
    }

    let is_mp4 = cli_tools_common::path_to_file_extension_string(options.output) == TARGET_EXTENSION;
    if is_mp4 {
        command.args(["-tag:v", "hvc1"]);
        if !options.movie_mode {
            command.args(["-movflags", "+faststart"]);
        }
    }

    if options.movie_mode {
        add_movie_mode_passthrough_codecs(&mut command);
    } else if options.copy_audio {
        command.args(["-c:a", "copy"]);
    } else {
        command.args(["-c:a", "aac", "-b:a", "128k"]);
    }

    command.arg(safe_ffmpeg_path(options.output));
    Ok(command)
}

/// Map NVENC constant-quality value (lower = better, 1..51) to `VideoToolbox` `-q:v` (higher = better, 1..100).
#[must_use]
pub fn videotoolbox_quality(cq_level: u8) -> u8 {
    let mapped = f64::from(cq_level).mul_add(-1.4, 100.0);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let clamped = mapped.round().clamp(1.0, 100.0) as u8;
    clamped
}

/// Build an ffmpeg command that embeds external subtitles without converting video.
///
/// # Errors
/// Returns an error if the stream languages of the input cannot be probed.
pub fn build_subtitle_mux_command(input: &Path, output: &Path, subtitle_files: &[SubtitleFile]) -> Result<Command> {
    build_movie_mux_command(input, output, subtitle_files, false, None)
}

/// Build a movie stream copy, optionally transcoding audio when remuxing to MP4.
pub(crate) fn build_movie_mux_command(
    input: &Path,
    output: &Path,
    subtitle_files: &[SubtitleFile],
    transcode_audio: bool,
    codec: Option<Codec>,
) -> Result<Command> {
    let mut command = Command::new("ffmpeg");
    command.args(FFMPEG_DEFAULT_ARGS).arg("-i").arg(safe_ffmpeg_path(input));
    for subtitle_file in subtitle_files {
        command.arg("-i").arg(safe_ffmpeg_path(&subtitle_file.path));
    }

    let audio_languages = probe_stream_languages(input, "a")?;
    let subtitle_counts = probe_stream_language_counts(input, "s")?;
    let subtitle_languages = subtitle_counts.keys().cloned().collect();
    add_movie_mode_stream_maps(&mut command, &audio_languages, &subtitle_languages);
    add_external_subtitle_maps(&mut command, subtitle_files, selected_subtitle_count(&subtitle_counts));
    command.args(["-c", "copy"]);
    if transcode_audio {
        command.args(["-c:a", "aac", "-b:a", "128k"]);
    }
    if cli_tools_common::path_to_file_extension_string(output) == TARGET_EXTENSION {
        command.args(["-movflags", "+faststart"]);
        if codec == Some(Codec::X265) {
            command.args(["-tag:v", "hvc1"]);
        }
    }
    command.arg(safe_ffmpeg_path(output));
    Ok(command)
}

/// Build an ffmpeg command that remuxes the primary video and all audio streams.
#[must_use]
pub fn build_remux_command(input: &Path, output: &Path, transcode_audio: bool, codec: Codec) -> Command {
    let mut command = Command::new("ffmpeg");
    command
        .args(FFMPEG_DEFAULT_ARGS)
        .arg("-i")
        .arg(safe_ffmpeg_path(input))
        .args([
            "-map", "0:v:0", "-map", "0:a?", "-map", "-0:t", "-map", "-0:d", "-sn", "-c:v", "copy",
        ]);

    if transcode_audio {
        command.args(["-c:a", "aac", "-b:a", "128k"]);
    } else {
        command.args(["-c:a", "copy"]);
    }

    command.args(["-movflags", "+faststart"]);
    if codec == Codec::X265 {
        command.args(["-tag:v", "hvc1"]);
    }
    command.arg(safe_ffmpeg_path(output));
    command
}

/// Request immediate termination of any active isolated ffmpeg child process.
pub fn request_force_abort() {
    FORCE_ABORT.store(true, Ordering::SeqCst);
}

/// Return whether any isolated ffmpeg child process is still running.
#[must_use]
pub fn has_active_command() -> bool {
    ACTIVE_COMMANDS.load(Ordering::SeqCst) > 0
}

/// Register a staged temporary output path for removal if a forced abort skips normal `Drop` cleanup.
pub(crate) fn register_staged_path(path: PathBuf) {
    let mut paths = ACTIVE_STAGED_PATHS.lock().unwrap_or_else(PoisonError::into_inner);
    paths.push(path);
}

/// Unregister a staged temporary output path once it has been published or dropped normally.
pub(crate) fn unregister_staged_path(path: &Path) {
    let mut paths = ACTIVE_STAGED_PATHS.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(pos) = paths.iter().position(|existing| existing == path) {
        paths.swap_remove(pos);
    }
}

/// Register a lock owner so forced abort releases its `active_locks` rows before exiting.
pub(crate) fn register_lock_owner(db_path: PathBuf, owner_id: String) {
    let mut owners = ACTIVE_LOCK_OWNERS.lock().unwrap_or_else(PoisonError::into_inner);
    owners.push((db_path, owner_id));
}

/// Unregister a lock owner once its conversion run has exited cleanly.
pub(crate) fn unregister_lock_owner(owner_id: &str) {
    let mut owners = ACTIVE_LOCK_OWNERS.lock().unwrap_or_else(PoisonError::into_inner);
    owners.retain(|(_, existing)| existing != owner_id);
}

/// Remove any registered staged temporary outputs and release active locks before a forced exit.
pub fn cleanup_registered_resources() {
    let staged_paths: Vec<PathBuf> = {
        let mut paths = ACTIVE_STAGED_PATHS.lock().unwrap_or_else(PoisonError::into_inner);
        std::mem::take(&mut *paths)
    };
    for path in staged_paths {
        let _ = std::fs::remove_file(&path);
    }

    let owners: Vec<(PathBuf, String)> = {
        let mut list = ACTIVE_LOCK_OWNERS.lock().unwrap_or_else(PoisonError::into_inner);
        std::mem::take(&mut *list)
    };
    for (db_path, owner_id) in owners {
        if let Ok(mut conn) = cli_tools_db::sqlite::open_at(&db_path)
            && let Ok(tx) = cli_tools_db::sqlite::immediate_transaction(&mut conn)
        {
            let _ = tx.execute(
                "DELETE FROM active_locks WHERE owner_id = ?1",
                rusqlite::params![owner_id],
            );
            let _ = tx.execute(
                "UPDATE pending_files SET claimed_by = NULL, claimed_at = NULL WHERE claimed_by = ?1",
                rusqlite::params![owner_id],
            );
            let _ = tx.commit();
        }
    }
}

/// Normalize a local file path for passing to `ffmpeg` or `ffprobe` when it could be misread as an option or protocol.
///
/// Relative paths starting with `-` or containing `:` get a `./` prefix.
/// Plain relative and absolute paths stay unchanged.
#[must_use]
fn safe_ffmpeg_path(path: &Path) -> PathBuf {
    if path.is_relative() && !path.starts_with(".") {
        let text = path.to_string_lossy();
        if text.starts_with('-') || text.contains(':') {
            return Path::new(".").join(path);
        }
    }
    path.to_path_buf()
}

/// Normalize a local file path for passing to `ffprobe` as an input argument.
#[must_use]
fn ffmpeg_input_path(path: &Path) -> PathBuf {
    cli_tools_media::video_info::ffmpeg_input_path(path)
}

/// Probe video metadata from the first real video stream in a media file.
///
/// # Errors
/// Returns an error if ffprobe cannot be run or its output has no usable video stream.
pub fn probe_video_info(path: &Path) -> Result<VideoInfo> {
    let input_path = ffmpeg_input_path(path);
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "V:0",
            "-show_entries",
            "stream=codec_name,bit_rate,width,height,r_frame_rate,avg_frame_rate,pix_fmt,bits_per_raw_sample:stream_tags=BPS,BPS-eng:format=bit_rate,size,duration",
            "-output_format",
            "default=nokey=0:noprint_wrappers=1",
            "-i",
        ])
        .arg(&input_path)
        .output()
        .context("Failed to execute ffprobe")?;

    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        anyhow::bail!("ffprobe failed: {}", stderr.trim());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    VideoInfo::from_ffprobe_output(&stdout, &stderr, path)
}

/// Probe unique language tags for one ffmpeg stream type.
///
/// # Errors
/// Returns an error if ffprobe cannot be run or fails.
pub fn probe_stream_languages(input: &Path, stream_type: &str) -> Result<BTreeSet<String>> {
    Ok(probe_stream_language_counts(input, stream_type)?.into_keys().collect())
}

/// Count each language separately, including multiple tracks with the same tag.
fn probe_stream_language_counts(input: &Path, stream_type: &str) -> Result<BTreeMap<String, usize>> {
    let input_path = ffmpeg_input_path(input);
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            stream_type,
            "-show_entries",
            "stream=index:stream_tags=language",
            "-of",
            "csv=p=0",
            "-i",
        ])
        .arg(&input_path)
        .output()
        .context("Failed to execute ffprobe")?;

    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        anyhow::bail!(
            "ffprobe failed while probing {stream_type} stream languages: {}",
            stderr.trim()
        );
    }

    Ok(parse_stream_language_counts(&String::from_utf8_lossy(&output.stdout)))
}

/// Parse `index,language` lines from ffprobe, lowercasing tags and using `"und"` for untagged streams.
fn parse_stream_language_counts(stdout: &str) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for line in stdout.lines().map(str::trim) {
        let (index, language) = line.split_once(',').unwrap_or((line, ""));
        if index.trim().parse::<usize>().is_err() {
            continue;
        }
        let language = language.trim();
        let language = if language.is_empty() {
            "und".to_owned()
        } else {
            language.to_ascii_lowercase()
        };
        *counts.entry(language).or_default() += 1;
    }
    counts
}

/// Return whether applying movie-mode stream maps would remove any internal tracks.
///
/// # Errors
/// Returns an error if the stream languages cannot be probed.
pub fn movie_stream_processing_required(input: &Path) -> Result<bool> {
    let audio_languages = probe_stream_languages(input, "a")?;
    let subtitle_languages = probe_stream_languages(input, "s")?;
    Ok(stream_languages_require_movie_processing(
        &audio_languages,
        &subtitle_languages,
    ))
}

/// Validate subtitle mux output before deleting or replacing any source files.
///
/// Returns the probed [`VideoInfo`] of the validated output so callers can cache it.
///
/// # Errors
/// Returns an error describing why the output cannot replace the input.
pub fn validate_mux_output(input: &Path, output: &Path, input_info: &VideoInfo) -> Result<VideoInfo> {
    let output_info = probe_video_info(output).context("Failed to get subtitle mux output info")?;
    if output_info.duration < input_info.duration * MIN_MUX_DURATION_RATIO {
        anyhow::bail!(
            "Muxed output duration {:.1}s is less than {:.0}% of original {:.1}s",
            output_info.duration,
            MIN_MUX_DURATION_RATIO * 100.0,
            input_info.duration
        );
    }

    let input_audio_streams = probe_stream_count(input, "a")?;
    if input_audio_streams > 0 {
        let output_audio_streams = probe_stream_count(output, "a")?;
        if output_audio_streams == 0 {
            anyhow::bail!(
                "Muxed output has no audio streams, but input has {}",
                cli_tools_common::count_label(input_audio_streams, "audio stream", "audio streams")
            );
        }
    }

    Ok(output_info)
}

/// Run a command in a new process group so Ctrl+C remains under the parent process's control.
///
/// When [`request_force_abort`] is called while the child is running, the child is killed cleanly
/// before returning an `Interrupted` error so callers can remove partial output files.
///
/// # Errors
/// Returns an error if the command cannot be started, is aborted, or cannot be waited on.
pub fn run_command_isolated(command: &mut Command) -> std::io::Result<ExitStatus> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(CREATE_NEW_PROCESS_GROUP);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.stdout(Stdio::inherit()).stderr(Stdio::inherit()).spawn()?;
    ACTIVE_COMMANDS.fetch_add(1, Ordering::SeqCst);
    let result = loop {
        if FORCE_ABORT.load(Ordering::SeqCst) {
            let _ = child.kill();
            let _ = child.wait();
            break Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "Command terminated by forced abort",
            ));
        }
        if let Some(status) = child.try_wait()? {
            break Ok(status);
        }
        std::thread::sleep(CHILD_POLL_INTERVAL);
    };
    ACTIVE_COMMANDS.fetch_sub(1, Ordering::SeqCst);
    result
}

/// Verify that if `input` has audio streams, `output` also preserves at least one audio stream.
///
/// # Errors
/// Returns an error if probing fails or if audio streams were lost.
pub(crate) fn validate_audio_preserved(input: &Path, output: &Path) -> Result<()> {
    let input_audio_streams = probe_stream_count(input, "a")?;
    if input_audio_streams > 0 {
        let output_audio_streams = probe_stream_count(output, "a")?;
        if output_audio_streams == 0 {
            anyhow::bail!(
                "Output has no audio streams, but input has {}",
                cli_tools_common::count_label(input_audio_streams, "audio stream", "audio streams")
            );
        }
    }
    Ok(())
}

/// Probe how many streams of one ffmpeg stream type exist in a file.
pub(crate) fn probe_stream_count(input: &Path, stream_type: &str) -> Result<usize> {
    let input_path = ffmpeg_input_path(input);
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            stream_type,
            "-show_entries",
            "stream=index",
            "-of",
            "csv=p=0",
            "-i",
        ])
        .arg(&input_path)
        .output()
        .context("Failed to execute ffprobe")?;

    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        anyhow::bail!("ffprobe failed while counting {stream_type} streams: {}", stderr.trim());
    }

    Ok(count_stream_lines(&String::from_utf8_lossy(&output.stdout)))
}

/// Count the non-empty stream index lines in ffprobe output.
fn count_stream_lines(stdout: &str) -> usize {
    stdout.lines().filter(|line| !line.trim().is_empty()).count()
}

/// Return whether movie-mode stream maps would remove any internal audio or subtitle tracks.
fn stream_languages_require_movie_processing(
    audio_languages: &BTreeSet<String>,
    subtitle_languages: &BTreeSet<String>,
) -> bool {
    let has_preferred_audio = audio_languages
        .iter()
        .any(|language| MOVIE_AUDIO_LANGUAGES.contains(&language.as_str()));

    let removes_audio = has_preferred_audio
        && audio_languages
            .iter()
            .any(|language| !MOVIE_AUDIO_LANGUAGES.contains(&language.as_str()));

    let removes_subtitles = subtitle_languages
        .iter()
        .any(|language| !MOVIE_SUBTITLE_LANGUAGES.contains(&language.as_str()));

    removes_audio || removes_subtitles
}

/// Add movie-mode stream mapping for real video streams and selected language tracks.
fn add_movie_mode_stream_maps(
    command: &mut Command,
    audio_languages: &BTreeSet<String>,
    subtitle_languages: &BTreeSet<String>,
) {
    command.args(["-map", "0:V"]);

    let mut mapped_audio = false;
    for &language in MOVIE_AUDIO_LANGUAGES {
        if audio_languages.contains(language) {
            command.arg("-map").arg(format!("0:a:m:language:{language}"));
            mapped_audio = true;
        }
    }
    if !mapped_audio {
        command.args(["-map", "0:a?"]);
    }

    for &language in MOVIE_SUBTITLE_LANGUAGES {
        if subtitle_languages.contains(language) {
            command.arg("-map").arg(format!("0:s:m:language:{language}"));
        }
    }

    command.args(["-map_metadata", "0", "-map_chapters", "0"]);
}

/// Configure movie-mode audio and subtitle streams for passthrough.
fn add_movie_mode_passthrough_codecs(command: &mut Command) {
    command.args(["-c:a", "copy", "-c:s", "copy"]);
}

/// Count retained embedded subtitle tracks to locate the first sidecar output stream.
fn selected_subtitle_count(subtitle_counts: &BTreeMap<String, usize>) -> usize {
    MOVIE_SUBTITLE_LANGUAGES
        .iter()
        .filter_map(|language| subtitle_counts.get(*language))
        .sum()
}

/// Map external subtitle sidecars and set inferred output languages.
///
/// Paired `VobSub` `.idx`/`.sub` files may contain multiple language tracks, so all tracks are mapped with `:s`.
/// Single-track text subtitles map `:s:0` and tag their inferred language when known.
fn add_external_subtitle_maps(command: &mut Command, subtitle_files: &[SubtitleFile], first_output_index: usize) {
    for (offset, subtitle_file) in subtitle_files.iter().enumerate() {
        let is_vobsub = subtitle_file.paired_sub_path.is_some() || subtitle_file.extension == "idx";
        if is_vobsub {
            command.arg("-map").arg(format!("{}:s", offset + 1));
        } else {
            command.arg("-map").arg(format!("{}:s:0", offset + 1));
        }
        if let Some(language) = subtitle_file.language() {
            command
                .arg(format!("-metadata:s:s:{}", first_output_index + offset))
                .arg(format!("language={language}"));
        }
    }
}

/// Verify the output contains the expected retained embedded subtitles and every muxed sidecar.
pub(crate) fn validate_movie_subtitles(input: &Path, output: &Path, sidecars: &[SubtitleFile]) -> Result<()> {
    let mut expected_named = probe_stream_language_counts(input, "s")?;
    expected_named.retain(|language, _| MOVIE_SUBTITLE_LANGUAGES.contains(&language.as_str()));
    let retained_embedded_total: usize = expected_named.values().sum();
    let mut unnamed_sidecar_count = 0usize;
    for sidecar in sidecars {
        if let Some(language) = sidecar.language() {
            *expected_named.entry(language.to_owned()).or_default() += 1;
        } else {
            unnamed_sidecar_count += 1;
        }
    }

    let actual = probe_stream_language_counts(output, "s")?;
    let actual_total: usize = actual.values().sum();
    let expected_min_total = retained_embedded_total + sidecars.len();
    if actual_total < expected_min_total {
        anyhow::bail!("Output has {actual_total} subtitle tracks, expected at least {expected_min_total}");
    }
    for (language, count) in expected_named {
        if actual.get(&language).copied().unwrap_or_default() < count {
            anyhow::bail!("Output is missing expected {language} subtitle tracks");
        }
    }
    let _ = unnamed_sidecar_count;
    Ok(())
}

#[cfg(test)]
mod test_helpers {
    use super::*;

    /// Collect a command's arguments as owned strings for assertions.
    pub fn command_args(command: &Command) -> Vec<String> {
        command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect()
    }

    /// Return whether adjacent command arguments contain an option and value pair.
    pub fn has_arg_pair(args: &[String], option: &str, value: &str) -> bool {
        args.windows(2).any(|pair| pair[0] == option && pair[1] == value)
    }
}

#[cfg(test)]
mod test_ffprobe_output_parsing {
    use super::*;

    #[test]
    fn collects_unique_languages_and_marks_untagged_streams() {
        let languages = parse_stream_language_counts("1,eng\n2,fin\n3,eng\n4,\n5\n")
            .into_keys()
            .collect::<BTreeSet<_>>();

        assert_eq!(
            languages,
            BTreeSet::from(["eng".to_string(), "fin".to_string(), "und".to_string()])
        );
    }

    #[test]
    fn trims_whitespace_around_language_tags() {
        let languages = parse_stream_language_counts("  1, jpn  \r\n")
            .into_keys()
            .collect::<BTreeSet<_>>();

        assert_eq!(languages, BTreeSet::from(["jpn".to_string()]));
    }

    #[test]
    fn no_streams_gives_no_languages() {
        assert!(parse_stream_language_counts("").is_empty());
    }

    #[test]
    fn counts_only_non_empty_stream_lines() {
        assert_eq!(count_stream_lines("1\n2\n\n  \n3\n"), 3);
        assert_eq!(count_stream_lines(""), 0);
    }
}

#[cfg(test)]
mod test_stream_languages_require_movie_processing {
    use super::*;

    #[test]
    fn allowed_movie_stream_languages_do_not_require_processing() {
        let audio_languages = BTreeSet::from(["eng".to_string(), "fra".to_string()]);
        let subtitle_languages = BTreeSet::from(["eng".to_string(), "fin".to_string()]);

        assert!(!stream_languages_require_movie_processing(
            &audio_languages,
            &subtitle_languages,
        ));
    }

    #[test]
    fn disallowed_movie_stream_languages_require_processing() {
        let audio_languages = BTreeSet::from(["eng".to_string(), "ger".to_string()]);
        let subtitle_languages = BTreeSet::from(["eng".to_string(), "spa".to_string()]);

        assert!(stream_languages_require_movie_processing(
            &audio_languages,
            &subtitle_languages,
        ));
    }

    #[test]
    fn fallback_audio_languages_do_not_require_ineffective_processing() {
        let audio_languages = BTreeSet::from(["ger".to_string(), "und".to_string()]);
        let subtitle_languages = BTreeSet::new();

        assert!(!stream_languages_require_movie_processing(
            &audio_languages,
            &subtitle_languages,
        ));
    }

    #[test]
    fn disallowed_subtitle_language_requires_processing_without_audio_changes() {
        let audio_languages = BTreeSet::from(["eng".to_string()]);
        let subtitle_languages = BTreeSet::from(["spa".to_string()]);

        assert!(stream_languages_require_movie_processing(
            &audio_languages,
            &subtitle_languages,
        ));
    }
}

#[cfg(test)]
mod test_command_helpers {
    use super::test_helpers::{command_args, has_arg_pair};
    use super::*;

    #[test]
    fn movie_mode_maps_only_allowed_audio_and_subtitle_languages_that_exist() {
        let mut command = Command::new("ffmpeg");
        let audio_languages = BTreeSet::from([
            "eng".to_string(),
            "fin".to_string(),
            "fra".to_string(),
            "fre".to_string(),
            "jpn".to_string(),
            "ger".to_string(),
        ]);
        let subtitle_languages = BTreeSet::from(["eng".to_string(), "swe".to_string(), "spa".to_string()]);

        add_movie_mode_stream_maps(&mut command, &audio_languages, &subtitle_languages);
        let args = command_args(&command);

        assert!(has_arg_pair(&args, "-map", "0:V"));
        assert!(!has_arg_pair(&args, "-map", "0:v"));
        assert!(!has_arg_pair(&args, "-map", "0"));
        assert!(has_arg_pair(&args, "-map", "0:a:m:language:eng"));
        assert!(has_arg_pair(&args, "-map", "0:a:m:language:fin"));
        assert!(has_arg_pair(&args, "-map", "0:a:m:language:fra"));
        assert!(has_arg_pair(&args, "-map", "0:a:m:language:fre"));
        assert!(has_arg_pair(&args, "-map", "0:a:m:language:jpn"));
        assert!(!has_arg_pair(&args, "-map", "0:a?"));
        assert!(!has_arg_pair(&args, "-map", "0:a:m:language:swe"));
        assert!(!has_arg_pair(&args, "-map", "0:a:m:language:ger"));
        assert!(has_arg_pair(&args, "-map", "0:s:m:language:eng"));
        assert!(!has_arg_pair(&args, "-map", "0:s:m:language:fin"));
        assert!(has_arg_pair(&args, "-map", "0:s:m:language:swe"));
        assert!(!has_arg_pair(&args, "-map", "0:s:m:language:spa"));
        assert!(has_arg_pair(&args, "-map_metadata", "0"));
        assert!(has_arg_pair(&args, "-map_chapters", "0"));
        assert!(!has_arg_pair(&args, "-c", "copy"));
    }

    #[test]
    fn movie_mode_falls_back_to_all_audio_when_no_preferred_language_exists() {
        let mut command = Command::new("ffmpeg");
        let audio_languages = BTreeSet::from(["und".to_string()]);
        let subtitle_languages = BTreeSet::new();

        add_movie_mode_stream_maps(&mut command, &audio_languages, &subtitle_languages);
        let args = command_args(&command);

        assert!(has_arg_pair(&args, "-map", "0:V"));
        assert!(has_arg_pair(&args, "-map", "0:a?"));
        assert!(!has_arg_pair(&args, "-map", "0:a:m:language:und"));
    }

    #[test]
    fn conversion_uses_8bit_pixel_format_for_8bit_source() {
        let options = ConversionOptions::new(Path::new("input.mkv"), Path::new("output.mp4"), 28, true, false, &[], 8);
        let command = build_conversion_command(&options).expect("conversion command should be constructed");
        let args = command_args(&command);

        assert!(has_arg_pair(&args, "-vf", "hwupload_cuda,scale_cuda=format=nv12"));
        assert!(!has_arg_pair(&args, "-profile:v", "main10"));
        assert!(!has_arg_pair(&args, "-pix_fmt", "p010le"));
    }

    #[test]
    fn conversion_preserves_10bit_for_10bit_source() {
        let options = ConversionOptions::new(
            Path::new("input.mkv"),
            Path::new("output.mp4"),
            28,
            true,
            false,
            &[],
            10,
        );
        let command = build_conversion_command(&options).expect("conversion command should be constructed");
        let args = command_args(&command);

        assert!(has_arg_pair(&args, "-vf", "hwupload_cuda,scale_cuda=format=p010le"));
        assert!(has_arg_pair(&args, "-profile:v", "main10"));
        assert!(has_arg_pair(&args, "-pix_fmt", "p010le"));
    }

    #[test]
    fn movie_mode_conversion_copies_audio_and_subtitles_without_generic_codec() {
        let mut command = Command::new("ffmpeg");

        add_movie_mode_passthrough_codecs(&mut command);
        let args = command_args(&command);

        assert!(has_arg_pair(&args, "-c:a", "copy"));
        assert!(has_arg_pair(&args, "-c:s", "copy"));
        assert!(!has_arg_pair(&args, "-c", "copy"));
    }
}

#[cfg(test)]
mod test_conversion_command {
    use super::test_helpers::{command_args, has_arg_pair};
    use super::*;

    #[test]
    fn cpu_conversion_builds_expected_transcoding_command() {
        let options = ConversionOptions::new(
            Path::new("input.mkv"),
            Path::new("output.mp4"),
            30,
            false,
            false,
            &[],
            8,
        )
        .without_cuda_filters()
        .with_quality_level(24);

        let command = build_conversion_command(&options).expect("conversion command should be constructed");

        assert_eq!(command.get_program(), "ffmpeg");
        assert_eq!(
            command_args(&command),
            [
                "-hide_banner",
                "-nostdin",
                "-stats",
                "-loglevel",
                "info",
                "-y",
                "-probesize",
                "50M",
                "-analyzeduration",
                "1M",
                "-i",
                "input.mkv",
                "-map",
                "0:V:0",
                "-map",
                "0:a?",
                "-sn",
                "-c:v",
                "hevc_nvenc",
                "-rc:v",
                "vbr",
                "-cq:v",
                "24",
                "-preset",
                "p5",
                "-b:v",
                "0",
                "-rc-lookahead",
                "48",
                "-spatial-aq",
                "1",
                "-temporal-aq",
                "1",
                "-tag:v",
                "hvc1",
                "-movflags",
                "+faststart",
                "-c:a",
                "aac",
                "-b:a",
                "128k",
                "output.mp4",
            ]
        );
    }

    #[test]
    fn non_target_container_copies_audio_without_hevc_tag() {
        let options = ConversionOptions::new(Path::new("input.mp4"), Path::new("output.mkv"), 28, true, false, &[], 8);

        let command = build_conversion_command(&options).expect("conversion command should be constructed");
        let args = command_args(&command);

        assert!(has_arg_pair(&args, "-extra_hw_frames", "64"));
        assert!(has_arg_pair(&args, "-c:a", "copy"));
        assert!(!has_arg_pair(&args, "-tag:v", "hvc1"));
        assert_eq!(args.last().map(String::as_str), Some("output.mkv"));
    }
}

#[cfg(test)]
mod test_remux_command {
    use super::test_helpers::command_args;
    use super::*;

    #[test]
    fn x265_remux_transcodes_audio_and_sets_compatibility_tag() {
        let command = build_remux_command(Path::new("input.mkv"), Path::new("output.mp4"), true, Codec::X265);

        assert_eq!(command.get_program(), "ffmpeg");
        assert_eq!(
            command_args(&command),
            [
                "-hide_banner",
                "-nostdin",
                "-stats",
                "-loglevel",
                "info",
                "-y",
                "-i",
                "input.mkv",
                "-map",
                "0:v:0",
                "-map",
                "0:a?",
                "-map",
                "-0:t",
                "-map",
                "-0:d",
                "-sn",
                "-c:v",
                "copy",
                "-c:a",
                "aac",
                "-b:a",
                "128k",
                "-movflags",
                "+faststart",
                "-tag:v",
                "hvc1",
                "output.mp4",
            ]
        );
    }

    #[test]
    fn av1_remux_copies_audio_without_hevc_tag() {
        let command = build_remux_command(Path::new("input.mkv"), Path::new("output.mp4"), false, Codec::Av1);

        assert_eq!(
            command_args(&command),
            [
                "-hide_banner",
                "-nostdin",
                "-stats",
                "-loglevel",
                "info",
                "-y",
                "-i",
                "input.mkv",
                "-map",
                "0:v:0",
                "-map",
                "0:a?",
                "-map",
                "-0:t",
                "-map",
                "-0:d",
                "-sn",
                "-c:v",
                "copy",
                "-c:a",
                "copy",
                "-movflags",
                "+faststart",
                "output.mp4",
            ]
        );
    }
}

#[cfg(test)]
mod test_external_subtitle_maps {
    use super::test_helpers::command_args;
    use super::*;

    #[test]
    fn maps_each_external_subtitle_and_indexes_tags_after_embedded_tracks() {
        let subtitle_files = [
            SubtitleFile::new(Path::new("movie.English.srt"), None),
            SubtitleFile::new(Path::new("movie.Finnish.forced.srt"), None),
        ];
        let mut command = Command::new("ffmpeg");

        add_external_subtitle_maps(&mut command, &subtitle_files, 2);

        assert_eq!(
            command_args(&command),
            [
                "-map",
                "1:s:0",
                "-metadata:s:s:2",
                "language=eng",
                "-map",
                "2:s:0",
                "-metadata:s:s:3",
                "language=fin"
            ]
        );
    }

    #[test]
    fn videotoolbox_conversion_builds_hardware_accelerated_command() {
        use super::test_helpers::has_arg_pair;

        let options = ConversionOptions::with_encoder(
            Path::new("-dash:clip.mkv"),
            Path::new("output.mp4"),
            28,
            true,
            false,
            &[],
            10,
            HardwareEncoder::VideoToolbox,
        );
        let command = build_conversion_command(&options).expect("build videotoolbox command");
        let args = command_args(&command);

        assert!(has_arg_pair(&args, "-i", "./-dash:clip.mkv"));
        assert!(has_arg_pair(&args, "-c:v", "hevc_videotoolbox"));
        assert!(has_arg_pair(&args, "-profile:v", "main10"));
        assert!(has_arg_pair(&args, "-pix_fmt", "p010le"));
        assert!(has_arg_pair(&args, "-tag:v", "hvc1"));
        assert!(has_arg_pair(&args, "-movflags", "+faststart"));
        assert!(!args.iter().any(|arg| arg.contains("cuda")));
    }

    #[test]
    fn uppercase_language_tags_are_lowercased_and_vobsub_maps_all_tracks() {
        let counts = parse_stream_language_counts("1,ENG\n2,Fin\n");
        assert_eq!(counts.get("eng"), Some(&1));
        assert_eq!(counts.get("fin"), Some(&1));

        let vobsub = SubtitleFile::new(Path::new("Movie.idx"), Some(PathBuf::from("Movie.sub")));
        let mut command = Command::new("ffmpeg");
        add_external_subtitle_maps(&mut command, &[vobsub], 0);
        assert_eq!(command_args(&command), ["-map", "1:s"]);
    }

    #[test]
    fn empty_external_subtitle_list_adds_no_arguments() {
        let mut command = Command::new("ffmpeg");

        add_external_subtitle_maps(&mut command, &[], 1);

        assert_eq!(command_args(&command), [] as [String; 0]);
    }
}

#[cfg(test)]
mod test_ffmpeg_input_path {
    use super::*;

    #[test]
    fn prepends_dot_for_relative_paths_and_preserves_absolute_paths() {
        assert_eq!(ffmpeg_input_path(Path::new("-dash.mkv")), PathBuf::from("./-dash.mkv"));
        assert_eq!(
            ffmpeg_input_path(Path::new("foo:bar.mkv")),
            PathBuf::from("./foo:bar.mkv")
        );
        assert_eq!(
            ffmpeg_input_path(Path::new("./already.mkv")),
            PathBuf::from("./already.mkv")
        );
        assert_eq!(
            ffmpeg_input_path(Path::new("/abs/movie.mkv")),
            PathBuf::from("/abs/movie.mkv")
        );
    }
}

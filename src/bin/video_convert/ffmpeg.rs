//! `FFmpeg` and ffprobe command handling for video conversion.
//!
//! Builds media processing commands, probes stream metadata, runs child processes in isolation, and validates muxed output.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};

use anyhow::{Context, Result};

use crate::config::{MOVIE_AUDIO_LANGUAGES, MOVIE_SUBTITLE_LANGUAGES, TARGET_EXTENSION};
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
}

impl<'a> ConversionOptions<'a> {
    /// Create conversion command options with CUDA filtering enabled.
    pub(crate) const fn new(
        input: &'a Path,
        output: &'a Path,
        quality_level: u8,
        copy_audio: bool,
        movie_mode: bool,
        subtitle_files: &'a [SubtitleFile],
        bit_depth: u8,
    ) -> Self {
        Self {
            input,
            output,
            quality_level,
            copy_audio,
            use_cuda_filters: true,
            movie_mode,
            subtitle_files,
            bit_depth,
        }
    }

    /// Return options configured for CPU filtering instead of CUDA filtering.
    pub(crate) const fn without_cuda_filters(mut self) -> Self {
        self.use_cuda_filters = false;
        self
    }

    /// Return options using a different encoder quality level.
    pub(crate) const fn with_quality_level(mut self, quality_level: u8) -> Self {
        self.quality_level = quality_level;
        self
    }
}

/// Build the ffmpeg command for an HEVC conversion.
///
/// CUDA filtering uploads frames to the GPU and preserves 10-bit input when required.
/// Movie mode maps selected internal streams and any external subtitle inputs.
pub fn build_conversion_command(options: &ConversionOptions<'_>) -> Result<Command> {
    let mut command = Command::new("ffmpeg");
    command
        .args(FFMPEG_DEFAULT_ARGS)
        .args(["-probesize", "50M", "-analyzeduration", "1M"]);

    if options.use_cuda_filters {
        command.args(["-extra_hw_frames", CUDA_EXTRA_HARDWARE_FRAMES]);
    }

    command.arg("-i").arg(options.input);
    for subtitle_file in options.subtitle_files {
        command.arg("-i").arg(&subtitle_file.path);
    }

    if options.movie_mode {
        let audio_languages = probe_stream_languages(options.input, "a")?;
        let subtitle_languages = probe_stream_languages(options.input, "s")?;
        add_movie_mode_stream_maps(&mut command, &audio_languages, &subtitle_languages);
        add_external_subtitle_maps(&mut command, options.subtitle_files, 1);
    }

    if options.use_cuda_filters {
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
        .args(["-spatial_aq", "1", "-temporal_aq", "1"]);

    if options.bit_depth > 8 {
        command.args(["-profile:v", "main10", "-pix_fmt", "p010le"]);
    }

    if cli_tools::path_to_file_extension_string(options.output) == TARGET_EXTENSION {
        command.args(["-tag:v", "hvc1"]);
    }

    if options.movie_mode {
        add_movie_mode_passthrough_codecs(&mut command);
    } else if options.copy_audio {
        command.args(["-c:a", "copy"]);
    } else {
        command.args(["-c:a", "aac", "-b:a", "128k"]);
    }

    command.arg(options.output);
    Ok(command)
}

/// Build an ffmpeg command that embeds external subtitles without converting video.
pub fn build_subtitle_mux_command(input: &Path, output: &Path, subtitle_files: &[SubtitleFile]) -> Result<Command> {
    let mut command = Command::new("ffmpeg");
    command.args(FFMPEG_DEFAULT_ARGS).arg("-i").arg(input);
    for subtitle_file in subtitle_files {
        command.arg("-i").arg(&subtitle_file.path);
    }

    let audio_languages = probe_stream_languages(input, "a")?;
    let subtitle_languages = probe_stream_languages(input, "s")?;
    add_movie_mode_stream_maps(&mut command, &audio_languages, &subtitle_languages);
    add_external_subtitle_maps(&mut command, subtitle_files, 1);
    command.args(["-c", "copy"]);
    command.arg(output);
    Ok(command)
}

/// Build an ffmpeg command that remuxes the primary video and all audio streams.
pub fn build_remux_command(input: &Path, output: &Path, transcode_audio: bool, codec: Codec) -> Command {
    let mut command = Command::new("ffmpeg");
    command.args(FFMPEG_DEFAULT_ARGS).arg("-i").arg(input).args([
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
    command.arg(output);
    command
}

/// Probe video metadata from the first real video stream in a media file.
pub fn probe_video_info(path: &Path) -> Result<VideoInfo> {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "V:0",
            "-show_entries",
            "stream=codec_name,bit_rate,width,height,r_frame_rate,pix_fmt,bits_per_raw_sample:stream_tags=BPS,BPS-eng:format=bit_rate,size,duration",
            "-output_format",
            "default=nokey=0:noprint_wrappers=1",
        ])
        .arg(path)
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
pub fn probe_stream_languages(input: &Path, stream_type: &str) -> Result<BTreeSet<String>> {
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
        ])
        .arg(input)
        .output()
        .context("Failed to execute ffprobe")?;

    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        anyhow::bail!(
            "ffprobe failed while probing {stream_type} stream languages: {}",
            stderr.trim()
        );
    }

    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| {
            let language = line.trim().split_once(',').map_or("", |(_, language)| language.trim());
            if language.is_empty() { "und" } else { language }
        })
        .map(ToOwned::to_owned)
        .collect())
}

/// Return whether applying movie-mode stream maps would remove any internal tracks.
pub fn movie_stream_processing_required(input: &Path) -> Result<bool> {
    let audio_languages = probe_stream_languages(input, "a")?;
    let subtitle_languages = probe_stream_languages(input, "s")?;
    Ok(stream_languages_require_movie_processing(
        &audio_languages,
        &subtitle_languages,
    ))
}

/// Validate subtitle mux output before deleting or replacing any source files.
pub fn validate_mux_output(input: &Path, output: &Path, input_info: &VideoInfo) -> Result<()> {
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
                cli_tools::count_label(input_audio_streams, "audio stream", "audio streams")
            );
        }
    }

    Ok(())
}

/// Run a command in a new process group so Ctrl+C remains under the parent process's control.
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
    command.stdout(Stdio::inherit()).stderr(Stdio::inherit()).status()
}

/// Probe how many streams of one ffmpeg stream type exist in a file.
fn probe_stream_count(input: &Path, stream_type: &str) -> Result<usize> {
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
        ])
        .arg(input)
        .output()
        .context("Failed to execute ffprobe")?;

    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        anyhow::bail!("ffprobe failed while counting {stream_type} streams: {}", stderr.trim());
    }

    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count())
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

/// Add stream maps for external subtitle inputs.
fn add_external_subtitle_maps(command: &mut Command, subtitle_files: &[SubtitleFile], first_input_index: usize) {
    for input_index in first_input_index..first_input_index + subtitle_files.len() {
        command.arg("-map").arg(format!("{input_index}:s?"));
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
}

#[cfg(test)]
mod test_command_helpers {
    use super::*;

    /// Collect a command's arguments as owned strings for assertions.
    fn command_args(command: &Command) -> Vec<String> {
        command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect()
    }

    /// Return whether adjacent command arguments contain an option and value pair.
    fn has_arg_pair(args: &[String], option: &str, value: &str) -> bool {
        args.windows(2).any(|pair| pair[0] == option && pair[1] == value)
    }

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
        let command = build_conversion_command(&options).unwrap();
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
        let command = build_conversion_command(&options).unwrap();
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

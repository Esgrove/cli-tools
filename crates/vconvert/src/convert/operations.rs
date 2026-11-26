//! Per-file ffmpeg operations for video convert.
//!
//! Remuxes HEVC and AV1 files to MP4, muxes external subtitles in movie mode, and converts video to HEVC,
//! retrying with fallback settings and validating each output before the original is removed.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use tempfile::TempPath;

use super::VideoConvert;
use crate::ffmpeg::{
    ConversionOptions, build_conversion_command, build_movie_mux_command, build_remux_command, probe_video_info,
    register_staged_path, run_command_isolated, unregister_staged_path, validate_movie_subtitles, validate_mux_output,
};
use crate::observer::{FilePosition, Operation, Retry};
use crate::stats::ConversionStats;
use crate::types::{ProcessResult, ProcessableFile, VideoInfo};

/// Minimum ratio of output duration to input duration for a conversion to be considered successful.
const MIN_DURATION_RATIO: f64 = 0.85;

/// RAII wrapper around a staged temporary output path that keeps the forced-abort registry in sync.
struct StagedOutput {
    temp_path: Option<TempPath>,
    registered_path: PathBuf,
}

impl StagedOutput {
    /// Reserve a hidden sibling temp file and register it for forced-abort cleanup.
    fn create(output: &Path) -> Result<Self> {
        let temp_path = stage_output(output)?;
        let registered_path = temp_path.to_path_buf();
        register_staged_path(registered_path.clone());
        Ok(Self {
            temp_path: Some(temp_path),
            registered_path,
        })
    }

    /// Borrow the underlying temporary path.
    fn path(&self) -> &Path {
        self.temp_path.as_deref().unwrap_or(&self.registered_path)
    }

    /// Consume the guard and return the underlying [`TempPath`], unregistering it from forced-abort cleanup.
    fn into_temp_path(mut self) -> Option<TempPath> {
        unregister_staged_path(&self.registered_path);
        self.temp_path.take()
    }
}

impl Drop for StagedOutput {
    fn drop(&mut self) {
        unregister_staged_path(&self.registered_path);
    }
}

impl VideoConvert<'_> {
    /// Remux HEVC or AV1 to MP4 container.
    pub(super) fn remux_to_mp4(
        &self,
        file: &ProcessableFile,
        position: FilePosition,
        abort_flag: &AtomicBool,
    ) -> (ProcessResult, Option<VideoInfo>) {
        if self.options.movie_mode {
            return self.mux_movie_streams(file, position, Operation::Remux, abort_flag);
        }
        let input = &file.file.path;
        let output = &file.output_path;
        let info = &file.info;
        let codec = info.codec_suffix();

        self.observer.file_started(Operation::Remux, position, file, None);

        if self.options.dry_run {
            self.observer
                .dry_run_command(&build_remux_command(input, output, false, codec));
            return (ProcessResult::Remuxed {}, None);
        }

        let staged = match StagedOutput::create(output) {
            Ok(staged) => staged,
            Err(error) => return (failed(format!("Failed to stage remux output: {error}")), None),
        };
        let mut command = build_remux_command(input, staged.path(), false, codec);
        let status = match run_command_isolated(&mut command) {
            Ok(status) => status,
            Err(error) => return (failed(format!("Failed to execute ffmpeg: {error}")), None),
        };

        if !status.success() {
            if abort_flag.load(Ordering::SeqCst) {
                return (failed("Remux aborted".to_string()), None);
            }
            // Transcode to AAC if stream copy failed with an incompatible audio codec.
            self.observer.retrying(Retry::AacAudio { status });
            if let Err(error) = clear_staged_output(staged.path()) {
                return (failed(error), None);
            }
            let mut command = build_remux_command(input, staged.path(), true, codec);
            let status = match run_command_isolated(&mut command) {
                Ok(status) => status,
                Err(error) => return (failed(format!("Failed to execute ffmpeg: {error}")), None),
            };
            if !status.success() {
                return (
                    failed(format!(
                        "ffmpeg remux with AAC transcode failed with status: {}",
                        status.code().unwrap_or(-1)
                    )),
                    None,
                );
            }
        }

        let output_info = match validate_mux_output(input, staged.path(), info) {
            Ok(output_info) => output_info,
            Err(error) => return (failed(format!("Remux output failed validation: {error:#}")), None),
        };
        if let Err(error) = self.publish_output(staged, input, output, false) {
            return (failed(error.to_string()), None);
        }

        (ProcessResult::Remuxed {}, Some(output_info))
    }

    /// Process movie-mode audio and subtitle streams without converting video.
    pub(super) fn mux_subtitles(
        &self,
        file: &ProcessableFile,
        position: FilePosition,
        abort_flag: &AtomicBool,
    ) -> (ProcessResult, Option<VideoInfo>) {
        self.mux_movie_streams(file, position, Operation::SubtitleMux, abort_flag)
    }

    /// Copy movie-mode streams, applying the same subtitle validation to both mux operations.
    fn mux_movie_streams(
        &self,
        file: &ProcessableFile,
        position: FilePosition,
        operation: Operation,
        abort_flag: &AtomicBool,
    ) -> (ProcessResult, Option<VideoInfo>) {
        let input = &file.file.path;
        let final_output = &file.output_path;
        let in_place = cli_tools_common::paths_refer_to_same_file(input, final_output);
        let info = &file.info;

        self.observer.file_started(operation, position, file, None);

        let staged = if self.options.dry_run {
            None
        } else {
            match StagedOutput::create(final_output) {
                Ok(staged) => Some(staged),
                Err(error) => return (failed(format!("Failed to stage subtitle mux output: {error}")), None),
            }
        };
        let command_output = staged.as_ref().map_or(final_output.as_path(), StagedOutput::path);
        let build_command = |transcode_audio| {
            build_movie_mux_command(
                input,
                command_output,
                &file.subtitle_files,
                transcode_audio,
                Some(info.codec_suffix()),
            )
            .map_err(|error| format!("Failed to build movie mux command: {error}"))
        };
        let mut command = match build_command(false) {
            Ok(command) => command,
            Err(error) => return (failed(error), None),
        };
        let remux = matches!(operation, Operation::Remux);
        let success = if remux {
            ProcessResult::Remuxed {}
        } else {
            ProcessResult::SubtitlesMuxed {}
        };
        if self.options.dry_run {
            self.observer.dry_run_command(&command);
            return (success, None);
        }

        let run = |command: &mut Command| {
            run_command_isolated(command).map_err(|error| format!("Failed to execute ffmpeg: {error}"))
        };
        let status = match run(&mut command) {
            Ok(status) => status,
            Err(error) => return (failed(error), None),
        };
        let status = if remux && !status.success() {
            if abort_flag.load(Ordering::SeqCst) {
                return (failed("Remux aborted".to_string()), None);
            }
            self.observer.retrying(Retry::AacAudio { status });
            match clear_staged_output(command_output)
                .and_then(|()| build_command(true))
                .and_then(|mut retry| run(&mut retry))
            {
                Ok(status) => status,
                Err(error) => return (failed(error), None),
            }
        } else {
            status
        };
        if !status.success() {
            return (
                failed(format!(
                    "ffmpeg movie mux failed with status: {}",
                    status.code().unwrap_or(-1)
                )),
                None,
            );
        }
        let Some(staged) = staged else {
            return (success, None);
        };

        let output_info = match validate_mux_output(input, staged.path(), info).and_then(|output_info| {
            validate_movie_subtitles(input, staged.path(), &file.subtitle_files).map(|()| output_info)
        }) {
            Ok(output_info) => output_info,
            Err(error) => return (failed(error.to_string()), None),
        };
        if let Err(error) = self.publish_output(staged, input, final_output, in_place) {
            return (failed(error.to_string()), None);
        }

        self.delete_subtitle_files(&file.subtitle_files);

        (success, Some(output_info))
    }

    /// Convert video to HEVC.
    pub(super) fn convert_to_hevc(
        &self,
        file: &ProcessableFile,
        position: FilePosition,
        abort_flag: &AtomicBool,
    ) -> (ProcessResult, Option<VideoInfo>) {
        let input = &file.file.path;
        let output = &file.output_path;
        let info = &file.info;
        let extension = &file.file.extension;

        let quality_level = if self.options.movie_mode {
            info.quality_level().saturating_sub(2)
        } else {
            info.quality_level()
        };

        self.observer
            .file_started(Operation::Convert, position, file, Some(quality_level));

        // Determine audio codec: copy for mp4/mkv, transcode for others
        let copy_audio = extension == "mp4" || extension == "mkv";

        let staged = if self.options.dry_run {
            None
        } else {
            match StagedOutput::create(output) {
                Ok(staged) => Some(staged),
                Err(error) => return (failed(format!("Failed to stage conversion output: {error}")), None),
            }
        };
        let subtitle_files = if self.options.movie_mode {
            file.subtitle_files.as_slice()
        } else {
            &[]
        };
        let make_options = |command_output| {
            ConversionOptions::new(
                input,
                command_output,
                quality_level,
                copy_audio,
                self.options.movie_mode,
                subtitle_files,
                info.bit_depth,
            )
        };
        let Some(staged) = staged else {
            match build_conversion_command(&make_options(output)) {
                Ok(command) => self.observer.dry_run_command(&command),
                Err(error) => return (failed(format!("Failed to build ffmpeg command: {error}")), None),
            }
            return (
                ProcessResult::converted(info.size_bytes, info.bitrate_kbps, info.size_bytes, info.bitrate_kbps),
                None,
            );
        };
        let conversion_options = make_options(staged.path());
        let mut ffmpeg_command = match build_conversion_command(&conversion_options) {
            Ok(command) => command,
            Err(error) => return (failed(format!("Failed to build ffmpeg command: {error}")), None),
        };

        let output_info = match self.run_conversion(
            &mut ffmpeg_command,
            conversion_options,
            staged.path(),
            quality_level,
            info,
            abort_flag,
        ) {
            Ok(output_info) => output_info,
            Err(error) => return (failed(error), None),
        };

        // Validate output duration
        if output_info.duration < info.duration * MIN_DURATION_RATIO {
            return (
                failed(format!(
                    "Output duration {:.1}s is less than {:.0}% of original {:.1}s",
                    output_info.duration,
                    MIN_DURATION_RATIO * 100.0,
                    info.duration
                )),
                None,
            );
        }

        if self.options.movie_mode
            && let Err(error) = validate_movie_subtitles(input, staged.path(), subtitle_files)
        {
            return (failed(error.to_string()), None);
        }
        if let Err(error) = self.publish_output(staged, input, output, false) {
            return (failed(error.to_string()), None);
        }
        self.delete_subtitle_files(subtitle_files);

        (
            ProcessResult::Converted {
                stats: ConversionStats::new(
                    info.size_bytes,
                    info.bitrate_kbps,
                    output_info.size_bytes,
                    output_info.bitrate_kbps,
                ),
            },
            Some(output_info),
        )
    }

    /// Install a validated output before disposing of the original.
    fn publish_output(&self, staged: StagedOutput, input: &Path, output: &Path, in_place: bool) -> Result<()> {
        if in_place {
            let result = self.replace_input_with_output(input, output, staged.path());
            if result.is_ok() {
                // The staged file has been renamed to `final_output`.
                // Avoid attempting to delete it on drop.
                let _ = staged.into_temp_path();
            }
            return result;
        }
        let Some(temp_path) = staged.into_temp_path() else {
            anyhow::bail!("Staged output handle was already consumed");
        };
        if self.options.overwrite {
            temp_path.persist(output)
        } else {
            temp_path.persist_noclobber(output)
        }
        .context("Failed to install validated output")?;
        self.delete_file_reporting_failure(input);
        Ok(())
    }

    /// Run the conversion and return the probed output.
    ///
    /// When the CUDA filters fail, the conversion is retried with CPU filtering.
    /// When the output is larger than the input, the file is converted once more at a lower quality.
    /// A failed run removes its partial output and returns the error message.
    fn run_conversion(
        &self,
        ffmpeg_command: &mut Command,
        mut conversion_options: ConversionOptions<'_>,
        output: &Path,
        quality_level: u8,
        info: &VideoInfo,
        abort_flag: &AtomicBool,
    ) -> Result<VideoInfo, String> {
        // First attempt: try with CUDA filters for better performance
        let status =
            run_command_isolated(ffmpeg_command).map_err(|error| format!("Failed to execute ffmpeg: {error}"))?;

        if !status.success() {
            if abort_flag.load(Ordering::SeqCst) {
                return Err("Conversion aborted".to_string());
            }
            clear_staged_output(output)?;

            // Retry without CUDA filters (fallback for format compatibility issues)
            self.observer.retrying(Retry::CpuFilters);
            conversion_options = conversion_options.without_cuda_filters();
            let mut retry_command = build_conversion_command(&conversion_options)
                .map_err(|error| format!("Failed to build retry ffmpeg command: {error}"))?;
            let status = run_command_isolated(&mut retry_command)
                .map_err(|error| format!("Failed to execute ffmpeg (retry): {error}"))?;

            if !status.success() {
                return Err(format!("ffmpeg failed with status: {}", status.code().unwrap_or(-1)));
            }
        }

        let output_info = probe_video_info(output).map_err(|error| format!("Failed to get output info: {error}"))?;

        if output_info.size_bytes <= info.size_bytes {
            return Ok(output_info);
        }

        if abort_flag.load(Ordering::SeqCst) {
            return Err("Conversion aborted before lower-quality retry".to_string());
        }

        // The output is larger than the input, so reconvert once with lower quality
        let quality_level = quality_level + 2;
        self.observer.retrying(Retry::LowerQuality {
            output_size: output_info.size_bytes,
            input_size: info.size_bytes,
            quality_level,
        });
        clear_staged_output(output)?;

        conversion_options = conversion_options.with_quality_level(quality_level);
        let mut reconvert_command = build_conversion_command(&conversion_options)
            .map_err(|error| format!("Failed to build reconvert ffmpeg command: {error}"))?;
        let status = run_command_isolated(&mut reconvert_command)
            .map_err(|error| format!("Failed to execute ffmpeg (reconvert): {error}"))?;

        if !status.success() {
            return Err(format!(
                "ffmpeg reconversion failed with status: {}",
                status.code().unwrap_or(-1)
            ));
        }

        probe_video_info(output).map_err(|error| format!("Failed to get reconverted video info: {error}"))
    }
}

/// Reserve a hidden sibling with the final extension so ffmpeg selects the right container.
fn stage_output(output: &Path) -> Result<TempPath> {
    let parent = output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let extension = cli_tools_common::path_to_file_extension_string(output);
    Ok(tempfile::Builder::new()
        .prefix(".vconvert-")
        .suffix(&format!(".{extension}"))
        .tempfile_in(parent)?
        .into_temp_path())
}

/// Clear a previous attempt so even a successful retry that writes nothing cannot publish stale bytes.
fn clear_staged_output(output: &Path) -> Result<(), String> {
    std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(output)
        .map(|_| ())
        .map_err(|error| format!("Failed to clear staged output: {error}"))
}

/// A failed processing result with the given error message.
const fn failed(error: String) -> ProcessResult {
    ProcessResult::Failed { error }
}

#[cfg(test)]
mod test_dryrun_operations {
    use super::super::test_helpers::{converter, processable};
    use super::*;
    use crate::options::ConvertOptions;

    fn dry_run_options() -> ConvertOptions {
        ConvertOptions {
            dry_run: true,
            ..ConvertOptions::default()
        }
    }

    #[test]
    fn dryrun_remux_reports_success_without_touching_files() {
        let directory = tempfile::tempdir().expect("Failed to create temporary directory");
        let path = directory.path().join("Movie.mkv");
        std::fs::write(&path, b"video").expect("Failed to write video file");
        let file = processable(&path, "hevc");
        let converter = converter(dry_run_options());

        let (result, _) = converter.remux_to_mp4(&file, FilePosition { index: 1, total: 1 }, &AtomicBool::new(false));

        assert!(matches!(result, ProcessResult::Remuxed {}));
        assert!(path.is_file());
        assert!(!file.output_path.exists());
    }

    #[test]
    fn dryrun_conversion_reports_the_original_size_without_touching_files() {
        let directory = tempfile::tempdir().expect("Failed to create temporary directory");
        let path = directory.path().join("Movie.avi");
        std::fs::write(&path, b"video").expect("Failed to write video file");
        let file = processable(&path, "h264");
        let converter = converter(dry_run_options());

        let (result, _) =
            converter.convert_to_hevc(&file, FilePosition { index: 1, total: 1 }, &AtomicBool::new(false));

        assert!(matches!(result, ProcessResult::Converted { .. }));
        assert!(path.is_file());
        assert!(!file.output_path.exists());
    }
}

#[cfg(test)]
mod test_in_place_case_insensitive {
    use super::super::test_helpers::converter;
    use super::*;
    use crate::classification::ClassificationRequest;
    use crate::options::ConvertOptions;
    use crate::types::{AnalysisFilter, AnalysisResult, SubtitleFile, VideoFile};

    #[test]
    fn refuses_in_place_conversion_when_only_extension_case_differs() {
        let directory = tempfile::tempdir().expect("temp dir");
        if cli_tools_common::directory_is_case_sensitive(directory.path()).expect("probe") {
            return;
        }
        let path = directory.path().join("x265.MP4");
        std::fs::write(&path, b"video").expect("write");

        let file = VideoFile::new_with_metadata(&path);
        let info = VideoInfo {
            codec: "h264".to_string(),
            bitrate_kbps: 20_000,
            size_bytes: 5,
            duration: 60.0,
            width: 1920,
            height: 1080,
            frames_per_second: 30.0,
            bit_depth: 8,
            warning: None,
        };
        let options = ConvertOptions {
            overwrite: true,
            ..ConvertOptions::default()
        };
        let filter = AnalysisFilter::from(&options);
        let result = ClassificationRequest::new(&filter, &info, false, Vec::new()).classify(file);
        assert!(
            matches!(result, AnalysisResult::Skip { .. }),
            "in-place conversion with differing extension case must be refused, got {result:?}"
        );
    }

    #[test]
    fn uppercase_mkv_subtitle_mux_preserves_video_on_case_insensitive_fs() {
        if !cli_tools_common::command_exists("ffmpeg") || !cli_tools_common::command_exists("ffprobe") {
            return;
        }
        let directory = tempfile::tempdir().expect("temp dir");
        if cli_tools_common::directory_is_case_sensitive(directory.path()).expect("probe") {
            return;
        }
        let input_path = directory.path().join("Movie.x265.MKV");
        let status = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=black:s=64x64:d=1",
                "-c:v",
                "libx265",
                "-tag:v",
                "hvc1",
                "-y",
            ])
            .arg(&input_path)
            .status()
            .expect("generate mkv");
        if !status.success() {
            return;
        }
        let subtitle_path = directory.path().join("Movie.x265.srt");
        std::fs::write(&subtitle_path, "1\n00:00:00,000 --> 00:00:00,500\nHello\n").expect("write srt");

        let video_file = VideoFile::new_with_metadata(&input_path);
        let info = probe_video_info(&input_path).expect("probe");
        let output_path = directory.path().join("Movie.x265.mkv");
        let processable = ProcessableFile::new(
            video_file,
            info,
            output_path.clone(),
            vec![SubtitleFile::new(&subtitle_path, None)],
        );
        let options = ConvertOptions {
            movie_mode: true,
            overwrite: true,
            delete: true,
            ..ConvertOptions::default()
        };
        let video_convert = converter(options);

        let (result, _) = video_convert.mux_subtitles(
            &processable,
            FilePosition { index: 1, total: 1 },
            &AtomicBool::new(false),
        );
        assert!(matches!(result, ProcessResult::SubtitlesMuxed {}), "got {result:?}");
        assert!(
            output_path.is_file(),
            "muxed video must exist after in-place subtitle mux"
        );
        assert!(
            std::fs::metadata(&output_path).expect("meta").len() > 0,
            "muxed video must be non-empty"
        );
        assert!(!subtitle_path.exists(), "muxed subtitle sidecar should be removed");
    }
}

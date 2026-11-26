//! Per-file ffmpeg operations for video convert.
//!
//! Remuxes HEVC and AV1 files to MP4, muxes external subtitles in movie mode, and converts video to HEVC,
//! retrying with fallback settings and validating each output before the original is removed.

use std::path::Path;
use std::process::Command;

use super::VideoConvert;
use crate::ffmpeg::{
    ConversionOptions, build_conversion_command, build_remux_command, build_subtitle_mux_command, probe_video_info,
    run_command_isolated, validate_mux_output,
};
use crate::helpers::temporary_output_path;
use crate::observer::{FilePosition, Operation, Retry};
use crate::stats::ConversionStats;
use crate::types::{ProcessResult, ProcessableFile, VideoInfo};

/// Minimum ratio of output duration to input duration for a conversion to be considered successful.
const MIN_DURATION_RATIO: f64 = 0.85;

impl VideoConvert<'_> {
    /// Remux HEVC or AV1 to MP4 container
    pub(super) fn remux_to_mp4(&self, file: &ProcessableFile, position: FilePosition) -> ProcessResult {
        let input = &file.file.path;
        let output = &file.output_path;
        let info = &file.info;
        let codec = info.codec_suffix();

        self.observer.file_started(Operation::Remux, position, file, None);

        let mut command = build_remux_command(input, output, false, codec);

        if self.options.dry_run {
            self.observer.dry_run_command(&command);
            return ProcessResult::Remuxed {};
        }

        let status = match run_command_isolated(&mut command) {
            Ok(status) => status,
            Err(error) => {
                self.remove_partial_output(output);
                return failed(format!("Failed to execute ffmpeg: {error}"));
            }
        };

        if status.success() {
            if let Some(failure) = self.reject_invalid_remux(input, output, info) {
                return failure;
            }
            self.delete_file_reporting_failure(input);
            return ProcessResult::Remuxed {};
        }

        // Fallback: if audio codec is not MP4-friendly, transcode audio to AAC
        self.observer.retrying(Retry::AacAudio { status });

        // Remove failed output file if it exists
        if output.exists() {
            self.remove_partial_output(output);
        }

        let mut command = build_remux_command(input, output, true, codec);

        let status = match run_command_isolated(&mut command) {
            Ok(status) => status,
            Err(error) => return failed(format!("Failed to execute ffmpeg: {error}")),
        };

        if !status.success() {
            self.remove_partial_output(output);
            return failed(format!(
                "ffmpeg remux with AAC transcode failed with status: {}",
                status.code().unwrap_or(-1)
            ));
        }

        if let Some(failure) = self.reject_invalid_remux(input, output, info) {
            return failure;
        }
        self.delete_file_reporting_failure(input);

        ProcessResult::Remuxed {}
    }

    /// Process movie-mode audio and subtitle streams without converting video.
    pub(super) fn mux_subtitles(&self, file: &ProcessableFile, position: FilePosition) -> ProcessResult {
        let input = &file.file.path;
        let final_output = &file.output_path;
        let in_place = cli_tools_common::paths_refer_to_same_file(input, final_output);
        let command_output = if in_place {
            temporary_output_path(final_output)
        } else {
            final_output.clone()
        };
        let info = &file.info;

        self.observer.file_started(Operation::SubtitleMux, position, file, None);

        let mut command = match build_subtitle_mux_command(input, &command_output, &file.subtitle_files) {
            Ok(command) => command,
            Err(error) => return failed(format!("Failed to build subtitle mux command: {error}")),
        };

        if self.options.dry_run {
            self.observer.dry_run_command(&command);
            return ProcessResult::SubtitlesMuxed {};
        }

        let status = match run_command_isolated(&mut command) {
            Ok(status) => status,
            Err(error) => {
                self.remove_partial_output(&command_output);
                return failed(format!("Failed to execute ffmpeg: {error}"));
            }
        };

        if !status.success() {
            self.remove_partial_output(&command_output);
            return failed(format!(
                "ffmpeg subtitle mux failed with status: {}",
                status.code().unwrap_or(-1)
            ));
        }

        if let Err(error) = validate_mux_output(input, &command_output, info) {
            self.delete_file_reporting_failure(&command_output);
            return failed(error.to_string());
        }

        if in_place {
            if let Err(error) = self.replace_input_with_output(input, final_output, &command_output) {
                return failed(error.to_string());
            }
        } else {
            self.delete_file_reporting_failure(input);
        }

        self.delete_subtitle_files(&file.subtitle_files);

        ProcessResult::SubtitlesMuxed {}
    }

    /// Convert video to HEVC using NVENC
    pub(super) fn convert_to_hevc(&self, file: &ProcessableFile, position: FilePosition) -> ProcessResult {
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

        let conversion_options = ConversionOptions::new(
            input,
            output,
            quality_level,
            copy_audio,
            self.options.movie_mode,
            &file.subtitle_files,
            info.bit_depth,
        );
        let mut ffmpeg_command = match build_conversion_command(&conversion_options) {
            Ok(command) => command,
            Err(error) => return failed(format!("Failed to build ffmpeg command: {error}")),
        };

        if self.options.dry_run {
            self.observer.dry_run_command(&ffmpeg_command);
            return ProcessResult::converted(info.size_bytes, info.bitrate_kbps, 0, 0);
        }

        let output_info =
            match self.run_conversion(&mut ffmpeg_command, conversion_options, output, quality_level, info) {
                Ok(output_info) => output_info,
                Err(error) => return failed(error),
            };

        // Validate output duration
        if output_info.duration < info.duration * MIN_DURATION_RATIO {
            self.delete_file_reporting_failure(output);
            return failed(format!(
                "Output duration {:.1}s is less than {:.0}% of original {:.1}s",
                output_info.duration,
                MIN_DURATION_RATIO * 100.0,
                info.duration
            ));
        }

        self.delete_file_reporting_failure(input);
        self.delete_subtitle_files(&file.subtitle_files);

        ProcessResult::Converted {
            stats: ConversionStats::new(
                info.size_bytes,
                info.bitrate_kbps,
                output_info.size_bytes,
                output_info.bitrate_kbps,
            ),
        }
    }

    /// Check a remux output before the original is deleted,
    /// removing the output and returning a failure when it is invalid.
    ///
    /// A stream copy can exit successfully on a truncated or corrupt input,
    /// so the output has to keep the duration and audio of the input.
    fn reject_invalid_remux(&self, input: &Path, output: &Path, info: &VideoInfo) -> Option<ProcessResult> {
        let error = validate_mux_output(input, output, info).err()?;
        self.remove_partial_output(output);
        Some(failed(format!("Remux output failed validation: {error:#}")))
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
    ) -> Result<VideoInfo, String> {
        // First attempt: try with CUDA filters for better performance
        let status = run_command_isolated(ffmpeg_command).map_err(|error| {
            self.remove_partial_output(output);
            format!("Failed to execute ffmpeg: {error}")
        })?;

        if !status.success() {
            self.remove_partial_output(output);

            // Retry without CUDA filters (fallback for format compatibility issues)
            self.observer.retrying(Retry::CpuFilters);
            conversion_options = conversion_options.without_cuda_filters();
            let mut retry_command = build_conversion_command(&conversion_options)
                .map_err(|error| format!("Failed to build retry ffmpeg command: {error}"))?;
            let status = run_command_isolated(&mut retry_command)
                .map_err(|error| format!("Failed to execute ffmpeg (retry): {error}"))?;

            if !status.success() {
                self.remove_partial_output(output);
                return Err(format!("ffmpeg failed with status: {}", status.code().unwrap_or(-1)));
            }
        }

        let output_info = probe_video_info(output).map_err(|error| {
            self.remove_partial_output(output);
            format!("Failed to get output info: {error}")
        })?;

        if output_info.size_bytes <= info.size_bytes {
            return Ok(output_info);
        }

        // The output is larger than the input, so reconvert once with lower quality
        let quality_level = quality_level + 2;
        self.observer.retrying(Retry::LowerQuality {
            output_size: output_info.size_bytes,
            input_size: info.size_bytes,
            quality_level,
        });
        self.remove_partial_output(output);

        conversion_options = conversion_options.with_quality_level(quality_level);
        let mut reconvert_command = build_conversion_command(&conversion_options)
            .map_err(|error| format!("Failed to build reconvert ffmpeg command: {error}"))?;
        let status = run_command_isolated(&mut reconvert_command)
            .map_err(|error| format!("Failed to execute ffmpeg (reconvert): {error}"))?;

        if !status.success() {
            self.remove_partial_output(output);
            return Err(format!(
                "ffmpeg reconversion failed with status: {}",
                status.code().unwrap_or(-1)
            ));
        }

        probe_video_info(output).map_err(|error| {
            self.remove_partial_output(output);
            format!("Failed to get reconverted video info: {error}")
        })
    }
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

        let result = converter.remux_to_mp4(&file, FilePosition { index: 1, total: 1 });

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

        let result = converter.convert_to_hevc(&file, FilePosition { index: 1, total: 1 });

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

        let result = video_convert.mux_subtitles(&processable, FilePosition { index: 1, total: 1 });
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

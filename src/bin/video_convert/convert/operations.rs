//! Per-file ffmpeg operations for video convert.
//!
//! Remuxes HEVC and AV1 files to MP4, muxes external subtitles in movie mode, and converts video to HEVC,
//! retrying with fallback settings and validating each output before the original is removed.

use std::time::Instant;

use colored::Colorize;

use cli_tools::{print_error, print_yellow};

use super::VideoConvert;
use crate::ffmpeg::{
    ConversionOptions, build_conversion_command, build_remux_command, build_subtitle_mux_command, probe_video_info,
    run_command_isolated, validate_mux_output,
};
use crate::helpers::{remove_partial_output, temporary_output_path};
use crate::stats::ConversionStats;
use crate::types::{ProcessResult, ProcessableFile, print_subtitle_files};

/// Minimum ratio of output duration to input duration for a conversion to be considered successful.
const MIN_DURATION_RATIO: f64 = 0.85;

impl VideoConvert {
    /// Remux HEVC or AV1 to MP4 container
    pub(super) fn remux_to_mp4(&self, file: &ProcessableFile, file_index: &str) -> ProcessResult {
        let input = &file.file.path;
        let output = &file.output_path;
        let info = &file.info;
        let codec = info.codec_suffix();

        println!(
            "{}",
            format!("{file_index} Remux: {}", cli_tools::path_to_string_relative(input))
                .bold()
                .green()
        );
        println!("{info}");

        if self.config.verbose {
            println!("Output: {}", cli_tools::path_to_string_relative(output));
        }

        self.log_start(input, "remux", file_index, info, None);
        let start = Instant::now();

        let mut cmd = build_remux_command(input, output, false, codec);

        if self.config.dryrun {
            println!("[DRYRUN] {cmd:#?}");
            return ProcessResult::Remuxed {};
        }

        let status = match run_command_isolated(&mut cmd) {
            Ok(s) => s,
            Err(e) => {
                return ProcessResult::Failed {
                    error: format!("Failed to execute ffmpeg: {e}"),
                };
            }
        };

        if status.success() {
            if let Err(e) = self.delete_file(input) {
                print_error!("Failed to delete original file: {e}");
            }
            let duration = start.elapsed();
            println!(
                "{}",
                format!("✓ Remuxed in {}", cli_tools::format_duration(duration)).green()
            );
            self.log_success(output, "remux", file_index, duration, None);
            return ProcessResult::Remuxed {};
        }

        // Fallback: if audio codec is not MP4-friendly, transcode audio to AAC
        print_yellow!("Remux failed with code {status}. Retrying with AAC audio transcode...");

        // Remove failed output file if it exists
        if output.exists() {
            remove_partial_output(output);
        }

        let mut cmd = build_remux_command(input, output, true, codec);

        let status = match run_command_isolated(&mut cmd) {
            Ok(s) => s,
            Err(e) => {
                return ProcessResult::Failed {
                    error: format!("Failed to execute ffmpeg: {e}"),
                };
            }
        };

        if !status.success() {
            remove_partial_output(output);
            let error = format!(
                "ffmpeg remux with AAC transcode failed with status: {}",
                status.code().unwrap_or(-1)
            );
            self.log_failure(input, "remux", file_index, &error);
            return ProcessResult::Failed { error };
        }

        if let Err(e) = self.delete_file(input) {
            print_error!("Failed to delete original file: {e}");
        }

        let duration = start.elapsed();
        println!(
            "{}",
            format!("✓ Remuxed in {}", cli_tools::format_duration(duration)).green()
        );
        self.log_success(output, "remux", file_index, duration, None);
        ProcessResult::Remuxed {}
    }

    /// Process movie-mode audio and subtitle streams without converting video.
    pub(super) fn mux_subtitles(&self, file: &ProcessableFile, file_index: &str) -> ProcessResult {
        let input = &file.file.path;
        let final_output = &file.output_path;
        let command_output = if input == final_output {
            temporary_output_path(final_output)
        } else {
            final_output.clone()
        };
        let info = &file.info;

        println!(
            "{}",
            format!(
                "{file_index} Process movie streams: {}",
                cli_tools::path_to_string_relative(input)
            )
            .bold()
            .cyan()
        );
        println!("{info}");

        if self.config.verbose {
            println!("Output: {}", cli_tools::path_to_string_relative(final_output));
            print_subtitle_files(&file.subtitle_files);
        }

        self.log_start(input, "subtitle-mux", file_index, info, None);
        let start = Instant::now();

        let mut cmd = match build_subtitle_mux_command(input, &command_output, &file.subtitle_files) {
            Ok(command) => command,
            Err(error) => {
                return ProcessResult::Failed {
                    error: format!("Failed to build subtitle mux command: {error}"),
                };
            }
        };

        if self.config.dryrun {
            println!("[DRYRUN] {cmd:#?}");
            return ProcessResult::SubtitlesMuxed {};
        }

        let status = match run_command_isolated(&mut cmd) {
            Ok(status) => status,
            Err(error) => {
                return ProcessResult::Failed {
                    error: format!("Failed to execute ffmpeg: {error}"),
                };
            }
        };

        if !status.success() {
            remove_partial_output(&command_output);
            let error = format!(
                "ffmpeg subtitle mux failed with status: {}",
                status.code().unwrap_or(-1)
            );
            self.log_failure(input, "subtitle-mux", file_index, &error);
            return ProcessResult::Failed { error };
        }

        if let Err(error) = validate_mux_output(input, &command_output, info) {
            if let Err(delete_error) = self.delete_file(&command_output) {
                print_error!("Failed to delete invalid subtitle mux output: {delete_error}");
            }
            let error = error.to_string();
            self.log_failure(input, "subtitle-mux", file_index, &error);
            return ProcessResult::Failed { error };
        }

        if input == final_output {
            if let Err(error) = self.replace_input_with_output(input, &command_output) {
                let error = error.to_string();
                self.log_failure(input, "subtitle-mux", file_index, &error);
                return ProcessResult::Failed { error };
            }
        } else if let Err(error) = self.delete_file(input) {
            print_error!("Failed to delete original file: {error}");
        }

        self.delete_subtitle_files(&file.subtitle_files);

        let duration = start.elapsed();
        println!(
            "{}",
            format!("✓ Processed movie streams in {}", cli_tools::format_duration(duration)).green()
        );
        self.log_success(final_output, "subtitle-mux", file_index, duration, None);
        ProcessResult::SubtitlesMuxed {}
    }

    /// Convert video to HEVC using NVENC
    #[allow(clippy::too_many_lines)]
    pub(super) fn convert_to_hevc(&self, file: &ProcessableFile, file_index: &str) -> ProcessResult {
        let input = &file.file.path;
        let output = &file.output_path;
        let info = &file.info;
        let extension = &file.file.extension;

        println!(
            "{}",
            format!("{file_index} Convert: {}", cli_tools::path_to_string_relative(input))
                .bold()
                .magenta()
        );
        println!("{info}");

        let quality_level = if self.config.movie_mode {
            info.quality_level().saturating_sub(2)
        } else {
            info.quality_level()
        };

        if self.config.verbose {
            println!("Output: {}", cli_tools::path_to_string_relative(output));
            println!("Using quality level: {quality_level}");
            print_subtitle_files(&file.subtitle_files);
        }

        self.log_start(input, "convert", file_index, info, Some(quality_level));
        let start = Instant::now();

        // Determine audio codec: copy for mp4/mkv, transcode for others
        let copy_audio = extension == "mp4" || extension == "mkv";

        let mut conversion_options = ConversionOptions::new(
            input,
            output,
            quality_level,
            copy_audio,
            self.config.movie_mode,
            &file.subtitle_files,
            info.bit_depth,
        );
        let mut ffmpeg_command = match build_conversion_command(&conversion_options) {
            Ok(command) => command,
            Err(e) => {
                return ProcessResult::Failed {
                    error: format!("Failed to build ffmpeg command: {e}"),
                };
            }
        };

        if self.config.dryrun {
            println!("[DRYRUN] {ffmpeg_command:#?}");
            return ProcessResult::converted(info.size_bytes, info.bitrate_kbps, 0, 0);
        }

        // First attempt: try with CUDA filters for better performance
        let status = match run_command_isolated(&mut ffmpeg_command) {
            Ok(s) => s,
            Err(e) => {
                return ProcessResult::Failed {
                    error: format!("Failed to execute ffmpeg: {e}"),
                };
            }
        };

        if !status.success() {
            // Clean up failed output file
            remove_partial_output(output);

            // Retry without CUDA filters (fallback for format compatibility issues)
            print_error!("CUDA filter failed, retrying with CPU-based filtering...");
            conversion_options = conversion_options.without_cuda_filters();
            ffmpeg_command = match build_conversion_command(&conversion_options) {
                Ok(command) => command,
                Err(e) => {
                    return ProcessResult::Failed {
                        error: format!("Failed to build retry ffmpeg command: {e}"),
                    };
                }
            };
            let status = match run_command_isolated(&mut ffmpeg_command) {
                Ok(s) => s,
                Err(e) => {
                    return ProcessResult::Failed {
                        error: format!("Failed to execute ffmpeg (retry): {e}"),
                    };
                }
            };

            if !status.success() {
                remove_partial_output(output);
                let error = format!("ffmpeg failed with status: {}", status.code().unwrap_or(-1));
                self.log_failure(input, "convert", file_index, &error);
                return ProcessResult::Failed { error };
            }
        }

        // Get output file info and validate
        let output_info = match probe_video_info(output) {
            Ok(info) => info,
            Err(e) => {
                remove_partial_output(output);
                let error = format!("Failed to get output info: {e}");
                self.log_failure(input, "convert", file_index, &error);
                return ProcessResult::Failed { error };
            }
        };

        // If output is larger than input, reconvert once with lower quality
        let output_info = if output_info.size_bytes > info.size_bytes {
            let new_quality_level = quality_level + 2;
            print_yellow!(
                "Output file ({}) is larger than input ({}), reconverting with lower quality level ({})",
                cli_tools::format_size(output_info.size_bytes),
                cli_tools::format_size(info.size_bytes),
                new_quality_level
            );
            remove_partial_output(output);

            conversion_options = conversion_options.with_quality_level(new_quality_level);
            ffmpeg_command = match build_conversion_command(&conversion_options) {
                Ok(command) => command,
                Err(e) => {
                    let error = format!("Failed to build reconvert ffmpeg command: {e}");
                    self.log_failure(input, "convert", file_index, &error);
                    return ProcessResult::Failed { error };
                }
            };
            let status = match run_command_isolated(&mut ffmpeg_command) {
                Ok(s) => s,
                Err(e) => {
                    let error = format!("Failed to execute ffmpeg (reconvert): {e}");
                    self.log_failure(input, "convert", file_index, &error);
                    return ProcessResult::Failed { error };
                }
            };

            if !status.success() {
                remove_partial_output(output);
                let error = format!(
                    "ffmpeg reconversion failed with status: {}",
                    status.code().unwrap_or(-1)
                );
                self.log_failure(input, "convert", file_index, &error);
                return ProcessResult::Failed { error };
            }

            match probe_video_info(output) {
                Ok(info) => info,
                Err(e) => {
                    remove_partial_output(output);
                    let error = format!("Failed to get reconverted video info: {e}");
                    self.log_failure(input, "convert", file_index, &error);
                    return ProcessResult::Failed { error };
                }
            }
        } else {
            output_info
        };

        // Validate output duration
        if output_info.duration < info.duration * MIN_DURATION_RATIO {
            if let Err(e) = self.delete_file(output) {
                print_error!("Failed to delete output file: {e}");
            }
            let error = format!(
                "Output duration {:.1}s is less than {:.0}% of original {:.1}s",
                output_info.duration,
                MIN_DURATION_RATIO * 100.0,
                info.duration
            );
            self.log_failure(input, "convert", file_index, &error);
            return ProcessResult::Failed { error };
        }

        if let Err(e) = self.delete_file(input) {
            print_error!("Failed to delete original file: {e}");
        }
        self.delete_subtitle_files(&file.subtitle_files);

        let conversion_stats = ConversionStats::new(
            info.size_bytes,
            info.bitrate_kbps,
            output_info.size_bytes,
            output_info.bitrate_kbps,
        );

        let duration = start.elapsed();

        println!(
            "{}",
            format!(
                "✓ Converted in {}: {conversion_stats}",
                cli_tools::format_duration(duration)
            )
            .cyan()
        );

        self.log_success(output, "convert", file_index, duration, Some(&conversion_stats));

        ProcessResult::Converted {
            stats: conversion_stats,
        }
    }
}

#[cfg(test)]
mod test_dryrun_operations {
    use super::super::test_helpers::{converter, processable};
    use super::*;
    use crate::config::Config;

    fn dryrun_config() -> Config {
        Config {
            dryrun: true,
            verbose: true,
            ..Config::default()
        }
    }

    #[test]
    fn dryrun_remux_reports_success_without_touching_files() {
        let directory = tempfile::tempdir().expect("Failed to create temporary directory");
        let path = directory.path().join("Movie.mkv");
        std::fs::write(&path, b"video").expect("Failed to write video file");
        let file = processable(&path, "hevc");
        let (_log, converter) = converter(dryrun_config());

        let result = converter.remux_to_mp4(&file, "[1/1]");

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
        let (_log, converter) = converter(dryrun_config());

        let result = converter.convert_to_hevc(&file, "[1/1]");

        assert!(matches!(result, ProcessResult::Converted { .. }));
        assert!(path.is_file());
        assert!(!file.output_path.exists());
    }
}

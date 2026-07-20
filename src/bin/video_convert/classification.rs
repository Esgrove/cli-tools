//! Video classification rules for the conversion pipeline.
//!
//! Determines whether files should be skipped, renamed, remuxed, or converted, and selects movie-mode streams.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::LazyLock;

use anyhow::{Context, Result};
use regex::Regex;

use crate::config::{MOVIE_AUDIO_LANGUAGES, MOVIE_SUBTITLE_LANGUAGES};
use crate::types::{AnalysisFilter, AnalysisResult, ProcessableFile, SkipReason, SubtitleFile, VideoFile, VideoInfo};

/// Regex to match x265 codec identifier in filenames (case-insensitive, word boundary).
pub static RE_X265: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\bx265\b").expect("Invalid x265 regex"));

/// Regex to match AV1 codec identifier in filenames (case-insensitive, word boundary).
pub static RE_AV1: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\bav1\b").expect("Invalid av1 regex"));

/// Regex to match 10-bit labels in filenames.
pub static RE_10BIT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b10[.\-_\s]*bit\b").expect("Invalid 10-bit regex"));

/// Regex to match source codec identifiers that should be replaced in output filenames.
pub static RE_SOURCE_CODEC: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(?:x264|h\.?264)\b").expect("Invalid source codec regex"));

/// Context required to determine the next processing action for a video file.
///
/// Combines analysis filters, probed video metadata, movie-mode settings, and matched subtitle sidecars.
/// Classification uses this context to skip, rename, remux, or convert the file.
/// In movie mode, it also determines whether audio and subtitle streams need filtering.
pub struct ClassificationRequest<'a> {
    filter: &'a AnalysisFilter,
    info: &'a VideoInfo,
    movie_mode: bool,
    subtitle_files: Vec<SubtitleFile>,
    movie_stream_processing_required: Option<bool>,
}

impl<'a> ClassificationRequest<'a> {
    /// Create a classification request using the selected mode.
    pub(crate) const fn new(
        filter: &'a AnalysisFilter,
        info: &'a VideoInfo,
        movie_mode: bool,
        subtitle_files: Vec<SubtitleFile>,
    ) -> Self {
        Self {
            filter,
            info,
            movie_mode,
            subtitle_files,
            movie_stream_processing_required: None,
        }
    }

    /// Classify one video file using this request.
    pub(crate) fn classify(self, file: VideoFile) -> AnalysisResult {
        let is_target_codec = self.info.is_target_codec();
        let movie_stream_processing_required = match self.movie_stream_processing_required(&file, is_target_codec) {
            Ok(required) => required,
            Err(error) => return Self::analysis_failed(file, error.to_string()),
        };

        if let Some(result) = self.stale_label_result(&file, is_target_codec) {
            return result;
        }
        if let Some(output_path) =
            self.movie_stream_output_path(&file, is_target_codec, movie_stream_processing_required)
        {
            return self.process_movie_streams_result(file, output_path);
        }
        if let Some(result) = self.already_converted_result(&file, is_target_codec) {
            return result;
        }
        if !is_target_codec && let Some(reason) = self.conversion_skip_reason() {
            return AnalysisResult::Skip { file, reason };
        }

        self.processing_result(file, is_target_codec)
    }

    /// Resolve whether movie-mode stream filtering would alter the file.
    fn movie_stream_processing_required(&self, file: &VideoFile, is_target_codec: bool) -> Result<bool> {
        if !self.movie_mode || !is_target_codec {
            return Ok(false);
        }
        self.movie_stream_processing_required
            .map_or_else(|| movie_stream_processing_required(&file.path), Ok)
    }

    /// Return a rename result when stale codec or bit-depth labels are present.
    fn stale_label_result(&self, file: &VideoFile, is_target_codec: bool) -> Option<AnalysisResult> {
        let remove_target_codec_label = !is_target_codec && file.has_target_codec_label();
        let remove_10bit_label = !self.info.is_10_bit() && file.has_10bit_label();
        let output_path = file.get_output_path_without_stale_labels(remove_target_codec_label, remove_10bit_label)?;
        Some(self.rename_result(file.clone(), output_path))
    }

    /// Resolve the movie-stream output path when stream processing is required.
    fn movie_stream_output_path(
        &self,
        file: &VideoFile,
        is_target_codec: bool,
        movie_stream_processing_required: bool,
    ) -> Option<PathBuf> {
        let has_external_subtitles = !self.subtitle_files.is_empty();
        if !self.movie_mode || !is_target_codec || (!has_external_subtitles && !movie_stream_processing_required) {
            return None;
        }
        Some(file.get_output_path_for_mode_and_bit_depth(
            self.info.codec_suffix(),
            true,
            has_external_subtitles,
            self.info.bit_depth,
        ))
    }

    /// Build the result for a required movie-stream processing pass.
    fn process_movie_streams_result(self, file: VideoFile, output_path: PathBuf) -> AnalysisResult {
        if self.output_exists(&file, &output_path) {
            return self.output_exists_result(file, output_path);
        }
        AnalysisResult::NeedsSubtitleMux(ProcessableFile::new(
            file,
            self.info.clone(),
            output_path,
            self.subtitle_files,
        ))
    }

    /// Return the target-container result when the file is already in a target codec.
    fn already_converted_result(&self, file: &VideoFile, is_target_codec: bool) -> Option<AnalysisResult> {
        let suffix = self.info.codec_suffix();
        let target_extension = file.target_extension(self.movie_mode, false);
        if !is_target_codec || file.extension != target_extension {
            return None;
        }
        if !suffix.regex().is_match(&file.name) || (self.info.is_10_bit() && !file.has_10bit_label()) {
            let output_path =
                file.get_output_path_for_mode_and_bit_depth(suffix, self.movie_mode, false, self.info.bit_depth);
            return Some(self.rename_result(file.clone(), output_path));
        }
        Some(AnalysisResult::Skip {
            file: file.clone(),
            reason: SkipReason::AlreadyConverted,
        })
    }

    /// Return the first conversion filter that rejects the file.
    fn conversion_skip_reason(&self) -> Option<SkipReason> {
        let info = self.info;
        let filter = self.filter;
        if info.bitrate_kbps < filter.min_bitrate {
            return Some(SkipReason::BitrateBelowThreshold {
                bitrate: info.bitrate_kbps,
                threshold: filter.min_bitrate,
            });
        }
        if let Some(threshold) = filter.max_bitrate
            && info.bitrate_kbps > threshold
        {
            return Some(SkipReason::BitrateAboveThreshold {
                bitrate: info.bitrate_kbps,
                threshold,
            });
        }
        if let Some(threshold) = filter.min_duration
            && info.duration < threshold
        {
            return Some(SkipReason::DurationBelowThreshold {
                duration: info.duration,
                threshold,
            });
        }
        if let Some(threshold) = filter.max_duration
            && info.duration > threshold
        {
            return Some(SkipReason::DurationAboveThreshold {
                duration: info.duration,
                threshold,
            });
        }
        if let Some(limit) = filter.min_resolution
            && info.width.min(info.height) < limit
        {
            return Some(SkipReason::ResolutionBelowLimit {
                width: info.width,
                height: info.height,
                limit,
            });
        }
        None
    }

    /// Build the final conversion or remux result.
    fn processing_result(self, file: VideoFile, is_target_codec: bool) -> AnalysisResult {
        let output_path = file.get_output_path_for_mode_and_bit_depth(
            self.info.codec_suffix(),
            self.movie_mode,
            !self.subtitle_files.is_empty(),
            self.info.bit_depth,
        );
        if output_path == file.path {
            return Self::analysis_failed(
                file,
                "Output path resolves to the input file, refusing in-place conversion/remux".to_string(),
            );
        }
        if self.output_exists(&file, &output_path) {
            return self.output_exists_result(file, output_path);
        }

        let processable = ProcessableFile::new(file, self.info.clone(), output_path, self.subtitle_files);
        if is_target_codec {
            AnalysisResult::NeedsRemux(processable)
        } else {
            AnalysisResult::NeedsConversion(processable)
        }
    }

    /// Build a rename or output-exists result.
    fn rename_result(&self, file: VideoFile, output_path: PathBuf) -> AnalysisResult {
        if self.output_exists(&file, &output_path) {
            self.output_exists_result(file, output_path)
        } else {
            AnalysisResult::NeedsRename(ProcessableFile::new(file, self.info.clone(), output_path, Vec::new()))
        }
    }

    /// Return whether an output path blocks processing.
    fn output_exists(&self, file: &VideoFile, output_path: &Path) -> bool {
        output_path.exists() && output_path != file.path && !self.filter.overwrite
    }

    /// Build an output-exists result.
    const fn output_exists_result(&self, file: VideoFile, output_path: PathBuf) -> AnalysisResult {
        AnalysisResult::Skip {
            file,
            reason: SkipReason::OutputExists {
                path: output_path,
                source_duration: self.info.duration,
            },
        }
    }

    /// Build an analysis-failed result.
    const fn analysis_failed(file: VideoFile, error: String) -> AnalysisResult {
        AnalysisResult::Skip {
            file,
            reason: SkipReason::AnalysisFailed { error },
        }
    }
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
fn movie_stream_processing_required(input: &Path) -> Result<bool> {
    let audio_languages = probe_stream_languages(input, "a")?;
    let subtitle_languages = probe_stream_languages(input, "s")?;
    Ok(stream_languages_require_movie_processing(
        &audio_languages,
        &subtitle_languages,
    ))
}

/// Return whether applying movie-mode stream maps would remove any internal tracks.
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
mod classification_test_helpers {
    use super::*;

    pub(super) fn classify_video_file(file: VideoFile, filter: &AnalysisFilter, info: &VideoInfo) -> AnalysisResult {
        ClassificationRequest {
            filter,
            info,
            movie_mode: false,
            subtitle_files: Vec::new(),
            movie_stream_processing_required: Some(false),
        }
        .classify(file)
    }

    pub(super) fn classify_movie_file(
        file: VideoFile,
        filter: &AnalysisFilter,
        info: &VideoInfo,
        subtitle_files: Vec<SubtitleFile>,
    ) -> AnalysisResult {
        classify_movie_file_with_stream_state(file, filter, info, subtitle_files, false)
    }

    pub(super) fn classify_movie_file_with_stream_state(
        file: VideoFile,
        filter: &AnalysisFilter,
        info: &VideoInfo,
        subtitle_files: Vec<SubtitleFile>,
        movie_stream_processing_required: bool,
    ) -> AnalysisResult {
        ClassificationRequest {
            filter,
            info,
            movie_mode: true,
            subtitle_files,
            movie_stream_processing_required: Some(movie_stream_processing_required),
        }
        .classify(file)
    }
}

#[cfg(test)]
mod test_classify_already_converted {
    use super::*;
    use crate::types::{AnalysisFilter, AnalysisResult, VideoFile, VideoInfo};

    fn default_filter() -> AnalysisFilter {
        AnalysisFilter {
            min_bitrate: 0,
            max_bitrate: None,
            min_duration: None,
            max_duration: None,
            min_resolution: None,
            overwrite: false,
        }
    }

    fn hevc_info() -> VideoInfo {
        VideoInfo {
            codec: "hevc".to_string(),
            bitrate_kbps: 5000,
            size_bytes: 500_000_000,
            duration: 3600.0,
            width: 1920,
            height: 1080,
            frames_per_second: 24.0,
            bit_depth: 8,
            warning: None,
        }
    }

    #[test]
    fn hevc_mp4_with_x265_suffix_is_already_converted() {
        let file = VideoFile::new(Path::new("/videos/movie.x265.mp4"), 0);
        let info = hevc_info();
        let filter = default_filter();

        let result = classification_test_helpers::classify_video_file(file, &filter, &info);

        assert!(
            matches!(
                result,
                AnalysisResult::Skip {
                    reason: SkipReason::AlreadyConverted,
                    ..
                }
            ),
            "Expected AlreadyConverted skip, got: {result:?}"
        );
    }

    #[test]
    fn hevc_mp4_with_x265_suffix_uppercase_is_already_converted() {
        let file = VideoFile::new(Path::new("/videos/movie.X265.mp4"), 0);
        let info = hevc_info();
        let filter = default_filter();

        let result = classification_test_helpers::classify_video_file(file, &filter, &info);

        assert!(
            matches!(
                result,
                AnalysisResult::Skip {
                    reason: SkipReason::AlreadyConverted,
                    ..
                }
            ),
            "Expected AlreadyConverted skip, got: {result:?}"
        );
    }

    #[test]
    fn av1_mp4_with_av1_suffix_is_already_converted() {
        let file = VideoFile::new(Path::new("/videos/movie.av1.mp4"), 0);
        let info = VideoInfo {
            codec: "av1".to_string(),
            bitrate_kbps: 3000,
            size_bytes: 300_000_000,
            duration: 1800.0,
            width: 3840,
            height: 2160,
            frames_per_second: 30.0,
            bit_depth: 8,
            warning: None,
        };
        let filter = default_filter();

        let result = classification_test_helpers::classify_video_file(file, &filter, &info);

        assert!(
            matches!(
                result,
                AnalysisResult::Skip {
                    reason: SkipReason::AlreadyConverted,
                    ..
                }
            ),
            "Expected AlreadyConverted skip, got: {result:?}"
        );
    }
}

#[cfg(test)]
mod test_classify_needs_rename {
    use super::*;
    use crate::types::{AnalysisFilter, AnalysisResult, VideoFile, VideoInfo};

    fn default_filter() -> AnalysisFilter {
        AnalysisFilter {
            min_bitrate: 0,
            max_bitrate: None,
            min_duration: None,
            max_duration: None,
            min_resolution: None,
            overwrite: false,
        }
    }

    #[test]
    fn hevc_mp4_without_suffix_needs_rename() {
        let file = VideoFile::new(Path::new("/videos/movie.mp4"), 0);
        let info = VideoInfo {
            codec: "hevc".to_string(),
            bitrate_kbps: 5000,
            size_bytes: 500_000_000,
            duration: 3600.0,
            width: 1920,
            height: 1080,
            frames_per_second: 24.0,
            bit_depth: 8,
            warning: None,
        };
        let filter = default_filter();

        let result = classification_test_helpers::classify_video_file(file, &filter, &info);

        assert!(
            matches!(result, AnalysisResult::NeedsRename(..)),
            "Expected NeedsRename, got: {result:?}"
        );
    }

    #[test]
    fn av1_mp4_without_suffix_needs_rename() {
        let file = VideoFile::new(Path::new("/videos/movie.mp4"), 0);
        let info = VideoInfo {
            codec: "av1".to_string(),
            bitrate_kbps: 3000,
            size_bytes: 300_000_000,
            duration: 1800.0,
            width: 3840,
            height: 2160,
            frames_per_second: 30.0,
            bit_depth: 8,
            warning: None,
        };
        let filter = default_filter();

        let result = classification_test_helpers::classify_video_file(file, &filter, &info);

        assert!(
            matches!(result, AnalysisResult::NeedsRename(..)),
            "Expected NeedsRename, got: {result:?}"
        );
    }
}

#[cfg(test)]
mod test_classify_needs_conversion {
    use super::*;
    use crate::types::{AnalysisFilter, AnalysisResult, VideoFile, VideoInfo};

    fn default_filter() -> AnalysisFilter {
        AnalysisFilter {
            min_bitrate: 0,
            max_bitrate: None,
            min_duration: None,
            max_duration: None,
            min_resolution: None,
            overwrite: false,
        }
    }

    fn h264_info() -> VideoInfo {
        VideoInfo {
            codec: "h264".to_string(),
            bitrate_kbps: 10000,
            size_bytes: 1_000_000_000,
            duration: 3600.0,
            width: 1920,
            height: 1080,
            frames_per_second: 24.0,
            bit_depth: 8,
            warning: None,
        }
    }

    #[test]
    fn h264_mkv_needs_conversion() {
        let file = VideoFile::new(Path::new("/videos/movie.mkv"), 0);
        let info = h264_info();
        let filter = default_filter();

        let result = classification_test_helpers::classify_video_file(file, &filter, &info);

        assert!(
            matches!(result, AnalysisResult::NeedsConversion(..)),
            "Expected NeedsConversion, got: {result:?}"
        );
    }

    #[test]
    fn h264_mkv_movie_mode_converts_to_mkv() {
        let file = VideoFile::new(Path::new("/videos/movie.mkv"), 0);
        let info = h264_info();
        let filter = default_filter();

        let result = classification_test_helpers::classify_movie_file(file, &filter, &info, Vec::new());

        let AnalysisResult::NeedsConversion(processable) = result else {
            panic!("Expected NeedsConversion");
        };
        assert_eq!(processable.output_path, PathBuf::from("/videos/movie.x265.mkv"));
    }

    #[test]
    fn h264_file_already_named_x265_renames_to_remove_stale_label() {
        let file = VideoFile::new(Path::new("/videos/movie.x265.mkv"), 0);
        let info = h264_info();
        let filter = default_filter();

        let result = classification_test_helpers::classify_movie_file(file, &filter, &info, Vec::new());

        let AnalysisResult::NeedsRename(processable) = result else {
            panic!("Expected NeedsRename");
        };
        assert_eq!(processable.output_path, PathBuf::from("/videos/movie.mkv"));
    }

    #[test]
    fn h264_8bit_file_named_10bit_renames_to_remove_stale_label() {
        let file = VideoFile::new(Path::new("/videos/movie.10bit.mkv"), 0);
        let info = h264_info();
        let filter = default_filter();

        let result = classification_test_helpers::classify_movie_file(file, &filter, &info, Vec::new());

        let AnalysisResult::NeedsRename(processable) = result else {
            panic!("Expected NeedsRename");
        };
        assert_eq!(processable.output_path, PathBuf::from("/videos/movie.mkv"));
    }

    #[test]
    fn h264_10bit_file_converts_to_output_with_10bit_label() {
        let file = VideoFile::new(Path::new("/videos/movie.mkv"), 0);
        let mut info = h264_info();
        info.bit_depth = 10;
        let filter = default_filter();

        let result = classification_test_helpers::classify_movie_file(file, &filter, &info, Vec::new());

        let AnalysisResult::NeedsConversion(processable) = result else {
            panic!("Expected NeedsConversion");
        };
        assert_eq!(processable.output_path, PathBuf::from("/videos/movie.10bit.x265.mkv"));
    }

    #[test]
    fn h264_mp4_needs_conversion() {
        let file = VideoFile::new(Path::new("/videos/movie.mp4"), 0);
        let info = h264_info();
        let filter = default_filter();

        let result = classification_test_helpers::classify_video_file(file, &filter, &info);

        assert!(
            matches!(result, AnalysisResult::NeedsConversion(..)),
            "Expected NeedsConversion, got: {result:?}"
        );
    }

    #[test]
    fn mpeg4_avi_needs_conversion() {
        let file = VideoFile::new(Path::new("/videos/movie.avi"), 0);
        let info = VideoInfo {
            codec: "mpeg4".to_string(),
            bitrate_kbps: 15000,
            size_bytes: 2_000_000_000,
            duration: 7200.0,
            width: 1280,
            height: 720,
            frames_per_second: 30.0,
            bit_depth: 8,
            warning: None,
        };
        let filter = default_filter();

        let result = classification_test_helpers::classify_video_file(file, &filter, &info);

        assert!(
            matches!(result, AnalysisResult::NeedsConversion(..)),
            "Expected NeedsConversion, got: {result:?}"
        );
    }
}

#[cfg(test)]
mod test_classify_needs_remux {
    use super::*;
    use crate::types::{AnalysisFilter, AnalysisResult, VideoFile, VideoInfo};

    fn default_filter() -> AnalysisFilter {
        AnalysisFilter {
            min_bitrate: 0,
            max_bitrate: None,
            min_duration: None,
            max_duration: None,
            min_resolution: None,
            overwrite: false,
        }
    }

    #[test]
    fn hevc_mkv_needs_remux() {
        let file = VideoFile::new(Path::new("/videos/movie.mkv"), 0);
        let info = VideoInfo {
            codec: "hevc".to_string(),
            bitrate_kbps: 5000,
            size_bytes: 500_000_000,
            duration: 3600.0,
            width: 1920,
            height: 1080,
            frames_per_second: 24.0,
            bit_depth: 8,
            warning: None,
        };
        let filter = default_filter();

        let result = classification_test_helpers::classify_video_file(file, &filter, &info);

        assert!(
            matches!(result, AnalysisResult::NeedsRemux(..)),
            "Expected NeedsRemux, got: {result:?}"
        );
    }

    #[test]
    fn av1_mkv_needs_remux() {
        let file = VideoFile::new(Path::new("/videos/movie.mkv"), 0);
        let info = VideoInfo {
            codec: "av1".to_string(),
            bitrate_kbps: 3000,
            size_bytes: 300_000_000,
            duration: 1800.0,
            width: 3840,
            height: 2160,
            frames_per_second: 30.0,
            bit_depth: 8,
            warning: None,
        };
        let filter = default_filter();

        let result = classification_test_helpers::classify_video_file(file, &filter, &info);

        assert!(
            matches!(result, AnalysisResult::NeedsRemux(..)),
            "Expected NeedsRemux, got: {result:?}"
        );
    }

    #[test]
    fn hevc_mkv_movie_mode_with_suffix_is_already_converted() {
        let file = VideoFile::new(Path::new("/videos/movie.x265.mkv"), 0);
        let info = VideoInfo {
            codec: "hevc".to_string(),
            bitrate_kbps: 5000,
            size_bytes: 500_000_000,
            duration: 3600.0,
            width: 1920,
            height: 1080,
            frames_per_second: 24.0,
            bit_depth: 8,
            warning: None,
        };
        let filter = default_filter();

        let result = classification_test_helpers::classify_movie_file(file, &filter, &info, Vec::new());

        assert!(
            matches!(
                result,
                AnalysisResult::Skip {
                    reason: SkipReason::AlreadyConverted,
                    ..
                }
            ),
            "Expected AlreadyConverted skip, got: {result:?}"
        );
    }

    #[test]
    fn hevc_mkv_movie_mode_with_disallowed_streams_processes_movie_streams() {
        let file = VideoFile::new(Path::new("/videos/movie.x265.mkv"), 0);
        let info = VideoInfo {
            codec: "hevc".to_string(),
            bitrate_kbps: 5000,
            size_bytes: 500_000_000,
            duration: 3600.0,
            width: 1920,
            height: 1080,
            frames_per_second: 24.0,
            bit_depth: 8,
            warning: None,
        };
        let filter = default_filter();

        let result =
            classification_test_helpers::classify_movie_file_with_stream_state(file, &filter, &info, Vec::new(), true);

        let AnalysisResult::NeedsSubtitleMux(processable) = result else {
            panic!("Expected NeedsSubtitleMux");
        };
        assert_eq!(processable.output_path, PathBuf::from("/videos/movie.x265.mkv"));
    }

    #[test]
    fn hevc_10bit_mkv_movie_mode_without_10bit_label_needs_rename() {
        let file = VideoFile::new(Path::new("/videos/movie.x265.mkv"), 0);
        let info = VideoInfo {
            codec: "hevc".to_string(),
            bitrate_kbps: 5000,
            size_bytes: 500_000_000,
            duration: 3600.0,
            width: 1920,
            height: 1080,
            frames_per_second: 24.0,
            bit_depth: 10,
            warning: None,
        };
        let filter = default_filter();

        let result = classification_test_helpers::classify_movie_file(file, &filter, &info, Vec::new());

        let AnalysisResult::NeedsRename(processable) = result else {
            panic!("Expected NeedsRename");
        };
        assert_eq!(processable.output_path, PathBuf::from("/videos/movie.10bit.x265.mkv"));
    }

    #[test]
    fn hevc_8bit_mkv_movie_mode_with_10bit_label_needs_rename() {
        let file = VideoFile::new(Path::new("/videos/movie.10bit.x265.mkv"), 0);
        let info = VideoInfo {
            codec: "hevc".to_string(),
            bitrate_kbps: 5000,
            size_bytes: 500_000_000,
            duration: 3600.0,
            width: 1920,
            height: 1080,
            frames_per_second: 24.0,
            bit_depth: 8,
            warning: None,
        };
        let filter = default_filter();

        let result = classification_test_helpers::classify_movie_file(file, &filter, &info, Vec::new());

        let AnalysisResult::NeedsRename(processable) = result else {
            panic!("Expected NeedsRename");
        };
        assert_eq!(processable.output_path, PathBuf::from("/videos/movie.x265.mkv"));
    }

    #[test]
    fn hevc_mkv_movie_mode_without_suffix_needs_rename() {
        let file = VideoFile::new(Path::new("/videos/movie.mkv"), 0);
        let info = VideoInfo {
            codec: "hevc".to_string(),
            bitrate_kbps: 5000,
            size_bytes: 500_000_000,
            duration: 3600.0,
            width: 1920,
            height: 1080,
            frames_per_second: 24.0,
            bit_depth: 8,
            warning: None,
        };
        let filter = default_filter();

        let result = classification_test_helpers::classify_movie_file(file, &filter, &info, Vec::new());

        let AnalysisResult::NeedsRename(processable) = result else {
            panic!("Expected NeedsRename");
        };
        assert_eq!(processable.output_path, PathBuf::from("/videos/movie.x265.mkv"));
    }

    #[test]
    fn hevc_mkv_movie_mode_with_external_subtitle_needs_subtitle_mux() {
        let file = VideoFile::new(Path::new("/videos/movie.x265.mkv"), 0);
        let info = VideoInfo {
            codec: "hevc".to_string(),
            bitrate_kbps: 5000,
            size_bytes: 500_000_000,
            duration: 3600.0,
            width: 1920,
            height: 1080,
            frames_per_second: 24.0,
            bit_depth: 8,
            warning: None,
        };
        let filter = default_filter();
        let subtitles = vec![SubtitleFile::new(Path::new("/videos/movie.English.x265.srt"), None)];

        let result = classification_test_helpers::classify_movie_file(file, &filter, &info, subtitles);

        let AnalysisResult::NeedsSubtitleMux(processable) = result else {
            panic!("Expected NeedsSubtitleMux");
        };
        assert_eq!(processable.output_path, PathBuf::from("/videos/movie.x265.mkv"));
        assert_eq!(processable.subtitle_files.len(), 1);
    }

    #[test]
    fn hevc_avi_needs_remux() {
        let file = VideoFile::new(Path::new("/videos/movie.avi"), 0);
        let info = VideoInfo {
            codec: "hevc".to_string(),
            bitrate_kbps: 8000,
            size_bytes: 800_000_000,
            duration: 5400.0,
            width: 1920,
            height: 1080,
            frames_per_second: 24.0,
            bit_depth: 8,
            warning: None,
        };
        let filter = default_filter();

        let result = classification_test_helpers::classify_video_file(file, &filter, &info);

        assert!(
            matches!(result, AnalysisResult::NeedsRemux(..)),
            "Expected NeedsRemux, got: {result:?}"
        );
    }
}

#[cfg(test)]
mod test_classify_bitrate_filtering {
    use super::*;
    use crate::types::{AnalysisFilter, AnalysisResult, SkipReason, VideoFile, VideoInfo};

    fn h264_info_with_bitrate(bitrate_kbps: u64) -> VideoInfo {
        VideoInfo {
            codec: "h264".to_string(),
            bitrate_kbps,
            size_bytes: 1_000_000_000,
            duration: 3600.0,
            width: 1920,
            height: 1080,
            frames_per_second: 24.0,
            bit_depth: 8,
            warning: None,
        }
    }

    #[test]
    fn below_min_bitrate_is_skipped() {
        let file = VideoFile::new(Path::new("/videos/movie.mkv"), 0);
        let info = h264_info_with_bitrate(5000);
        let filter = AnalysisFilter {
            min_bitrate: 8000,
            max_bitrate: None,
            min_duration: None,
            max_duration: None,
            min_resolution: None,
            overwrite: false,
        };

        let result = classification_test_helpers::classify_video_file(file, &filter, &info);

        assert!(
            matches!(
                result,
                AnalysisResult::Skip {
                    reason: SkipReason::BitrateBelowThreshold {
                        bitrate: 5000,
                        threshold: 8000
                    },
                    ..
                }
            ),
            "Expected BitrateBelowThreshold skip, got: {result:?}"
        );
    }

    #[test]
    fn above_max_bitrate_is_skipped() {
        let file = VideoFile::new(Path::new("/videos/movie.mkv"), 0);
        let info = h264_info_with_bitrate(60000);
        let filter = AnalysisFilter {
            min_bitrate: 0,
            max_bitrate: Some(50000),
            min_duration: None,
            max_duration: None,
            min_resolution: None,
            overwrite: false,
        };

        let result = classification_test_helpers::classify_video_file(file, &filter, &info);

        assert!(
            matches!(
                result,
                AnalysisResult::Skip {
                    reason: SkipReason::BitrateAboveThreshold {
                        bitrate: 60000,
                        threshold: 50000
                    },
                    ..
                }
            ),
            "Expected BitrateAboveThreshold skip, got: {result:?}"
        );
    }

    #[test]
    fn bitrate_at_min_threshold_is_not_skipped() {
        let file = VideoFile::new(Path::new("/videos/movie.mkv"), 0);
        let info = h264_info_with_bitrate(8000);
        let filter = AnalysisFilter {
            min_bitrate: 8000,
            max_bitrate: None,
            min_duration: None,
            max_duration: None,
            min_resolution: None,
            overwrite: false,
        };

        let result = classification_test_helpers::classify_video_file(file, &filter, &info);

        assert!(
            matches!(result, AnalysisResult::NeedsConversion(..)),
            "Expected NeedsConversion at exact min threshold, got: {result:?}"
        );
    }

    #[test]
    fn bitrate_at_max_threshold_is_not_skipped() {
        let file = VideoFile::new(Path::new("/videos/movie.mkv"), 0);
        let info = h264_info_with_bitrate(50000);
        let filter = AnalysisFilter {
            min_bitrate: 0,
            max_bitrate: Some(50000),
            min_duration: None,
            max_duration: None,
            min_resolution: None,
            overwrite: false,
        };

        let result = classification_test_helpers::classify_video_file(file, &filter, &info);

        assert!(
            matches!(result, AnalysisResult::NeedsConversion(..)),
            "Expected NeedsConversion at exact max threshold, got: {result:?}"
        );
    }

    #[test]
    fn bitrate_filter_does_not_apply_to_remux() {
        // hevc in mkv needs remux — bitrate limits should not block it
        let file = VideoFile::new(Path::new("/videos/movie.mkv"), 0);
        let info = VideoInfo {
            codec: "hevc".to_string(),
            bitrate_kbps: 500,
            size_bytes: 100_000_000,
            duration: 3600.0,
            width: 1920,
            height: 1080,
            frames_per_second: 24.0,
            bit_depth: 8,
            warning: None,
        };
        let filter = AnalysisFilter {
            min_bitrate: 8000,
            max_bitrate: Some(50000),
            min_duration: None,
            max_duration: None,
            min_resolution: None,
            overwrite: false,
        };

        let result = classification_test_helpers::classify_video_file(file, &filter, &info);

        assert!(
            matches!(result, AnalysisResult::NeedsRemux(..)),
            "Expected NeedsRemux (bitrate filter should not apply to remux), got: {result:?}"
        );
    }
}

#[cfg(test)]
mod test_classify_duration_filtering {
    use super::*;
    use crate::types::{AnalysisFilter, AnalysisResult, SkipReason, VideoFile, VideoInfo};

    fn h264_info_with_duration(duration: f64) -> VideoInfo {
        VideoInfo {
            codec: "h264".to_string(),
            bitrate_kbps: 10000,
            size_bytes: 1_000_000_000,
            duration,
            width: 1920,
            height: 1080,
            frames_per_second: 24.0,
            bit_depth: 8,
            warning: None,
        }
    }

    #[test]
    fn below_min_duration_is_skipped() {
        let file = VideoFile::new(Path::new("/videos/clip.mkv"), 0);
        let info = h264_info_with_duration(30.0);
        let filter = AnalysisFilter {
            min_bitrate: 0,
            max_bitrate: None,
            min_duration: Some(60.0),
            max_duration: None,
            min_resolution: None,
            overwrite: false,
        };

        let result = classification_test_helpers::classify_video_file(file, &filter, &info);

        assert!(
            matches!(
                result,
                AnalysisResult::Skip {
                    reason: SkipReason::DurationBelowThreshold { .. },
                    ..
                }
            ),
            "Expected DurationBelowThreshold skip, got: {result:?}"
        );
    }

    #[test]
    fn above_max_duration_is_skipped() {
        let file = VideoFile::new(Path::new("/videos/long.mkv"), 0);
        let info = h264_info_with_duration(14400.0);
        let filter = AnalysisFilter {
            min_bitrate: 0,
            max_bitrate: None,
            min_duration: None,
            max_duration: Some(7200.0),
            min_resolution: None,
            overwrite: false,
        };

        let result = classification_test_helpers::classify_video_file(file, &filter, &info);

        assert!(
            matches!(
                result,
                AnalysisResult::Skip {
                    reason: SkipReason::DurationAboveThreshold { .. },
                    ..
                }
            ),
            "Expected DurationAboveThreshold skip, got: {result:?}"
        );
    }

    #[test]
    fn duration_filter_does_not_apply_to_remux() {
        let file = VideoFile::new(Path::new("/videos/movie.mkv"), 0);
        let info = VideoInfo {
            codec: "hevc".to_string(),
            bitrate_kbps: 5000,
            size_bytes: 500_000_000,
            duration: 10.0,
            width: 1920,
            height: 1080,
            frames_per_second: 24.0,
            bit_depth: 8,
            warning: None,
        };
        let filter = AnalysisFilter {
            min_bitrate: 0,
            max_bitrate: None,
            min_duration: Some(60.0),
            max_duration: Some(7200.0),
            min_resolution: None,
            overwrite: false,
        };

        let result = classification_test_helpers::classify_video_file(file, &filter, &info);

        assert!(
            matches!(result, AnalysisResult::NeedsRemux(..)),
            "Expected NeedsRemux (duration filter should not apply to remux), got: {result:?}"
        );
    }
}

#[cfg(test)]
mod test_classify_resolution_filtering {
    use super::*;
    use crate::types::{AnalysisFilter, AnalysisResult, SkipReason, VideoFile, VideoInfo};

    #[test]
    fn below_min_resolution_is_skipped() {
        let file = VideoFile::new(Path::new("/videos/low_res.mkv"), 0);
        let info = VideoInfo {
            codec: "h264".to_string(),
            bitrate_kbps: 10000,
            size_bytes: 500_000_000,
            duration: 3600.0,
            width: 640,
            height: 480,
            frames_per_second: 24.0,
            bit_depth: 8,
            warning: None,
        };
        let filter = AnalysisFilter {
            min_bitrate: 0,
            max_bitrate: None,
            min_duration: None,
            max_duration: None,
            min_resolution: Some(720),
            overwrite: false,
        };

        let result = classification_test_helpers::classify_video_file(file, &filter, &info);

        assert!(
            matches!(
                result,
                AnalysisResult::Skip {
                    reason: SkipReason::ResolutionBelowLimit {
                        width: 640,
                        height: 480,
                        limit: 720
                    },
                    ..
                }
            ),
            "Expected ResolutionBelowLimit skip, got: {result:?}"
        );
    }

    #[test]
    fn at_min_resolution_is_not_skipped() {
        let file = VideoFile::new(Path::new("/videos/hd.mkv"), 0);
        let info = VideoInfo {
            codec: "h264".to_string(),
            bitrate_kbps: 10000,
            size_bytes: 500_000_000,
            duration: 3600.0,
            width: 1280,
            height: 720,
            frames_per_second: 24.0,
            bit_depth: 8,
            warning: None,
        };
        let filter = AnalysisFilter {
            min_bitrate: 0,
            max_bitrate: None,
            min_duration: None,
            max_duration: None,
            min_resolution: Some(720),
            overwrite: false,
        };

        let result = classification_test_helpers::classify_video_file(file, &filter, &info);

        assert!(
            matches!(result, AnalysisResult::NeedsConversion(..)),
            "Expected NeedsConversion at exact min resolution, got: {result:?}"
        );
    }

    #[test]
    fn vertical_video_uses_smaller_dimension() {
        // 1080x720 vertical — smaller dimension is 720 which is below 1080 min
        let file = VideoFile::new(Path::new("/videos/vertical.mkv"), 0);
        let info = VideoInfo {
            codec: "h264".to_string(),
            bitrate_kbps: 10000,
            size_bytes: 500_000_000,
            duration: 3600.0,
            width: 720,
            height: 1280,
            frames_per_second: 24.0,
            bit_depth: 8,
            warning: None,
        };
        let filter = AnalysisFilter {
            min_bitrate: 0,
            max_bitrate: None,
            min_duration: None,
            max_duration: None,
            min_resolution: Some(1080),
            overwrite: false,
        };

        let result = classification_test_helpers::classify_video_file(file, &filter, &info);

        assert!(
            matches!(
                result,
                AnalysisResult::Skip {
                    reason: SkipReason::ResolutionBelowLimit { .. },
                    ..
                }
            ),
            "Expected ResolutionBelowLimit for vertical video, got: {result:?}"
        );
    }

    #[test]
    fn resolution_filter_does_not_apply_to_remux() {
        let file = VideoFile::new(Path::new("/videos/small_hevc.mkv"), 0);
        let info = VideoInfo {
            codec: "hevc".to_string(),
            bitrate_kbps: 5000,
            size_bytes: 100_000_000,
            duration: 3600.0,
            width: 640,
            height: 480,
            frames_per_second: 24.0,
            bit_depth: 8,
            warning: None,
        };
        let filter = AnalysisFilter {
            min_bitrate: 0,
            max_bitrate: None,
            min_duration: None,
            max_duration: None,
            min_resolution: Some(1080),
            overwrite: false,
        };

        let result = classification_test_helpers::classify_video_file(file, &filter, &info);

        assert!(
            matches!(result, AnalysisResult::NeedsRemux(..)),
            "Expected NeedsRemux (resolution filter should not apply to remux), got: {result:?}"
        );
    }
}

#[cfg(test)]
mod test_classify_output_exists {
    use super::*;
    use crate::types::{AnalysisFilter, AnalysisResult, SkipReason, VideoFile, VideoInfo};

    fn filter_no_overwrite() -> AnalysisFilter {
        AnalysisFilter {
            min_bitrate: 0,
            max_bitrate: None,
            min_duration: None,
            max_duration: None,
            min_resolution: None,
            overwrite: false,
        }
    }

    fn filter_with_overwrite() -> AnalysisFilter {
        AnalysisFilter {
            min_bitrate: 0,
            max_bitrate: None,
            min_duration: None,
            max_duration: None,
            min_resolution: None,
            overwrite: true,
        }
    }

    #[test]
    fn skips_when_output_exists_no_overwrite() {
        // Use a real path whose output (*.x265.mp4) exists.
        // We create a temp dir with both source and output files.
        let temp_dir = tempfile::TempDir::new().expect("Failed to create temp dir");
        let source = temp_dir.path().join("video.mkv");
        let output = temp_dir.path().join("video.x265.mp4");
        std::fs::write(&source, "").expect("Failed to create source");
        std::fs::write(&output, "").expect("Failed to create output");

        let file = VideoFile::new(&source, 0);
        let info = VideoInfo {
            codec: "h264".to_string(),
            bitrate_kbps: 10000,
            size_bytes: 1_000_000_000,
            duration: 3600.0,
            width: 1920,
            height: 1080,
            frames_per_second: 24.0,
            bit_depth: 8,
            warning: None,
        };

        let result = classification_test_helpers::classify_video_file(file, &filter_no_overwrite(), &info);

        assert!(
            matches!(
                result,
                AnalysisResult::Skip {
                    reason: SkipReason::OutputExists { .. },
                    ..
                }
            ),
            "Expected OutputExists skip, got: {result:?}"
        );
    }

    #[test]
    fn converts_when_output_exists_with_overwrite() {
        let temp_dir = tempfile::TempDir::new().expect("Failed to create temp dir");
        let source = temp_dir.path().join("video.mkv");
        let output = temp_dir.path().join("video.x265.mp4");
        std::fs::write(&source, "").expect("Failed to create source");
        std::fs::write(&output, "").expect("Failed to create output");

        let file = VideoFile::new(&source, 0);
        let info = VideoInfo {
            codec: "h264".to_string(),
            bitrate_kbps: 10000,
            size_bytes: 1_000_000_000,
            duration: 3600.0,
            width: 1920,
            height: 1080,
            frames_per_second: 24.0,
            bit_depth: 8,
            warning: None,
        };

        let result = classification_test_helpers::classify_video_file(file, &filter_with_overwrite(), &info);

        assert!(
            matches!(result, AnalysisResult::NeedsConversion(..)),
            "Expected NeedsConversion with overwrite, got: {result:?}"
        );
    }

    #[test]
    fn rename_skipped_when_output_exists_no_overwrite() {
        // hevc in mp4 without suffix — rename target already exists
        let temp_dir = tempfile::TempDir::new().expect("Failed to create temp dir");
        let source = temp_dir.path().join("video.mp4");
        let output = temp_dir.path().join("video.x265.mp4");
        std::fs::write(&source, "").expect("Failed to create source");
        std::fs::write(&output, "").expect("Failed to create output");

        let file = VideoFile::new(&source, 0);
        let info = VideoInfo {
            codec: "hevc".to_string(),
            bitrate_kbps: 5000,
            size_bytes: 500_000_000,
            duration: 3600.0,
            width: 1920,
            height: 1080,
            frames_per_second: 24.0,
            bit_depth: 8,
            warning: None,
        };

        let result = classification_test_helpers::classify_video_file(file, &filter_no_overwrite(), &info);

        assert!(
            matches!(
                result,
                AnalysisResult::Skip {
                    reason: SkipReason::OutputExists { .. },
                    ..
                }
            ),
            "Expected OutputExists skip for rename target, got: {result:?}"
        );
    }

    #[test]
    fn rename_proceeds_when_output_exists_with_overwrite() {
        let temp_dir = tempfile::TempDir::new().expect("Failed to create temp dir");
        let source = temp_dir.path().join("video.mp4");
        let output = temp_dir.path().join("video.x265.mp4");
        std::fs::write(&source, "").expect("Failed to create source");
        std::fs::write(&output, "").expect("Failed to create output");

        let file = VideoFile::new(&source, 0);
        let info = VideoInfo {
            codec: "hevc".to_string(),
            bitrate_kbps: 5000,
            size_bytes: 500_000_000,
            duration: 3600.0,
            width: 1920,
            height: 1080,
            frames_per_second: 24.0,
            bit_depth: 8,
            warning: None,
        };

        let result = classification_test_helpers::classify_video_file(file, &filter_with_overwrite(), &info);

        assert!(
            matches!(result, AnalysisResult::NeedsRename(..)),
            "Expected NeedsRename with overwrite, got: {result:?}"
        );
    }

    #[test]
    fn remux_skipped_when_output_exists_no_overwrite() {
        let temp_dir = tempfile::TempDir::new().expect("Failed to create temp dir");
        let source = temp_dir.path().join("video.mkv");
        let output = temp_dir.path().join("video.x265.mp4");
        std::fs::write(&source, "").expect("Failed to create source");
        std::fs::write(&output, "").expect("Failed to create output");

        let file = VideoFile::new(&source, 0);
        let info = VideoInfo {
            codec: "hevc".to_string(),
            bitrate_kbps: 5000,
            size_bytes: 500_000_000,
            duration: 3600.0,
            width: 1920,
            height: 1080,
            frames_per_second: 24.0,
            bit_depth: 8,
            warning: None,
        };

        let result = classification_test_helpers::classify_video_file(file, &filter_no_overwrite(), &info);

        assert!(
            matches!(
                result,
                AnalysisResult::Skip {
                    reason: SkipReason::OutputExists { .. },
                    ..
                }
            ),
            "Expected OutputExists skip for remux, got: {result:?}"
        );
    }
}

#[cfg(test)]
mod test_classify_combined_filters {
    use super::*;
    use crate::types::{AnalysisFilter, AnalysisResult, SkipReason, VideoFile, VideoInfo};

    #[test]
    fn first_failing_filter_wins_bitrate_before_duration() {
        // Both bitrate and duration fail — bitrate check comes first
        let file = VideoFile::new(Path::new("/videos/movie.mkv"), 0);
        let info = VideoInfo {
            codec: "h264".to_string(),
            bitrate_kbps: 500,
            size_bytes: 100_000_000,
            duration: 10.0,
            width: 1920,
            height: 1080,
            frames_per_second: 24.0,
            bit_depth: 8,
            warning: None,
        };
        let filter = AnalysisFilter {
            min_bitrate: 8000,
            max_bitrate: None,
            min_duration: Some(60.0),
            max_duration: None,
            min_resolution: None,
            overwrite: false,
        };

        let result = classification_test_helpers::classify_video_file(file, &filter, &info);

        assert!(
            matches!(
                result,
                AnalysisResult::Skip {
                    reason: SkipReason::BitrateBelowThreshold { .. },
                    ..
                }
            ),
            "Expected BitrateBelowThreshold (first filter checked), got: {result:?}"
        );
    }

    #[test]
    fn passes_all_filters_gets_converted() {
        let file = VideoFile::new(Path::new("/videos/movie.mkv"), 0);
        let info = VideoInfo {
            codec: "h264".to_string(),
            bitrate_kbps: 10000,
            size_bytes: 1_000_000_000,
            duration: 3600.0,
            width: 1920,
            height: 1080,
            frames_per_second: 24.0,
            bit_depth: 8,
            warning: None,
        };
        let filter = AnalysisFilter {
            min_bitrate: 8000,
            max_bitrate: Some(50000),
            min_duration: Some(60.0),
            max_duration: Some(7200.0),
            min_resolution: Some(720),
            overwrite: false,
        };

        let result = classification_test_helpers::classify_video_file(file, &filter, &info);

        assert!(
            matches!(result, AnalysisResult::NeedsConversion(..)),
            "Expected NeedsConversion with all filters passing, got: {result:?}"
        );
    }
}

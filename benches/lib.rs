use std::hint::black_box;
use std::path::PathBuf;
use std::time::Duration;

use criterion::{Criterion, criterion_group, criterion_main};

use cli_tools::{
    append_extension_to_path, color_diff, colorize_bool, format_duration, format_duration_seconds, format_size,
    get_normalized_dir_name, get_normalized_file_name_and_extension, insert_suffix_before_extension,
    is_system_directory_path, show_diff,
};

fn bench_format_size(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("format_size");

    group.bench_function("bytes", |bencher| {
        bencher.iter(|| format_size(black_box(512)));
    });

    group.bench_function("kilobytes", |bencher| {
        bencher.iter(|| format_size(black_box(150_000)));
    });

    group.bench_function("megabytes", |bencher| {
        bencher.iter(|| format_size(black_box(52_428_800)));
    });

    group.bench_function("gigabytes", |bencher| {
        bencher.iter(|| format_size(black_box(4_294_967_296)));
    });

    group.bench_function("terabytes", |bencher| {
        bencher.iter(|| format_size(black_box(2_199_023_255_552)));
    });

    group.finish();
}

fn bench_format_duration(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("format_duration");

    group.bench_function("seconds_only", |bencher| {
        bencher.iter(|| format_duration(black_box(Duration::from_secs(42))));
    });

    group.bench_function("minutes_and_seconds", |bencher| {
        bencher.iter(|| format_duration(black_box(Duration::from_secs(185))));
    });

    group.bench_function("hours_minutes_seconds", |bencher| {
        bencher.iter(|| format_duration(black_box(Duration::from_secs(7384))));
    });

    group.finish();
}

fn bench_format_duration_seconds(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("format_duration_seconds");

    group.bench_function("seconds", |bencher| {
        bencher.iter(|| format_duration_seconds(black_box(42.7)));
    });

    group.bench_function("minutes", |bencher| {
        bencher.iter(|| format_duration_seconds(black_box(185.3)));
    });

    group.bench_function("hours", |bencher| {
        bencher.iter(|| format_duration_seconds(black_box(7384.9)));
    });

    group.bench_function("negative", |bencher| {
        bencher.iter(|| format_duration_seconds(black_box(-5.0)));
    });

    group.finish();
}

fn bench_color_diff(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("color_diff");

    group.bench_function("identical_strings", |bencher| {
        bencher.iter(|| {
            color_diff(
                black_box("Artist.Name.Song.Title.mp3"),
                black_box("Artist.Name.Song.Title.mp3"),
                false,
            )
        });
    });

    group.bench_function("partial_change_inline", |bencher| {
        bencher.iter(|| {
            color_diff(
                black_box("Artist - Song Title (Original Mix).mp3"),
                black_box("Artist.Song.Title.Original.Mix.mp3"),
                false,
            )
        });
    });

    group.bench_function("partial_change_stacked", |bencher| {
        bencher.iter(|| {
            color_diff(
                black_box("Constantine - Onde As Satisfaction (Club Tool).aif"),
                black_box("Darude - Onde As Satisfaction (Constantine Club Tool).aif"),
                true,
            )
        });
    });

    group.bench_function("completely_different", |bencher| {
        bencher.iter(|| color_diff(black_box("abcdefghijklmnop"), black_box("zyxwvutsrqponmlk"), false));
    });

    group.bench_function("long_strings", |bencher| {
        let old = "Very.Long.File.Name.With.Many.Parts.And.Some.Extra.Info.2024.1080p.x264.mp4";
        let new = "Very.Long.File.Name.With.Many.Parts.And.Different.Info.2024.720p.x265.mkv";
        bencher.iter(|| color_diff(black_box(old), black_box(new), false));
    });

    group.finish();
}

fn bench_show_diff(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("show_diff");

    group.bench_function("short_strings", |bencher| {
        bencher.iter(|| show_diff(black_box("old_name.mp4"), black_box("new_name.mp4")));
    });

    group.bench_function("long_strings", |bencher| {
        let old = "Some.Artist.Name.Track.Title.Original.Mix.2024.wav";
        let new = "Some.Artist.Name.2024.Track.Title.Original.Mix.wav";
        bencher.iter(|| show_diff(black_box(old), black_box(new)));
    });

    group.finish();
}

fn bench_colorize_bool(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("colorize_bool");

    group.bench_function("true", |bencher| {
        bencher.iter(|| colorize_bool(black_box(true)));
    });

    group.bench_function("false", |bencher| {
        bencher.iter(|| colorize_bool(black_box(false)));
    });

    group.finish();
}

fn bench_path_utilities(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("path_utilities");

    group.bench_function("append_extension_to_path", |bencher| {
        let path = PathBuf::from("video.1080p");
        bencher.iter(|| append_extension_to_path(black_box(&path), "mp4"));
    });

    group.bench_function("insert_suffix_before_extension", |bencher| {
        let path = PathBuf::from("video.mp4");
        bencher.iter(|| insert_suffix_before_extension(black_box(&path), "_backup"));
    });

    group.bench_function("insert_suffix_no_extension", |bencher| {
        let path = PathBuf::from("video");
        bencher.iter(|| insert_suffix_before_extension(black_box(&path), "_backup"));
    });

    group.bench_function("insert_suffix_with_directory", |bencher| {
        let path = PathBuf::from("/some/path/to/video.mp4");
        bencher.iter(|| insert_suffix_before_extension(black_box(&path), "_copy"));
    });

    group.finish();
}

fn bench_get_normalized_file_name_and_extension(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("get_normalized_file_name_and_extension");

    group.bench_function("simple_file", |bencher| {
        let path = PathBuf::from("video.mp4");
        bencher.iter(|| get_normalized_file_name_and_extension(black_box(&path)));
    });

    group.bench_function("uppercase_extension", |bencher| {
        let path = PathBuf::from("VIDEO.MP4");
        bencher.iter(|| get_normalized_file_name_and_extension(black_box(&path)));
    });

    group.bench_function("no_extension", |bencher| {
        let path = PathBuf::from("README");
        bencher.iter(|| get_normalized_file_name_and_extension(black_box(&path)));
    });

    group.bench_function("complex_path", |bencher| {
        let path = PathBuf::from("/some/long/path/to/Artist.Name.Song.Title.1080p.MP4");
        bencher.iter(|| get_normalized_file_name_and_extension(black_box(&path)));
    });

    group.finish();
}

fn bench_get_normalized_dir_name(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("get_normalized_dir_name");

    group.bench_function("simple_name", |bencher| {
        let path = PathBuf::from("/videos/My Directory");
        bencher.iter(|| get_normalized_dir_name(black_box(&path)));
    });

    group.bench_function("unicode_name", |bencher| {
        let path = PathBuf::from("/videos/Ärtiständ Nämé");
        bencher.iter(|| get_normalized_dir_name(black_box(&path)));
    });

    group.finish();
}

fn bench_is_system_directory_path(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("is_system_directory_path");

    group.bench_function("recycle_bin", |bencher| {
        let path = PathBuf::from("$RECYCLE.BIN");
        bencher.iter(|| is_system_directory_path(black_box(&path)));
    });

    group.bench_function("system_volume_information", |bencher| {
        let path = PathBuf::from("System Volume Information");
        bencher.iter(|| is_system_directory_path(black_box(&path)));
    });

    group.bench_function("normal_directory", |bencher| {
        let path = PathBuf::from("My Videos");
        bencher.iter(|| is_system_directory_path(black_box(&path)));
    });

    group.bench_function("spotlight", |bencher| {
        let path = PathBuf::from(".Spotlight-V100");
        bencher.iter(|| is_system_directory_path(black_box(&path)));
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_format_size,
    bench_format_duration,
    bench_format_duration_seconds,
    bench_color_diff,
    bench_show_diff,
    bench_colorize_bool,
    bench_path_utilities,
    bench_get_normalized_file_name_and_extension,
    bench_get_normalized_dir_name,
    bench_is_system_directory_path,
);

criterion_main!(benches);

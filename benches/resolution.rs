use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};

use cli_tools::resolution::Resolution;

/// Benchmark exact resolution label matching for common horizontal resolutions.
fn bench_label_exact_horizontal(c: &mut Criterion) {
    let resolutions = [
        ("480p_640x480", Resolution::new(640, 480)),
        ("540p_960x540", Resolution::new(960, 540)),
        ("720p", Resolution::new(1280, 720)),
        ("1080p", Resolution::new(1920, 1080)),
        ("1440p", Resolution::new(2560, 1440)),
        ("2160p", Resolution::new(3840, 2160)),
    ];

    let mut group = c.benchmark_group("resolution/label/exact_horizontal");
    for (name, res) in &resolutions {
        group.bench_with_input(BenchmarkId::new("label", name), res, |b, res| {
            b.iter(|| black_box(res.label()));
        });
    }
    group.finish();
}

/// Benchmark exact resolution label matching for vertical (portrait) resolutions.
fn bench_label_exact_vertical(c: &mut Criterion) {
    let resolutions = [
        ("Vertical.480p", Resolution::new(480, 640)),
        ("Vertical.720p", Resolution::new(720, 1280)),
        ("Vertical.1080p", Resolution::new(1080, 1920)),
        ("Vertical.2160p", Resolution::new(2160, 3840)),
    ];

    let mut group = c.benchmark_group("resolution/label/exact_vertical");
    for (name, res) in &resolutions {
        group.bench_with_input(BenchmarkId::new("label", name), res, |b, res| {
            b.iter(|| black_box(res.label()));
        });
    }
    group.finish();
}

/// Benchmark fuzzy resolution label matching (resolutions that don't have exact matches).
fn bench_label_fuzzy(c: &mut Criterion) {
    let resolutions = [
        ("near_1080p_1918x1078", Resolution::new(1918, 1078)),
        ("near_720p_1278x718", Resolution::new(1278, 718)),
        ("near_2160p_3838x2158", Resolution::new(3838, 2158)),
        ("near_480p_638x478", Resolution::new(638, 478)),
        ("slightly_cropped_1080p", Resolution::new(1920, 1076)),
    ];

    let mut group = c.benchmark_group("resolution/label/fuzzy");
    for (name, res) in &resolutions {
        group.bench_with_input(BenchmarkId::new("label", name), res, |b, res| {
            b.iter(|| black_box(res.label()));
        });
    }
    group.finish();
}

/// Benchmark resolution labeling for unknown resolutions that fall through all matching.
fn bench_label_unknown(c: &mut Criterion) {
    let resolutions = [
        ("unknown_300x200", Resolution::new(300, 200)),
        ("unknown_5120x2880", Resolution::new(5120, 2880)),
        ("ultrawide_3440x1440", Resolution::new(3440, 1440)),
        ("square_1000x1000", Resolution::new(1000, 1000)),
    ];

    let mut group = c.benchmark_group("resolution/label/unknown");
    for (name, res) in &resolutions {
        group.bench_with_input(BenchmarkId::new("label", name), res, |b, res| {
            b.iter(|| black_box(res.label()));
        });
    }
    group.finish();
}

/// Benchmark `to_labeled_string` for various orientations.
fn bench_to_labeled_string(c: &mut Criterion) {
    let resolutions = [
        ("landscape_1920x1080", Resolution::new(1920, 1080)),
        ("portrait_1080x1920", Resolution::new(1080, 1920)),
        ("square_1080x1080", Resolution::new(1080, 1080)),
    ];

    let mut group = c.benchmark_group("resolution/to_labeled_string");
    for (name, res) in &resolutions {
        group.bench_with_input(BenchmarkId::new("format", name), res, |b, res| {
            b.iter(|| black_box(res.to_labeled_string()));
        });
    }
    group.finish();
}

/// Benchmark the `dimension_regex` method for cached (known) and uncached (unknown) resolutions.
fn bench_dimension_regex(c: &mut Criterion) {
    let mut group = c.benchmark_group("resolution/dimension_regex");

    // Known resolution (cached)
    let known = Resolution::new(1920, 1080);
    group.bench_function("cached_1920x1080", |b| {
        b.iter(|| black_box(known.dimension_regex().expect("should create regex")));
    });

    // Unknown resolution (not cached, compiled on demand)
    let unknown = Resolution::new(1234, 5678);
    group.bench_function("uncached_1234x5678", |b| {
        b.iter(|| black_box(unknown.dimension_regex().expect("should create regex")));
    });

    // Square resolution (different regex pattern branch)
    let square = Resolution::new(1080, 1080);
    group.bench_function("square_1080x1080", |b| {
        b.iter(|| black_box(square.dimension_regex().expect("should create regex")));
    });

    group.finish();
}

/// Benchmark the static regex accessors.
fn bench_static_regexes(c: &mut Criterion) {
    let mut group = c.benchmark_group("resolution/static_regex");

    let filenames = [
        "video.1920x1080.mp4",
        "video.Vertical.1080x1920.mp4",
        "video.1080p.mp4",
        "no.resolution.here.mp4",
        "Movie.Name.2024.720p.x265.mkv",
    ];

    for filename in &filenames {
        group.bench_with_input(
            BenchmarkId::new("full_resolution_regex", filename),
            filename,
            |b, filename| {
                let regex = Resolution::full_resolution_regex();
                b.iter(|| black_box(regex.is_match(filename)));
            },
        );

        group.bench_with_input(BenchmarkId::new("p_label_regex", filename), filename, |b, filename| {
            let regex = Resolution::p_label_regex();
            b.iter(|| black_box(regex.is_match(filename)));
        });
    }

    group.finish();
}

/// Benchmark `dimension_regex` matching against filenames.
fn bench_dimension_regex_matching(c: &mut Criterion) {
    let mut group = c.benchmark_group("resolution/dimension_regex_matching");

    let res = Resolution::new(1920, 1080);
    let regex = res.dimension_regex().expect("should create regex");

    let filenames = [
        ("match_standard", "video.1920x1080.mp4"),
        ("match_flipped", "video.1080x1920.mp4"),
        ("match_vertical_prefix", "video.Vertical.1080x1920.mp4"),
        ("no_match", "video.2560x1440.mp4"),
        ("long_filename", "Some.Movie.Name.2024.1920x1080.BluRay.x265.HEVC.mkv"),
    ];

    for (name, filename) in &filenames {
        group.bench_with_input(BenchmarkId::new("is_match", name), filename, |b, filename| {
            b.iter(|| black_box(regex.is_match(filename)));
        });
    }

    group.finish();
}

/// Benchmark resolution utility methods.
fn bench_resolution_utilities(c: &mut Criterion) {
    let mut group = c.benchmark_group("resolution/utilities");

    let res = Resolution::new(1920, 1080);

    group.bench_function("pixel_count", |b| {
        b.iter(|| black_box(res.pixel_count()));
    });

    group.bench_function("is_landscape", |b| {
        b.iter(|| black_box(res.is_landscape()));
    });

    group.bench_function("aspect_ratio", |b| {
        b.iter(|| black_box(res.aspect_ratio()));
    });

    group.bench_function("is_smaller_than_720", |b| {
        b.iter(|| black_box(res.is_smaller_than(720)));
    });

    group.bench_function("display", |b| {
        b.iter(|| black_box(format!("{res}")));
    });

    group.finish();
}

/// Benchmark labeling a batch of resolutions (simulating real-world batch processing).
fn bench_label_batch(c: &mut Criterion) {
    let resolutions: Vec<Resolution> = vec![
        Resolution::new(640, 480),
        Resolution::new(1280, 720),
        Resolution::new(1920, 1080),
        Resolution::new(3840, 2160),
        Resolution::new(1080, 1920),
        Resolution::new(720, 1280),
        Resolution::new(1918, 1078),
        Resolution::new(300, 200),
        Resolution::new(2560, 1440),
        Resolution::new(960, 540),
        Resolution::new(1920, 1076),
        Resolution::new(3440, 1440),
    ];

    c.bench_function("resolution/label_batch_12", |b| {
        b.iter(|| {
            for res in &resolutions {
                black_box(res.label());
            }
        });
    });
}

criterion_group!(
    benches,
    bench_label_exact_horizontal,
    bench_label_exact_vertical,
    bench_label_fuzzy,
    bench_label_unknown,
    bench_to_labeled_string,
    bench_dimension_regex,
    bench_static_regexes,
    bench_dimension_regex_matching,
    bench_resolution_utilities,
    bench_label_batch,
);

criterion_main!(benches);

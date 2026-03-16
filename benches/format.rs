//! Benchmarks for the dot-rename formatting pipeline.
//!
//! These benchmarks measure the performance of filename formatting operations
//! including replacements, date reordering, special character removal, and
//! prefix/suffix application.

use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};

use cli_tools::dot_rename::{
    DotFormat, DotRenameConfig, collapse_consecutive_dots, collapse_consecutive_dots_in_place, remove_extra_dots,
};

/// Create a default `DotRenameConfig` for benchmarking.
///
/// Sets `date_starts_with_year` to `true` to match real-world user config defaults.
fn default_config() -> DotRenameConfig {
    DotRenameConfig {
        date_starts_with_year: true,
        ..DotRenameConfig::default()
    }
}

/// Create a config with `date_starts_with_year` enabled.
fn year_first_config() -> DotRenameConfig {
    DotRenameConfig {
        date_starts_with_year: true,
        ..default_config()
    }
}

/// Create a config with replacements configured.
fn config_with_replacements() -> DotRenameConfig {
    DotRenameConfig {
        replace: vec![
            ("oldword".to_string(), "newword".to_string()),
            ("remove_me".to_string(), String::new()),
            ("foo".to_string(), "bar".to_string()),
        ],
        ..default_config()
    }
}

/// Create a config with prefix and suffix.
fn config_with_prefix_suffix() -> DotRenameConfig {
    DotRenameConfig {
        prefix: Some("MyPrefix".to_string()),
        suffix: Some("MySuffix".to_string()),
        ..default_config()
    }
}

// ---------------------------------------------------------------------------
// Test input data
// ---------------------------------------------------------------------------

/// Simple filenames that require minimal transformation.
const SIMPLE_NAMES: &[&str] = &["hello world", "My File Name", "document", "photo_2024", "track01"];

/// Filenames with brackets, parentheses, and special characters.
const COMPLEX_NAMES: &[&str] = &[
    "Song Title (Radio Edit) [Clean Version]",
    "Artist - Track Name (feat. Other Artist) {Remix}",
    "Movie.Name.2024.1080p.BluRay.x264-GROUP",
    "TV Show S01E05 - Episode Title [720p] (HDTV)",
    "Photo_(January 2024)_[Final Version]!!",
];

/// Filenames containing dates in various formats that need reordering.
const DATE_NAMES: &[&str] = &[
    "Event.25.12.2023.Recording",
    "Show.01.06.2024.Episode",
    "Concert.31.1.2024.Live",
    "Photo.5.3.2023.Original",
    "Meeting.15.11.2022.Notes",
];

/// Filenames with mixed case, underscores, and other formatting issues.
const MESSY_NAMES: &[&str] = &[
    "SOME__UGLY___FILE   NAME",
    "lots...of....dots...here",
    "MiXeD CaSe FiLe NaMe",
    "file - with - lots - of - dashes",
    "  leading and trailing spaces  ",
];

/// Very long filenames to test scaling behavior.
const LONG_NAMES: &[&str] = &[
    "This.Is.A.Very.Long.File.Name.That.Contains.Many.Dot.Separated.Parts.And.Should.Test.The.Performance.Of.The.Formatter.With.Long.Input.Strings",
    "Another.Extremely.Long.Filename.With.Multiple.Parts.Including.A.Date.25.12.2023.And.Some.Resolution.1080p.And.Even.More.Parts.After.That.To.Make.It.Really.Long",
    "Short.But.Repeated.Short.But.Repeated.Short.But.Repeated.Short.But.Repeated.Short.But.Repeated.Short.But.Repeated",
];

/// Written date filenames like "January 15, 2024".
const WRITTEN_DATE_NAMES: &[&str] = &[
    "Event January 15, 2024 Recording",
    "Show February 3, 2023 Episode",
    "Concert December 25, 2024 Live",
    "Photo March 1, 2024 Original",
    "Meeting November 30, 2022 Notes",
];

// ---------------------------------------------------------------------------
// Benchmarks for `format_name`
// ---------------------------------------------------------------------------

fn bench_format_name_simple(c: &mut Criterion) {
    let config = default_config();
    let formatter = DotFormat::new(&config);

    let mut group = c.benchmark_group("format_name/simple");
    for name in SIMPLE_NAMES {
        group.bench_with_input(BenchmarkId::from_parameter(name), name, |b, input| {
            b.iter(|| formatter.format_name(black_box(input)));
        });
    }
    group.finish();
}

fn bench_format_name_complex(c: &mut Criterion) {
    let config = default_config();
    let formatter = DotFormat::new(&config);

    let mut group = c.benchmark_group("format_name/complex");
    for name in COMPLEX_NAMES {
        group.bench_with_input(
            BenchmarkId::from_parameter(&name[..name.len().min(40)]),
            name,
            |b, input| {
                b.iter(|| formatter.format_name(black_box(input)));
            },
        );
    }
    group.finish();
}

fn bench_format_name_with_dates(c: &mut Criterion) {
    let config = default_config();
    let formatter = DotFormat::new(&config);

    let mut group = c.benchmark_group("format_name/dates");
    for name in DATE_NAMES {
        group.bench_with_input(BenchmarkId::from_parameter(name), name, |b, input| {
            b.iter(|| formatter.format_name(black_box(input)));
        });
    }
    group.finish();
}

fn bench_format_name_messy(c: &mut Criterion) {
    let config = default_config();
    let formatter = DotFormat::new(&config);

    let mut group = c.benchmark_group("format_name/messy");
    for name in MESSY_NAMES {
        group.bench_with_input(
            BenchmarkId::from_parameter(&name.trim()[..name.trim().len().min(30)]),
            name,
            |b, input| {
                b.iter(|| formatter.format_name(black_box(input)));
            },
        );
    }
    group.finish();
}

fn bench_format_name_long(c: &mut Criterion) {
    let config = default_config();
    let formatter = DotFormat::new(&config);

    let mut group = c.benchmark_group("format_name/long");
    for name in LONG_NAMES {
        group.bench_with_input(BenchmarkId::new("chars", name.len()), name, |b, input| {
            b.iter(|| formatter.format_name(black_box(input)));
        });
    }
    group.finish();
}

fn bench_format_name_written_dates(c: &mut Criterion) {
    let config = default_config();
    let formatter = DotFormat::new(&config);

    let mut group = c.benchmark_group("format_name/written_dates");
    for name in WRITTEN_DATE_NAMES {
        group.bench_with_input(
            BenchmarkId::from_parameter(&name[..name.len().min(30)]),
            name,
            |b, input| {
                b.iter(|| formatter.format_name(black_box(input)));
            },
        );
    }
    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmarks for `format_name` with various configs
// ---------------------------------------------------------------------------

fn bench_format_name_year_first(c: &mut Criterion) {
    let config = year_first_config();
    let formatter = DotFormat::new(&config);

    let mut group = c.benchmark_group("format_name/year_first");
    for name in DATE_NAMES {
        group.bench_with_input(BenchmarkId::from_parameter(name), name, |b, input| {
            b.iter(|| formatter.format_name(black_box(input)));
        });
    }
    group.finish();
}

fn bench_format_name_with_replacements(c: &mut Criterion) {
    let config = config_with_replacements();
    let formatter = DotFormat::new(&config);

    let names = [
        "oldword.in.the.filename",
        "remove_me.from.this.name",
        "foo.bar.baz.foo.bar",
        "no.replacements.needed.here",
        "oldword.and.remove_me.and.foo.combined",
    ];

    let mut group = c.benchmark_group("format_name/replacements");
    for name in &names {
        group.bench_with_input(BenchmarkId::from_parameter(name), name, |b, input| {
            b.iter(|| formatter.format_name(black_box(input)));
        });
    }
    group.finish();
}

fn bench_format_name_with_prefix_suffix(c: &mut Criterion) {
    let config = config_with_prefix_suffix();
    let formatter = DotFormat::new(&config);

    let mut group = c.benchmark_group("format_name/prefix_suffix");
    for name in SIMPLE_NAMES {
        group.bench_with_input(BenchmarkId::from_parameter(name), name, |b, input| {
            b.iter(|| formatter.format_name(black_box(input)));
        });
    }
    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmarks for `format_name_without_prefix_suffix`
// ---------------------------------------------------------------------------

fn bench_format_name_without_prefix_suffix(c: &mut Criterion) {
    let config = default_config();
    let formatter = DotFormat::new(&config);

    let mut group = c.benchmark_group("format_name_without_prefix_suffix");
    for name in COMPLEX_NAMES {
        group.bench_with_input(
            BenchmarkId::from_parameter(&name[..name.len().min(40)]),
            name,
            |b, input| {
                b.iter(|| formatter.format_name_without_prefix_suffix(black_box(input)));
            },
        );
    }
    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmarks for `format_directory_name`
// ---------------------------------------------------------------------------

fn bench_format_directory_name(c: &mut Criterion) {
    let config = default_config();
    let formatter = DotFormat::new(&config);

    let dir_names = [
        "Some Directory Name",
        "25.12.2023.Event.Photos",
        "TV Show Season 01",
        "Artist - Discography (2020-2024)",
        "Project [Final] (v2)",
    ];

    let mut group = c.benchmark_group("format_directory_name");
    for name in &dir_names {
        group.bench_with_input(
            BenchmarkId::from_parameter(&name[..name.len().min(30)]),
            name,
            |b, input| {
                b.iter(|| formatter.format_directory_name(black_box(input)));
            },
        );
    }
    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmarks for dot-collapsing utilities
// ---------------------------------------------------------------------------

fn bench_collapse_consecutive_dots(c: &mut Criterion) {
    let inputs = [
        ("no_dots", "hello world no dots here"),
        ("single_dots", "hello.world.single.dots"),
        ("double_dots", "hello..world..double..dots"),
        ("triple_dots", "hello...world...triple...dots"),
        ("many_dots", "hello......world......many......dots"),
        ("mixed", "a.b..c...d....e.....f"),
    ];

    let mut group = c.benchmark_group("collapse_consecutive_dots");
    for (label, input) in &inputs {
        group.bench_with_input(BenchmarkId::from_parameter(label), input, |b, input| {
            b.iter(|| collapse_consecutive_dots(black_box(input)));
        });
    }
    group.finish();
}

fn bench_collapse_consecutive_dots_in_place(c: &mut Criterion) {
    let inputs = [
        ("no_dots", "hello world no dots here"),
        ("single_dots", "hello.world.single.dots"),
        ("double_dots", "hello..world..double..dots"),
        ("many_dots", "hello......world......many......dots"),
    ];

    let mut group = c.benchmark_group("collapse_consecutive_dots_in_place");
    for (label, input) in &inputs {
        group.bench_with_input(BenchmarkId::from_parameter(label), input, |b, input| {
            b.iter(|| {
                let mut s = input.to_string();
                collapse_consecutive_dots_in_place(black_box(&mut s));
                s
            });
        });
    }
    group.finish();
}

fn bench_remove_extra_dots(c: &mut Criterion) {
    let inputs = [
        ("clean", "hello.world"),
        ("leading_trailing", ".hello.world."),
        ("double_dots", "hello..world"),
        ("complex", "..hello...world..test."),
        ("only_dots", "...."),
    ];

    let mut group = c.benchmark_group("remove_extra_dots");
    for (label, input) in &inputs {
        group.bench_with_input(BenchmarkId::from_parameter(label), input, |b, input| {
            b.iter(|| {
                let mut s = input.to_string();
                remove_extra_dots(black_box(&mut s));
                s
            });
        });
    }
    group.finish();
}

// ---------------------------------------------------------------------------
// Throughput benchmark: batch formatting
// ---------------------------------------------------------------------------

fn bench_format_name_batch(c: &mut Criterion) {
    let config = default_config();
    let formatter = DotFormat::new(&config);

    let all_names: Vec<&str> = SIMPLE_NAMES
        .iter()
        .chain(COMPLEX_NAMES.iter())
        .chain(DATE_NAMES.iter())
        .chain(MESSY_NAMES.iter())
        .chain(WRITTEN_DATE_NAMES.iter())
        .copied()
        .collect();

    c.bench_function("format_name/batch_all_types", |b| {
        b.iter(|| {
            for name in &all_names {
                let _ = formatter.format_name(black_box(name));
            }
        });
    });
}

// ---------------------------------------------------------------------------
// Criterion groups and main
// ---------------------------------------------------------------------------

criterion_group!(
    format_benches,
    bench_format_name_simple,
    bench_format_name_complex,
    bench_format_name_with_dates,
    bench_format_name_messy,
    bench_format_name_long,
    bench_format_name_written_dates,
    bench_format_name_year_first,
    bench_format_name_with_replacements,
    bench_format_name_with_prefix_suffix,
    bench_format_name_without_prefix_suffix,
    bench_format_directory_name,
    bench_collapse_consecutive_dots,
    bench_collapse_consecutive_dots_in_place,
    bench_remove_extra_dots,
    bench_format_name_batch,
);

criterion_main!(format_benches);

use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};

use cli_tools::date::Date;

fn bench_reorder_filename_date(c: &mut Criterion) {
    let mut group = c.benchmark_group("date_reorder_filename");

    let test_cases = [
        ("full_date_dd_mm_yyyy", "Some.File.24.12.2023.720p.mp4"),
        ("full_date_mm_dd_yyyy", "Some.File.01.25.2024.1080p.mp4"),
        ("short_date", "Some.File.24.12.23.720p.mp4"),
        ("already_correct", "Some.File.2023.12.24.720p.mp4"),
        ("no_date", "Some.File.Without.Date.720p.mp4"),
        (
            "date_with_long_name",
            "Very.Long.Filename.With.Many.Parts.24.12.2023.1080p.x264.mp4",
        ),
        ("single_digit_date", "Some.File.5.3.2024.mp4"),
    ];

    for (name, input) in &test_cases {
        group.bench_with_input(BenchmarkId::new("year_first_false", name), input, |b, input| {
            b.iter(|| Date::reorder_filename_date(black_box(input), false, false, false));
        });

        group.bench_with_input(BenchmarkId::new("year_first_true", name), input, |b, input| {
            b.iter(|| Date::reorder_filename_date(black_box(input), true, false, false));
        });
    }

    group.finish();
}

fn bench_reorder_directory_date(c: &mut Criterion) {
    let mut group = c.benchmark_group("date_reorder_directory");

    let test_cases = [
        ("dd_mm_yyyy", "24.12.2023 Concert Name"),
        ("yyyy_mm_dd", "2023.12.24 Concert Name"),
        ("no_date", "Concert Name Without Date"),
        ("date_only", "24.12.2023"),
    ];

    for (name, input) in &test_cases {
        group.bench_with_input(BenchmarkId::new("reorder", name), input, |b, input| {
            b.iter(|| Date::reorder_directory_date(black_box(input)));
        });
    }

    group.finish();
}

fn bench_swap_year(c: &mut Criterion) {
    let mut group = c.benchmark_group("date_swap_year");

    let test_cases = [
        ("correct_format", "Some.File.2005.12.23.mp4"),
        ("no_date", "Some.File.Without.Date.mp4"),
        ("already_swapped", "Some.File.2023.12.05.mp4"),
    ];

    for (name, input) in &test_cases {
        group.bench_with_input(BenchmarkId::new("swap", name), input, |b, input| {
            b.iter(|| Date::reorder_filename_date(black_box(input), false, true, false));
        });
    }

    group.finish();
}

fn bench_date_struct_operations(c: &mut Criterion) {
    let mut group = c.benchmark_group("date_struct");

    group.bench_function("try_from_valid", |b| {
        b.iter(|| Date::try_from(black_box(2023), black_box(12), black_box(24)));
    });

    group.bench_function("try_from_invalid_year", |b| {
        b.iter(|| Date::try_from(black_box(1980), black_box(12), black_box(24)));
    });

    group.bench_function("parse_from_short", |b| {
        b.iter(|| Date::parse_from_short(black_box("23"), black_box("12"), black_box("24")));
    });

    group.bench_function("dash_format", |b| {
        let date = Date::try_from(2023, 12, 24).expect("Valid date");
        b.iter(|| black_box(&date).dash_format());
    });

    group.bench_function("dot_format", |b| {
        let date = Date::try_from(2023, 12, 24).expect("Valid date");
        b.iter(|| black_box(&date).dot_format());
    });

    group.bench_function("swap_year", |b| {
        let date = Date::try_from(2005, 12, 23).expect("Valid date");
        b.iter(|| black_box(&date).swap_year());
    });

    group.bench_function("display", |b| {
        let date = Date::try_from(2023, 12, 24).expect("Valid date");
        b.iter(|| format!("{}", black_box(&date)));
    });

    group.finish();
}

fn bench_replace_file_date_with_directory_date(c: &mut Criterion) {
    let mut group = c.benchmark_group("date_replace_file_with_directory");

    let test_cases = [
        ("has_date", "Name.2025.12.24"),
        ("no_date", "Name.Without.Date"),
        ("multiple_numbers", "Name.2025.12.24.Extra.2024.01.01"),
    ];

    for (name, input) in &test_cases {
        group.bench_with_input(BenchmarkId::new("replace", name), input, |b, input| {
            b.iter(|| Date::replace_file_date_with_directory_date(black_box(input)));
        });
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_reorder_filename_date,
    bench_reorder_directory_date,
    bench_swap_year,
    bench_date_struct_operations,
    bench_replace_file_date_with_directory_date,
);
criterion_main!(benches);

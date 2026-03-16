//! Benchmarks for `dupefind` duplicate detection and filename normalization.
//!
//! Uses the algorithmic functions extracted to the `cli_tools::dupe_find` library module.

use std::collections::HashMap;
use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use regex::Regex;

use cli_tools::dupe_find::{
    DupeFileInfo, DuplicateGroup, MatchRange, RE_RESOLUTION, merge_indices_into_groups, normalize_stem,
};

// ---------------------------------------------------------------------------
// Benchmarks for normalize_stem
// ---------------------------------------------------------------------------

fn bench_normalize_stem(c: &mut Criterion) {
    let stems = [
        ("simple", "Some.Movie.Name"),
        ("with_resolution_720p", "Some.Movie.Name.720p"),
        ("with_resolution_1080p", "Some.Movie.Name.1080p"),
        ("with_resolution_2160p", "Some.Movie.Name.2160p"),
        ("with_resolution_dimensions", "Some.Movie.Name.1920x1080"),
        ("with_codec_x264", "Some.Movie.Name.x264"),
        ("with_codec_x265", "Some.Movie.Name.x265"),
        ("with_codec_h264", "Some.Movie.Name.h264"),
        ("with_codec_h265", "Some.Movie.Name.h265"),
        ("with_resolution_and_codec", "Some.Movie.Name.1080p.x265"),
        ("full_release_name", "Some.Movie.Name.2024.1080p.BluRay.x265.HEVC-GROUP"),
        ("with_multi_dots", "Some..Movie...Name....1080p.x264"),
        ("with_spaces_and_dashes", "Some Movie Name - 1080p - x264"),
        ("only_resolution", "1080p"),
        ("only_codec", "x265"),
        ("empty_after_strip", "1080p.x264"),
        ("no_changes_needed", "plain.movie.name"),
        (
            "very_long_name",
            "This.Is.A.Very.Long.Movie.Name.With.Many.Parts.2024.1080p.BluRay.x265.HEVC.DTS-HD.MA.5.1-GROUP",
        ),
    ];

    let mut group = c.benchmark_group("dupe_find/normalize_stem");
    for (name, stem) in &stems {
        group.bench_with_input(BenchmarkId::new("normalize", name), stem, |b, stem| {
            b.iter(|| normalize_stem(black_box(stem)));
        });
    }
    group.finish();
}

/// Benchmark batch normalization to measure throughput.
fn bench_normalize_stem_batch(c: &mut Criterion) {
    let stems: Vec<&str> = vec![
        "Movie.One.2024.1080p.x265",
        "Movie.Two.2023.720p.x264",
        "Movie.Three.2022.2160p.h265",
        "TV.Show.S01E01.1080p.x264",
        "TV.Show.S01E02.1080p.x264",
        "TV.Show.S01E03.720p.x265",
        "Documentary.Name.2024.1080p.BluRay",
        "Another.Movie.2023.1920x1080.h264",
        "Short.Film.720p",
        "Long.Movie.Name.With.Extra.Parts.2024.2160p.x265.HEVC",
        "plain.name.no.resolution",
        "UPPERCASE.NAME.1080P.X265",
        "mixed.Case.Name.1080p.X264",
        "name-with-dashes.1080p",
        "name_with_underscores.720p",
        "name with spaces 1080p",
    ];

    c.bench_function("dupe_find/normalize_stem_batch_16", |b| {
        b.iter(|| {
            for stem in &stems {
                black_box(normalize_stem(black_box(stem)));
            }
        });
    });
}

// ---------------------------------------------------------------------------
// Benchmarks for merge_indices_into_groups
// ---------------------------------------------------------------------------

fn bench_merge_indices(c: &mut Criterion) {
    let mut group = c.benchmark_group("dupe_find/merge_indices");

    // Small merge: 2 groups
    group.bench_function("merge_2_indices", |b| {
        b.iter(|| {
            let mut file_to_group: HashMap<usize, String> = HashMap::new();
            let mut groups: HashMap<String, Vec<usize>> = HashMap::new();

            file_to_group.insert(0, "group_a".to_string());
            file_to_group.insert(1, "group_b".to_string());
            groups.insert("group_a".to_string(), vec![0]);
            groups.insert("group_b".to_string(), vec![1]);

            merge_indices_into_groups(black_box(&[0, 1]), &mut file_to_group, &mut groups);
        });
    });

    // Medium merge: 5 groups
    group.bench_function("merge_5_indices", |b| {
        b.iter(|| {
            let mut file_to_group: HashMap<usize, String> = HashMap::new();
            let mut groups: HashMap<String, Vec<usize>> = HashMap::new();

            for idx in 0..5 {
                let key = format!("group_{idx}");
                file_to_group.insert(idx, key.clone());
                groups.insert(key, vec![idx]);
            }

            merge_indices_into_groups(black_box(&[0, 1, 2, 3, 4]), &mut file_to_group, &mut groups);
        });
    });

    // Large merge: 20 groups
    group.bench_function("merge_20_indices", |b| {
        b.iter(|| {
            let mut file_to_group: HashMap<usize, String> = HashMap::new();
            let mut groups: HashMap<String, Vec<usize>> = HashMap::new();
            let indices: Vec<usize> = (0..20).collect();

            for idx in 0..20 {
                let key = format!("group_{idx}");
                file_to_group.insert(idx, key.clone());
                groups.insert(key, vec![idx]);
            }

            merge_indices_into_groups(black_box(&indices), &mut file_to_group, &mut groups);
        });
    });

    // Merge when already in the same group (no-op)
    group.bench_function("merge_already_same_group", |b| {
        b.iter(|| {
            let mut file_to_group: HashMap<usize, String> = HashMap::new();
            let mut groups: HashMap<String, Vec<usize>> = HashMap::new();

            for idx in 0..5 {
                file_to_group.insert(idx, "same_group".to_string());
            }
            groups.insert("same_group".to_string(), vec![0, 1, 2, 3, 4]);

            merge_indices_into_groups(black_box(&[0, 1, 2, 3, 4]), &mut file_to_group, &mut groups);
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmarks for full duplicate finding pipeline
// ---------------------------------------------------------------------------

/// Simulate the full duplicate-finding pipeline on a set of filenames.
/// This replicates the core logic of `DupeFind::find_all_duplicates` without
/// file I/O, progress bars, or pattern matching.
fn find_duplicates_by_normalized_name(filenames: &[&str]) -> Vec<(String, Vec<usize>)> {
    let normalized_keys: Vec<String> = filenames.iter().map(|f| normalize_stem(f)).collect();

    let mut file_to_group: HashMap<usize, String> = HashMap::new();
    let mut groups: HashMap<String, Vec<usize>> = HashMap::new();

    for (idx, key) in normalized_keys.into_iter().enumerate() {
        file_to_group.insert(idx, key.clone());
        groups.entry(key).or_default().push(idx);
    }

    // Merge by exact filename match (lowercased)
    let mut filename_to_indices: HashMap<String, Vec<usize>> = HashMap::new();
    for (idx, filename) in filenames.iter().enumerate() {
        filename_to_indices
            .entry(filename.to_lowercase())
            .or_default()
            .push(idx);
    }

    for indices in filename_to_indices.values() {
        if indices.len() > 1 {
            merge_indices_into_groups(indices, &mut file_to_group, &mut groups);
        }
    }

    let mut result: Vec<(String, Vec<usize>)> = groups.into_iter().filter(|(_, indices)| indices.len() > 1).collect();
    result.sort_by(|a, b| a.0.cmp(&b.0));
    result
}

fn bench_find_duplicates_small(c: &mut Criterion) {
    let filenames: Vec<&str> = vec![
        "Movie.Name.2024.1080p.x265",
        "Movie.Name.2024.720p.x264",
        "Movie.Name.2024.2160p.h265",
        "Different.Movie.2023.1080p",
        "Another.Film.2022.720p",
    ];

    c.bench_function("dupe_find/find_duplicates_5_files", |b| {
        b.iter(|| find_duplicates_by_normalized_name(black_box(&filenames)));
    });
}

fn bench_find_duplicates_medium(c: &mut Criterion) {
    let filenames: Vec<&str> = vec![
        // Group 1: same movie, different quality
        "Movie.Name.2024.1080p.BluRay.x265",
        "Movie.Name.2024.720p.WEB.x264",
        "Movie.Name.2024.2160p.BluRay.h265",
        // Group 2: another movie
        "Another.Movie.2023.1080p.x264",
        "Another.Movie.2023.720p.h264",
        // Group 3: TV show
        "TV.Show.S01E01.1080p.x265",
        "TV.Show.S01E01.720p.x264",
        "TV.Show.S01E01.2160p.h265",
        // Unique files
        "Unique.Film.2022.1080p.x265",
        "Solo.Documentary.2024.720p",
        "Standalone.Movie.1080p.BluRay",
        "Independent.Film.2023.2160p",
        // Group 4: another duplicate
        "Classic.Movie.1999.1080p",
        "Classic.Movie.1999.720p",
        "Classic.Movie.1999.2160p",
        // More unique files
        "Random.Video.2024.1080p",
        "Test.File.720p.x264",
        "Sample.Movie.2023.1080p",
        "Demo.Film.2022.720p",
        "Preview.Movie.2024.2160p",
    ];

    c.bench_function("dupe_find/find_duplicates_20_files", |b| {
        b.iter(|| find_duplicates_by_normalized_name(black_box(&filenames)));
    });
}

fn bench_find_duplicates_large(c: &mut Criterion) {
    // Generate a realistic large dataset with multiple duplicate groups
    let mut filenames: Vec<String> = Vec::new();

    // 10 groups of 5 resolution variants each = 50 duplicates
    for group_index in 0..10 {
        let base = format!("Movie.Title.{group_index}.2024");
        for resolution in &["1080p", "720p", "2160p", "480p", "1080p.REMUX"] {
            for codec in &["x265", "x264"] {
                filenames.push(format!("{base}.{resolution}.BluRay.{codec}"));
            }
        }
    }

    // 20 groups of 2 codec variants each = 40 duplicates
    for group_index in 0..20 {
        let base = format!("TV.Show.{group_index}.S01E01.1080p");
        for codec in &["x264", "x265"] {
            filenames.push(format!("{base}.{codec}"));
        }
    }

    // 30 unique files
    for unique_index in 0..30 {
        filenames.push(format!("Unique.Movie.{unique_index}.2023.1080p.x265"));
    }

    // 5 groups of exact duplicates (same filename)
    for group_index in 0..5 {
        let name = format!("Exact.Duplicate.{group_index}.1080p.x265");
        filenames.push(name.clone());
        filenames.push(name);
    }

    let filename_refs: Vec<&str> = filenames.iter().map(String::as_str).collect();

    c.bench_function(
        &format!("dupe_find/find_duplicates_{}_files", filename_refs.len()),
        |b| {
            b.iter(|| find_duplicates_by_normalized_name(black_box(&filename_refs)));
        },
    );
}

// ---------------------------------------------------------------------------
// Benchmarks for regex compilation and matching
// ---------------------------------------------------------------------------

fn bench_regex_matching(c: &mut Criterion) {
    let mut group = c.benchmark_group("dupe_find/regex_matching");

    let filenames = [
        ("no_match", "plain.movie.name"),
        ("resolution_720p", "movie.720p.mkv"),
        ("resolution_1080p", "movie.1080p.x265.mkv"),
        ("resolution_2160p", "movie.2160p.x265.mkv"),
        ("resolution_dimensions", "movie.1920x1080.mkv"),
        (
            "long_with_resolution",
            "Very.Long.Movie.Name.2024.1080p.BluRay.x265.HEVC.DTS-HD.MA.5.1-GROUP",
        ),
    ];

    for (name, filename) in &filenames {
        group.bench_with_input(
            BenchmarkId::new("resolution_is_match", name),
            filename,
            |b, filename| {
                b.iter(|| RE_RESOLUTION.is_match(black_box(filename)));
            },
        );

        group.bench_with_input(
            BenchmarkId::new("resolution_replace_all", name),
            filename,
            |b, filename| {
                b.iter(|| RE_RESOLUTION.replace_all(black_box(filename), "").to_string());
            },
        );
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmark for pattern-based duplicate detection
// ---------------------------------------------------------------------------

fn bench_pattern_matching(c: &mut Criterion) {
    let patterns: Vec<Regex> = vec![
        Regex::new(r"[A-Z]{3,4}-\d{3,4}").expect("valid regex"),
        Regex::new(r"S\d{2}E\d{2}").expect("valid regex"),
    ];

    let filenames = [
        "Some.Movie.ABC-123.1080p.x265.mkv",
        "Some.Movie.ABC-123.720p.x264.mkv",
        "TV.Show.S01E05.1080p.mkv",
        "TV.Show.S01E05.720p.mkv",
        "No.Pattern.Here.1080p.mkv",
        "Another.WXYZ-5678.2160p.mkv",
        "Random.File.Without.Pattern.mkv",
        "Show.S02E10.Special.Edition.mkv",
    ];

    let mut group = c.benchmark_group("dupe_find/pattern_matching");

    group.bench_function("find_first_pattern_match", |b| {
        b.iter(|| {
            for filename in &filenames {
                for pattern in &patterns {
                    if pattern.find(black_box(filename)).is_some() {
                        break;
                    }
                }
            }
        });
    });

    group.bench_function("collect_all_pattern_matches", |b| {
        b.iter(|| {
            let mut matches: HashMap<String, Vec<usize>> = HashMap::new();
            for (idx, filename) in filenames.iter().enumerate() {
                for pattern in &patterns {
                    if let Some(m) = pattern.find(black_box(filename)) {
                        matches.entry(m.as_str().to_string()).or_default().push(idx);
                        break;
                    }
                }
            }
            matches
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmark filename lowercasing and grouping (first pass of find_all_duplicates)
// ---------------------------------------------------------------------------

fn bench_filename_grouping(c: &mut Criterion) {
    let filenames: Vec<&str> = vec![
        "Movie.Name.2024.1080p.x265.mkv",
        "movie.name.2024.1080p.x265.mkv",
        "MOVIE.NAME.2024.1080P.X265.MKV",
        "Movie.Name.2024.720p.x264.mkv",
        "Different.Movie.2023.1080p.mkv",
        "different.movie.2023.1080p.mkv",
        "TV.Show.S01E01.1080p.mkv",
        "tv.show.s01e01.1080p.mkv",
        "Unique.File.2024.mkv",
        "Another.Unique.File.mkv",
    ];

    let mut group = c.benchmark_group("dupe_find/filename_grouping");

    group.bench_function("group_by_exact_lowercase", |b| {
        b.iter(|| {
            let mut groups: HashMap<String, Vec<usize>> = HashMap::new();
            for (idx, filename) in filenames.iter().enumerate() {
                groups.entry(black_box(filename).to_lowercase()).or_default().push(idx);
            }
            groups
        });
    });

    group.bench_function("group_by_normalized_stem", |b| {
        b.iter(|| {
            let mut groups: HashMap<String, Vec<usize>> = HashMap::new();
            for (idx, filename) in filenames.iter().enumerate() {
                let key = normalize_stem(black_box(filename));
                groups.entry(key).or_default().push(idx);
            }
            groups
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmarks for DuplicateGroup::display_name
// ---------------------------------------------------------------------------

fn bench_display_name(c: &mut Criterion) {
    let mut group = c.benchmark_group("dupe_find/display_name");

    // Group where all files share the same pattern match text
    group.bench_function("all_same_pattern", |b| {
        let files = vec![
            DupeFileInfo {
                path: "a/ABC-123.1080p.mkv".into(),
                filename: "ABC-123.1080p.mkv".into(),
                stem: "ABC-123.1080p".into(),
                extension: "mkv".into(),
                pattern_match: Some(MatchRange { start: 0, end: 7 }),
            },
            DupeFileInfo {
                path: "b/ABC-123.720p.mkv".into(),
                filename: "ABC-123.720p.mkv".into(),
                stem: "ABC-123.720p".into(),
                extension: "mkv".into(),
                pattern_match: Some(MatchRange { start: 0, end: 7 }),
            },
        ];
        let duplicate_group = DuplicateGroup::new("abc-123".to_string(), files);
        b.iter(|| black_box(&duplicate_group).display_name());
    });

    // Group where files have no pattern matches
    group.bench_function("no_pattern_matches", |b| {
        let files = vec![
            DupeFileInfo {
                path: "a/movie.1080p.mkv".into(),
                filename: "movie.1080p.mkv".into(),
                stem: "movie.1080p".into(),
                extension: "mkv".into(),
                pattern_match: None,
            },
            DupeFileInfo {
                path: "b/movie.720p.mkv".into(),
                filename: "movie.720p.mkv".into(),
                stem: "movie.720p".into(),
                extension: "mkv".into(),
                pattern_match: None,
            },
        ];
        let duplicate_group = DuplicateGroup::new("movie".to_string(), files);
        b.iter(|| black_box(&duplicate_group).display_name());
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmarks for MatchRange::extract_from
// ---------------------------------------------------------------------------

fn bench_match_range(c: &mut Criterion) {
    let mut group = c.benchmark_group("dupe_find/match_range");

    let filename = "Some.Movie.ABC-123.1080p.x265.mkv";
    let range = MatchRange { start: 11, end: 18 };

    group.bench_function("extract_from", |b| {
        b.iter(|| range.extract_from(black_box(filename)));
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Criterion groups and main
// ---------------------------------------------------------------------------

criterion_group!(
    benches,
    bench_normalize_stem,
    bench_normalize_stem_batch,
    bench_merge_indices,
    bench_find_duplicates_small,
    bench_find_duplicates_medium,
    bench_find_duplicates_large,
    bench_regex_matching,
    bench_pattern_matching,
    bench_filename_grouping,
    bench_display_name,
    bench_match_range,
);

criterion_main!(benches);

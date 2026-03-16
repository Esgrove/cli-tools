//! Benchmarks for `dir_move` prefix grouping, file matching, and filtering logic.
//!
//! Uses the algorithmic types and functions extracted to the `cli_tools::dir_move`
//! library module.

use std::hint::black_box;
use std::path::PathBuf;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};

use cli_tools::dir_move::{
    FileInfo, FilteredParts, PrefixGroupBuilder, count_prefix_chars, filter_numeric_resolution_and_glue_parts,
    find_prefix_candidates, get_all_n_part_sequences, is_unwanted_directory, normalize_name,
    parts_are_contiguous_with_combined, prefix_matches_normalized_precomputed,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Create `FileInfo` entries from filenames by applying standard filtering.
fn make_filtered_files(names: &[&str]) -> Vec<FileInfo<'static>> {
    names
        .iter()
        .map(|name| {
            let filtered = filter_numeric_resolution_and_glue_parts(name);
            FileInfo::new(PathBuf::from(*name), (*name).to_string(), filtered)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Test data sets
// ---------------------------------------------------------------------------

/// A small set of files sharing a two-part prefix.
const SMALL_FILE_SET: &[&str] = &[
    "Jane.Doe.Episode.01.720p.mp4",
    "Jane.Doe.Episode.02.1080p.mp4",
    "Jane.Doe.Episode.03.720p.mp4",
    "Other.Show.Episode.01.mp4",
    "Other.Show.Episode.02.mp4",
];

/// A medium set simulating a realistic directory with multiple series and noise files.
const MEDIUM_FILE_SET: &[&str] = &[
    "Jane.Doe.S01E01.720p.x264.mp4",
    "Jane.Doe.S01E02.720p.x264.mp4",
    "Jane.Doe.S01E03.1080p.x265.mp4",
    "Jane.Doe.S01E04.1080p.x265.mp4",
    "Jane.Doe.S01E05.720p.x264.mp4",
    "John.Smith.Episode.01.1080p.mp4",
    "John.Smith.Episode.02.1080p.mp4",
    "John.Smith.Episode.03.720p.mp4",
    "Random.File.2024.mp4",
    "Another.Random.File.mp4",
    "Standalone.720p.mp4",
    "Jane.Doe.Special.1080p.mp4",
    "John.Smith.Special.720p.mp4",
    "Third.Series.S01E01.mp4",
    "Third.Series.S01E02.mp4",
    "Third.Series.S01E03.mp4",
];

/// A large set to stress-test the grouping algorithm.
const LARGE_FILE_SET: &[&str] = &[
    "Alpha.Beta.S01E01.720p.x264.mp4",
    "Alpha.Beta.S01E02.720p.x264.mp4",
    "Alpha.Beta.S01E03.1080p.x265.mp4",
    "Alpha.Beta.S01E04.1080p.x265.mp4",
    "Alpha.Beta.S01E05.720p.x264.mp4",
    "Alpha.Beta.S01E06.1080p.mp4",
    "Alpha.Beta.S01E07.720p.mp4",
    "Alpha.Beta.S01E08.1080p.mp4",
    "Gamma.Delta.Episode.01.1080p.mp4",
    "Gamma.Delta.Episode.02.1080p.mp4",
    "Gamma.Delta.Episode.03.720p.mp4",
    "Gamma.Delta.Episode.04.720p.mp4",
    "Gamma.Delta.Episode.05.1080p.mp4",
    "Epsilon.Zeta.Eta.Part.01.mp4",
    "Epsilon.Zeta.Eta.Part.02.mp4",
    "Epsilon.Zeta.Eta.Part.03.mp4",
    "Epsilon.Zeta.Eta.Part.04.mp4",
    "Theta.Iota.2024.Special.mp4",
    "Theta.Iota.2023.Special.mp4",
    "Theta.Iota.Movie.1080p.mp4",
    "Kappa.Lambda.S02E01.mp4",
    "Kappa.Lambda.S02E02.mp4",
    "Kappa.Lambda.S02E03.mp4",
    "Kappa.Lambda.S02E04.mp4",
    "Mu.Nu.Xi.Omicron.Part1.mp4",
    "Mu.Nu.Xi.Omicron.Part2.mp4",
    "Mu.Nu.Xi.Omicron.Part3.mp4",
    "Random.Noise.File.2024.mp4",
    "Another.Noise.File.mp4",
    "Standalone.Movie.2024.1080p.BluRay.mp4",
    "Single.File.Only.mp4",
    "Alpha.Beta.Movie.2024.1080p.mp4",
    "Pi.Rho.Sigma.S01E01.720p.mp4",
    "Pi.Rho.Sigma.S01E02.720p.mp4",
    "Pi.Rho.Sigma.S01E03.1080p.mp4",
    "Tau.Upsilon.S01E01.mp4",
    "Tau.Upsilon.S01E02.mp4",
    "Tau.Upsilon.S01E03.mp4",
    "Tau.Upsilon.S01E04.mp4",
    "Tau.Upsilon.S01E05.mp4",
];

/// Files mixing concatenated and dotted naming conventions.
const MIXED_CONVENTION_FILES: &[&str] = &[
    "JaneDoe.Episode.01.720p.mp4",
    "Jane.Doe.Episode.02.1080p.mp4",
    "janedoe.Episode.03.720p.mp4",
    "JANE.DOE.Episode.04.1080p.mp4",
    "JohnSmith.Episode.01.mp4",
    "John.Smith.Episode.02.mp4",
    "johnsmith.Episode.03.mp4",
];

// ---------------------------------------------------------------------------
// normalize_name
// ---------------------------------------------------------------------------

fn bench_normalize_name(c: &mut Criterion) {
    let mut group = c.benchmark_group("dir_move/normalize_name");

    let inputs = [
        ("simple", "Jane Doe"),
        ("dotted", "Jane.Doe"),
        ("concatenated", "JaneDoe"),
        ("mixed_case", "jAnE dOe"),
        ("long", "Some.Very.Long.Name.With.Many.Parts.And.Dots.And.Spaces"),
        ("already_normalized", "janedoe"),
    ];

    for (label, input) in &inputs {
        group.bench_with_input(BenchmarkId::from_parameter(label), input, |b, input| {
            b.iter(|| normalize_name(black_box(input)));
        });
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// filter_numeric_resolution_and_glue_parts
// ---------------------------------------------------------------------------

fn bench_filter_numeric_resolution_and_glue_parts(c: &mut Criterion) {
    let mut group = c.benchmark_group("dir_move/filter_parts");

    let inputs = [
        ("no_filter", "Jane.Doe.Episode.01.mp4"),
        ("year_only", "Show.2024.S01E01.mkv"),
        ("resolution_only", "Show.1080p.S01E01.mkv"),
        ("glue_words", "Show.and.the.Name.of.Something.mp4"),
        ("mixed", "Show.Name.2024.1080p.and.Episode.S01E01.the.End.mp4"),
        ("dimension", "Movie.1920x1080.x264.mp4"),
        ("all_numeric", "1234.5678.9012.mp4"),
        ("complex", "Artist.Name.ft.Other.2024.1080p.720p.and.More.mp4"),
    ];

    for (label, input) in &inputs {
        group.bench_with_input(BenchmarkId::from_parameter(label), input, |b, input| {
            b.iter(|| filter_numeric_resolution_and_glue_parts(black_box(input)));
        });
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// get_all_n_part_sequences
// ---------------------------------------------------------------------------

fn bench_get_all_n_part_sequences(c: &mut Criterion) {
    let mut group = c.benchmark_group("dir_move/n_part_sequences");

    let filename = "Alpha.Beta.Gamma.Delta.Epsilon.Zeta.Eta.Theta.mp4";

    for n_parts in [1, 2, 3] {
        group.bench_with_input(BenchmarkId::new("parts", n_parts), &n_parts, |b, &n_parts| {
            b.iter(|| get_all_n_part_sequences(black_box(filename), n_parts));
        });
    }

    let short_name = "Alpha.Beta.mp4";
    group.bench_function("short_filename_3_parts", |b| {
        b.iter(|| get_all_n_part_sequences(black_box(short_name), 3));
    });

    let long_name = "A.B.C.D.E.F.G.H.I.J.K.L.M.N.O.P.Q.R.S.T.mp4";
    group.bench_function("long_filename_3_parts", |b| {
        b.iter(|| get_all_n_part_sequences(black_box(long_name), 3));
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// count_prefix_chars
// ---------------------------------------------------------------------------

fn bench_count_prefix_chars(c: &mut Criterion) {
    let mut group = c.benchmark_group("dir_move/count_prefix_chars");

    let inputs = [
        ("short", "AB"),
        ("medium", "Jane.Doe"),
        ("long", "Very.Long.Prefix.Name"),
        ("no_dots", "JaneDoeEpisode"),
        ("unicode", "Ärtiständ.Nämé"),
    ];

    for (label, input) in &inputs {
        group.bench_with_input(BenchmarkId::from_parameter(label), input, |b, input| {
            b.iter(|| count_prefix_chars(black_box(input)));
        });
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// FilteredParts::new
// ---------------------------------------------------------------------------

fn bench_filtered_parts_new(c: &mut Criterion) {
    let mut group = c.benchmark_group("dir_move/filtered_parts_new");

    let inputs = [
        ("short", "Jane.Doe.mp4"),
        ("medium", "Jane.Doe.S01E01.720p.x264.mp4"),
        (
            "long",
            "Some.Very.Long.Name.With.Many.Parts.S01E01.1080p.x265.BluRay.mp4",
        ),
        ("single_part", "JaneDoe"),
        ("two_parts", "Jane.Doe"),
    ];

    for (label, input) in &inputs {
        group.bench_with_input(BenchmarkId::from_parameter(label), input, |b, input| {
            b.iter(|| FilteredParts::new(black_box(input)));
        });
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// FilteredParts::prefix_matches_normalized
// ---------------------------------------------------------------------------

fn bench_prefix_matches_normalized(c: &mut Criterion) {
    let mut group = c.benchmark_group("dir_move/prefix_matches_normalized");

    let parts = FilteredParts::new("Jane.Doe.S01E01.720p.x264.mp4");

    let targets = [
        ("single_exact", "jane"),
        ("single_no_match", "nonexistent"),
        ("two_part_exact", "janedoe"),
        ("three_part_exact", "janedoes01e01"),
        ("starts_with_boundary", "janed"),
        ("empty", ""),
    ];

    for (label, target) in &targets {
        group.bench_with_input(BenchmarkId::from_parameter(label), target, |b, target| {
            b.iter(|| parts.prefix_matches_normalized(black_box(target)));
        });
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// has_word_boundary_at
// ---------------------------------------------------------------------------

fn bench_has_word_boundary_at(c: &mut Criterion) {
    let mut group = c.benchmark_group("dir_move/has_word_boundary_at");

    let cases = [
        ("uppercase_boundary", "JaneDoeEpisode", 4),
        ("lowercase_no_boundary", "janedoe", 4),
        ("digit_boundary", "Alpha123Beta", 5),
        ("at_end", "Jane", 4),
        ("at_start", "Jane", 0),
        ("unicode_boundary", "ÄrtistName", 7),
        ("mid_codepoint", "Ärtiständ", 1),
    ];

    for (label, text, position) in &cases {
        group.bench_with_input(
            BenchmarkId::from_parameter(label),
            &(*text, *position),
            |b, (text, pos)| {
                b.iter(|| FilteredParts::has_word_boundary_at(black_box(text), black_box(*pos)));
            },
        );
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// parts_are_contiguous_with_combined
// ---------------------------------------------------------------------------

fn bench_parts_are_contiguous(c: &mut Criterion) {
    let mut group = c.benchmark_group("dir_move/parts_are_contiguous");

    let original_parts: Vec<String> = vec![
        "Jane".into(),
        "Doe".into(),
        "S01E01".into(),
        "720p".into(),
        "x264".into(),
        "mp4".into(),
    ];

    // Exact contiguous match at start
    let prefix_exact: Vec<&str> = vec!["Jane", "Doe"];
    let combined_exact = "janedoe".to_string();
    group.bench_function("exact_match_start", |b| {
        b.iter(|| {
            parts_are_contiguous_with_combined(
                black_box(&original_parts),
                black_box(&prefix_exact),
                black_box(&combined_exact),
            )
        });
    });

    // Exact contiguous match in middle
    let prefix_mid: Vec<&str> = vec!["S01E01", "720p"];
    let combined_mid = "s01e01720p".to_string();
    group.bench_function("exact_match_middle", |b| {
        b.iter(|| {
            parts_are_contiguous_with_combined(
                black_box(&original_parts),
                black_box(&prefix_mid),
                black_box(&combined_mid),
            )
        });
    });

    // Concatenated form match
    let concat_original: Vec<String> = vec!["JaneDoe".into(), "Episode".into(), "01".into()];
    let prefix_split: Vec<&str> = vec!["Jane", "Doe"];
    let combined_split = "janedoe".to_string();
    group.bench_function("concatenated_form", |b| {
        b.iter(|| {
            parts_are_contiguous_with_combined(
                black_box(&concat_original),
                black_box(&prefix_split),
                black_box(&combined_split),
            )
        });
    });

    // No match at all
    let prefix_none: Vec<&str> = vec!["Nonexistent", "Prefix"];
    let combined_none = "nonexistentprefix".to_string();
    group.bench_function("no_match", |b| {
        b.iter(|| {
            parts_are_contiguous_with_combined(
                black_box(&original_parts),
                black_box(&prefix_none),
                black_box(&combined_none),
            )
        });
    });

    // Extended starts-with match
    let extended_original: Vec<String> = vec!["JaneDoeTV".into(), "Episode".into(), "01".into()];
    let prefix_extended: Vec<&str> = vec!["Jane", "Doe"];
    let combined_extended = "janedoe".to_string();
    group.bench_function("extended_starts_with", |b| {
        b.iter(|| {
            parts_are_contiguous_with_combined(
                black_box(&extended_original),
                black_box(&prefix_extended),
                black_box(&combined_extended),
            )
        });
    });

    // Long original parts list
    let long_original: Vec<String> = (0..20).map(|i| format!("Part{i}")).collect();
    let prefix_end: Vec<&str> = vec!["Part18", "Part19"];
    let combined_end = "part18part19".to_string();
    group.bench_function("long_list_match_at_end", |b| {
        b.iter(|| {
            parts_are_contiguous_with_combined(
                black_box(&long_original),
                black_box(&prefix_end),
                black_box(&combined_end),
            )
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// prefix_matches_normalized_precomputed
// ---------------------------------------------------------------------------

fn bench_prefix_matches_normalized_precomputed(c: &mut Criterion) {
    let mut group = c.benchmark_group("dir_move/prefix_matches_precomputed");

    let file = FileInfo::new(
        PathBuf::from("Jane.Doe.S01E01.720p.x264.mp4"),
        "Jane.Doe.S01E01.720p.x264.mp4".to_string(),
        "Jane.Doe.S01E01.x264.mp4".to_string(),
    );

    let targets = [
        ("match_single", "jane"),
        ("match_two_part", "janedoe"),
        ("match_three_part", "janedoes01e01"),
        ("no_match", "nonexistent"),
    ];

    for (label, target) in &targets {
        group.bench_with_input(BenchmarkId::from_parameter(label), target, |b, target| {
            b.iter(|| prefix_matches_normalized_precomputed(black_box(&file), black_box(target)));
        });
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// find_prefix_candidates
// ---------------------------------------------------------------------------

fn bench_find_prefix_candidates_small(c: &mut Criterion) {
    let files = make_filtered_files(SMALL_FILE_SET);

    let mut group = c.benchmark_group("dir_move/find_prefix_candidates/small");
    group.bench_function("first_file", |b| {
        b.iter(|| {
            find_prefix_candidates(
                black_box(&files[0].filtered_name),
                black_box(&files),
                black_box(2),
                black_box(5),
            )
        });
    });
    group.finish();
}

fn bench_find_prefix_candidates_medium(c: &mut Criterion) {
    let files = make_filtered_files(MEDIUM_FILE_SET);

    let mut group = c.benchmark_group("dir_move/find_prefix_candidates/medium");

    // Benchmark finding candidates for the first file in each logical group
    let representative_indices = [0, 5, 13]; // Jane.Doe, John.Smith, Third.Series
    for &index in &representative_indices {
        let label = &files[index].filtered_name;
        let label_short = if label.len() > 30 { &label[..30] } else { label.as_ref() };
        group.bench_with_input(BenchmarkId::from_parameter(label_short), &index, |b, &index| {
            b.iter(|| {
                find_prefix_candidates(
                    black_box(&files[index].filtered_name),
                    black_box(&files),
                    black_box(2),
                    black_box(5),
                )
            });
        });
    }

    group.finish();
}

fn bench_find_prefix_candidates_large(c: &mut Criterion) {
    let files = make_filtered_files(LARGE_FILE_SET);

    let mut group = c.benchmark_group("dir_move/find_prefix_candidates/large");

    group.bench_function("large_group_file", |b| {
        b.iter(|| {
            find_prefix_candidates(
                black_box(&files[0].filtered_name),
                black_box(&files),
                black_box(2),
                black_box(5),
            )
        });
    });

    group.bench_function("standalone_file", |b| {
        // "Standalone.Movie.2024.1080p.BluRay.mp4" — index 29
        b.iter(|| {
            find_prefix_candidates(
                black_box(&files[29].filtered_name),
                black_box(&files),
                black_box(2),
                black_box(5),
            )
        });
    });

    group.bench_function("min_group_size_1", |b| {
        b.iter(|| {
            find_prefix_candidates(
                black_box(&files[0].filtered_name),
                black_box(&files),
                black_box(1),
                black_box(5),
            )
        });
    });

    group.bench_function("min_prefix_chars_3", |b| {
        b.iter(|| {
            find_prefix_candidates(
                black_box(&files[0].filtered_name),
                black_box(&files),
                black_box(2),
                black_box(3),
            )
        });
    });

    group.finish();
}

/// Benchmark finding candidates across all files (simulates the first pass of
/// `collect_all_prefix_groups`).
fn bench_find_prefix_candidates_all_files(c: &mut Criterion) {
    let mut group = c.benchmark_group("dir_move/find_prefix_candidates/all_files");

    let small_files = make_filtered_files(SMALL_FILE_SET);
    group.bench_function("small_set", |b| {
        b.iter(|| {
            for file in &small_files {
                let _ = find_prefix_candidates(
                    black_box(&file.filtered_name),
                    black_box(&small_files),
                    black_box(2),
                    black_box(5),
                );
            }
        });
    });

    let medium_files = make_filtered_files(MEDIUM_FILE_SET);
    group.bench_function("medium_set", |b| {
        b.iter(|| {
            for file in &medium_files {
                let _ = find_prefix_candidates(
                    black_box(&file.filtered_name),
                    black_box(&medium_files),
                    black_box(2),
                    black_box(5),
                );
            }
        });
    });

    let large_files = make_filtered_files(LARGE_FILE_SET);
    group.bench_function("large_set", |b| {
        b.iter(|| {
            for file in &large_files {
                let _ = find_prefix_candidates(
                    black_box(&file.filtered_name),
                    black_box(&large_files),
                    black_box(2),
                    black_box(5),
                );
            }
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Mixed convention handling (concatenated vs dotted)
// ---------------------------------------------------------------------------

fn bench_mixed_conventions(c: &mut Criterion) {
    let files = make_filtered_files(MIXED_CONVENTION_FILES);

    let mut group = c.benchmark_group("dir_move/mixed_conventions");

    // Concatenated form
    group.bench_function("concatenated_file", |b| {
        b.iter(|| {
            find_prefix_candidates(
                black_box(&files[0].filtered_name), // JaneDoe...
                black_box(&files),
                black_box(2),
                black_box(5),
            )
        });
    });

    // Dotted form
    group.bench_function("dotted_file", |b| {
        b.iter(|| {
            find_prefix_candidates(
                black_box(&files[1].filtered_name), // Jane.Doe...
                black_box(&files),
                black_box(2),
                black_box(5),
            )
        });
    });

    // Lowercase form
    group.bench_function("lowercase_file", |b| {
        b.iter(|| {
            find_prefix_candidates(
                black_box(&files[2].filtered_name), // janedoe...
                black_box(&files),
                black_box(2),
                black_box(5),
            )
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// FileInfo construction (pre-computation of parts)
// ---------------------------------------------------------------------------

fn bench_file_info_construction(c: &mut Criterion) {
    let mut group = c.benchmark_group("dir_move/file_info_new");

    let cases = [
        ("short", "Jane.Doe.mp4"),
        ("medium", "Jane.Doe.S01E01.720p.x264.mp4"),
        (
            "long",
            "Some.Very.Long.Filename.With.Many.Dot.Separated.Parts.2024.1080p.x264.BluRay.mp4",
        ),
        ("concatenated", "JaneDoeEpisode01.mp4"),
    ];

    for (label, name) in &cases {
        group.bench_with_input(BenchmarkId::from_parameter(label), name, |b, name| {
            let filtered = filter_numeric_resolution_and_glue_parts(name);
            b.iter(|| {
                FileInfo::new(
                    black_box(PathBuf::from(name)),
                    black_box(name.to_string()),
                    black_box(filtered.clone()),
                )
            });
        });
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// PrefixGroupBuilder
// ---------------------------------------------------------------------------

fn bench_prefix_group_builder(c: &mut Criterion) {
    let mut group = c.benchmark_group("dir_move/prefix_group_builder");

    group.bench_function("new", |b| {
        b.iter(|| {
            PrefixGroupBuilder::new(
                black_box("JaneDoe".to_string()),
                black_box(PathBuf::from("Jane.Doe.Episode.01.mp4")),
                black_box(2),
                black_box(true),
                black_box(0),
            )
        });
    });

    group.bench_function("add_file_concatenated", |b| {
        b.iter_batched(
            || {
                PrefixGroupBuilder::new(
                    "JaneDoe".to_string(),
                    PathBuf::from("Jane.Doe.Episode.01.mp4"),
                    2,
                    true,
                    0,
                )
            },
            |mut builder| {
                builder.add_file(
                    black_box(PathBuf::from("Jane.Doe.Episode.02.mp4")),
                    black_box(2),
                    black_box(true),
                    black_box(0),
                    black_box("JaneDoe".to_string()),
                );
            },
            criterion::BatchSize::SmallInput,
        );
    });

    group.bench_function("add_file_dotted", |b| {
        b.iter_batched(
            || {
                PrefixGroupBuilder::new(
                    "Jane.Doe".to_string(),
                    PathBuf::from("Jane.Doe.Episode.01.mp4"),
                    2,
                    false,
                    0,
                )
            },
            |mut builder| {
                builder.add_file(
                    black_box(PathBuf::from("Jane.Doe.Episode.02.mp4")),
                    black_box(2),
                    black_box(false),
                    black_box(0),
                    black_box("Jane.Doe".to_string()),
                );
            },
            criterion::BatchSize::SmallInput,
        );
    });

    group.bench_function("into_prefix_group", |b| {
        b.iter_batched(
            || {
                let mut builder = PrefixGroupBuilder::new(
                    "JaneDoe".to_string(),
                    PathBuf::from("Jane.Doe.Episode.01.mp4"),
                    2,
                    true,
                    0,
                );
                for index in 2..=10 {
                    builder.add_file(
                        PathBuf::from(format!("Jane.Doe.Episode.{index:02}.mp4")),
                        2,
                        true,
                        0,
                        "JaneDoe".to_string(),
                    );
                }
                builder
            },
            |builder| {
                black_box(builder.into_prefix_group());
            },
            criterion::BatchSize::SmallInput,
        );
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// End-to-end: full batch pre-processing (filter + FileInfo creation)
// ---------------------------------------------------------------------------

fn bench_batch_preprocessing(c: &mut Criterion) {
    let mut group = c.benchmark_group("dir_move/batch_preprocessing");

    group.bench_function("small_set", |b| {
        b.iter(|| make_filtered_files(black_box(SMALL_FILE_SET)));
    });

    group.bench_function("medium_set", |b| {
        b.iter(|| make_filtered_files(black_box(MEDIUM_FILE_SET)));
    });

    group.bench_function("large_set", |b| {
        b.iter(|| make_filtered_files(black_box(LARGE_FILE_SET)));
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// is_unwanted_directory
// ---------------------------------------------------------------------------

fn bench_is_unwanted_directory(c: &mut Criterion) {
    let mut group = c.benchmark_group("dir_move/is_unwanted_directory");

    let inputs = [
        ("unwanted_exact", ".unwanted"),
        ("unwanted_uppercase", ".UNWANTED"),
        ("normal_dir", "My Videos"),
        ("hidden_dir", ".hidden"),
        ("recycle", "$RECYCLE.BIN"),
    ];

    for (label, input) in &inputs {
        group.bench_with_input(BenchmarkId::from_parameter(label), input, |b, input| {
            b.iter(|| is_unwanted_directory(black_box(input)));
        });
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// Criterion groups
// ---------------------------------------------------------------------------

criterion_group!(
    benches,
    bench_normalize_name,
    bench_filter_numeric_resolution_and_glue_parts,
    bench_get_all_n_part_sequences,
    bench_count_prefix_chars,
    bench_filtered_parts_new,
    bench_prefix_matches_normalized,
    bench_has_word_boundary_at,
    bench_parts_are_contiguous,
    bench_prefix_matches_normalized_precomputed,
    bench_find_prefix_candidates_small,
    bench_find_prefix_candidates_medium,
    bench_find_prefix_candidates_large,
    bench_find_prefix_candidates_all_files,
    bench_mixed_conventions,
    bench_file_info_construction,
    bench_prefix_group_builder,
    bench_batch_preprocessing,
    bench_is_unwanted_directory,
);

criterion_main!(benches);

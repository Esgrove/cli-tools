//! Benchmarks comparing the `cli_tools::simd` kernels with the scalar code they could replace.
//!
//! Each group runs both versions over inputs from token length to file length,
//! so the results show the length where the SIMD dispatch starts to pay off.

use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};

use cli_tools::simd::{byte_positions, contains_byte_of, count_chars, find_byte_of, has_adjacent_bytes_of, level};

/// Input lengths in bytes, from a short token to a large file.
const LENGTHS: &[usize] = &[4, 8, 16, 32, 64, 128, 1024, 65_536];

/// Bytes that can start a string or comment in Rust source, the scanner trigger set.
const TRIGGERS: &[u8] = b"/\"'`<#r";

/// Separators collapsed by `collapse_repeated_separators`.
const SEPARATORS: &[u8] = b".-_ \t";

/// ASCII prose of the given length without any trigger or separator pair.
fn ascii_text(length: usize) -> String {
    "lets count some plain words now "
        .chars()
        .cycle()
        .take(length)
        .collect()
}

/// Prose of about the given length that mixes in multibyte characters.
fn mixed_text(length: usize) -> String {
    let mut text = String::new();
    for character in "Hyvää päivää — naïve café ".chars().cycle() {
        if text.len() + character.len_utf8() > length {
            break;
        }
        text.push(character);
    }
    text
}

/// The width computation `Token::width` uses today.
fn scalar_text_width(text: &str) -> usize {
    if text.is_ascii() {
        text.len()
    } else {
        text.chars().count()
    }
}

/// Byte lookup table for a set, the fastest scalar membership test.
fn byte_table(set: &[u8]) -> [bool; 256] {
    let mut table = [false; 256];
    for &byte in set {
        if let Some(entry) = table.get_mut(usize::from(byte)) {
            *entry = true;
        }
    }
    table
}

/// Benchmark SIMD character counting against `chars().count()` and the width logic `Token::width` uses.
fn bench_count_chars(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("simd/count_chars");
    for &length in LENGTHS {
        for (label, text) in [("ascii", ascii_text(length)), ("mixed", mixed_text(length))] {
            let parameter = format!("{label}_{length}");
            group.bench_with_input(
                BenchmarkId::new("std_chars_count", &parameter),
                &text,
                |bencher, text| {
                    bencher.iter(|| black_box(text.as_str()).chars().count());
                },
            );
            group.bench_with_input(
                BenchmarkId::new("scalar_text_width", &parameter),
                &text,
                |bencher, text| {
                    bencher.iter(|| scalar_text_width(black_box(text)));
                },
            );
            group.bench_with_input(BenchmarkId::new("simd", &parameter), &text, |bencher, text| {
                bencher.iter(|| count_chars(black_box(text)));
            });
        }
    }
    group.finish();
}

/// Benchmark searching for a scanner trigger byte against a slice search and a lookup table.
fn bench_find_byte_of(criterion: &mut Criterion) {
    let table = byte_table(TRIGGERS);
    let mut group = criterion.benchmark_group("simd/contains_trigger");
    for &length in LENGTHS {
        let text = ascii_text(length).replace('r', "s");
        let bytes = text.as_bytes();
        group.bench_with_input(BenchmarkId::new("scalar_contains", length), bytes, |bencher, bytes| {
            bencher.iter(|| black_box(bytes).iter().any(|byte| TRIGGERS.contains(byte)));
        });
        group.bench_with_input(BenchmarkId::new("scalar_table", length), bytes, |bencher, bytes| {
            bencher.iter(|| {
                black_box(bytes)
                    .iter()
                    .any(|&byte| table.get(usize::from(byte)).copied().unwrap_or(false))
            });
        });
        group.bench_with_input(BenchmarkId::new("simd", length), bytes, |bencher, bytes| {
            bencher.iter(|| contains_byte_of(black_box(bytes), TRIGGERS));
        });
    }
    group.finish();
}

/// Benchmark finding the next whitespace candidate against the character loop the tokenizer used before.
fn bench_find_whitespace(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("simd/find_whitespace");
    // The same candidate set as `CHUNK_END_CANDIDATES` in the private slb tokenizer module.
    let whitespace = b" \t\n\x0b\x0c\r\xc2\xe1\xe2\xe3";
    for &length in LENGTHS {
        let word = "x".repeat(length);
        let bytes = word.as_bytes();
        group.bench_with_input(BenchmarkId::new("std_char_indices", length), &word, |bencher, word| {
            bencher.iter(|| {
                black_box(word.as_str())
                    .char_indices()
                    .find(|(_, character)| character.is_whitespace())
                    .map(|(offset, _)| offset)
            });
        });
        group.bench_with_input(BenchmarkId::new("simd", length), bytes, |bencher, bytes| {
            bencher.iter(|| find_byte_of(black_box(bytes), whitespace));
        });
    }
    group.finish();
}

/// Benchmark detecting two adjacent separators against a scalar `windows(2)` scan.
fn bench_adjacent_separators(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("simd/adjacent_separators");
    for &length in LENGTHS {
        let name: String = "Some.Movie.Name-2024_x".chars().cycle().take(length).collect();
        let bytes = name.as_bytes();
        group.bench_with_input(BenchmarkId::new("scalar_windows", length), bytes, |bencher, bytes| {
            bencher.iter(|| {
                black_box(bytes)
                    .windows(2)
                    .any(|pair| pair.iter().all(|byte| SEPARATORS.contains(byte)))
            });
        });
        group.bench_with_input(BenchmarkId::new("simd", length), bytes, |bencher, bytes| {
            bencher.iter(|| has_adjacent_bytes_of(black_box(bytes), SEPARATORS));
        });
    }
    group.finish();
}

/// Benchmark collecting every dot position against a scalar `enumerate` and `filter_map`.
fn bench_byte_positions(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("simd/dot_positions");
    for &length in LENGTHS {
        let name: String = "Some.Movie.Name.".chars().cycle().take(length).collect();
        let bytes = name.as_bytes();
        group.bench_with_input(BenchmarkId::new("scalar_enumerate", length), bytes, |bencher, bytes| {
            bencher.iter(|| {
                black_box(bytes)
                    .iter()
                    .enumerate()
                    .filter_map(|(index, &byte)| (byte == b'.').then_some(index))
                    .collect::<Vec<usize>>()
            });
        });
        group.bench_with_input(BenchmarkId::new("simd", length), bytes, |bencher, bytes| {
            bencher.iter(|| byte_positions(black_box(bytes), b'.'));
        });
    }
    group.finish();
}

/// Benchmark the fixed dispatch cost on empty input, and print the detected SIMD level.
fn bench_dispatch(criterion: &mut Criterion) {
    eprintln!("SIMD level: {:?}", level());
    let mut group = criterion.benchmark_group("simd/dispatch");
    group.bench_function("count_chars_empty", |bencher| {
        bencher.iter(|| count_chars(black_box("")));
    });
    group.bench_function("contains_byte_of_empty", |bencher| {
        bencher.iter(|| contains_byte_of(black_box(b""), TRIGGERS));
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_count_chars,
    bench_find_byte_of,
    bench_find_whitespace,
    bench_adjacent_separators,
    bench_byte_positions,
    bench_dispatch
);
criterion_main!(benches);

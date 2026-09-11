//! Benchmarks for the semantic line breaks prose engine.
//!
//! Measures tokenization, boundary detection, paragraph reflow, and whole file formatting.

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};

use cli_tools::semantic_line_breaks::prose::{find_boundaries, reflow_paragraph, tokenize_line};
use cli_tools::semantic_line_breaks::{FileKind, FormatOptions, HardBreak, Paragraph, format};

/// A long sentence with several clause boundaries.
const LONG_SENTENCE: &str = "The formatter joins lines that were wrapped in the middle of a clause, \
    rewords semicolons into separate sentences, and breaks long lines before conjunctions such as \
    and or but because that usually reads best when the halves are balanced.";

/// A hard-wrapped Rust doc comment block.
const RUST_SOURCE: &str = r"//! Module docs that were hard wrapped at eighty
//! columns by an editor, which is exactly the kind
//! of text the formatter is supposed to repair.

/// Parse the header; the caller handles errors — always.
/// Returns the parsed
/// header struct.
pub fn parse(input: &str) -> Header {
    let width = 80; // default width
    Header::new(input, width)
}
";

fn paragraph() -> Paragraph {
    let lines: Vec<String> = LONG_SENTENCE
        .split(", ")
        .map(std::string::ToString::to_string)
        .collect();
    let hard_breaks = vec![HardBreak::None; lines.len()];
    Paragraph {
        start_line: 0,
        end_line: lines.len(),
        first_prefix: "/// ".to_string(),
        rest_prefix: "/// ".to_string(),
        last_suffix: String::new(),
        lines,
        hard_breaks,
    }
}

fn bench_tokenize(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("tokenize_line");
    group.bench_function("long_sentence", |bencher| {
        bencher.iter(|| tokenize_line(black_box(LONG_SENTENCE), 0, true));
    });
    group.finish();
}

fn bench_boundaries(criterion: &mut Criterion) {
    let options = FormatOptions::default();
    let tokens = tokenize_line(LONG_SENTENCE, 0, true);
    let mut group = criterion.benchmark_group("find_boundaries");
    group.bench_function("long_sentence", |bencher| {
        bencher.iter(|| find_boundaries(black_box(&tokens), &options));
    });
    group.finish();
}

fn bench_reflow(criterion: &mut Criterion) {
    let options = FormatOptions::with_width(100);
    let paragraph = paragraph();
    let mut group = criterion.benchmark_group("reflow_paragraph");
    group.bench_function("clauses_to_lines", |bencher| {
        bencher.iter(|| reflow_paragraph(black_box(&paragraph), &options));
    });
    group.finish();
}

fn bench_format(criterion: &mut Criterion) {
    let options = FormatOptions::default();
    let mut group = criterion.benchmark_group("format");
    group.bench_function("rust_source", |bencher| {
        bencher.iter(|| format(black_box(RUST_SOURCE), FileKind::Rust, &options));
    });
    group.finish();
}

criterion_group!(benches, bench_tokenize, bench_boundaries, bench_reflow, bench_format);
criterion_main!(benches);

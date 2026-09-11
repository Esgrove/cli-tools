//! Benchmarks for the semantic line breaks prose engine.
//!
//! Measures tokenization, boundary detection, paragraph reflow, and whole file formatting.

use std::fmt::Write as _;
use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};

use cli_tools::semantic_line_breaks::comments::{fix_trailing_comments, split_source_regions};
use cli_tools::semantic_line_breaks::markdown::split_paragraphs;
use cli_tools::semantic_line_breaks::prose::{find_boundaries, reflow_paragraph, tokenize_line};
use cli_tools::semantic_line_breaks::{FileKind, FormatOptions, HardBreak, Paragraph, check, format};

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

/// A Rust source file with code, doc comments, strings, and trailing comments repeated many times.
///
/// Most lines are code that the formatter copies verbatim,
/// which is the common case when checking a whole project.
fn large_rust_source() -> String {
    let mut source = String::from("//! Module level documentation for the generated benchmark input.\n");
    for index in 0..200 {
        write!(
            source,
            r##"
/// Parse the {index} header and return the struct, this doc line is deliberately hard wrapped
/// so that the reflow pass has to join it back together again.
pub fn parse_{index}(input: &str) -> Header {{
    let pattern = "a // b /* c */ string that must not be treated as a comment";
    let width = 80; // default width
    let raw = r#"raw {index} string"#;
    // A normal comment block that is already formatted correctly.
    // It stays as it is.
    Header::new(input, width, pattern, raw)
}}
"##
        )
        .expect("writing to a string cannot fail");
    }
    source
}

/// A Markdown document with prose paragraphs, lists, tables, and fenced code blocks.
fn large_markdown() -> String {
    let mut document = String::from("# Benchmark document\n");
    for index in 0..100 {
        write!(
            document,
            r"
## Section {index}

{LONG_SENTENCE}

- A list item that is long enough to need a break at a clause boundary, because it keeps going.
- A short item.

| Column | Value |
| ------ | ----- |
| {index} | yes |

```rust
let value = {index};
```
"
        )
        .expect("writing to a string cannot fail");
    }
    document
}

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
    let rust_file = large_rust_source();
    let markdown_file = large_markdown();
    let mut group = criterion.benchmark_group("format");
    group.bench_function("rust_source", |bencher| {
        bencher.iter(|| format(black_box(RUST_SOURCE), FileKind::Rust, &options));
    });
    group.bench_function("large_rust_file", |bencher| {
        bencher.iter(|| format(black_box(&rust_file), FileKind::Rust, &options));
    });
    group.bench_function("large_markdown_file", |bencher| {
        bencher.iter(|| format(black_box(&markdown_file), FileKind::Markdown, &options));
    });
    group.finish();
}

fn bench_check(criterion: &mut Criterion) {
    let options = FormatOptions::default();
    let rust_file = large_rust_source();
    let mut group = criterion.benchmark_group("check");
    group.bench_function("large_rust_file", |bencher| {
        bencher.iter(|| check(black_box(&rust_file), FileKind::Rust, &options));
    });
    group.finish();
}

fn bench_scan(criterion: &mut Criterion) {
    let options = FormatOptions::default();
    let source = large_rust_source();
    let lines: Vec<&str> = source.lines().collect();
    let mut group = criterion.benchmark_group("scan");
    group.bench_function("split_source_regions", |bencher| {
        bencher.iter(|| split_source_regions(black_box(&lines), FileKind::Rust));
    });
    group.bench_function("fix_trailing_comments", |bencher| {
        bencher.iter(|| fix_trailing_comments(black_box(&lines), FileKind::Rust, &options));
    });
    group.finish();
}

fn bench_split_paragraphs(criterion: &mut Criterion) {
    let document = large_markdown();
    let lines: Vec<&str> = document.lines().collect();
    let mut group = criterion.benchmark_group("split_paragraphs");
    group.bench_function("large_markdown_file", |bencher| {
        bencher.iter(|| split_paragraphs(black_box(&lines), "", 0, true));
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_tokenize,
    bench_boundaries,
    bench_reflow,
    bench_format,
    bench_check,
    bench_scan,
    bench_split_paragraphs
);
criterion_main!(benches);

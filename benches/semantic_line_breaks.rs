//! Benchmarks for the semantic line breaks prose engine.
//!
//! Measures tokenization, boundary detection, paragraph reflow, and whole file formatting.

use std::fmt::Write as _;
use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};

use cli_tools::glob_to_regex;
use cli_tools::semantic_line_breaks::boundaries::{clause_rank, find_boundaries};
use cli_tools::semantic_line_breaks::comments::{fix_trailing_comments, split_source_regions};
use cli_tools::semantic_line_breaks::markdown::split_paragraphs;
use cli_tools::semantic_line_breaks::project_config::discover_width;
use cli_tools::semantic_line_breaks::reflow::reflow_paragraph;
use cli_tools::semantic_line_breaks::tokenizer::tokenize_line;
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

/// A line that is almost entirely punctuation, brackets, code spans, and emphasis.
///
/// This is the worst case for the tokenizer, since nearly every chunk peels leading and trailing characters.
const PUNCTUATED_LINE: &str = "Note (see `docs/setup.md`): the **first value**, e.g. `a_b(1)`, \
    is read [from the header][spec] — the second one, *which is optional*, is not, \
    so pass `--force` to override it.";

/// A line of non-ASCII prose, where a character count differs from a byte count.
const UNICODE_LINE: &str = "Tämä rivi sisältää ääkkösiä ja muita merkkejä, esim. „lainausmerkkejä“, \
    joten merkkien määrä ei ole sama kuin tavujen määrä, ja rivin leveys on laskettava merkeistä.";

/// A Rust source file that is mostly doc comment prose rather than code.
///
/// The reflow pass dominates here, which is the case `large_rust_source` does not cover
/// because most of its lines are code that is copied verbatim.
fn large_prose_source() -> String {
    let mut source = String::new();
    for index in 0..200 {
        write!(
            source,
            r"
/// Parse the {index} header: the caller owns the buffer, so the parser borrows it for the call
/// and returns a view into it, which keeps the hot path free of copies.
///
/// {LONG_SENTENCE}
pub fn parse_{index}() {{}}
"
        )
        .expect("writing to a string cannot fail");
    }
    source
}

/// A shell script with `#` comments, a heredoc, and an aligned usage block.
fn shell_source() -> String {
    let mut source = String::from("#!/usr/bin/env bash\nset -euo pipefail\n");
    for index in 0..200 {
        write!(
            source,
            r"
# Step {index}: prepare the inputs; the caller checks the exit code afterwards.
#   SERVER_PORT       port to expect (default 9339)
#   PUBLIC_HOST_IP    address handed to the clients
run_step_{index}() {{
    cat <<'NOTES' > notes.txt
# not a comment
NOTES
    echo 'done' # print the result
}}
"
        )
        .expect("writing to a string cannot fail");
    }
    source
}

/// A Python module with docstrings and `#` comments.
fn python_source() -> String {
    let mut source = String::from("#!/usr/bin/env python3\n");
    for index in 0..200 {
        write!(
            source,
            r#"
def parse_{index}(value):
    """Parse the {index} value and return it; the caller handles the errors.

    {LONG_SENTENCE}
    """
    total = value + {index}  # running total
    return total
"#
        )
        .expect("writing to a string cannot fail");
    }
    source
}

/// A C++ source file with block comments.
fn c_source() -> String {
    let mut source = String::new();
    for index in 0..200 {
        write!(
            source,
            r"
/**
 * Compute the {index} checksum and return it; zero means empty.
 *
 * {LONG_SENTENCE}
 */
int checksum_{index}(const char* buffer) {{
    int sum = 0; // running total
    return sum;
}}
"
        )
        .expect("writing to a string cannot fail");
    }
    source
}

/// Two hundred small files, the shape of a repository walk rather than one large file.
fn many_small_files() -> Vec<String> {
    (0..200)
        .map(|index| {
            format!(
                r"//! Module {index} documentation that runs past the limit and has to be broken somewhere.

/// Parse the header; the caller handles the errors — always.
pub fn parse_{index}(input: &str) -> Header {{
    let width = 80; // default width
    Header::new(input, width)
}}
"
            )
        })
        .collect()
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

/// A paragraph of many short clauses joined with semicolons.
///
/// Every semicolon is a candidate for rewording,
/// so the cost of deciding one of them is multiplied by the count.
/// The `stray` variant adds an unclosed bracket,
/// which is the input that tells matched bracket counting apart from a running total.
fn semicolon_paragraph(stray: bool) -> Paragraph {
    let mut line = if stray {
        "the run starts (here".to_string()
    } else {
        "the run starts here".to_string()
    };
    for index in 0..120 {
        write!(line, "; the step {index} follows the one before it").expect("writing to a string cannot fail");
    }
    Paragraph {
        start_line: 0,
        end_line: 1,
        first_prefix: "/// ".to_string(),
        rest_prefix: "/// ".to_string(),
        last_suffix: String::new(),
        lines: vec![line],
        hard_breaks: vec![HardBreak::None],
    }
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
    group.bench_function("punctuated_line", |bencher| {
        bencher.iter(|| tokenize_line(black_box(PUNCTUATED_LINE), 0, true));
    });
    group.bench_function("unicode_line", |bencher| {
        bencher.iter(|| tokenize_line(black_box(UNICODE_LINE), 0, true));
    });
    group.finish();
}

fn bench_boundaries(criterion: &mut Criterion) {
    let options = FormatOptions::default();
    let tokens = tokenize_line(LONG_SENTENCE, 0, true);
    let punctuated = tokenize_line(PUNCTUATED_LINE, 0, true);
    let mut group = criterion.benchmark_group("find_boundaries");
    group.bench_function("long_sentence", |bencher| {
        bencher.iter(|| find_boundaries(black_box(&tokens), &options));
    });
    group.bench_function("punctuated_line", |bencher| {
        bencher.iter(|| find_boundaries(black_box(&punctuated), &options));
    });
    group.finish();
}

fn bench_clause_rank(criterion: &mut Criterion) {
    let options = FormatOptions::default();
    let tokens = tokenize_line(LONG_SENTENCE, 0, true);
    let mut group = criterion.benchmark_group("clause_rank");
    group.bench_function("every_token", |bencher| {
        bencher.iter(|| {
            let mut ranks = 0usize;
            for index in 0..tokens.len() {
                ranks += usize::from(clause_rank(black_box(&tokens), index, &options).is_some());
            }
            ranks
        });
    });
    group.finish();
}

fn bench_reflow(criterion: &mut Criterion) {
    let options = FormatOptions::with_width(100);
    let paragraph = paragraph();
    let mut group = criterion.benchmark_group("reflow_paragraph");
    group.bench_function("clauses_to_lines", |bencher| {
        bencher.iter(|| reflow_paragraph(black_box(&paragraph), &options, true));
    });
    group.finish();
}

fn bench_reword_semicolons(criterion: &mut Criterion) {
    let options = FormatOptions::with_width(100);
    let balanced = semicolon_paragraph(false);
    let stray_bracket = semicolon_paragraph(true);
    let mut group = criterion.benchmark_group("reword_semicolons");
    group.bench_function("many_semicolons", |bencher| {
        bencher.iter(|| reflow_paragraph(black_box(&balanced), &options, true));
    });
    group.bench_function("many_semicolons_with_a_stray_bracket", |bencher| {
        bencher.iter(|| reflow_paragraph(black_box(&stray_bracket), &options, true));
    });
    group.finish();
}

fn bench_format(criterion: &mut Criterion) {
    let options = FormatOptions::default();
    let rust_file = large_rust_source();
    let markdown_file = large_markdown();
    let prose_file = large_prose_source();
    let small_files = many_small_files();
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
    group.bench_function("large_prose_file", |bencher| {
        bencher.iter(|| format(black_box(&prose_file), FileKind::Rust, &options));
    });
    group.bench_function("many_small_files", |bencher| {
        bencher.iter(|| {
            for file in black_box(&small_files) {
                black_box(format(file, FileKind::Rust, &options));
            }
        });
    });
    group.finish();
}

fn bench_check(criterion: &mut Criterion) {
    let options = FormatOptions::default();
    let rust_file = large_rust_source();
    let prose_file = large_prose_source();
    let markdown_file = large_markdown();
    let mut group = criterion.benchmark_group("check");
    group.bench_function("large_rust_file", |bencher| {
        bencher.iter(|| check(black_box(&rust_file), FileKind::Rust, &options));
    });
    group.bench_function("large_prose_file", |bencher| {
        bencher.iter(|| check(black_box(&prose_file), FileKind::Rust, &options));
    });
    group.bench_function("large_markdown_file", |bencher| {
        bencher.iter(|| check(black_box(&markdown_file), FileKind::Markdown, &options));
    });
    group.finish();
}

fn bench_scan(criterion: &mut Criterion) {
    let options = FormatOptions::default();
    let source = large_rust_source();
    let shell = shell_source();
    let python = python_source();
    let c_plus_plus = c_source();
    let lines: Vec<&str> = source.lines().collect();
    let shell_lines: Vec<&str> = shell.lines().collect();
    let python_lines: Vec<&str> = python.lines().collect();
    let c_lines: Vec<&str> = c_plus_plus.lines().collect();
    let mut group = criterion.benchmark_group("scan");
    group.bench_function("split_source_regions", |bencher| {
        bencher.iter(|| split_source_regions(black_box(&lines), FileKind::Rust));
    });
    group.bench_function("fix_trailing_comments", |bencher| {
        bencher.iter(|| fix_trailing_comments(black_box(&lines), FileKind::Rust, &options));
    });
    group.bench_function("split_shell_regions", |bencher| {
        bencher.iter(|| split_source_regions(black_box(&shell_lines), FileKind::Shell));
    });
    group.bench_function("split_python_regions", |bencher| {
        bencher.iter(|| split_source_regions(black_box(&python_lines), FileKind::Python));
    });
    group.bench_function("split_c_regions", |bencher| {
        bencher.iter(|| split_source_regions(black_box(&c_lines), FileKind::CLike));
    });
    group.finish();
}

fn bench_project_config(criterion: &mut Criterion) {
    let root = tempfile::tempdir().expect("the temporary directory should be created");
    let nested = root.path().join("src").join("bin").join("tool");
    std::fs::create_dir_all(&nested).expect("the directories should be created");
    std::fs::write(
        root.path().join(".editorconfig"),
        "root = true
[*]
max_line_length = 120
[*.md]
max_line_length = 100
",
    )
    .expect("the file should be written");
    std::fs::write(
        root.path().join("rustfmt.toml"),
        "max_width = 120
",
    )
    .expect("the file should be written");
    let file = nested.join("main.rs");
    std::fs::write(
        &file,
        "fn main() {}
",
    )
    .expect("the file should be written");

    let mut group = criterion.benchmark_group("project_config");
    group.bench_function("discover_width", |bencher| {
        bencher.iter(|| discover_width(black_box(&file), FileKind::Rust));
    });
    group.bench_function("glob_to_regex", |bencher| {
        bencher.iter(|| glob_to_regex(black_box("**/*.{md,markdown}")));
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
    bench_clause_rank,
    bench_reflow,
    bench_reword_semicolons,
    bench_format,
    bench_check,
    bench_scan,
    bench_split_paragraphs,
    bench_project_config
);
criterion_main!(benches);

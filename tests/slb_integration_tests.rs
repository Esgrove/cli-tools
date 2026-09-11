//! Integration tests for the `slb` formatter using fixture file pairs.
//!
//! Each `*.in.*` fixture under `tests/fixtures/slb/` is formatted and compared to the matching `*.out.*` file.
//! The expected output is formatted again to verify that formatting is idempotent.

use std::fs;
use std::path::{Path, PathBuf};

use cli_tools::semantic_line_breaks::{FileKind, FormatOptions, format};

/// Directory holding the fixture pairs.
fn fixture_directory() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("slb")
}

/// Read a fixture file.
fn read_fixture(name: &str) -> String {
    let path = fixture_directory().join(name);
    fs::read_to_string(&path).expect("failed to read fixture file")
}

/// Format the input fixture and compare with the expected output, then check idempotence.
fn assert_fixture(input_name: &str, output_name: &str) {
    let input = read_fixture(input_name);
    let expected = read_fixture(output_name);
    let kind = FileKind::from_path(Path::new(output_name)).expect("fixture should have a known kind");
    let options = FormatOptions::default();

    let result = format(&input, kind, &options);
    let actual = result.fixed_text.unwrap_or_else(|| input.clone());
    assert_eq!(
        actual, expected,
        "formatted output of {input_name} differs from {output_name}"
    );

    let second = format(&expected, kind, &options);
    assert_eq!(second.fixed_text, None, "formatting {output_name} again changed it");
    assert!(
        second.violations.iter().all(|violation| !violation.fixable),
        "fixable violations remain in {output_name}: {:?}",
        second.violations
    );
}

#[test]
fn rust_doc_comments_fixture() {
    assert_fixture("rust_doc_comments.in.rs", "rust_doc_comments.out.rs");
}

#[test]
fn python_docstring_fixture() {
    assert_fixture("python_docstring.in.py", "python_docstring.out.py");
}

#[test]
fn markdown_document_fixture() {
    assert_fixture("markdown_document.in.md", "markdown_document.out.md");
}

#[test]
fn c_block_comment_fixture() {
    assert_fixture("c_block_comment.in.cpp", "c_block_comment.out.cpp");
}

#[test]
fn yaml_comments_fixture() {
    assert_fixture("yaml_comments.in.yml", "yaml_comments.out.yml");
}

#[test]
fn every_input_fixture_has_an_expected_output() {
    let entries = fs::read_dir(fixture_directory()).expect("fixture directory should exist");
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Some((stem, rest)) = name.split_once(".in.") {
            let expected = fixture_directory().join(format!("{stem}.out.{rest}"));
            assert!(expected.exists(), "missing expected output for {name}");
        }
    }
}

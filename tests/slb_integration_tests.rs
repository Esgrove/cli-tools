//! Integration tests for the `slb` formatter using fixture file pairs.
//!
//! Each `*.in.*` fixture under `tests/fixtures/slb/` is formatted
//! and compared to the matching `*.out.*` file.
//! The expected output is formatted again to verify that formatting is idempotent.

use std::fs;
use std::path::{Path, PathBuf};

use cli_tools::semantic_line_breaks::{FileKind, FormatOptions, RuleSet, format};

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

/// Every `*.in.*` fixture paired with the name of its expected output.
fn fixture_pairs() -> Vec<(String, String)> {
    let entries = fs::read_dir(fixture_directory()).expect("fixture directory should exist");
    entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let (stem, rest) = name.split_once(".in.")?;
            Some((name.clone(), format!("{stem}.out.{rest}")))
        })
        .collect()
}

/// Format the input fixture and compare with the expected output, then check idempotence.
fn assert_fixture(input_name: &str, output_name: &str) {
    let input = read_fixture(input_name);
    let expected = read_fixture(output_name);
    let kind = FileKind::from_path(Path::new(output_name)).expect("fixture should have a known kind");
    // The fixtures cover the opt-in trailing comment rule too, so every rule is on here.
    let options = FormatOptions {
        rules: RuleSet::ALL,
        ..FormatOptions::default()
    };

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
fn typescript_comments_fixture() {
    assert_fixture("typescript_comments.in.ts", "typescript_comments.out.ts");
}

#[test]
fn go_comments_fixture() {
    assert_fixture("go_comments.in.go", "go_comments.out.go");
}

#[test]
fn shell_script_fixture() {
    assert_fixture("shell_script.in.sh", "shell_script.out.sh");
}

#[test]
fn sql_queries_fixture() {
    assert_fixture("sql_queries.in.sql", "sql_queries.out.sql");
}

#[test]
fn toml_config_fixture() {
    assert_fixture("toml_config.in.toml", "toml_config.out.toml");
}

#[test]
fn bracket_prose_fixture() {
    assert_fixture("bracket_prose.in.ts", "bracket_prose.out.ts");
}

#[test]
fn markdown_readme_fixture() {
    assert_fixture("markdown_readme.in.md", "markdown_readme.out.md");
}

#[test]
fn markdown_github_fixture() {
    assert_fixture("markdown_github.in.md", "markdown_github.out.md");
}

#[test]
fn markdown_emphasis_fixture() {
    assert_fixture("markdown_emphasis.in.md", "markdown_emphasis.out.md");
}

#[test]
fn dash_and_colon_fixture() {
    assert_fixture("dash_and_colon.in.rs", "dash_and_colon.out.rs");
}

#[test]
fn ruby_comments_fixture() {
    assert_fixture("ruby_comments.in.rb", "ruby_comments.out.rb");
}

#[test]
fn lua_comments_fixture() {
    assert_fixture("lua_comments.in.lua", "lua_comments.out.lua");
}

#[test]
fn dockerfile_fixture() {
    assert_fixture("container.in.dockerfile", "container.out.dockerfile");
}

#[test]
fn makefile_fixture() {
    assert_fixture("build_rules.in.mk", "build_rules.out.mk");
}

#[test]
fn cmake_comments_fixture() {
    assert_fixture("cmake_comments.in.cmake", "cmake_comments.out.cmake");
}

#[test]
fn cmake_lists_fixture() {
    assert_fixture("cmake_lists.in.cmake", "cmake_lists.out.cmake");
}

#[test]
fn jenkins_pipeline_fixture() {
    assert_fixture("jenkins_pipeline.in.jenkinsfile", "jenkins_pipeline.out.jenkinsfile");
}

#[test]
fn doc_tags_fixture() {
    assert_fixture("doc_tags.in.java", "doc_tags.out.java");
}

#[test]
fn comment_clauses_fixture() {
    assert_fixture("comment_clauses.in.c", "comment_clauses.out.c");
}

#[test]
fn markdown_balance_fixture() {
    assert_fixture("markdown_balance.in.md", "markdown_balance.out.md");
}

#[test]
fn shell_arguments_fixture() {
    assert_fixture("shell_arguments.in.sh", "shell_arguments.out.sh");
}

#[test]
fn sql_comment_block_fixture() {
    assert_fixture("sql_comment_block.in.sql", "sql_comment_block.out.sql");
}

#[test]
fn manual_reflow_comments_fixture() {
    assert_fixture("manual_reflow_comments.in.cpp", "manual_reflow_comments.out.cpp");
}

#[test]
fn manual_reflow_markdown_fixture() {
    assert_fixture("manual_reflow_markdown.in.md", "manual_reflow_markdown.out.md");
}

#[test]
fn manual_reflow_python_fixture() {
    assert_fixture("manual_reflow_python.in.py", "manual_reflow_python.out.py");
}

#[test]
fn clause_words_fixture() {
    assert_fixture("clause_words.in.md", "clause_words.out.md");
}

#[test]
fn dangling_words_fixture() {
    assert_fixture("dangling_words.in.md", "dangling_words.out.md");
}

#[test]
fn conjunctions_fixture() {
    assert_fixture("conjunctions.in.md", "conjunctions.out.md");
}

#[test]
fn abbreviations_fixture() {
    assert_fixture("abbreviations.in.md", "abbreviations.out.md");
}

#[test]
fn dashes_and_semicolons_fixture() {
    assert_fixture("dashes_and_semicolons.in.md", "dashes_and_semicolons.out.md");
}

#[test]
fn brackets_and_quotes_fixture() {
    assert_fixture("brackets_and_quotes.in.md", "brackets_and_quotes.out.md");
}

#[test]
fn inline_html_fixture() {
    assert_fixture("inline_html.in.md", "inline_html.out.md");
}

#[test]
fn code_like_lines_fixture() {
    assert_fixture("code_like_lines.in.md", "code_like_lines.out.md");
}

#[test]
fn directive_comments_fixture() {
    assert_fixture("directive_comments.in.ts", "directive_comments.out.ts");
}

#[test]
fn code_like_comments_fixture() {
    assert_fixture("code_like_comments.in.py", "code_like_comments.out.py");
}

#[test]
fn every_fixture_pair_formats_to_its_expected_output() {
    let mut pairs = fixture_pairs();
    pairs.sort();
    assert!(
        pairs.len() >= 40,
        "expected every fixture pair to be found, got {pairs:?}"
    );
    for (input, output) in pairs {
        assert_fixture(&input, &output);
    }
}

#[test]
fn every_input_fixture_has_an_expected_output() {
    for (input, output) in fixture_pairs() {
        let expected = fixture_directory().join(&output);
        assert!(expected.exists(), "missing expected output for {input}");
    }
}

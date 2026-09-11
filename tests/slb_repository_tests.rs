//! Property tests that run the `slb` formatter over the real source files of this repository.
//!
//! The fixture tests cover hand written examples.
//! These tests check the invariants that must hold for any real input:
//! formatting is idempotent, a fixed file has no fixable violations left,
//! and code lines are never touched when the trailing comment rule is off.

use std::path::{Path, PathBuf};

use cli_tools::semantic_line_breaks::{FileKind, FormatOptions, RuleSet, ViolationKind, check, format};

/// Directories whose sources are used as test input.
const SOURCE_DIRECTORIES: &[&str] = &["src", "benches"];

/// Line width used for the repository sources, matching `rustfmt.toml`.
const REPOSITORY_WIDTH: usize = 120;

/// All source files under the given repository directories, sorted by path.
fn source_files() -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files: Vec<PathBuf> = SOURCE_DIRECTORIES
        .iter()
        .flat_map(|directory| walkdir::WalkDir::new(root.join(directory)))
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .map(walkdir::DirEntry::into_path)
        .filter(|path| FileKind::from_path(path).is_some())
        .collect();
    files.sort();
    files
}

/// Lines that are not part of a comment block, which the prose pass must never change.
fn code_lines(text: &str) -> Vec<&str> {
    text.lines()
        .filter(|line| {
            let trimmed = line.trim_start();
            !trimmed.is_empty() && !trimmed.starts_with("//") && !trimmed.starts_with("/*") && !trimmed.starts_with('*')
        })
        .collect()
}

/// Options for the repository sources, with the given rules.
fn options(rules: RuleSet) -> FormatOptions {
    FormatOptions {
        rules,
        ..FormatOptions::with_width(REPOSITORY_WIDTH)
    }
}

#[test]
fn the_source_files_are_found() {
    let files = source_files();
    assert!(
        files.len() > 20,
        "expected the repository sources as test input, found {}",
        files.len()
    );
}

#[test]
fn formatting_every_source_file_is_idempotent() {
    let options = options(RuleSet::ALL);
    for file in source_files() {
        let text = std::fs::read_to_string(&file).expect("source file should be readable");
        let kind = FileKind::from_path(&file).expect("source file should have a known kind");
        let Some(fixed) = format(&text, kind, &options).fixed_text else {
            continue;
        };
        let second = format(&fixed, kind, &options);
        assert_eq!(
            second.fixed_text,
            None,
            "formatting {} twice changed it again",
            file.display()
        );
    }
}

#[test]
fn no_fixable_violation_remains_after_fixing_a_source_file() {
    let options = options(RuleSet::ALL);
    for file in source_files() {
        let text = std::fs::read_to_string(&file).expect("source file should be readable");
        let kind = FileKind::from_path(&file).expect("source file should have a known kind");
        let fixed = format(&text, kind, &options).fixed_text.unwrap_or(text);
        let violations = check(&fixed, kind, &options);
        let remaining: Vec<ViolationKind> = violations
            .iter()
            .filter(|violation| violation.fixable)
            .map(|violation| violation.kind)
            .collect();
        assert!(
            remaining.is_empty(),
            "{} still has fixable violations after fixing: {remaining:?}",
            file.display()
        );
    }
}

#[test]
fn the_prose_pass_never_changes_code_lines() {
    let rules = RuleSet {
        trailing_comment: false,
        ..RuleSet::ALL
    };
    let options = options(rules);
    for file in source_files() {
        let text = std::fs::read_to_string(&file).expect("source file should be readable");
        let kind = FileKind::from_path(&file).expect("source file should have a known kind");
        let Some(fixed) = format(&text, kind, &options).fixed_text else {
            continue;
        };
        assert_eq!(
            code_lines(&fixed),
            code_lines(&text),
            "the prose pass changed code lines in {}",
            file.display()
        );
    }
}

#[test]
fn a_source_file_is_unchanged_when_no_rule_is_enabled() {
    let options = options(RuleSet::NONE);
    for file in source_files() {
        let text = std::fs::read_to_string(&file).expect("source file should be readable");
        let kind = FileKind::from_path(&file).expect("source file should have a known kind");
        let result = format(&text, kind, &options);
        assert_eq!(
            result.fixed_text,
            None,
            "{} changed with every rule disabled",
            file.display()
        );
        assert!(
            result.violations.is_empty(),
            "{} reported violations with every rule disabled",
            file.display()
        );
    }
}

#[test]
fn checking_a_source_file_reports_the_same_violations_as_formatting_it() {
    let options = options(RuleSet::ALL);
    for file in source_files() {
        let text = std::fs::read_to_string(&file).expect("source file should be readable");
        let kind = FileKind::from_path(&file).expect("source file should have a known kind");
        assert_eq!(
            check(&text, kind, &options),
            format(&text, kind, &options).violations,
            "check and format disagree about {}",
            file.display()
        );
    }
}

#[test]
fn the_line_ending_style_of_a_source_file_is_preserved() {
    let options = options(RuleSet::ALL);
    for file in source_files() {
        let text = std::fs::read_to_string(&file).expect("source file should be readable");
        let kind = FileKind::from_path(&file).expect("source file should have a known kind");
        let Some(fixed) = format(&text, kind, &options).fixed_text else {
            continue;
        };
        assert!(
            !fixed.contains('\r'),
            "{} gained a carriage return, but the source uses line feeds",
            file.display()
        );
        assert_eq!(
            text.ends_with('\n'),
            fixed.ends_with('\n'),
            "the final newline of {} changed",
            file.display()
        );
    }
}

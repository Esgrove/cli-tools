//! End to end tests for the `slb` binary.
//!
//! Each test runs the compiled binary in a temporary directory and checks the exit code and output.
//! The behaviors verified here are the ones documented in the README:
//! the default mode reports violations and exits with code 1,
//! `--fix` rewrites files, `--print` shows a diff, and `--stdin` formats text from stdin.
//!
//! The user config during tests resolves to `tests/fixtures/sample_config.toml`,
//! which sets `width = 110` and `extensions = ["rs", "md"]`,
//! so tests that depend on the width or on other file types pass the matching option explicitly.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use tempfile::TempDir;

/// A doc comment with two sentences on one line, which the formatter splits at the sentence end.
const TWO_SENTENCES: &str = "/// One sentence that is already quite long. Another sentence follows it here.\n";

/// Create a temporary directory with a visible name, so the file walker does not skip it as hidden.
fn temporary_directory() -> TempDir {
    tempfile::Builder::new()
        .prefix("slb_cli_test")
        .tempdir()
        .expect("temporary directory should be created")
}

/// Write a file inside the directory and return its path.
fn write_file(directory: &TempDir, name: &str, content: &str) -> PathBuf {
    let path = directory.path().join(name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("parent directories should be created");
    }
    std::fs::write(&path, content).expect("file should be written");
    path
}

/// Run the binary with the given arguments.
fn run(arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_slb"))
        .args(arguments)
        .output()
        .expect("the slb binary should run")
}

/// Run the binary with the given arguments and text on stdin.
fn run_with_stdin(arguments: &[&str], input: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_slb"))
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the slb binary should start");
    child
        .stdin
        .as_mut()
        .expect("stdin should be piped")
        .write_all(input.as_bytes())
        .expect("stdin should be writable");
    child.wait_with_output().expect("the slb binary should finish")
}

/// Exit code of a finished process.
fn exit_code(output: &Output) -> i32 {
    output.status.code().expect("the process should exit normally")
}

/// Standard output as text.
fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Standard error as text.
fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Path as a command line argument.
fn argument(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Text without the terminal color codes, so a comparison does not depend on the terminal.
fn plain(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut characters = text.chars();
    while let Some(character) = characters.next() {
        if character != '\u{1b}' {
            result.push(character);
            continue;
        }
        for escape in characters.by_ref() {
            if escape == 'm' {
                break;
            }
        }
    }
    result
}

#[test]
fn check_mode_reports_violations_and_exits_with_one() {
    let directory = temporary_directory();
    let path = write_file(&directory, "long.rs", TWO_SENTENCES);

    let output = run(&[&argument(&path), "-w", "40"]);

    assert_eq!(exit_code(&output), 1);
    let text = stdout(&output);
    assert!(text.contains("too-long"), "{text}");
    assert!(text.contains("long.rs:1"), "{text}");
    assert!(text.contains("found"), "{text}");
    assert_eq!(
        std::fs::read_to_string(&path).expect("file should be readable"),
        TWO_SENTENCES,
        "check mode must not change the file"
    );
}

#[test]
fn a_file_without_violations_exits_with_zero() {
    let directory = temporary_directory();
    let path = write_file(&directory, "clean.rs", "/// Short comment.\nfn parse() {}\n");

    let output = run(&[&argument(&path)]);

    assert_eq!(exit_code(&output), 0);
    assert!(stdout(&output).contains("Checked 1 file, no violations"));
}

#[test]
fn fix_mode_rewrites_the_file_and_exits_with_zero() {
    let directory = temporary_directory();
    let path = write_file(&directory, "fix.rs", TWO_SENTENCES);

    let output = run(&[&argument(&path), "-w", "40", "--fix"]);

    assert_eq!(exit_code(&output), 0);
    let text = stdout(&output);
    assert!(text.contains("Fixed"), "{text}");
    assert!(text.contains("0 violation remaining"), "{text}");
    let fixed = std::fs::read_to_string(&path).expect("file should be readable");
    assert_eq!(
        fixed,
        "/// One sentence that is already quite long.\n/// Another sentence follows it here.\n"
    );
}

#[test]
fn print_mode_shows_a_diff_without_writing_the_file() {
    let directory = temporary_directory();
    let path = write_file(&directory, "diff.rs", TWO_SENTENCES);

    let output = run(&[&argument(&path), "-w", "40", "--print"]);

    assert_eq!(exit_code(&output), 1);
    let text = stdout(&output);
    assert!(text.contains("--- "), "{text}");
    assert!(
        text.contains("- /// One sentence that is already quite long. Another sentence follows it here."),
        "{text}"
    );
    assert!(
        text.contains("+ /// One sentence that is already quite long."),
        "{text}"
    );
    assert_eq!(
        std::fs::read_to_string(&path).expect("file should be readable"),
        TWO_SENTENCES,
        "print mode must not change the file"
    );
}

#[test]
fn stdin_mode_writes_the_formatted_text_to_stdout() {
    let output = run_with_stdin(
        &["--stdin", "--type", "markdown", "-w", "40"],
        "One sentence that is quite long here. Another sentence follows it.\n",
    );

    assert_eq!(
        stdout(&output),
        "One sentence that is quite long here.\nAnother sentence follows it.\n"
    );
    assert_eq!(exit_code(&output), 0);
}

#[test]
fn stdin_mode_passes_unchanged_text_through() {
    let output = run_with_stdin(&["--stdin", "--type", "rust"], "// Short comment.\n");

    assert_eq!(stdout(&output), "// Short comment.\n");
    assert_eq!(exit_code(&output), 0);
}

#[test]
fn stdin_mode_requires_the_type_option() {
    let output = run_with_stdin(&["--stdin"], "Text.\n");

    assert_eq!(exit_code(&output), 2);
    assert!(stderr(&output).contains("The --type option is required when reading from stdin"));
}

#[test]
fn quiet_mode_prints_only_the_summary() {
    let directory = temporary_directory();
    let path = write_file(&directory, "long.rs", TWO_SENTENCES);

    let output = run(&[&argument(&path), "-w", "40", "--quiet"]);

    assert_eq!(exit_code(&output), 1);
    let text = stdout(&output);
    assert!(text.contains("found"), "{text}");
    assert!(!text.contains("too-long"), "{text}");
}

#[test]
fn verbose_mode_prints_the_file_and_the_resolved_width() {
    let directory = temporary_directory();
    let path = write_file(&directory, "clean.rs", "/// Short comment.\n");
    write_file(&directory, "rustfmt.toml", "max_width = 90\n");

    let output = run(&[&argument(&path), "--verbose"]);

    assert_eq!(exit_code(&output), 0);
    let text = stdout(&output);
    assert!(text.contains("clean.rs"), "{text}");
    assert!(text.contains("width 110 from options"), "{text}");
}

#[test]
fn the_width_from_the_project_config_is_used_when_no_width_option_is_given() {
    let directory = temporary_directory();
    let path = write_file(&directory, "wide.rs", "/// Short comment.\n");
    write_file(&directory, "rustfmt.toml", "max_width = 40\n");

    let output = run(&[&argument(&path), "--verbose", "--ignore-project-config"]);

    assert!(
        stdout(&output).contains("width 110 from options"),
        "{}",
        stdout(&output)
    );
}

#[test]
fn the_rules_option_limits_the_reported_violations() {
    let directory = temporary_directory();
    let path = write_file(
        &directory,
        "rules.rs",
        "/// A clause; another clause that is here.\nlet value = 1; // trailing comment\n",
    );

    let semicolon_only = run(&[&argument(&path), "--rules", "semicolon"]);
    let text = stdout(&semicolon_only);
    assert!(text.contains("semicolon"), "{text}");
    assert!(!text.contains("trailing"), "{text}");

    let trailing_only = run(&[&argument(&path), "--rules", "trailing"]);
    let text = stdout(&trailing_only);
    assert!(text.contains("trailing"), "{text}");
    assert!(!text.contains("semicolon"), "{text}");
}

#[test]
fn a_trailing_comment_is_moved_above_the_code() {
    let directory = temporary_directory();
    let path = write_file(&directory, "trailing.rs", "let width = 80; // default width\n");

    let output = run(&[&argument(&path), "--fix"]);

    assert_eq!(exit_code(&output), 0);
    assert_eq!(
        std::fs::read_to_string(&path).expect("file should be readable"),
        "// default width\nlet width = 80;\n"
    );
}

#[test]
fn a_directive_comment_is_left_on_the_code_line() {
    let directory = temporary_directory();
    let path = write_file(&directory, "directive.rs", "let value = 1; // clippy::all\n");

    let output = run(&[&argument(&path), "--fix"]);

    assert_eq!(exit_code(&output), 0);
    assert_eq!(
        std::fs::read_to_string(&path).expect("file should be readable"),
        "let value = 1; // clippy::all\n"
    );
}

#[test]
fn a_semicolon_and_an_em_dash_are_rewritten() {
    let directory = temporary_directory();
    let path = write_file(
        &directory,
        "prose.md",
        "The parser reads the header; the caller handles errors — always.\n",
    );

    let output = run(&[&argument(&path), "--fix"]);

    assert_eq!(exit_code(&output), 0);
    let fixed = std::fs::read_to_string(&path).expect("file should be readable");
    assert!(!fixed.contains(';'), "{fixed}");
    assert!(!fixed.contains('—'), "{fixed}");
    assert!(fixed.contains("The parser reads the header."), "{fixed}");
}

#[test]
fn excluded_directories_are_skipped_when_walking() {
    let directory = temporary_directory();
    write_file(&directory, "skipped/long.rs", TWO_SENTENCES);
    write_file(&directory, "kept/clean.rs", "/// Short comment.\n");

    let output = run(&[&argument(directory.path()), "-w", "40", "--exclude", "skipped"]);

    assert_eq!(exit_code(&output), 0);
    assert!(
        stdout(&output).contains("Checked 1 file, no violations"),
        "{}",
        stdout(&output)
    );
}

#[test]
fn the_extension_filter_limits_the_walked_files() {
    let directory = temporary_directory();
    write_file(&directory, "long.rs", TWO_SENTENCES);
    write_file(
        &directory,
        "notes.md",
        "One sentence that is quite long here. Another one follows.\n",
    );

    let output = run(&[&argument(directory.path()), "-w", "40", "--extensions", "md", "--quiet"]);

    assert_eq!(exit_code(&output), 1);
    assert!(stdout(&output).contains("Checked 1 file"), "{}", stdout(&output));
}

#[test]
fn an_unsupported_file_type_is_reported_and_skipped() {
    let directory = temporary_directory();
    let path = write_file(&directory, "archive.zip", "content\n");

    let output = run(&[&argument(&path)]);

    // Warnings go to standard error, so a piped run only gets the formatted output on standard output.
    assert_eq!(exit_code(&output), 0);
    let text = stderr(&output);
    assert!(text.contains("Skipping unsupported file type"), "{text}");
    assert!(text.contains("No supported files found"), "{text}");
    assert!(stdout(&output).is_empty(), "{}", stdout(&output));
}

#[test]
fn a_directory_without_supported_files_is_reported() {
    let directory = temporary_directory();
    write_file(&directory, "data.bin", "content\n");

    let output = run(&[&argument(directory.path())]);

    assert_eq!(exit_code(&output), 0);
    assert!(
        stderr(&output).contains("No supported files found"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn the_file_ignore_marker_skips_the_whole_file() {
    let directory = temporary_directory();
    let content = format!("// slb-ignore-file\n{TWO_SENTENCES}");
    let path = write_file(&directory, "ignored.rs", &content);

    let output = run(&[&argument(&path), "-w", "40"]);

    assert_eq!(exit_code(&output), 0);
    assert!(stdout(&output).contains("no violations"));
}

#[test]
fn a_missing_path_is_an_error_with_exit_code_two() {
    let directory = temporary_directory();
    let output = run(&[&argument(&directory.path().join("missing.rs"))]);

    assert_eq!(exit_code(&output), 2);
    assert!(!stderr(&output).is_empty());
}

#[test]
fn several_paths_are_checked_in_one_run() {
    let directory = temporary_directory();
    let first = write_file(&directory, "first.rs", "/// Short comment.\n");
    let second = write_file(&directory, "second.rs", "/// Another short comment.\n");

    let output = run(&[&argument(&first), &argument(&second)]);

    assert_eq!(exit_code(&output), 0);
    assert!(stdout(&output).contains("Checked 2 files, no violations"));
}

#[test]
fn the_completion_subcommand_prints_a_script() {
    let output = run(&["completion", "bash"]);

    assert_eq!(exit_code(&output), 0);
    assert!(stdout(&output).contains("slb"));
}

#[test]
fn the_version_flag_prints_the_version() {
    let output = run(&["--version"]);

    assert_eq!(exit_code(&output), 0);
    assert!(stdout(&output).contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn the_help_output_matches_the_readme() {
    let output = run(&["-h"]);
    assert_eq!(exit_code(&output), 0);

    let readme = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("README.md"))
        .expect("README should be readable");
    let documented = readme
        .split("\n## ")
        .find(|section| section.starts_with("Slb\n"))
        .and_then(|section| section.split("```console\n").nth(1))
        .and_then(|block| block.split("```").next())
        .expect("README should document the slb usage");

    assert_eq!(
        plain(&stdout(&output)).trim_end(),
        documented.trim_end(),
        "the slb usage in README.md is out of date, update it with: cargo run --bin slb -- -h"
    );
}

//! Compile-time version and build metadata of the cli-tools binaries.
//!
//! Values are populated by `build.rs` with `cargo:rustc-env` directives.
//! The binaries pass `VERSION_INFO` to clap as their `-V` and `--version` output,
//! and the filefind tray shows `build_details` in its About window,
//! so the output can be tied back to the exact build that produced it.
//! A value that could not be read at build time is an empty string.

#[cfg(test)]
mod format;

/// UTC timestamp recorded when `build.rs` generated the metadata, formatted as `%Y-%m-%d_%H:%M:%S`.
pub static BUILD_TIME: &str = env!("BUILD_TIME");
/// Git branch name captured by `build.rs`, empty for a detached `HEAD`.
pub static GIT_BRANCH: &str = env!("GIT_BRANCH");
/// Short git commit SHA captured by `build.rs`.
pub static GIT_COMMIT: &str = env!("GIT_COMMIT");
/// Workspace package version compiled into the binaries.
pub static PACKAGE_VERSION: &str = env!("CARGO_PKG_VERSION");
/// Rust compiler version captured by `build.rs`.
pub static RUST_VERSION: &str = env!("RUST_VERSION");

/// One-line version composed by `build.rs` from the non-empty values:
/// `<version> <build_time> <git_branch> <git_commit>`.
///
/// clap prefixes it with the binary name.
pub static VERSION_INFO: &str = env!("VERSION_INFO");

/// One labelled value of the build details.
struct DetailLine<'a> {
    /// Label shown before the value.
    label: &'a str,
    /// Value shown after the label, left out when empty.
    value: &'a str,
}

/// Build metadata as aligned, labelled lines for display in a user interface.
///
/// Shows the version, commit, branch, build time, and compiler, one per line,
/// and leaves out the lines whose value is empty.
#[must_use]
pub fn build_details() -> String {
    let built = display_build_time(BUILD_TIME);
    format_detail_lines(&[
        DetailLine {
            label: "Version",
            value: PACKAGE_VERSION,
        },
        DetailLine {
            label: "Commit",
            value: GIT_COMMIT,
        },
        DetailLine {
            label: "Branch",
            value: GIT_BRANCH,
        },
        DetailLine {
            label: "Built",
            value: &built,
        },
        DetailLine {
            label: "Compiler",
            value: RUST_VERSION,
        },
    ])
}

/// Build time in a readable form, with a space between the date and the time and a "UTC" suffix.
fn display_build_time(build_time: &str) -> String {
    if build_time.is_empty() {
        String::new()
    } else {
        format!("{} UTC", build_time.replace('_', " "))
    }
}

/// Format the non-empty lines with the values aligned after the longest label.
fn format_detail_lines(lines: &[DetailLine<'_>]) -> String {
    let present: Vec<&DetailLine<'_>> = lines.iter().filter(|line| !line.value.is_empty()).collect();
    let width = present
        .iter()
        .map(|line| line.label.len() + 1)
        .max()
        .unwrap_or_default();
    present
        .iter()
        .map(|line| format!("{:<width$} {}", format!("{}:", line.label), line.value))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod test_version_info {
    use super::*;

    #[test]
    fn starts_with_package_version() {
        assert!(VERSION_INFO.starts_with(PACKAGE_VERSION));
    }

    #[test]
    fn has_no_empty_fields() {
        assert!(!VERSION_INFO.contains("  "));
        assert_eq!(VERSION_INFO, VERSION_INFO.trim());
    }

    #[test]
    fn contains_build_metadata() {
        assert_eq!(
            VERSION_INFO,
            format::join_present(&[PACKAGE_VERSION, BUILD_TIME, GIT_BRANCH, GIT_COMMIT])
        );
    }

    #[test]
    fn commit_is_short_hex_or_empty() {
        assert!(
            GIT_COMMIT.is_empty()
                || (GIT_COMMIT.len() == 8 && GIT_COMMIT.chars().all(|character| character.is_ascii_hexdigit())),
            "unexpected commit: {GIT_COMMIT}"
        );
    }

    #[test]
    fn build_time_has_expected_format() {
        let bytes = BUILD_TIME.as_bytes();
        assert_eq!(bytes.len(), 19, "unexpected build time: {BUILD_TIME}");
        for (index, byte) in bytes.iter().enumerate() {
            match index {
                4 | 7 => assert_eq!(*byte, b'-'),
                10 => assert_eq!(*byte, b'_'),
                13 | 16 => assert_eq!(*byte, b':'),
                _ => assert!(byte.is_ascii_digit(), "unexpected build time: {BUILD_TIME}"),
            }
        }
    }

    #[test]
    fn rust_version_names_the_compiler() {
        assert!(
            RUST_VERSION.starts_with("rustc "),
            "unexpected Rust version: {RUST_VERSION}"
        );
    }
}

#[cfg(test)]
mod test_join_present {
    use super::format::join_present;

    #[test]
    fn joins_with_single_spaces() {
        assert_eq!(
            join_present(&["1.0.0", "2026-11-28_12:44:33", "main", "bb1888da"]),
            "1.0.0 2026-11-28_12:44:33 main bb1888da"
        );
    }

    #[test]
    fn skips_empty_values() {
        assert_eq!(
            join_present(&["1.0.0", "2026-11-28_12:44:33", "", ""]),
            "1.0.0 2026-11-28_12:44:33"
        );
        assert_eq!(join_present(&["1.0.0", "", "", "bb1888da"]), "1.0.0 bb1888da");
    }

    #[test]
    fn all_empty_gives_empty_string() {
        assert_eq!(join_present(&["", ""]), "");
    }
}

#[cfg(test)]
mod test_build_details {
    use super::*;

    #[test]
    fn contains_every_non_empty_value() {
        let details = build_details();
        for value in [PACKAGE_VERSION, GIT_COMMIT, GIT_BRANCH, RUST_VERSION] {
            assert!(details.contains(value), "{value} missing from:\n{details}");
        }
        assert!(details.contains(&display_build_time(BUILD_TIME)));
    }

    #[test]
    fn aligns_values_after_the_longest_label() {
        let details = format_detail_lines(&[
            DetailLine {
                label: "Version",
                value: "13.0.0",
            },
            DetailLine {
                label: "Compiler",
                value: "rustc 1.100.0",
            },
        ]);
        assert_eq!(details, "Version:  13.0.0\nCompiler: rustc 1.100.0");
    }

    #[test]
    fn leaves_out_empty_values() {
        let details = format_detail_lines(&[
            DetailLine {
                label: "Version",
                value: "13.0.0",
            },
            DetailLine {
                label: "Commit",
                value: "",
            },
            DetailLine {
                label: "Compiler",
                value: "",
            },
        ]);
        assert_eq!(details, "Version: 13.0.0");
    }

    #[test]
    fn all_empty_gives_empty_string() {
        let details = format_detail_lines(&[DetailLine {
            label: "Commit",
            value: "",
        }]);
        assert_eq!(details, "");
    }

    #[test]
    fn build_time_is_readable() {
        assert_eq!(display_build_time("2026-11-28_12:44:33"), "2026-11-28 12:44:33 UTC");
        assert_eq!(display_build_time(""), "");
    }
}

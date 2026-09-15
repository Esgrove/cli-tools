//! File discovery and filtering for `slb`.
//!
//! Walks the given input paths, skips excluded directories and unsupported file types,
//! and applies the extension filter to build the list of files to process.

use std::path::{Path, PathBuf};

use anyhow::Result;
use walkdir::WalkDir;

use cli_tools::print_yellow;
use cli_tools::semantic_line_breaks::FileKind;

use crate::config::Config;

/// Collect the files to process from the given paths, walking directories recursively.
///
/// The file kind is decided during the walk and carried with the path,
/// so it does not have to be worked out from the name a second time.
pub fn collect_files(paths: &[PathBuf], config: &Config) -> Result<Vec<(PathBuf, FileKind)>> {
    let roots: Vec<PathBuf> = if paths.is_empty() {
        vec![cli_tools::resolve_input_path(None)?]
    } else {
        paths
            .iter()
            .map(|path| cli_tools::resolve_input_path(Some(path)))
            .collect::<Result<Vec<_>>>()?
    };

    let excludes_have_separator = config.exclude.iter().any(|pattern| pattern.contains('/'));
    let mut files = Vec::new();
    for root in roots {
        if root.is_file() {
            if let Some(kind) = config.kind.or_else(|| FileKind::from_path(&root)) {
                files.push((root, kind));
            } else {
                print_yellow!("Skipping unsupported file type: {}", display_path(&root));
            }
            continue;
        }
        // The hidden and system directory skip does not apply to the root,
        // so an explicitly given directory such as ".github" is still walked.
        let walker = WalkDir::new(&root).into_iter().filter_entry(|entry| {
            (entry.depth() == 0 || !cli_tools::should_skip_entry(entry))
                && !is_excluded(entry.path(), config, excludes_have_separator)
        });
        for entry in walker.filter_map(Result::ok) {
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.into_path();
            let Some(kind) = config.kind.or_else(|| FileKind::from_path(&path)) else {
                continue;
            };
            if matches_extensions(&path, config) {
                files.push((path, kind));
            }
        }
    }
    files.sort_by(|(left, _), (right, _)| left.cmp(right));
    files.dedup_by(|(left, _), (right, _)| left == right);
    Ok(files)
}

/// Whether the path has a component equal to an exclude pattern, or contains a pattern with a separator.
///
/// The whole path is only rendered as text when some pattern holds a separator,
/// since that rendering costs an allocation for every entry of the walk.
fn is_excluded(path: &Path, config: &Config, any_pattern_has_separator: bool) -> bool {
    let path_string = if any_pattern_has_separator {
        path.to_string_lossy().replace('\\', "/")
    } else {
        String::new()
    };
    config.exclude.iter().any(|pattern| {
        if pattern.contains('/') {
            path_string.contains(pattern.trim_matches('/'))
        } else {
            path.components()
                .any(|component| component.as_os_str().to_string_lossy() == *pattern)
        }
    })
}

/// Whether the file passes the extension filter.
fn matches_extensions(path: &Path, config: &Config) -> bool {
    if config.extensions.is_empty() {
        return true;
    }
    let extension = path
        .extension()
        .map(|extension| extension.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    config
        .extensions
        .iter()
        .any(|allowed| *allowed == extension || *allowed == file_name)
}

/// Path relative to the current working directory for display.
pub fn display_path(path: &Path) -> String {
    cli_tools::get_relative_path_from_current_working_directory(path)
        .display()
        .to_string()
}

/// Path relative to the given working directory for display.
///
/// Taking the directory as an argument keeps the run from asking the system for it once per file.
pub fn display_path_relative(path: &Path, working_directory: Option<&Path>) -> String {
    working_directory
        .and_then(|directory| path.strip_prefix(directory).ok())
        .unwrap_or(path)
        .display()
        .to_string()
}

#[cfg(test)]
mod test_helpers {
    use clap::Parser;
    use tempfile::TempDir;

    use super::*;
    use crate::Args;

    /// Whether the path is excluded, working out the separator flag the way the walk does.
    pub fn excluded(path: &Path, config: &Config) -> bool {
        let any_pattern_has_separator = config.exclude.iter().any(|pattern| pattern.contains('/'));
        super::is_excluded(path, config, any_pattern_has_separator)
    }

    /// Build a config from command line arguments as the binary would.
    pub fn config(arguments: &[&str]) -> Config {
        let args = Args::try_parse_from(arguments).expect("arguments should parse");
        Config::from_args(&args).expect("config should build")
    }

    /// Build a config with the exclude and extension lists replaced.
    pub fn config_with(exclude: Vec<&str>, extensions: Vec<&str>) -> Config {
        let mut config = config(&["slb"]);
        config.exclude = exclude.into_iter().map(String::from).collect();
        config.extensions = extensions.into_iter().map(String::from).collect();
        config
    }

    /// Create a temporary directory with a visible name, so the walker does not skip it as hidden.
    pub fn temporary_directory() -> TempDir {
        tempfile::Builder::new()
            .prefix("slb_test")
            .tempdir()
            .expect("temporary directory should be created")
    }

    /// Write a file inside the directory, creating parent directories as needed.
    pub fn write(directory: &TempDir, relative: &str, content: &str) -> PathBuf {
        let path = directory.path().join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("parent directories should be created");
        }
        std::fs::write(&path, content).expect("file should be written");
        path
    }

    /// File names of the collected files, sorted.
    pub fn collected_names(files: &[(PathBuf, FileKind)]) -> Vec<String> {
        let mut names: Vec<String> = files
            .iter()
            .map(|(path, _)| path.file_name().unwrap_or_default().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }
}

#[cfg(test)]
mod test_file_filters {
    use super::test_helpers::*;
    use super::*;

    #[test]
    fn exclude_matches_whole_components_only() {
        let config = config_with(vec!["build"], vec![]);
        assert!(excluded(Path::new("project/build/out.rs"), &config));
        assert!(!excluded(Path::new("project/src/builder.rs"), &config));
    }

    #[test]
    fn exclude_with_separator_matches_path_substring() {
        let config = config_with(vec!["docs/generated"], vec![]);
        assert!(excluded(Path::new("repo/docs/generated/api.md"), &config));
        assert!(!excluded(Path::new("repo/docs/manual/api.md"), &config));
    }

    #[test]
    fn extension_filter_accepts_matching_files() {
        let config = config_with(vec![], vec!["rs", "dockerfile"]);
        assert!(matches_extensions(Path::new("src/main.RS"), &config));
        assert!(matches_extensions(Path::new("Dockerfile"), &config));
        assert!(!matches_extensions(Path::new("README.md"), &config));
        assert!(matches_extensions(Path::new("x.md"), &config_with(vec![], vec![])));
    }
}

#[cfg(test)]
mod test_collect_files {
    use super::test_helpers::*;
    use super::*;

    #[test]
    fn walks_a_directory_and_keeps_only_supported_files() {
        let directory = temporary_directory();
        write(&directory, "lib.rs", "// comment\n");
        write(&directory, "README.md", "Text.\n");
        write(&directory, "data.bin", "binary\n");
        write(&directory, "nested/deep/main.rs", "// comment\n");

        let files = collect_files(&[directory.path().to_path_buf()], &config_with(vec![], vec![]))
            .expect("collecting should succeed");

        assert_eq!(collected_names(&files), vec!["README.md", "lib.rs", "main.rs"]);
    }

    #[test]
    fn the_extension_filter_limits_the_collected_files() {
        let directory = temporary_directory();
        write(&directory, "lib.rs", "// comment\n");
        write(&directory, "README.md", "Text.\n");

        let files = collect_files(&[directory.path().to_path_buf()], &config_with(vec![], vec!["md"]))
            .expect("collecting should succeed");

        assert_eq!(collected_names(&files), vec!["README.md"]);
    }

    #[test]
    fn excluded_directories_are_not_walked() {
        let directory = temporary_directory();
        write(&directory, "lib.rs", "// comment\n");
        write(&directory, "target/debug/build.rs", "// comment\n");
        write(&directory, "node_modules/package/index.js", "// comment\n");

        let files = collect_files(
            &[directory.path().to_path_buf()],
            &config_with(vec!["target", "node_modules"], vec![]),
        )
        .expect("collecting should succeed");

        assert_eq!(collected_names(&files), vec!["lib.rs"]);
    }

    #[test]
    fn hidden_files_inside_a_directory_are_skipped() {
        let directory = temporary_directory();
        write(&directory, "lib.rs", "// comment\n");
        write(&directory, ".hidden.rs", "// comment\n");
        write(&directory, ".hidden/inner.rs", "// comment\n");

        let files = collect_files(&[directory.path().to_path_buf()], &config_with(vec![], vec![]))
            .expect("collecting should succeed");

        assert_eq!(collected_names(&files), vec!["lib.rs"]);
    }

    #[test]
    fn an_explicitly_given_hidden_directory_is_walked() {
        let directory = temporary_directory();
        let hidden = write(&directory, ".github/workflows/ci.yml", "# comment\n");

        let files = collect_files(
            &[hidden.parent().expect("file should have a parent").to_path_buf()],
            &config_with(vec![], vec![]),
        )
        .expect("collecting should succeed");

        assert_eq!(collected_names(&files), vec!["ci.yml"]);
    }

    #[test]
    fn a_single_file_path_is_collected_directly() {
        let directory = temporary_directory();
        let path = write(&directory, "lib.rs", "// comment\n");

        let files = collect_files(std::slice::from_ref(&path), &config_with(vec![], vec![]))
            .expect("collecting should succeed");

        assert_eq!(files.len(), 1);
        assert!(files.first().is_some_and(|(path, _)| path.ends_with("lib.rs")));
    }

    #[test]
    fn a_single_file_with_an_unsupported_type_is_reported_and_skipped() {
        let directory = temporary_directory();
        let path = write(&directory, "archive.zip", "content\n");

        let files = collect_files(&[path], &config_with(vec![], vec![])).expect("collecting should succeed");

        assert!(files.is_empty());
    }

    #[test]
    fn a_forced_kind_accepts_any_file_name() {
        let directory = temporary_directory();
        let path = write(&directory, "notes.unknown", "# comment\n");
        let mut config = config_with(vec![], vec![]);
        config.kind = Some(FileKind::Shell);

        let files = collect_files(&[path], &config).expect("collecting should succeed");

        assert_eq!(collected_names(&files), vec!["notes.unknown"]);
    }

    #[test]
    fn duplicate_paths_are_collected_once() {
        let directory = temporary_directory();
        let path = write(&directory, "lib.rs", "// comment\n");

        let files = collect_files(
            &[directory.path().to_path_buf(), path.clone(), path],
            &config_with(vec![], vec![]),
        )
        .expect("collecting should succeed");

        assert_eq!(files.len(), 1);
    }

    #[test]
    fn several_directories_are_collected_in_sorted_order() {
        let first = temporary_directory();
        let second = temporary_directory();
        write(&first, "a.rs", "// comment\n");
        write(&second, "b.rs", "// comment\n");

        let files = collect_files(
            &[first.path().to_path_buf(), second.path().to_path_buf()],
            &config_with(vec![], vec![]),
        )
        .expect("collecting should succeed");

        assert_eq!(collected_names(&files), vec!["a.rs", "b.rs"]);
    }

    #[test]
    fn no_path_falls_back_to_the_current_directory() {
        let files = collect_files(&[], &config_with(vec!["target"], vec!["toml"])).expect("collecting should succeed");

        assert!(
            files.iter().any(|(path, _)| path.ends_with("Cargo.toml")),
            "the working directory should be walked: {files:?}"
        );
    }

    #[test]
    fn a_missing_path_is_an_error() {
        let directory = temporary_directory();
        assert!(collect_files(&[directory.path().join("missing")], &config_with(vec![], vec![])).is_err());
    }
}

#[cfg(test)]
mod test_display_path {
    use super::test_helpers::*;
    use super::*;

    #[test]
    fn a_path_in_the_working_directory_is_shown_relative() {
        let current = std::env::current_dir().expect("current directory should be available");
        let path = current.join("src").join("lib.rs");
        assert_eq!(display_path(&path), format!("src{}lib.rs", std::path::MAIN_SEPARATOR));
    }

    #[test]
    fn a_path_outside_the_working_directory_is_shown_as_is() {
        let directory = temporary_directory();
        let path = write(&directory, "outside.rs", "// comment\n");
        assert!(display_path(&path).ends_with("outside.rs"));
    }
}

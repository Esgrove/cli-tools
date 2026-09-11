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
pub fn collect_files(paths: &[PathBuf], config: &Config) -> Result<Vec<PathBuf>> {
    let roots: Vec<PathBuf> = if paths.is_empty() {
        vec![cli_tools::resolve_input_path(None)?]
    } else {
        paths
            .iter()
            .map(|path| cli_tools::resolve_input_path(Some(path)))
            .collect::<Result<Vec<_>>>()?
    };

    let mut files = Vec::new();
    for root in roots {
        if root.is_file() {
            if config.kind.is_some() || FileKind::from_path(&root).is_some() {
                files.push(root);
            } else {
                print_yellow!("Skipping unsupported file type: {}", display_path(&root));
            }
            continue;
        }
        let walker = WalkDir::new(&root)
            .into_iter()
            .filter_entry(|entry| !cli_tools::should_skip_entry(entry) && !is_excluded(entry.path(), config));
        for entry in walker.filter_map(Result::ok) {
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.into_path();
            let kind_known = config.kind.is_some() || FileKind::from_path(&path).is_some();
            if kind_known && matches_extensions(&path, config) {
                files.push(path);
            }
        }
    }
    files.sort();
    files.dedup();
    Ok(files)
}

/// Whether the path has a component equal to an exclude pattern, or contains a pattern with a separator.
fn is_excluded(path: &Path, config: &Config) -> bool {
    let path_string = path.to_string_lossy().replace('\\', "/");
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

#[cfg(test)]
mod test_file_filters {
    use clap::Parser;

    use super::*;
    use crate::Args;

    fn config_with(exclude: Vec<&str>, extensions: Vec<&str>) -> Config {
        let args = Args::try_parse_from(["slb"]).expect("arguments should parse");
        let mut config = Config::from_args(&args).expect("config should build");
        config.exclude = exclude.into_iter().map(String::from).collect();
        config.extensions = extensions.into_iter().map(String::from).collect();
        config
    }

    #[test]
    fn exclude_matches_whole_components_only() {
        let config = config_with(vec!["build"], vec![]);
        assert!(is_excluded(Path::new("project/build/out.rs"), &config));
        assert!(!is_excluded(Path::new("project/src/builder.rs"), &config));
    }

    #[test]
    fn exclude_with_separator_matches_path_substring() {
        let config = config_with(vec!["docs/generated"], vec![]);
        assert!(is_excluded(Path::new("repo/docs/generated/api.md"), &config));
        assert!(!is_excluded(Path::new("repo/docs/manual/api.md"), &config));
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

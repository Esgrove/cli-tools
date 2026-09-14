//! Line length discovery from project configuration files.
//!
//! Walks up from a file's directory looking for `.editorconfig`, `rustfmt.toml`, `pyproject.toml`,
//! flake8 configuration, Prettier and markdownlint configuration, and `.clang-format`,
//! and returns the first line length limit that applies to the file.
//! Results are cached per directory and file kind.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;

use super::types::FileKind;

/// Matches a Prettier `printWidth` setting in JSON or YAML.
static RE_PRINT_WIDTH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#""?printWidth"?\s*[:=]\s*(\d+)"#).expect("Invalid printWidth regex"));

/// Matches a clang-format `ColumnLimit` setting.
static RE_COLUMN_LIMIT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^\s*ColumnLimit:\s*(\d+)").expect("Invalid ColumnLimit regex"));

/// Matches a markdownlint `line_length` setting in JSON or YAML.
static RE_LINE_LENGTH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#""?line_length"?\s*[:=]\s*(\d+)"#).expect("Invalid line_length regex"));

/// A discovered line width limit and where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WidthSource {
    /// The maximum line width.
    pub width: usize,
    /// Configuration file that defined it.
    pub file: PathBuf,
    /// Setting name inside the file.
    pub key: String,
}

/// Resolves and caches project line widths per directory and file kind.
#[derive(Debug, Default)]
pub struct WidthResolver {
    /// Cached results keyed by directory and file kind.
    cache: HashMap<(PathBuf, FileKind), Option<WidthSource>>,
}

impl WidthResolver {
    /// Create an empty resolver.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Width for the given file, using the cache when the directory was seen before.
    pub fn resolve(&mut self, file: &Path, kind: FileKind) -> Option<WidthSource> {
        let directory = file.parent().map_or_else(PathBuf::new, Path::to_path_buf);
        let key = (directory, kind);
        if let Some(cached) = self.cache.get(&key) {
            return cached.clone();
        }
        let result = discover_width(file, kind);
        self.cache.insert(key, result.clone());
        result
    }
}

/// Walk up from the file's directory and return the first applicable line width limit.
#[must_use]
pub fn discover_width(file: &Path, kind: FileKind) -> Option<WidthSource> {
    let start = file.parent()?;
    for directory in start.ancestors() {
        let (editorconfig, is_root) = editorconfig_width(directory, file);
        if editorconfig.is_some() {
            return editorconfig;
        }
        let specific = match kind {
            FileKind::Rust => rustfmt_width(directory),
            FileKind::Python => pyproject_width(directory).or_else(|| flake8_width(directory)),
            FileKind::JavaScript => prettier_width(directory),
            FileKind::Markdown => markdownlint_width(directory).or_else(|| prettier_width(directory)),
            FileKind::CLike => clang_format_width(directory),
            _ => None,
        };
        if specific.is_some() {
            return specific;
        }
        if is_root {
            break;
        }
    }
    None
}

/// Convert an editorconfig glob into an anchored regular expression.
#[must_use]
pub fn glob_to_regex(glob: &str) -> Option<Regex> {
    let mut pattern = String::from("^");
    let chars: Vec<char> = glob.chars().collect();
    let mut index = 0;
    let mut in_braces = false;
    while let Some(&character) = chars.get(index) {
        match character {
            '*' if chars.get(index + 1) == Some(&'*') => {
                if chars.get(index + 2) == Some(&'/') {
                    pattern.push_str("(?:.*/)?");
                    index += 3;
                } else {
                    pattern.push_str(".*");
                    index += 2;
                }
                continue;
            }
            '*' => pattern.push_str("[^/]*"),
            '?' => pattern.push_str("[^/]"),
            '{' => {
                in_braces = true;
                pattern.push_str("(?:");
            }
            '}' if in_braces => {
                in_braces = false;
                pattern.push(')');
            }
            ',' if in_braces => pattern.push('|'),
            '[' | ']' => pattern.push(character),
            _ => pattern.push_str(&regex::escape(&character.to_string())),
        }
        index += 1;
    }
    pattern.push('$');
    Regex::new(&pattern).ok()
}

/// Width from an `.editorconfig` in the directory, and whether that file declares itself the root.
fn editorconfig_width(directory: &Path, file: &Path) -> (Option<WidthSource>, bool) {
    let path = directory.join(".editorconfig");
    let Ok(content) = fs::read_to_string(&path) else {
        return (None, false);
    };
    let relative = file
        .strip_prefix(directory)
        .ok()
        .map(|relative| relative.to_string_lossy().replace('\\', "/"))
        .unwrap_or_default();
    let file_name = file
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();

    let mut is_root = false;
    let mut current_matches = false;
    let mut width: Option<usize> = None;
    let mut found_off = false;
    for raw_line in content.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with(['#', ';']) {
            continue;
        }
        if let Some(section) = line.strip_prefix('[').and_then(|rest| rest.strip_suffix(']')) {
            current_matches = glob_matches(section, &relative, &file_name);
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim();
        if key == "root" && value.eq_ignore_ascii_case("true") {
            is_root = true;
        }
        if current_matches && key == "max_line_length" {
            if value.eq_ignore_ascii_case("off") {
                found_off = true;
                width = None;
            } else if let Ok(parsed) = value.parse::<usize>() {
                found_off = false;
                width = Some(parsed);
            }
        }
    }
    let _ = found_off;
    let source = width.map(|width| WidthSource {
        width,
        file: path,
        key: "max_line_length".to_string(),
    });
    (source, is_root)
}

/// Whether an editorconfig section glob matches the file.
fn glob_matches(glob: &str, relative_path: &str, file_name: &str) -> bool {
    let glob = glob.trim();
    if glob.contains('/') {
        let trimmed = glob.strip_prefix('/').unwrap_or(glob);
        glob_to_regex(trimmed).is_some_and(|regex| regex.is_match(relative_path))
    } else {
        glob_to_regex(glob).is_some_and(|regex| regex.is_match(file_name))
    }
}

/// Width from `rustfmt.toml` or `.rustfmt.toml`.
fn rustfmt_width(directory: &Path) -> Option<WidthSource> {
    ["rustfmt.toml", ".rustfmt.toml"].iter().find_map(|name| {
        let path = directory.join(name);
        let value = read_toml(&path)?;
        toml_integer(&value, &["comment_width"])
            .map(|width| (width, "comment_width"))
            .or_else(|| toml_integer(&value, &["max_width"]).map(|width| (width, "max_width")))
            .map(|(width, key)| WidthSource {
                width,
                file: path.clone(),
                key: key.to_string(),
            })
    })
}

/// Width from `pyproject.toml` ruff or black settings.
fn pyproject_width(directory: &Path) -> Option<WidthSource> {
    let path = directory.join("pyproject.toml");
    let value = read_toml(&path)?;
    let candidates: [(&[&str], &str); 3] = [
        (
            &["tool", "ruff", "lint", "pycodestyle", "max-doc-length"],
            "tool.ruff.lint.pycodestyle.max-doc-length",
        ),
        (&["tool", "ruff", "line-length"], "tool.ruff.line-length"),
        (&["tool", "black", "line-length"], "tool.black.line-length"),
    ];
    candidates.iter().find_map(|(keys, name)| {
        toml_integer(&value, keys).map(|width| WidthSource {
            width,
            file: path.clone(),
            key: (*name).to_string(),
        })
    })
}

/// Width from a `[flake8]` section in `setup.cfg`, `tox.ini`, or `.flake8`.
fn flake8_width(directory: &Path) -> Option<WidthSource> {
    ["setup.cfg", "tox.ini", ".flake8"].iter().find_map(|name| {
        let path = directory.join(name);
        let content = fs::read_to_string(&path).ok()?;
        let mut in_section = false;
        let mut doc_length = None;
        let mut line_length = None;
        for raw_line in content.lines() {
            let line = raw_line.trim();
            if line.starts_with('[') {
                in_section = line.eq_ignore_ascii_case("[flake8]");
                continue;
            }
            if !in_section {
                continue;
            }
            if let Some((key, value)) = line.split_once('=') {
                let parsed = value.trim().parse::<usize>().ok();
                match key.trim() {
                    "max-doc-length" => doc_length = parsed,
                    "max-line-length" => line_length = parsed,
                    _ => {}
                }
            }
        }
        doc_length
            .map(|width| (width, "max-doc-length"))
            .or_else(|| line_length.map(|width| (width, "max-line-length")))
            .map(|(width, key)| WidthSource {
                width,
                file: path.clone(),
                key: key.to_string(),
            })
    })
}

/// Width from a Prettier configuration file.
fn prettier_width(directory: &Path) -> Option<WidthSource> {
    [
        ".prettierrc",
        ".prettierrc.json",
        ".prettierrc.yaml",
        ".prettierrc.yml",
        ".prettierrc.toml",
    ]
    .iter()
    .find_map(|name| {
        let path = directory.join(name);
        let content = fs::read_to_string(&path).ok()?;
        let width = RE_PRINT_WIDTH
            .captures(&content)
            .and_then(|captures| captures.get(1))
            .and_then(|group| group.as_str().parse::<usize>().ok())?;
        Some(WidthSource {
            width,
            file: path,
            key: "printWidth".to_string(),
        })
    })
}

/// Width from `.clang-format`.
fn clang_format_width(directory: &Path) -> Option<WidthSource> {
    [".clang-format", "_clang-format"].iter().find_map(|name| {
        let path = directory.join(name);
        let content = fs::read_to_string(&path).ok()?;
        let width = RE_COLUMN_LIMIT
            .captures(&content)
            .and_then(|captures| captures.get(1))
            .and_then(|group| group.as_str().parse::<usize>().ok())?;
        Some(WidthSource {
            width,
            file: path,
            key: "ColumnLimit".to_string(),
        })
    })
}

/// Width from a markdownlint configuration file.
fn markdownlint_width(directory: &Path) -> Option<WidthSource> {
    [
        ".markdownlint.json",
        ".markdownlint.jsonc",
        ".markdownlint.yaml",
        ".markdownlint.yml",
        ".markdownlint-cli2.jsonc",
        ".markdownlint-cli2.yaml",
    ]
    .iter()
    .find_map(|name| {
        let path = directory.join(name);
        let content = fs::read_to_string(&path).ok()?;
        let width = RE_LINE_LENGTH
            .captures(&content)
            .and_then(|captures| captures.get(1))
            .and_then(|group| group.as_str().parse::<usize>().ok())?;
        Some(WidthSource {
            width,
            file: path,
            key: "line_length".to_string(),
        })
    })
}

/// Parse a TOML file, returning `None` when it is missing or invalid.
fn read_toml(path: &Path) -> Option<toml::Value> {
    let content = fs::read_to_string(path).ok()?;
    toml::from_str(&content).ok()
}

/// Read a positive integer at a nested key path.
fn toml_integer(value: &toml::Value, keys: &[&str]) -> Option<usize> {
    let mut current = value;
    for key in keys {
        current = current.get(key)?;
    }
    current.as_integer().and_then(|integer| usize::try_from(integer).ok())
}

#[cfg(test)]
mod test_helpers {
    use std::fs;
    use std::path::{Path, PathBuf};

    use tempfile::TempDir;

    /// Create a temporary directory tree root.
    pub fn temp_root() -> TempDir {
        tempfile::tempdir().expect("failed to create temporary directory")
    }

    /// Write a file under the root, creating parent directories as needed.
    pub fn write_file(root: &Path, relative: &str, content: &str) -> PathBuf {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("failed to create parent directories");
        }
        fs::write(&path, content).expect("failed to write file");
        path
    }

    /// Path of a file under the root that does not need to exist.
    pub fn file_path(root: &Path, relative: &str) -> PathBuf {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("failed to create parent directories");
        }
        path
    }
}

#[cfg(test)]
mod test_editorconfig {
    use super::test_helpers::*;
    use super::*;

    #[test]
    fn wildcard_section_applies_to_any_file() {
        let root = temp_root();
        let config = write_file(root.path(), ".editorconfig", "[*]\nmax_line_length = 100\n");
        let rust_file = file_path(root.path(), "a.rs");
        let markdown_file = file_path(root.path(), "b.md");

        let rust_result = discover_width(&rust_file, FileKind::Rust).expect("expected a width for a.rs");
        assert_eq!(rust_result.width, 100);
        assert_eq!(rust_result.file, config);
        assert_eq!(rust_result.key, "max_line_length");

        let markdown_result = discover_width(&markdown_file, FileKind::Markdown).expect("expected a width for b.md");
        assert_eq!(markdown_result.width, 100);
    }

    #[test]
    fn later_extension_section_overrides_wildcard() {
        let root = temp_root();
        write_file(
            root.path(),
            ".editorconfig",
            "[*]\nmax_line_length = 100\n\n[*.rs]\nmax_line_length = 110\n",
        );
        let rust_file = file_path(root.path(), "a.rs");
        let markdown_file = file_path(root.path(), "b.md");

        assert_eq!(
            discover_width(&rust_file, FileKind::Rust).map(|source| source.width),
            Some(110)
        );
        assert_eq!(
            discover_width(&markdown_file, FileKind::Markdown).map(|source| source.width),
            Some(100)
        );
    }

    #[test]
    fn brace_alternation_matches_markdown_extension() {
        let root = temp_root();
        write_file(
            root.path(),
            ".editorconfig",
            "[*.{md,markdown}]\nmax_line_length = 90\n",
        );
        let markdown_file = file_path(root.path(), "x.md");
        let rust_file = file_path(root.path(), "x.rs");

        assert_eq!(
            discover_width(&markdown_file, FileKind::Markdown).map(|source| source.width),
            Some(90)
        );
        assert_eq!(discover_width(&rust_file, FileKind::Rust), None);
    }

    #[test]
    fn section_with_slash_matches_relative_path() {
        let root = temp_root();
        write_file(root.path(), ".editorconfig", "[src/**/*.rs]\nmax_line_length = 95\n");
        let deep_file = file_path(root.path(), "src/deep/a.rs");
        let other_file = file_path(root.path(), "other/a.rs");

        assert_eq!(
            discover_width(&deep_file, FileKind::Rust).map(|source| source.width),
            Some(95)
        );
        assert_eq!(discover_width(&other_file, FileKind::Rust), None);
    }

    #[test]
    fn max_line_length_off_yields_no_result() {
        let root = temp_root();
        write_file(root.path(), ".editorconfig", "[*]\nmax_line_length = off\n");
        let rust_file = file_path(root.path(), "a.rs");

        assert_eq!(discover_width(&rust_file, FileKind::Rust), None);
    }

    #[test]
    fn root_true_stops_walk_but_same_directory_is_consulted() {
        let root = temp_root();
        write_file(root.path(), "rustfmt.toml", "max_width = 100\n");
        write_file(root.path(), "child/.editorconfig", "root = true\n");
        let rust_file = file_path(root.path(), "child/a.rs");

        assert_eq!(discover_width(&rust_file, FileKind::Rust), None);

        let child_config = write_file(root.path(), "child/rustfmt.toml", "max_width = 90\n");
        let result = discover_width(&rust_file, FileKind::Rust).expect("expected a width from the child directory");
        assert_eq!(result.width, 90);
        assert_eq!(result.file, child_config);
    }

    #[test]
    fn comments_and_blank_lines_are_ignored() {
        let root = temp_root();
        write_file(
            root.path(),
            ".editorconfig",
            "# comment\n; another comment\n\n[*]\n\nmax_line_length = 88\n",
        );
        let rust_file = file_path(root.path(), "a.rs");

        assert_eq!(
            discover_width(&rust_file, FileKind::Rust).map(|source| source.width),
            Some(88)
        );
    }
}

#[cfg(test)]
mod test_discovery_order {
    use super::test_helpers::*;
    use super::*;

    #[test]
    fn nearest_directory_wins() {
        let root = temp_root();
        write_file(root.path(), "rustfmt.toml", "max_width = 100\n");
        let child_config = write_file(root.path(), "child/rustfmt.toml", "max_width = 90\n");
        let rust_file = file_path(root.path(), "child/a.rs");

        let result = discover_width(&rust_file, FileKind::Rust).expect("expected a width");
        assert_eq!(result.width, 90);
        assert_eq!(result.file, child_config);
    }

    #[test]
    fn parent_config_is_found_from_nested_directory() {
        let root = temp_root();
        let config = write_file(root.path(), "rustfmt.toml", "max_width = 100\n");
        let rust_file = file_path(root.path(), "a/b/c/a.rs");

        let result = discover_width(&rust_file, FileKind::Rust).expect("expected a width");
        assert_eq!(result.width, 100);
        assert_eq!(result.file, config);
    }

    #[test]
    fn editorconfig_wins_over_tool_config_in_same_directory() {
        let root = temp_root();
        write_file(root.path(), ".editorconfig", "[*]\nmax_line_length = 100\n");
        write_file(root.path(), "rustfmt.toml", "max_width = 80\n");
        let rust_file = file_path(root.path(), "a.rs");

        let result = discover_width(&rust_file, FileKind::Rust).expect("expected a width");
        assert_eq!(result.width, 100);
        assert_eq!(result.key, "max_line_length");
    }

    #[test]
    fn no_config_files_gives_none() {
        let root = temp_root();
        let rust_file = file_path(root.path(), "a.rs");

        assert_eq!(discover_width(&rust_file, FileKind::Rust), None);
        assert_eq!(discover_width(&rust_file, FileKind::Markdown), None);
    }
}

#[cfg(test)]
mod test_rustfmt {
    use super::test_helpers::*;
    use super::*;

    #[test]
    fn comment_width_wins_over_max_width() {
        let root = temp_root();
        write_file(root.path(), "rustfmt.toml", "max_width = 120\ncomment_width = 100\n");
        let rust_file = file_path(root.path(), "a.rs");

        let result = discover_width(&rust_file, FileKind::Rust).expect("expected a width");
        assert_eq!(result.width, 100);
        assert_eq!(result.key, "comment_width");
    }

    #[test]
    fn max_width_is_used_without_comment_width() {
        let root = temp_root();
        write_file(root.path(), "rustfmt.toml", "max_width = 120\n");
        let rust_file = file_path(root.path(), "a.rs");

        let result = discover_width(&rust_file, FileKind::Rust).expect("expected a width");
        assert_eq!(result.width, 120);
        assert_eq!(result.key, "max_width");
    }

    #[test]
    fn hidden_rustfmt_file_is_read() {
        let root = temp_root();
        write_file(root.path(), ".rustfmt.toml", "max_width = 105\n");
        let rust_file = file_path(root.path(), "a.rs");

        assert_eq!(
            discover_width(&rust_file, FileKind::Rust).map(|source| source.width),
            Some(105)
        );
    }

    #[test]
    fn rustfmt_is_ignored_for_python() {
        let root = temp_root();
        write_file(root.path(), "rustfmt.toml", "max_width = 120\n");
        let python_file = file_path(root.path(), "a.py");

        assert_eq!(discover_width(&python_file, FileKind::Python), None);
    }
}

#[cfg(test)]
mod test_python {
    use super::test_helpers::*;
    use super::*;

    #[test]
    fn pyproject_prefers_ruff_doc_length_over_ruff_line_length_over_black() {
        let root = temp_root();
        write_file(
            root.path(),
            "pyproject.toml",
            "[tool.black]\nline-length = 88\n\n[tool.ruff]\nline-length = 100\n\n[tool.ruff.lint.pycodestyle]\nmax-doc-length = 90\n",
        );
        let python_file = file_path(root.path(), "a.py");

        let result = discover_width(&python_file, FileKind::Python).expect("expected a width");
        assert_eq!(result.width, 90);
        assert_eq!(result.key, "tool.ruff.lint.pycodestyle.max-doc-length");
    }

    #[test]
    fn pyproject_prefers_ruff_line_length_over_black() {
        let root = temp_root();
        write_file(
            root.path(),
            "pyproject.toml",
            "[tool.black]\nline-length = 88\n\n[tool.ruff]\nline-length = 100\n",
        );
        let python_file = file_path(root.path(), "a.py");

        let result = discover_width(&python_file, FileKind::Python).expect("expected a width");
        assert_eq!(result.width, 100);
        assert_eq!(result.key, "tool.ruff.line-length");
    }

    #[test]
    fn pyproject_falls_back_to_black() {
        let root = temp_root();
        write_file(root.path(), "pyproject.toml", "[tool.black]\nline-length = 88\n");
        let python_file = file_path(root.path(), "a.py");

        let result = discover_width(&python_file, FileKind::Python).expect("expected a width");
        assert_eq!(result.width, 88);
        assert_eq!(result.key, "tool.black.line-length");
    }

    #[test]
    fn pyproject_without_width_settings_gives_none() {
        let root = temp_root();
        write_file(root.path(), "pyproject.toml", "[project]\nname = \"example\"\n");
        let python_file = file_path(root.path(), "a.py");

        assert_eq!(discover_width(&python_file, FileKind::Python), None);
    }

    #[test]
    fn setup_cfg_flake8_max_line_length_applies_to_python() {
        let root = temp_root();
        let config = write_file(
            root.path(),
            "setup.cfg",
            "[metadata]\nname = example\n\n[flake8]\nmax-line-length = 99\n",
        );
        let python_file = file_path(root.path(), "a.py");

        let result = discover_width(&python_file, FileKind::Python).expect("expected a width");
        assert_eq!(result.width, 99);
        assert_eq!(result.file, config);
        assert_eq!(result.key, "max-line-length");
    }

    #[test]
    fn setup_cfg_flake8_is_ignored_for_rust() {
        let root = temp_root();
        write_file(root.path(), "setup.cfg", "[flake8]\nmax-line-length = 99\n");
        let rust_file = file_path(root.path(), "a.rs");

        assert_eq!(discover_width(&rust_file, FileKind::Rust), None);
    }
}

#[cfg(test)]
mod test_prettier_clang_markdownlint {
    use super::test_helpers::*;
    use super::*;

    #[test]
    fn prettierrc_applies_to_typescript_and_markdown() {
        let root = temp_root();
        let config = write_file(root.path(), ".prettierrc", "{\"printWidth\": 90}\n");
        let typescript_file = file_path(root.path(), "a.ts");
        let markdown_file = file_path(root.path(), "a.md");

        let typescript_result =
            discover_width(&typescript_file, FileKind::JavaScript).expect("expected a width for a.ts");
        assert_eq!(typescript_result.width, 90);
        assert_eq!(typescript_result.file, config);
        assert_eq!(typescript_result.key, "printWidth");

        let markdown_result = discover_width(&markdown_file, FileKind::Markdown).expect("expected a width for a.md");
        assert_eq!(markdown_result.width, 90);
    }

    #[test]
    fn prettierrc_is_ignored_for_rust() {
        let root = temp_root();
        write_file(root.path(), ".prettierrc", "{\"printWidth\": 90}\n");
        let rust_file = file_path(root.path(), "a.rs");

        assert_eq!(discover_width(&rust_file, FileKind::Rust), None);
    }

    #[test]
    fn clang_format_column_limit_applies_to_cpp() {
        let root = temp_root();
        let config = write_file(root.path(), ".clang-format", "BasedOnStyle: Google\nColumnLimit: 100\n");
        let cpp_file = file_path(root.path(), "a.cpp");

        let result = discover_width(&cpp_file, FileKind::CLike).expect("expected a width");
        assert_eq!(result.width, 100);
        assert_eq!(result.file, config);
        assert_eq!(result.key, "ColumnLimit");
    }

    #[test]
    fn markdownlint_line_length_applies_to_markdown() {
        let root = temp_root();
        let config = write_file(
            root.path(),
            ".markdownlint.json",
            "{\"MD013\": {\"line_length\": 80}}\n",
        );
        let markdown_file = file_path(root.path(), "a.md");

        let result = discover_width(&markdown_file, FileKind::Markdown).expect("expected a width");
        assert_eq!(result.width, 80);
        assert_eq!(result.file, config);
        assert_eq!(result.key, "line_length");
    }

    #[test]
    fn markdownlint_beats_prettier_in_same_directory() {
        let root = temp_root();
        write_file(root.path(), ".prettierrc", "{\"printWidth\": 90}\n");
        let markdownlint = write_file(
            root.path(),
            ".markdownlint.json",
            "{\"MD013\": {\"line_length\": 80}}\n",
        );
        let markdown_file = file_path(root.path(), "a.md");
        let typescript_file = file_path(root.path(), "a.ts");

        let markdown_result = discover_width(&markdown_file, FileKind::Markdown).expect("expected a width for a.md");
        assert_eq!(markdown_result.width, 80);
        assert_eq!(markdown_result.file, markdownlint);

        let typescript_result =
            discover_width(&typescript_file, FileKind::JavaScript).expect("expected a width for a.ts");
        assert_eq!(typescript_result.width, 90);
    }
}

#[cfg(test)]
mod test_width_resolver {
    use super::test_helpers::*;
    use super::*;

    #[test]
    fn returns_same_result_twice() {
        let root = temp_root();
        write_file(root.path(), "rustfmt.toml", "max_width = 100\n");
        let rust_file = file_path(root.path(), "a.rs");
        let mut resolver = WidthResolver::new();

        let first = resolver.resolve(&rust_file, FileKind::Rust);
        let second = resolver.resolve(&rust_file, FileKind::Rust);

        assert_eq!(first.as_ref().map(|source| source.width), Some(100));
        assert_eq!(first, second);
    }

    #[test]
    fn caches_per_directory() {
        let root = temp_root();
        let config = write_file(root.path(), "rustfmt.toml", "max_width = 100\n");
        let first_file = file_path(root.path(), "a.rs");
        let second_file = file_path(root.path(), "b.rs");
        let mut resolver = WidthResolver::new();

        let first = resolver.resolve(&first_file, FileKind::Rust);
        assert_eq!(first.as_ref().map(|source| source.width), Some(100));

        std::fs::remove_file(&config).expect("failed to remove config");
        let second = resolver.resolve(&second_file, FileKind::Rust);
        assert_eq!(first, second);
        assert_eq!(discover_width(&second_file, FileKind::Rust), None);
    }

    #[test]
    fn caches_per_file_kind() {
        let root = temp_root();
        write_file(root.path(), "rustfmt.toml", "max_width = 100\n");
        let rust_file = file_path(root.path(), "a.rs");
        let python_file = file_path(root.path(), "a.py");
        let mut resolver = WidthResolver::new();

        assert_eq!(
            resolver.resolve(&rust_file, FileKind::Rust).map(|source| source.width),
            Some(100)
        );
        assert_eq!(resolver.resolve(&python_file, FileKind::Python), None);
    }
}

#[cfg(test)]
mod test_glob_to_regex {
    use super::*;

    #[test]
    fn single_star_does_not_cross_directories() {
        let regex = glob_to_regex("*.rs").expect("glob should convert");
        assert!(regex.is_match("a.rs"));
        assert!(!regex.is_match("a/b.rs"));
        assert!(!regex.is_match("a.rst"));
    }

    #[test]
    fn double_star_crosses_directories() {
        let regex = glob_to_regex("**/*.rs").expect("glob should convert");
        assert!(regex.is_match("a/b.rs"));
        assert!(regex.is_match("a/b/c.rs"));
        assert!(regex.is_match("a.rs"));
    }

    #[test]
    fn question_mark_matches_single_character() {
        let regex = glob_to_regex("?.md").expect("glob should convert");
        assert!(regex.is_match("a.md"));
        assert!(!regex.is_match("ab.md"));
        assert!(!regex.is_match(".md"));
    }

    #[test]
    fn brace_alternation_matches_both_extensions() {
        let regex = glob_to_regex("*.{md,markdown}").expect("glob should convert");
        assert!(regex.is_match("a.md"));
        assert!(regex.is_match("a.markdown"));
        assert!(!regex.is_match("a.mdx"));
    }

    #[test]
    fn literal_characters_are_escaped() {
        let regex = glob_to_regex("a.b").expect("glob should convert");
        assert!(regex.is_match("a.b"));
        assert!(!regex.is_match("axb"));
    }
}

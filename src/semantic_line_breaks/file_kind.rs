//! File kind detection and the comment syntax each kind uses.

use std::path::Path;

use clap::ValueEnum;

/// Kind of a file, deciding comment markers and string syntax.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, ValueEnum)]
pub enum FileKind {
    /// Rust source with `//`, `///`, and `//!` comments.
    Rust,
    /// C, C++, Java, Kotlin, Swift, C#, and similar languages with `//` and `/* */` comments.
    #[value(name = "c")]
    CLike,
    /// JavaScript and TypeScript with template literals.
    #[value(name = "javascript")]
    JavaScript,
    /// Go source with backtick raw strings.
    Go,
    /// Python source with `#` comments and docstrings.
    Python,
    /// Shell scripts with `#` comments.
    Shell,
    /// TOML files with `#` comments.
    Toml,
    /// YAML files with `#` comments that need a leading space.
    Yaml,
    /// Dockerfiles with `#` comments.
    Dockerfile,
    /// Makefiles with `#` comments.
    Makefile,
    /// `CMake` scripts with `#` comments.
    #[value(name = "cmake")]
    CMake,
    /// Ruby source with `#` comments.
    Ruby,
    /// SQL files with `--` comments.
    Sql,
    /// Lua source with `--` comments.
    Lua,
    /// Markdown documents.
    Markdown,
}

/// Block comment syntax of a language.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct BlockCommentStyle {
    /// Opening delimiter, for example `/*`.
    pub(super) open: &'static str,
    /// Closing delimiter, for example `*/`.
    pub(super) close: &'static str,
    /// Marker for continuation lines inside the block, for example `*`.
    pub(super) continuation: &'static str,
}

/// Comment syntax of a file kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct CommentStyle {
    /// Line comment markers, longest first so `//!` and `///` win over `//`.
    pub(super) line_markers: &'static [&'static str],
    /// Block comment delimiters when the language has them.
    pub(super) block: Option<BlockCommentStyle>,
    /// Whether triple quoted docstrings are prose blocks.
    pub(super) docstrings: bool,
}

impl FileKind {
    /// Detect the file kind from the file extension or well known file name.
    #[must_use]
    pub fn from_path(path: &Path) -> Option<Self> {
        let file_name = path.file_name().and_then(|name| name.to_str()).unwrap_or_default();
        match file_name {
            "Dockerfile" | "Containerfile" => return Some(Self::Dockerfile),
            "Makefile" | "makefile" | "GNUmakefile" => return Some(Self::Makefile),
            "Gemfile" | "Rakefile" => return Some(Self::Ruby),
            ".bashrc" | ".zshrc" | ".zshenv" | ".zprofile" | ".profile" | ".bash_profile" => return Some(Self::Shell),
            _ if file_name.eq_ignore_ascii_case("CMakeLists.txt") => return Some(Self::CMake),
            _ => {}
        }
        let extension = path.extension().and_then(|ext| ext.to_str())?.to_ascii_lowercase();
        Self::from_extension(&extension)
    }

    /// Detect the file kind from a file extension without the leading dot.
    #[must_use]
    pub fn from_extension(extension: &str) -> Option<Self> {
        let kind = match extension.to_ascii_lowercase().as_str() {
            "rs" => Self::Rust,
            "c" | "cc" | "cpp" | "cxx" | "h" | "hpp" | "hh" | "hxx" | "java" | "kt" | "kts" | "swift" | "cs"
            | "scala" | "dart" | "m" | "mm" => Self::CLike,
            "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "mts" | "cts" => Self::JavaScript,
            "go" => Self::Go,
            "py" | "pyi" => Self::Python,
            "sh" | "bash" | "zsh" | "fish" => Self::Shell,
            "toml" => Self::Toml,
            "yml" | "yaml" => Self::Yaml,
            "dockerfile" => Self::Dockerfile,
            "mk" => Self::Makefile,
            "cmake" => Self::CMake,
            "rb" => Self::Ruby,
            "sql" => Self::Sql,
            "lua" => Self::Lua,
            "md" | "markdown" => Self::Markdown,
            _ => return None,
        };
        Some(kind)
    }

    /// Comment syntax of this file kind.
    #[must_use]
    pub(super) const fn comment_style(self) -> CommentStyle {
        const C_BLOCK: BlockCommentStyle = BlockCommentStyle {
            open: "/*",
            close: "*/",
            continuation: "*",
        };
        match self {
            Self::Rust => CommentStyle {
                line_markers: &["//!", "///", "//"],
                block: Some(C_BLOCK),
                docstrings: false,
            },
            Self::CLike | Self::JavaScript | Self::Go => CommentStyle {
                line_markers: &["///", "//"],
                block: Some(C_BLOCK),
                docstrings: false,
            },
            Self::Python => CommentStyle {
                line_markers: &["#"],
                block: None,
                docstrings: true,
            },
            Self::Shell | Self::Toml | Self::Yaml | Self::Dockerfile | Self::Makefile | Self::CMake | Self::Ruby => {
                CommentStyle {
                    line_markers: &["#"],
                    block: None,
                    docstrings: false,
                }
            }
            Self::Sql | Self::Lua => CommentStyle {
                line_markers: &["--"],
                block: None,
                docstrings: false,
            },
            Self::Markdown => CommentStyle {
                line_markers: &[],
                block: None,
                docstrings: false,
            },
        }
    }

    /// Whether the trailing comment scanner understands this language well enough to run.
    #[must_use]
    pub(super) const fn supports_trailing_comment_check(self) -> bool {
        matches!(
            self,
            Self::Rust
                | Self::CLike
                | Self::JavaScript
                | Self::Go
                | Self::Python
                | Self::Shell
                | Self::Toml
                | Self::Yaml
        )
    }
}

#[cfg(test)]
mod test_file_kind {
    use super::*;

    #[test]
    fn detects_kind_from_extension_and_name() {
        assert_eq!(FileKind::from_path(Path::new("src/lib.rs")), Some(FileKind::Rust));
        assert_eq!(FileKind::from_path(Path::new("a/b.PY")), Some(FileKind::Python));
        assert_eq!(FileKind::from_path(Path::new("x.tsx")), Some(FileKind::JavaScript));
        assert_eq!(FileKind::from_path(Path::new("Dockerfile")), Some(FileKind::Dockerfile));
        assert_eq!(FileKind::from_path(Path::new("Makefile")), Some(FileKind::Makefile));
        assert_eq!(FileKind::from_path(Path::new("CMakeLists.txt")), Some(FileKind::CMake));
        assert_eq!(
            FileKind::from_path(Path::new("modules/helpers.cmake")),
            Some(FileKind::CMake)
        );
        assert_eq!(FileKind::from_path(Path::new("README.md")), Some(FileKind::Markdown));
        assert_eq!(FileKind::from_path(Path::new("archive.zip")), None);
        assert_eq!(FileKind::from_path(Path::new("LICENSE")), None);
    }

    #[test]
    fn comment_style_orders_markers_longest_first() {
        let style = FileKind::Rust.comment_style();
        assert_eq!(style.line_markers, &["//!", "///", "//"]);
        assert!(style.block.is_some());
        assert!(FileKind::Python.comment_style().docstrings);
        assert!(FileKind::Markdown.comment_style().line_markers.is_empty());
    }

    #[test]
    fn trailing_comment_support_matches_tiers() {
        assert!(FileKind::Rust.supports_trailing_comment_check());
        assert!(FileKind::Yaml.supports_trailing_comment_check());
        assert!(!FileKind::Dockerfile.supports_trailing_comment_check());
        assert!(!FileKind::CMake.supports_trailing_comment_check());
        assert!(!FileKind::Markdown.supports_trailing_comment_check());
        assert!(!FileKind::Lua.supports_trailing_comment_check());
    }
}

#[cfg(test)]
mod test_file_kind_extensions {
    use super::*;

    #[test]
    fn every_supported_extension_maps_to_a_kind() {
        let expected = [
            ("rs", FileKind::Rust),
            ("cpp", FileKind::CLike),
            ("java", FileKind::CLike),
            ("kt", FileKind::CLike),
            ("swift", FileKind::CLike),
            ("cs", FileKind::CLike),
            ("scala", FileKind::CLike),
            ("dart", FileKind::CLike),
            ("mm", FileKind::CLike),
            ("ts", FileKind::JavaScript),
            ("mjs", FileKind::JavaScript),
            ("go", FileKind::Go),
            ("py", FileKind::Python),
            ("pyi", FileKind::Python),
            ("sh", FileKind::Shell),
            ("zsh", FileKind::Shell),
            ("fish", FileKind::Shell),
            ("toml", FileKind::Toml),
            ("yaml", FileKind::Yaml),
            ("yml", FileKind::Yaml),
            ("dockerfile", FileKind::Dockerfile),
            ("mk", FileKind::Makefile),
            ("cmake", FileKind::CMake),
            ("rb", FileKind::Ruby),
            ("sql", FileKind::Sql),
            ("lua", FileKind::Lua),
            ("markdown", FileKind::Markdown),
        ];
        for (extension, kind) in expected {
            assert_eq!(FileKind::from_extension(extension), Some(kind), "extension {extension}");
            assert_eq!(
                FileKind::from_extension(&extension.to_ascii_uppercase()),
                Some(kind),
                "uppercase extension {extension}"
            );
        }
        assert_eq!(FileKind::from_extension("zip"), None);
        assert_eq!(FileKind::from_extension(""), None);
    }

    #[test]
    fn well_known_file_names_map_to_a_kind() {
        let expected = [
            ("Dockerfile", FileKind::Dockerfile),
            ("Containerfile", FileKind::Dockerfile),
            ("Makefile", FileKind::Makefile),
            ("makefile", FileKind::Makefile),
            ("GNUmakefile", FileKind::Makefile),
            ("CMakeLists.txt", FileKind::CMake),
            ("cmakelists.txt", FileKind::CMake),
            ("Gemfile", FileKind::Ruby),
            ("Rakefile", FileKind::Ruby),
            (".bashrc", FileKind::Shell),
            (".zshrc", FileKind::Shell),
            (".zshenv", FileKind::Shell),
            (".zprofile", FileKind::Shell),
            (".profile", FileKind::Shell),
            (".bash_profile", FileKind::Shell),
        ];
        for (name, kind) in expected {
            assert_eq!(FileKind::from_path(Path::new(name)), Some(kind), "file name {name}");
            assert_eq!(
                FileKind::from_path(Path::new("some/directory").join(name).as_path()),
                Some(kind),
                "nested file name {name}"
            );
        }
    }

    #[test]
    fn comment_styles_cover_every_kind() {
        let kinds = [
            FileKind::Rust,
            FileKind::CLike,
            FileKind::JavaScript,
            FileKind::Go,
            FileKind::Python,
            FileKind::Shell,
            FileKind::Toml,
            FileKind::Yaml,
            FileKind::Dockerfile,
            FileKind::Makefile,
            FileKind::CMake,
            FileKind::Ruby,
            FileKind::Sql,
            FileKind::Lua,
            FileKind::Markdown,
        ];
        for kind in kinds {
            let style = kind.comment_style();
            let has_syntax = !style.line_markers.is_empty() || style.block.is_some() || style.docstrings;
            assert_eq!(
                has_syntax,
                kind != FileKind::Markdown,
                "{kind:?} should only lack comment syntax for Markdown"
            );
        }
        assert_eq!(FileKind::Sql.comment_style().line_markers, &["--"]);
        assert_eq!(FileKind::Lua.comment_style().line_markers, &["--"]);
        assert_eq!(FileKind::Go.comment_style().line_markers, &["///", "//"]);
        assert!(FileKind::CLike.comment_style().block.is_some());
        assert!(FileKind::Yaml.comment_style().block.is_none());
    }
}

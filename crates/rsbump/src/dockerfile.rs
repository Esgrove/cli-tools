//! Dockerfile detection and the `rust` base image rewrite.
//!
//! Finds `FROM` lines that use the official `rust` image,
//! and pins their tag to the target Rust version while keeping the image variant,
//! so `rust:1-slim-trixie` becomes `rust:1.100-slim-trixie`.
//! A digest on the line is replaced with the digest of the new tag.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::LazyLock;

use anyhow::Result;
use regex::Regex;

use crate::manifest::RustVersion;

/// Matches a `FROM` instruction using the official `rust` image, with an optional tag and digest.
static FROM_RUST_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?P<prefix>[ \t]*(?i:FROM)[ \t]+(?:--\S+[ \t]+)*)(?P<image>(?:docker\.io/)?(?:library/)?rust)(?::(?P<tag>[A-Za-z0-9_.\-]+))?(?:@(?P<digest>sha256:[0-9a-f]{64}))?(?:\s|$)",
    )
    .expect("valid FROM regex")
});

/// One image reference that was changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageChange {
    /// Image reference before the change.
    pub old: String,
    /// Image reference after the change.
    pub new: String,
}

/// Result of rewriting one Dockerfile.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DockerfileUpdate {
    /// Full file content after the rewrite.
    pub content: String,
    /// Image references that changed.
    pub changes: Vec<ImageChange>,
    /// Problems that left a line unchanged.
    pub warnings: Vec<String>,
}

/// Image tag split into its version and variant parts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ImageTag<'a> {
    /// Version part, such as "1", "1.94.0", or "latest".
    version: Option<&'a str>,
    /// Variant part, such as "slim-trixie" or "alpine".
    variant: Option<&'a str>,
}

/// Terminator of a heredoc in a Docker instruction.
struct Heredoc {
    delimiter: String,
    strip_tabs: bool,
}

impl DockerfileUpdate {
    /// Check if the rewrite changed anything.
    #[must_use]
    pub const fn is_changed(&self) -> bool {
        !self.changes.is_empty()
    }
}

impl<'a> ImageTag<'a> {
    /// Split a tag like "1.94-slim-trixie" into "1.94" and "slim-trixie".
    ///
    /// A tag without a leading version, such as "alpine", is all variant.
    fn parse(tag: &'a str) -> Self {
        let (first, rest) = tag
            .split_once('-')
            .map_or((tag, None), |(first, rest)| (first, Some(rest)));
        let is_version = first == "latest"
            || (first.starts_with(|c: char| c.is_ascii_digit()) && RustVersion::parse(first).is_some());
        if is_version {
            Self {
                version: Some(first),
                variant: rest.filter(|variant| !variant.is_empty()),
            }
        } else {
            Self {
                version: None,
                variant: Some(tag),
            }
        }
    }

    /// Build the tag for the target version with the same variant.
    fn with_version(&self, target: RustVersion) -> String {
        let version = target.major_minor();
        self.variant
            .map_or_else(|| version.clone(), |variant| format!("{version}-{variant}"))
    }

    /// Check if the version part is a pinned version newer than the target.
    fn is_newer_than(&self, target: RustVersion) -> bool {
        self.version
            .and_then(RustVersion::parse)
            .is_some_and(|version| version > target)
    }
}

/// Check if a file name looks like a Dockerfile:
/// "Dockerfile", "backend.Dockerfile", or "Dockerfile.dev".
#[must_use]
pub fn is_dockerfile(path: &Path) -> bool {
    path.file_name().and_then(|name| name.to_str()).is_some_and(|name| {
        let lower = name.to_ascii_lowercase();
        lower == "dockerfile" || lower.ends_with(".dockerfile") || lower.starts_with("dockerfile.")
    })
}

/// Rewrite every `rust` base image in the Dockerfile content to the target version.
///
/// `resolve_digest` returns the digest for a `rust` image tag, or `None` when the tag does not exist.
/// It is only called for lines that already pin a digest.
pub fn update_dockerfile(
    content: &str,
    target: RustVersion,
    resolve_digest: &dyn Fn(&str) -> Result<Option<String>>,
) -> DockerfileUpdate {
    let mut update = DockerfileUpdate {
        content: String::with_capacity(content.len()),
        ..DockerfileUpdate::default()
    };

    let escape = escape_character(content.trim_start_matches('\u{feff}'));
    let mut heredocs: VecDeque<Heredoc> = VecDeque::new();
    let mut rust_is_stage = false;
    let mut instruction = String::new();
    let mut first_line = None;
    let mut offset = 0;

    for line in content.split_inclusive('\n') {
        let start = offset;
        offset += line.len();
        let physical_line = if start == 0 {
            line.strip_prefix('\u{feff}').unwrap_or(line)
        } else {
            line
        };
        let source = physical_line.trim_end_matches(['\r', '\n']);
        if let Some(heredoc) = heredocs.front() {
            let candidate = if heredoc.strip_tabs {
                source.trim_start_matches('\t')
            } else {
                source
            };
            if candidate == heredoc.delimiter {
                heredocs.pop_front();
            }
            update.content.push_str(line);
            continue;
        }
        if source.trim().is_empty() || source.trim_start().starts_with('#') {
            if first_line.is_none() {
                update.content.push_str(line);
            }
            continue;
        }

        let start = *first_line.get_or_insert(start);
        if let Some(fragment) = continued_fragment(source, escape) {
            instruction.push_str(fragment);
            continue;
        }
        instruction.push_str(source);
        let original = content.get(start..offset).unwrap_or(line);
        if let Some((reference, alias)) = from_instruction(&instruction) {
            let first = original.split_inclusive('\n').next().unwrap_or(original);
            if !(rust_is_stage && reference.eq_ignore_ascii_case("rust"))
                && from_instruction(first).is_some_and(|(physical_reference, _)| physical_reference == reference)
                && let Some(rewritten) = rewrite_line(first, target, resolve_digest, &mut update)
            {
                update.content.push_str(&rewritten);
                update
                    .content
                    .push_str(original.strip_prefix(first).unwrap_or_default());
            } else {
                update.content.push_str(original);
            }
            rust_is_stage |= alias.is_some_and(|alias| alias.eq_ignore_ascii_case("rust"));
        } else {
            heredocs.extend(instruction_heredocs(&instruction));
            update.content.push_str(original);
        }
        instruction.clear();
        first_line = None;
    }
    if let Some(remaining) = first_line.and_then(|start| content.get(start..)) {
        update.content.push_str(remaining);
    }
    update
}

/// Read Docker's escape parser directive before the first ordinary line or comment.
fn escape_character(content: &str) -> char {
    for line in content.lines() {
        let Some(comment) = line.trim_start().strip_prefix('#') else {
            break;
        };
        let Some((key, value)) = comment.split_once('=') else {
            break;
        };
        let key = key.trim();
        if key.eq_ignore_ascii_case("escape") {
            return if value.trim() == "`" { '`' } else { '\\' };
        }
        if !key.eq_ignore_ascii_case("syntax") && !key.eq_ignore_ascii_case("check") {
            break;
        }
    }
    '\\'
}

/// Remove the Docker continuation character unless preceded immediately by another escape.
fn continued_fragment(line: &str, escape: char) -> Option<&str> {
    let fragment = line.trim_end_matches([' ', '\t']).strip_suffix(escape)?;
    (!fragment.ends_with(escape)).then_some(fragment)
}

/// Extract the image or stage reference and optional name from a logical `FROM` instruction.
fn from_instruction(instruction: &str) -> Option<(&str, Option<&str>)> {
    let mut words = instruction.split_ascii_whitespace();
    if !words.next()?.eq_ignore_ascii_case("from") {
        return None;
    }
    let reference = words.find(|word| !word.starts_with("--"))?;
    let alias = words
        .next()
        .filter(|word| word.eq_ignore_ascii_case("as"))
        .and_then(|_| words.next());
    Some((reference, alias))
}

/// Read heredoc operands from the supported shell form instructions.
fn instruction_heredocs(mut instruction: &str) -> Vec<Heredoc> {
    instruction = instruction.trim_start();
    let Some((mut keyword, mut arguments)) = instruction.split_once(char::is_whitespace) else {
        return Vec::new();
    };
    if keyword.eq_ignore_ascii_case("onbuild") {
        let Some(trigger) = arguments.trim_start().split_once(char::is_whitespace) else {
            return Vec::new();
        };
        (keyword, arguments) = trigger;
    }
    if !["run", "copy", "add"]
        .iter()
        .any(|kind| keyword.eq_ignore_ascii_case(kind))
    {
        return Vec::new();
    }
    let mut arguments = arguments.trim_start();
    while arguments.starts_with("--") {
        let Some((_, remaining)) = arguments.split_once(char::is_whitespace) else {
            return Vec::new();
        };
        arguments = remaining.trim_start();
    }
    if arguments.starts_with('[') {
        return Vec::new();
    }

    let mut heredocs = Vec::new();
    let mut start = 0;
    let mut quote = None;
    let mut escaped = false;
    for (index, character) in arguments.char_indices().chain(std::iter::once((arguments.len(), ' '))) {
        if escaped {
            escaped = false;
        } else if character == '\\' && quote != Some('\'') {
            escaped = true;
        } else if let Some(delimiter) = quote {
            if character == delimiter {
                quote = None;
            }
        } else if matches!(character, '\'' | '"') {
            quote = Some(character);
        } else if character.is_whitespace() {
            if let Some(heredoc) = arguments.get(start..index).and_then(heredoc_operand) {
                heredocs.push(heredoc);
            }
            start = index + character.len_utf8();
        }
    }
    heredocs
}

/// Parse a heredoc operator and remove its shell quoting from the delimiter.
fn heredoc_operand(word: &str) -> Option<Heredoc> {
    let word = word
        .trim_start_matches(|character: char| character.is_ascii_digit())
        .strip_prefix("<<")?;
    let (word, strip_tabs) = word.strip_prefix('-').map_or((word, false), |rest| (rest, true));
    if word.starts_with('<') {
        return None;
    }
    let mut delimiter = String::new();
    let mut quote = None;
    let mut characters = word.chars();
    while let Some(character) = characters.next() {
        match (quote, character) {
            (Some(active), character) if active == character => quote = None,
            (None, '\'' | '"') => quote = Some(character),
            (None, '\\') => delimiter.push(characters.next()?),
            (Some('"'), '\\') if characters.as_str().starts_with(['$', '`', '"', '\\']) => {
                delimiter.push(characters.next()?);
            }
            _ => delimiter.push(character),
        }
    }
    (!delimiter.is_empty()).then_some(Heredoc { delimiter, strip_tabs })
}

/// Rewrite one line, returning `None` when it stays unchanged.
fn rewrite_line(
    line: &str,
    target: RustVersion,
    resolve_digest: &dyn Fn(&str) -> Result<Option<String>>,
    update: &mut DockerfileUpdate,
) -> Option<String> {
    let captures = FROM_RUST_REGEX.captures(line)?;
    let prefix = captures.name("prefix")?;
    let image = captures.name("image")?;
    let old_tag = captures.name("tag").map_or("latest", |tag| tag.as_str());
    if old_tag.contains('$') {
        return None;
    }
    let old_digest = captures.name("digest").map(|digest| digest.as_str());
    let reference_end = captures
        .name("digest")
        .or_else(|| captures.name("tag"))
        .unwrap_or(image)
        .end();

    let tag = ImageTag::parse(old_tag);
    if tag.is_newer_than(target) {
        return None;
    }
    let new_tag = tag.with_version(target);

    let new_digest = match old_digest {
        None => None,
        Some(_) => match resolve_digest(&new_tag) {
            Ok(Some(digest)) => Some(digest),
            Ok(None) => {
                update
                    .warnings
                    .push(format!("Image tag rust:{new_tag} not found, keeping {}", line.trim()));
                return None;
            }
            Err(error) => {
                update
                    .warnings
                    .push(format!("Failed to resolve digest for rust:{new_tag}: {error:#}"));
                return None;
            }
        },
    };

    let old_reference = line.get(image.start()..reference_end)?.to_string();
    let new_reference = new_digest.map_or_else(
        || format!("{}:{new_tag}", image.as_str()),
        |digest| format!("{}:{new_tag}@{digest}", image.as_str()),
    );
    if new_reference == old_reference {
        return None;
    }

    let rest = line.get(reference_end..)?;
    update.changes.push(ImageChange {
        old: old_reference,
        new: new_reference.clone(),
    });
    Some(format!("{}{new_reference}{rest}", prefix.as_str()))
}

#[cfg(test)]
mod test_image_tag {
    use super::*;

    const TARGET: RustVersion = RustVersion::new(1, 100, 0);

    #[test]
    fn splits_version_and_variant() {
        let tag = ImageTag::parse("1-slim-trixie");
        assert_eq!(tag.version, Some("1"));
        assert_eq!(tag.variant, Some("slim-trixie"));
        assert_eq!(tag.with_version(TARGET), "1.100-slim-trixie");
    }

    #[test]
    fn latest_becomes_version() {
        assert_eq!(ImageTag::parse("latest").with_version(TARGET), "1.100");
    }

    #[test]
    fn variant_only_gets_version_prefix() {
        assert_eq!(ImageTag::parse("alpine").with_version(TARGET), "1.100-alpine");
        assert_eq!(
            ImageTag::parse("slim-bookworm").with_version(TARGET),
            "1.100-slim-bookworm"
        );
    }

    #[test]
    fn full_version_with_variant() {
        assert_eq!(
            ImageTag::parse("1.94.0-alpine3.20").with_version(TARGET),
            "1.100-alpine3.20"
        );
    }

    #[test]
    fn detects_newer_pin() {
        assert!(ImageTag::parse("1.101-slim").is_newer_than(TARGET));
        assert!(!ImageTag::parse("1-slim").is_newer_than(TARGET));
        assert!(!ImageTag::parse("latest").is_newer_than(TARGET));
    }
}

#[cfg(test)]
mod test_update_dockerfile {
    use super::*;

    const TARGET: RustVersion = RustVersion::new(1, 100, 0);
    const OLD_DIGEST: &str = "sha256:6f14b2b4b278ec0d50aa1d69541c304ce0f95e0f6268487aa9924af1c2e9aebe";
    const NEW_DIGEST: &str = "sha256:1111111111111111111111111111111111111111111111111111111111111111";

    #[expect(clippy::unnecessary_wraps, reason = "matches the resolver signature")]
    fn fake_resolver(tag: &str) -> Result<Option<String>> {
        Ok((tag == "1.100-slim-trixie").then(|| NEW_DIGEST.to_string()))
    }

    fn update(content: &str) -> DockerfileUpdate {
        update_dockerfile(content, TARGET, &fake_resolver)
    }

    #[test]
    fn pins_tag_and_refreshes_digest() {
        let content = format!("FROM rust:1-slim-trixie@{OLD_DIGEST} AS builder\n");
        let result = update(&content);
        assert_eq!(
            result.content,
            format!("FROM rust:1.100-slim-trixie@{NEW_DIGEST} AS builder\n")
        );
        assert_eq!(result.changes.len(), 1);
        assert_eq!(result.warnings, [] as [std::string::String; 0]);
    }

    #[test]
    fn handles_platform_flag() {
        let content = format!("FROM --platform=$BUILDPLATFORM rust:1-slim-trixie@{OLD_DIGEST} AS builder-amd64\n");
        let result = update(&content);
        assert_eq!(
            result.content,
            format!("FROM --platform=$BUILDPLATFORM rust:1.100-slim-trixie@{NEW_DIGEST} AS builder-amd64\n")
        );
    }

    #[test]
    fn rewrites_every_matching_line() {
        let content = format!(
            "# syntax=docker/dockerfile:1\nFROM rust:1-slim-trixie@{OLD_DIGEST} AS builder\nRUN cargo build\n\nFROM debian:trixie-slim\nFROM rust:1-slim-trixie@{OLD_DIGEST} AS other\n"
        );
        let result = update(&content);
        assert_eq!(result.changes.len(), 2);
        assert!(result.content.contains("RUN cargo build\n\nFROM debian:trixie-slim\n"));
        assert!(!result.content.contains(OLD_DIGEST));
    }

    #[test]
    fn latest_without_digest() {
        let result = update("FROM rust:latest AS builder\nWORKDIR /app\n");
        assert_eq!(result.content, "FROM rust:1.100 AS builder\nWORKDIR /app\n");
    }

    #[test]
    fn variant_without_version() {
        let result = update("FROM rust:alpine AS deps\n");
        assert_eq!(result.content, "FROM rust:1.100-alpine AS deps\n");
    }

    #[test]
    fn image_without_tag() {
        let result = update("FROM rust\n");
        assert_eq!(result.content, "FROM rust:1.100\n");
    }

    #[test]
    fn build_arg_variable_tag_is_left_unchanged() {
        let content = "ARG RUST_VERSION=1.90\nFROM rust:${RUST_VERSION}-slim AS builder\nFROM rust:$TAG\n";
        let result = update(content);
        assert_eq!(result.content, content);
        assert!(!result.is_changed());
    }

    #[test]
    fn registry_prefixed_image() {
        let result = update("from docker.io/library/rust:1.90-slim as build\r\n");
        assert_eq!(result.content, "from docker.io/library/rust:1.100-slim as build\r\n");
    }

    #[test]
    fn ignores_other_images() {
        let content = "FROM rustlang/rust:nightly\nFROM debian:trixie-slim\nFROM ghcr.io/rust:1\nFROM rust-builder:1\n";
        let result = update(content);
        assert_eq!(result.content, content);
        assert!(!result.is_changed());
    }

    #[test]
    fn already_current_is_unchanged() {
        let result = update("FROM rust:1.100-slim\n");
        assert!(!result.is_changed());
    }

    #[test]
    fn a_previous_stage_alias_shadows_an_unqualified_image_name() {
        let original =
            "FROM rust AS first\nFROM deBIAN aS RuSt\nFROM rust AS release\nFROM docker.io/library/rust AS upstream\n";
        let result = update(original);
        assert_eq!(
            result.content,
            "FROM rust:1.100 AS first\nFROM deBIAN aS RuSt\nFROM rust AS release\nFROM docker.io/library/rust:1.100 AS upstream\n"
        );
        assert_eq!(result.changes.len(), 2);
    }

    #[test]
    fn heredoc_bodies_and_continued_instruction_arguments_are_not_instructions() {
        let original = concat!(
            "# escape=`\r\n",
            "FROM rust:1 AS build\r\n",
            "RUN cat <<'FIRST' `\r\n",
            "    <<-\"SECOND\"\r\n",
            "FROM debian AS rust\r\n",
            "FROM rust:1\r\n",
            "FIRST\r\n",
            "FROM rust:1\r\n",
            "\tSECOND\r\n",
            "COPY <<END /tmp/example\r\n",
            "FROM rust:1\r\n",
            "END\r\n",
            "RUN printf '%s' `\r\n",
            "FROM rust:1\r\n",
            "FROM rust:1",
        );
        let expected = original.replacen("FROM rust:1 AS build", "FROM rust:1.100 AS build", 1);
        let expected = format!("{}FROM rust:1.100", expected.strip_suffix("FROM rust:1").unwrap());
        let result = update(original);
        assert_eq!(result.content, expected);
        assert_eq!(result.changes.len(), 2);
    }

    #[test]
    fn newer_pin_is_unchanged() {
        let result = update("FROM rust:1.101-slim\n");
        assert!(!result.is_changed());
    }

    #[test]
    fn missing_tag_keeps_line_and_warns() {
        let content = format!("FROM rust:1-alpine@{OLD_DIGEST}\n");
        let result = update(&content);
        assert_eq!(result.content, content);
        assert_eq!(result.warnings.len(), 1);
    }

    #[test]
    fn keeps_content_without_trailing_newline() {
        let result = update("FROM rust:1.90");
        assert_eq!(result.content, "FROM rust:1.100");
    }
}

#[cfg(test)]
mod test_is_dockerfile {
    use super::*;

    #[test]
    fn matches_dockerfile_names() {
        assert!(is_dockerfile(Path::new("Dockerfile")));
        assert!(is_dockerfile(Path::new("infra/backend.Dockerfile")));
        assert!(is_dockerfile(Path::new("Dockerfile.dev")));
        assert!(is_dockerfile(Path::new("dockerfile")));
    }

    #[test]
    fn rejects_other_names() {
        assert!(!is_dockerfile(Path::new("docker-compose.yml")));
        assert!(!is_dockerfile(Path::new("Dockerfiles")));
        assert!(!is_dockerfile(Path::new("Cargo.toml")));
    }
}

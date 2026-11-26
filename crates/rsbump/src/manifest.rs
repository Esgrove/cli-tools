//! Rust version parsing and the `rust-version` edit in `Cargo.toml`.
//!
//! Reads the local toolchain version from `rustc --version`,
//! and rewrites the `rust-version` field of a manifest with `toml_edit`,
//! so the rest of the file keeps its formatting and comments.

use std::fmt;
use std::process::Command;

use anyhow::{Context, Result, anyhow};
use toml_edit::{DocumentMut, Item, Value};

/// Result of updating the `rust-version` field of one manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestUpdate {
    /// The field was changed, with the new manifest content.
    Updated {
        /// Version string before the change.
        old: String,
        /// Version string after the change.
        new: String,
        /// Full manifest content after the change.
        content: String,
    },
    /// The field already matches the target, or is newer than it.
    Current(String),
    /// The manifest has no `rust-version` of its own.
    NoField,
}

/// Location of the `rust-version` field inside a manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FieldLocation {
    /// `[workspace.package]`
    WorkspacePackage,
    /// `[package]`
    Package,
}

/// The local `rustc` version and the stable release it targets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalToolchain {
    /// Stable release to bump projects to.
    pub target: RustVersion,
    /// The full `rustc --version` output when the toolchain is a nightly or beta,
    /// whose own version is not released yet.
    pub prerelease: Option<String>,
}

/// A Rust version with major, minor, and patch numbers.
///
/// Missing components count as zero when comparing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RustVersion {
    /// Major version.
    pub major: u64,
    /// Minor version.
    pub minor: u64,
    /// Patch version.
    pub patch: u64,
}

impl RustVersion {
    /// Create a version from its components.
    #[must_use]
    pub const fn new(major: u64, minor: u64, patch: u64) -> Self {
        Self { major, minor, patch }
    }

    /// Read the version of the local `rustc` and return the stable release it targets.
    ///
    /// # Errors
    /// Returns an error if `rustc` cannot be run or its output cannot be parsed.
    pub fn from_local_toolchain() -> Result<LocalToolchain> {
        let output = Command::new("rustc")
            .arg("--version")
            .output()
            .context("Failed to run rustc --version")?;
        if !output.status.success() {
            return Err(anyhow!("rustc --version failed"));
        }
        let text = String::from_utf8_lossy(&output.stdout);
        let target = Self::from_rustc_output(&text)
            .ok_or_else(|| anyhow!("Unexpected rustc --version output: {}", text.trim()))?;
        let prerelease = Self::is_prerelease_rustc_output(&text).then(|| text.trim().to_string());
        Ok(LocalToolchain { target, prerelease })
    }

    /// Parse the output of `rustc --version`, for example "rustc 1.100.0 (0ef1bc386 2026-11-09)".
    ///
    /// A nightly or beta toolchain such as "rustc 1.101.0-nightly" is ahead of the newest stable release,
    /// and a manifest or Docker image cannot use a version that is not released yet,
    /// so it gives the stable release before it, here 1.100.0.
    #[must_use]
    pub fn from_rustc_output(output: &str) -> Option<Self> {
        let version = output.split_whitespace().nth(1)?;
        let (numeric, prerelease) = version
            .split_once('-')
            .map_or((version, false), |(numeric, _)| (numeric, true));
        let parsed = Self::parse(numeric)?;
        if prerelease && parsed.minor > 0 {
            Some(Self::new(parsed.major, parsed.minor - 1, 0))
        } else {
            Some(parsed)
        }
    }

    /// Parse a version with one to three numeric components, such as "1", "1.94", or "1.94.0".
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let mut parts = text.trim().split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next().map_or(Some(0), |part| part.parse().ok())?;
        let patch = parts.next().map_or(Some(0), |part| part.parse().ok())?;
        if parts.next().is_some() {
            return None;
        }
        Some(Self::new(major, minor, patch))
    }

    /// Format with the same number of components as `template`,
    /// so "1.94" becomes "1.100" and "1.94.0" becomes "1.100.0".
    #[must_use]
    pub fn format_like(&self, template: &str) -> String {
        if template.trim().split('.').count() >= 3 {
            self.to_string()
        } else {
            self.major_minor()
        }
    }

    /// The version as "major.minor".
    #[must_use]
    pub fn major_minor(&self) -> String {
        format!("{}.{}", self.major, self.minor)
    }

    /// Whether `rustc --version` output is from a nightly or beta toolchain.
    fn is_prerelease_rustc_output(output: &str) -> bool {
        output
            .split_whitespace()
            .nth(1)
            .is_some_and(|version| version.contains('-'))
    }
}

impl fmt::Display for RustVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// Set the `rust-version` field in the manifest content to the target version.
///
/// Uses `[workspace.package]` when it has the field, and `[package]` otherwise.
/// A field inherited with `rust-version.workspace = true` counts as missing,
/// since the workspace root owns the value.
/// A field that is already newer than the target is left alone.
///
/// # Errors
/// Returns an error if the manifest is not valid TOML.
pub fn update_rust_version(content: &str, target: RustVersion) -> Result<ManifestUpdate> {
    let mut document: DocumentMut = content.parse().context("Failed to parse Cargo.toml")?;
    let Some(location) = find_field(&document) else {
        return Ok(ManifestUpdate::NoField);
    };
    let Some(value) = field_mut(&mut document, location).and_then(Item::as_value_mut) else {
        return Ok(ManifestUpdate::NoField);
    };
    let Some(old) = value.as_str().map(ToString::to_string) else {
        return Ok(ManifestUpdate::NoField);
    };

    if RustVersion::parse(&old).is_some_and(|current| current >= target) {
        return Ok(ManifestUpdate::Current(old));
    }

    let new = target.format_like(&old);
    if new == old {
        return Ok(ManifestUpdate::Current(old));
    }
    let decor = value.decor().clone();
    *value = Value::from(new.as_str());
    *value.decor_mut() = decor;

    Ok(ManifestUpdate::Updated {
        old,
        new,
        content: document.to_string(),
    })
}

/// Check if the manifest content defines a workspace.
#[must_use]
pub fn has_workspace_table(content: &str) -> bool {
    content
        .parse::<DocumentMut>()
        .is_ok_and(|document| document.contains_key("workspace"))
}

/// Find which table holds a string `rust-version` field.
fn find_field(document: &DocumentMut) -> Option<FieldLocation> {
    let is_string = |item: Option<&Item>| item.and_then(Item::as_str).is_some();
    if is_string(
        document
            .get("workspace")
            .and_then(|workspace| workspace.get("package"))
            .and_then(|package| package.get("rust-version")),
    ) {
        return Some(FieldLocation::WorkspacePackage);
    }
    if is_string(document.get("package").and_then(|package| package.get("rust-version"))) {
        return Some(FieldLocation::Package);
    }
    None
}

/// Mutable access to the `rust-version` field at the given location.
fn field_mut(document: &mut DocumentMut, location: FieldLocation) -> Option<&mut Item> {
    match location {
        FieldLocation::WorkspacePackage => document
            .get_mut("workspace")?
            .get_mut("package")?
            .get_mut("rust-version"),
        FieldLocation::Package => document.get_mut("package")?.get_mut("rust-version"),
    }
}

#[cfg(test)]
mod test_rust_version {
    use super::*;

    #[test]
    fn parses_rustc_output() {
        let version = RustVersion::from_rustc_output("rustc 1.100.0 (0ef1bc386 2026-11-09)\n");
        assert_eq!(version, Some(RustVersion::new(1, 100, 0)));
    }

    #[test]
    fn a_nightly_or_beta_toolchain_targets_the_latest_stable_release() {
        let nightly = RustVersion::from_rustc_output("rustc 1.101.0-nightly (abcdef123 2026-11-20)");
        let beta = RustVersion::from_rustc_output("rustc 1.101.0-beta.3 (abcdef123 2026-11-20)");
        assert_eq!(nightly, Some(RustVersion::new(1, 100, 0)));
        assert_eq!(beta, Some(RustVersion::new(1, 100, 0)));
    }

    #[test]
    fn parses_partial_versions() {
        assert_eq!(RustVersion::parse("1"), Some(RustVersion::new(1, 0, 0)));
        assert_eq!(RustVersion::parse("1.94"), Some(RustVersion::new(1, 94, 0)));
        assert_eq!(RustVersion::parse("1.94.2"), Some(RustVersion::new(1, 94, 2)));
    }

    #[test]
    fn rejects_invalid_versions() {
        assert_eq!(RustVersion::parse("latest"), None);
        assert_eq!(RustVersion::parse("1.2.3.4"), None);
        assert_eq!(RustVersion::parse("1.x"), None);
    }

    #[test]
    fn compares_numerically() {
        assert!(RustVersion::new(1, 100, 0) > RustVersion::new(1, 99, 5));
    }

    #[test]
    fn formats_like_template() {
        let version = RustVersion::new(1, 100, 0);
        assert_eq!(version.format_like("1.94"), "1.100");
        assert_eq!(version.format_like("1.94.0"), "1.100.0");
        assert_eq!(version.format_like("1"), "1.100");
    }
}

#[cfg(test)]
mod test_update_rust_version {
    use super::*;

    const TARGET: RustVersion = RustVersion::new(1, 100, 0);

    #[test]
    fn updates_package_field() {
        let content = "[package]\nname = \"demo\"\nedition = \"2024\"\nrust-version = \"1.94\"\n";
        let update = update_rust_version(content, TARGET).expect("valid manifest");
        assert_eq!(
            update,
            ManifestUpdate::Updated {
                old: "1.94".to_string(),
                new: "1.100".to_string(),
                content: "[package]\nname = \"demo\"\nedition = \"2024\"\nrust-version = \"1.100\"\n".to_string(),
            }
        );
    }

    #[test]
    fn updates_workspace_package_field() {
        let content = "[workspace]\nmembers = [\"crates/*\"]\n\n[workspace.package]\nrust-version = \"1.98\"\n";
        let ManifestUpdate::Updated { content, .. } = update_rust_version(content, TARGET).expect("valid manifest")
        else {
            unreachable!("expected an update");
        };
        assert!(content.contains("[workspace.package]\nrust-version = \"1.100\"\n"));
    }

    #[test]
    fn keeps_three_components() {
        let content = "[package]\nname = \"demo\"\nrust-version = \"1.93.0\"\n";
        let ManifestUpdate::Updated { new, .. } = update_rust_version(content, TARGET).expect("valid manifest") else {
            unreachable!("expected an update");
        };
        assert_eq!(new, "1.100.0");
    }

    #[test]
    fn does_not_downgrade() {
        let content = "[package]\nname = \"demo\"\nrust-version = \"1.101\"\n";
        let update = update_rust_version(content, TARGET).expect("valid manifest");
        assert_eq!(update, ManifestUpdate::Current("1.101".to_string()));
    }

    #[test]
    fn reports_current_version() {
        let content = "[package]\nname = \"demo\"\nrust-version = \"1.100\"\n";
        let update = update_rust_version(content, TARGET).expect("valid manifest");
        assert_eq!(update, ManifestUpdate::Current("1.100".to_string()));
    }

    #[test]
    fn reports_missing_field() {
        let content = "[package]\nname = \"demo\"\nedition = \"2024\"\n";
        let update = update_rust_version(content, TARGET).expect("valid manifest");
        assert_eq!(update, ManifestUpdate::NoField);
    }

    #[test]
    fn inherited_field_counts_as_missing() {
        let content = "[package]\nname = \"demo\"\nrust-version.workspace = true\n";
        let update = update_rust_version(content, TARGET).expect("valid manifest");
        assert_eq!(update, ManifestUpdate::NoField);
    }

    #[test]
    fn preserves_comments_and_formatting() {
        let content = "# Top comment\n[package]\nname    = \"demo\"\nrust-version = \"1.90\" # minimum\n\n[dependencies]\nanyhow = \"1\"\n";
        let ManifestUpdate::Updated { content, .. } = update_rust_version(content, TARGET).expect("valid manifest")
        else {
            unreachable!("expected an update");
        };
        assert_eq!(
            content,
            "# Top comment\n[package]\nname    = \"demo\"\nrust-version = \"1.100\" # minimum\n\n[dependencies]\nanyhow = \"1\"\n"
        );
    }

    #[test]
    fn invalid_toml_is_an_error() {
        assert!(update_rust_version("[package\n", TARGET).is_err());
    }
}

#[cfg(test)]
mod test_has_workspace_table {
    use super::*;

    #[test]
    fn detects_workspace() {
        assert!(has_workspace_table("[workspace]\n"));
        assert!(has_workspace_table(
            "[package]\nname = \"fuzz\"\n\n[workspace]\nmembers = []\n"
        ));
        assert!(has_workspace_table("[workspace.package]\nedition = \"2024\"\n"));
    }

    #[test]
    fn package_is_not_workspace() {
        assert!(!has_workspace_table("[package]\nname = \"demo\"\n"));
        assert!(!has_workspace_table("not toml ["));
    }
}

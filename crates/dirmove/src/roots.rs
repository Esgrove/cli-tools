//! Input and output roots, and the display paths shown relative to them.
//!
//! `Roots` resolves the independent input and output root sets from the given paths,
//! ranks destination directories by their depth below an output root,
//! and formats source and destination paths compactly for display.

use std::path::{Path, PathBuf};

use anyhow::Result;

use cli_tools_common::{nearest_containing_root, relative_path_within};

use cli_tools_common::{get_relative_path_or_filename, path_to_filename_string, path_to_string_relative};

use crate::types::DirectoryInfo;
use crate::utils;

/// Input roots to scan and output roots holding the candidate destination directories.
///
/// Inputs and outputs are not paired.
/// Every input root contributes files and directories,
/// and every output root contributes candidate destination directories.
/// Both lists are non-empty.
#[derive(Debug, Clone)]
pub struct Roots {
    /// Resolved input files and directories.
    input: Vec<PathBuf>,
    /// Resolved output directories.
    output: Vec<PathBuf>,
}

impl Roots {
    /// Create roots from resolved input and output root lists.
    ///
    /// # Panics
    /// Panics if either list is empty.
    #[must_use]
    pub fn new(input: Vec<PathBuf>, output: Vec<PathBuf>) -> Self {
        assert!(!input.is_empty(), "Roots need at least one input root");
        assert!(!output.is_empty(), "Roots need at least one output root");
        Self { input, output }
    }

    /// Resolve input and output root sets from the given paths.
    ///
    /// If no input is given, the current directory is used.
    /// If no output is given, the resolved input roots are reused as output roots.
    ///
    /// # Errors
    /// Returns an error if a path cannot be resolved.
    pub fn resolve(inputs: &[PathBuf], outputs: &[PathBuf]) -> Result<Self> {
        let input = if inputs.is_empty() {
            vec![cli_tools_common::resolve_input_path(None)?]
        } else {
            inputs
                .iter()
                .map(|path| cli_tools_common::resolve_input_path(Some(path)))
                .collect::<Result<Vec<_>>>()?
        };

        let mut seen = std::collections::HashSet::new();
        let output = if outputs.is_empty() {
            input
                .iter()
                .map(|path| {
                    if path.is_file() {
                        path.parent().map_or_else(|| path.clone(), Path::to_path_buf)
                    } else {
                        path.clone()
                    }
                })
                .filter(|path| seen.insert(path.clone()))
                .collect()
        } else {
            let mut resolved_outputs = Vec::with_capacity(outputs.len());
            for path in outputs {
                let resolved = cli_tools_common::resolve_required_input_path(path)?;
                if !resolved.is_dir() {
                    anyhow::bail!("Output path must be a directory: {}", resolved.display());
                }
                if seen.insert(resolved.clone()) {
                    resolved_outputs.push(resolved);
                }
            }
            resolved_outputs
        };

        Ok(Self::new(input, output))
    }

    /// Return the input roots.
    #[must_use]
    pub fn input(&self) -> &[PathBuf] {
        &self.input
    }

    /// Return the output roots.
    #[must_use]
    pub fn output(&self) -> &[PathBuf] {
        &self.output
    }

    /// Return the first output root, used for implicit destinations such as newly created directories.
    ///
    /// # Panics
    /// Never panics in practice, since `new` requires at least one output root.
    #[must_use]
    pub fn first_output(&self) -> &Path {
        self.output.first().expect("Roots always have at least one output root")
    }

    /// Return the normalized name of the first output root.
    ///
    /// A group with this name would create a directory named like the root it is created in.
    #[must_use]
    pub fn first_output_normalized_name(&self) -> Option<String> {
        self.first_output()
            .file_name()
            .and_then(|name| name.to_str())
            .map(utils::normalize_name)
    }

    /// Compute a directory's depth relative to the output root that contains it.
    ///
    /// If multiple output roots contain the directory, the deepest relative depth is used
    /// so nested matches still win when roots overlap.
    /// Paths outside all output roots fall back to absolute component count,
    /// which keeps ordering deterministic for synthetic paths.
    #[must_use]
    pub(crate) fn output_depth(&self, directory: &Path) -> usize {
        self.output
            .iter()
            .filter_map(|output_root| directory.strip_prefix(output_root).ok())
            .map(|path| path.components().count())
            .max()
            .unwrap_or_else(|| directory.components().count())
    }

    /// Format a destination directory for display relative to the nearest output root.
    ///
    /// With multiple output roots this avoids displaying long absolute paths
    /// when the destination can be shown relative to one of the configured roots.
    #[must_use]
    pub fn directory_display_path(&self, directory: &DirectoryInfo) -> String {
        self.output
            .iter()
            .filter_map(|output_root| {
                directory
                    .path
                    .strip_prefix(output_root)
                    .ok()
                    .map(|relative| (output_root, relative))
            })
            .min_by_key(|(_, relative)| relative.components().count())
            .map_or_else(
                || get_relative_path_or_filename(&directory.path, self.first_output()),
                |(output_root, _)| get_relative_path_or_filename(&directory.path, output_root),
            )
    }

    /// Format a destination directory label.
    ///
    /// The compact relative form is used only
    /// when every offered source file is being moved within that file's input root.
    /// If any file would cross to a different root, the full destination path is shown.
    /// This keeps the drive letter, UNC path, or mapped drive location visible.
    #[must_use]
    pub fn move_to_display_path(&self, target: &Path, files: &[PathBuf]) -> String {
        if files.is_empty() {
            return target.display().to_string();
        }

        let source_roots: Vec<PathBuf> = files
            .iter()
            .map(|file_path| self.source_root_for_file(file_path))
            .collect();
        if source_roots
            .iter()
            .any(|source_root| relative_path_within(target, source_root).is_none())
        {
            return target.display().to_string();
        }

        relative_display_path_from_nearest_root(target, &source_roots).unwrap_or_else(|| target.display().to_string())
    }

    /// Format paths for displaying whole directory merges.
    ///
    /// Input root paths stay compact.
    /// Output only roots are shown in full,
    /// so source and target directories with the same name are not visually ambiguous.
    #[must_use]
    pub fn merge_display_path(&self, path: &Path) -> String {
        if let Some(input_root) = nearest_containing_root(path, &self.input) {
            return get_relative_path_or_filename(path, input_root);
        }

        if let Some(output_root) = nearest_containing_root(path, &self.output) {
            if self
                .input
                .iter()
                .any(|input_root| cli_tools_common::paths_refer_to_same_file(input_root, output_root))
            {
                return get_relative_path_or_filename(path, output_root);
            }
            return path.display().to_string();
        }

        path_to_string_relative(path)
    }

    /// Return the input root associated with a source file.
    ///
    /// Explicit file inputs use their parent directory as the display root.
    /// Directory inputs use the nearest containing input root.
    /// Filesystem identity checks cover Windows mapped drive aliases that do not share the same textual prefix.
    fn source_root_for_file(&self, file_path: &Path) -> PathBuf {
        self.input
            .iter()
            .filter_map(|input_root| source_root_candidate_for_file(input_root, file_path))
            .min_by_key(|(_, relative_depth)| *relative_depth)
            .map(|(source_root, _)| source_root)
            .or_else(|| file_path.parent().map(Path::to_path_buf))
            .unwrap_or_else(|| PathBuf::from("."))
    }
}

/// Return a candidate source root and file depth for one configured input root.
fn source_root_candidate_for_file(input_root: &Path, file_path: &Path) -> Option<(PathBuf, usize)> {
    if input_root.is_file() {
        return cli_tools_common::paths_refer_to_same_file(input_root, file_path)
            .then(|| input_root.parent().map(|parent| (parent.to_path_buf(), 0)))
            .flatten();
    }

    relative_path_within(file_path, input_root)
        .map(|relative_path| (input_root.to_path_buf(), relative_path.components().count()))
}

/// Format `path` relative to the nearest root that contains the same filesystem location.
fn relative_display_path_from_nearest_root(path: &Path, roots: &[PathBuf]) -> Option<String> {
    roots
        .iter()
        .filter_map(|root| {
            let relative_path = relative_path_within(path, root)?;
            Some((relative_path.components().count(), relative_path))
        })
        .min_by_key(|(relative_depth, _)| *relative_depth)
        .map(|(_, relative_path)| relative_path_to_display(path, &relative_path))
}

/// Format a relative path, falling back to the original path's file name for an empty relative path.
fn relative_path_to_display(path: &Path, relative_path: &Path) -> String {
    if relative_path.as_os_str().is_empty() {
        path_to_filename_string(path)
    } else {
        relative_path.display().to_string()
    }
}

#[cfg(test)]
mod test_resolve {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn defaults_both_to_current_dir() -> anyhow::Result<()> {
        let roots = Roots::resolve(&[], &[])?;
        let current_dir = cli_tools_common::resolve_input_path(None)?;

        assert_eq!(roots.input(), std::slice::from_ref(&current_dir));
        assert_eq!(roots.output(), [current_dir]);
        Ok(())
    }

    #[test]
    fn defaults_output_to_input() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let roots = Roots::resolve(&[tmp.path().to_path_buf()], &[])?;

        assert_eq!(roots.input(), roots.output());
        Ok(())
    }

    #[test]
    fn defaults_input_to_current_dir_when_only_output_given() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let roots = Roots::resolve(&[], &[tmp.path().to_path_buf()])?;
        let current_dir = cli_tools_common::resolve_input_path(None)?;
        let expected_output = cli_tools_common::resolve_input_path(Some(tmp.path()))?;

        assert_eq!(roots.input(), [current_dir]);
        assert_eq!(roots.output(), [expected_output]);
        Ok(())
    }

    #[test]
    fn single_file_input_defaults_output_to_parent_directory() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let file_a = tmp.path().join("Show.S01E01.mkv");
        let file_b = tmp.path().join("Show.S01E02.mkv");
        std::fs::write(&file_a, b"a")?;
        std::fs::write(&file_b, b"b")?;

        let roots = Roots::resolve(&[file_a, file_b], &[])?;
        let expected_dir = cli_tools_common::resolve_input_path(Some(tmp.path()))?;
        assert_eq!(roots.output(), [expected_dir]);
        Ok(())
    }
}

#[cfg(test)]
mod test_display_paths {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn directory_merge_display_uses_full_output_path_when_roots_differ() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let input_root = tmp.path().join("input");
        let output_root = tmp.path().join("output");
        let input_directory = input_root.join("Example");
        let output_directory = output_root.join("Example");
        std::fs::create_dir_all(&input_directory)?;
        std::fs::create_dir_all(&output_directory)?;

        let roots = Roots::new(vec![input_root], vec![output_root]);

        assert_eq!(roots.merge_display_path(&input_directory), "Example");
        assert_eq!(
            roots.merge_display_path(&output_directory),
            output_directory.display().to_string()
        );
        Ok(())
    }

    #[test]
    fn move_to_display_uses_full_output_path_when_roots_differ() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let input_root = tmp.path().join("input");
        let output_root = tmp.path().join("output");
        let output_directory = output_root.join("Example");
        let input_file = input_root.join("Example.S01E01.mp4");
        std::fs::create_dir_all(&input_root)?;
        std::fs::create_dir_all(&output_directory)?;
        std::fs::write(&input_file, "video")?;

        let roots = Roots::new(vec![input_root], vec![output_root]);

        assert_eq!(
            roots.move_to_display_path(&output_directory, &[input_file]),
            output_directory.display().to_string()
        );
        Ok(())
    }

    #[test]
    fn move_to_display_stays_relative_when_target_is_in_input_root() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let input_root = tmp.path().join("input");
        let output_directory = input_root.join("Example");
        let input_file = input_root.join("Example.S01E01.mp4");
        std::fs::create_dir_all(&output_directory)?;
        std::fs::write(&input_file, "video")?;

        let roots = Roots::new(vec![input_root.clone()], vec![input_root]);

        assert_eq!(roots.move_to_display_path(&output_directory, &[input_file]), "Example");
        Ok(())
    }
}

#[cfg(test)]
mod test_first_output_normalized_name {
    use super::*;

    #[test]
    fn normalizes_the_first_output_root_name() {
        let roots = Roots::new(
            vec![PathBuf::from("input")],
            vec![PathBuf::from("/media/My.Shows"), PathBuf::from("/media/Other")],
        );
        assert_eq!(roots.first_output_normalized_name().as_deref(), Some("myshows"));
    }

    #[test]
    fn a_root_without_a_name_has_none() {
        let roots = Roots::new(vec![PathBuf::from("input")], vec![PathBuf::from("/")]);
        assert_eq!(roots.first_output_normalized_name(), None);
    }
}

#[cfg(test)]
mod test_output_depth {
    use super::*;

    #[test]
    fn depth_is_relative_to_the_output_root() {
        let roots = Roots::new(vec![PathBuf::from("/input")], vec![PathBuf::from("/media")]);

        assert_eq!(roots.output_depth(Path::new("/media/Show")), 1);
        assert_eq!(roots.output_depth(Path::new("/media/Show/Season 1")), 2);
    }

    #[test]
    fn the_deepest_relative_depth_wins_when_roots_overlap() {
        let roots = Roots::new(
            vec![PathBuf::from("/input")],
            vec![PathBuf::from("/media/shows"), PathBuf::from("/media")],
        );

        assert_eq!(roots.output_depth(Path::new("/media/shows/Show")), 2);
    }

    #[test]
    fn paths_outside_every_root_use_their_component_count() {
        let roots = Roots::new(vec![PathBuf::from("/input")], vec![PathBuf::from("/media")]);

        assert_eq!(roots.output_depth(Path::new("relative/Show")), 2);
    }
}

#[cfg(test)]
mod test_merge_display_path_with_several_inputs {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn each_input_directory_is_shown_relative_to_its_own_input_root() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let first_root = tmp.path().join("first");
        let second_root = tmp.path().join("second");
        let output_root = tmp.path().join("output");
        std::fs::create_dir_all(first_root.join("Alpha"))?;
        std::fs::create_dir_all(second_root.join("nested").join("Beta"))?;
        std::fs::create_dir_all(&output_root)?;

        let roots = Roots::new(vec![first_root.clone(), second_root.clone()], vec![output_root]);

        assert_eq!(roots.merge_display_path(&first_root.join("Alpha")), "Alpha");
        assert_eq!(
            roots.merge_display_path(&second_root.join("nested").join("Beta")),
            Path::new("nested").join("Beta").display().to_string()
        );
        Ok(())
    }
}

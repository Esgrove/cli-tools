//! Line selection for `slb`.
//!
//! Parses the `--lines` specs into the lines to process,
//! either as plain line ranges for a single file
//! or as `file:line` locations in the form the violation report prints,
//! and answers which lines of a given file a run should touch.

use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Result, bail};

use cli_tools::semantic_line_breaks::{FileKind, LineRanges};

/// One `--lines` value, as it was written on the command line.
///
/// Clap parses each value into one of these,
/// so the syntax of a value is checked while the arguments are,
/// and only the rules spanning several values are left to [`LineSelection`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineSpec {
    /// The file and the lines of each comma separated part of the value,
    /// with the file carried onto the parts that left it out.
    /// The paths are resolved later, against the files of the run.
    parts: Vec<(Option<String>, LineRanges)>,
}

/// The lines a run is limited to, as given on the command line.
///
/// Plain ranges and `file:line` locations are not mixed,
/// so exactly one of the two holds the selection.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LineSelection {
    /// Ranges given without a path, which need a single file to apply to.
    bare: LineRanges,
    /// Resolved path of each named file and the lines selected in it.
    per_file: Vec<(PathBuf, LineRanges)>,
}

impl LineSpec {
    /// The file and the lines of each part of the value, in the order they were written.
    pub fn parts(&self) -> impl Iterator<Item = (Option<&str>, &LineRanges)> {
        self.parts.iter().map(|(path, ranges)| (path.as_deref(), ranges))
    }
}

impl LineSelection {
    /// Build the selection from the `--lines` values.
    ///
    /// # Errors
    /// Returns an error if a value names a path that does not exist,
    /// or if plain ranges and paths are mixed.
    pub fn parse(specs: &[LineSpec]) -> Result<Self> {
        Self::from_specs(specs, true)
    }

    /// Build the selection for a run over stdin, where a path has nothing to name.
    ///
    /// # Errors
    /// Returns an error if a value names a path.
    pub fn for_stdin(specs: &[LineSpec]) -> Result<Self> {
        Self::from_specs(specs, false)
    }

    /// Collect the values, resolving the paths in them only when the run has files to match them against.
    fn from_specs(specs: &[LineSpec], allow_paths: bool) -> Result<Self> {
        let mut selection = Self::default();
        for spec in specs {
            for (path, ranges) in spec.parts() {
                match path {
                    Some(_) if !allow_paths => {
                        bail!("Only plain line ranges can be given with --stdin, since the text has no path");
                    }
                    Some(path) => selection.add_file(path, ranges.clone())?,
                    None => selection.bare = selection.bare.union(ranges),
                }
            }
        }
        if !selection.bare.is_empty() && !selection.per_file.is_empty() {
            bail!("Use either plain line ranges or 'file:line' locations in --lines, not both");
        }
        Ok(selection)
    }

    /// Whether no lines were selected, which means the whole of every file is processed.
    pub const fn is_empty(&self) -> bool {
        self.bare.is_empty() && self.per_file.is_empty()
    }

    /// The ranges given without a path.
    ///
    /// These apply to the single file of the run, and to the text read from stdin.
    pub const fn bare(&self) -> &LineRanges {
        &self.bare
    }

    /// The files named by the specs, empty when only plain ranges were given.
    pub fn paths(&self) -> Vec<PathBuf> {
        self.per_file.iter().map(|(path, _)| path.clone()).collect()
    }

    /// Whether the file has any selected lines and should be processed at all.
    pub fn includes_file(&self, file: &Path) -> bool {
        !self.bare.is_empty() || self.per_file.iter().any(|(path, _)| path == file)
    }

    /// The lines selected in the given file.
    pub fn ranges_for(&self, file: &Path) -> &LineRanges {
        self.per_file
            .iter()
            .find(|(path, _)| path == file)
            .map_or(&self.bare, |(_, ranges)| ranges)
    }

    /// Check the selection against the files the run resolved.
    ///
    /// # Errors
    /// Returns an error if a named file is not among them,
    /// or if plain ranges were given for more than one file,
    /// where the same line numbers would be applied to every one of them.
    pub fn validate(&self, files: &[(PathBuf, FileKind)]) -> Result<()> {
        for (path, _) in &self.per_file {
            if !files.iter().any(|(file, _)| file == path) {
                bail!(
                    "The file selected with --lines is not among the files to process: '{}'",
                    cli_tools::path_to_string_relative(path)
                );
            }
        }
        if !self.bare.is_empty() && files.len() > 1 {
            bail!(
                "Plain line ranges in --lines need a single file, but {} files were found. \
                 Use 'file:line' to select lines in more than one file",
                files.len()
            );
        }
        Ok(())
    }

    /// Add the ranges for one named file, merging them with the ranges it already has.
    fn add_file(&mut self, path: &str, ranges: LineRanges) -> Result<()> {
        let resolved = cli_tools::resolve_input_path(Some(Path::new(path)))?;
        match self.per_file.iter_mut().find(|(existing, _)| *existing == resolved) {
            Some((_, existing)) => *existing = existing.union(&ranges),
            None => self.per_file.push((resolved, ranges)),
        }
        Ok(())
    }
}

impl FromStr for LineSpec {
    type Err = String;

    /// Parse one `--lines` value.
    ///
    /// A part is either a plain line or range such as "10" or "10-25",
    /// or a path followed by a line, such as "src/main.rs:14" or "src/main.rs:14-20".
    /// A column after the line is accepted and ignored,
    /// so a location copied straight out of the violation report can be pasted back in.
    ///
    /// Commas separate the parts, and a part without a path belongs to the file the part before it named,
    /// so "src/main.rs:14,20" selects two lines of one file and the path only has to be written once.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let mut parts = Vec::new();
        let mut named_file: Option<String> = None;
        for part in text.split(',') {
            let (path, ranges) = split_spec(part)?;
            if let Some(path) = path {
                named_file = Some(path.to_string());
            }
            parts.push((named_file.clone(), ranges));
        }
        Ok(Self { parts })
    }
}

/// Split one spec into its optional path and its line ranges.
///
/// The line numbers are read from the end, one colon separated segment at a time,
/// so a Windows path with a drive letter is not mistaken for a line
/// and a column after the line, as the violation report prints it, is dropped.
/// The leftmost segment that holds line numbers is the line, and whatever precedes it is the path.
fn split_spec(spec: &str) -> Result<(Option<&str>, LineRanges), String> {
    let spec = spec.trim().trim_end_matches(':');
    if spec.is_empty() {
        return Err("Empty line selection".to_string());
    }
    let mut head = spec;
    let mut ranges: Option<LineRanges> = None;
    while let Some((before, segment)) = head.rsplit_once(':') {
        let Ok(parsed) = segment.parse::<LineRanges>() else {
            break;
        };
        ranges = Some(parsed);
        head = before;
    }
    if head.is_empty() {
        return ranges.map_or_else(|| Err("Empty line selection".to_string()), |ranges| Ok((None, ranges)));
    }
    // A head that holds line numbers itself means every segment did,
    // so the spec is a plain range with no path and the head is the line.
    match head.parse::<LineRanges>() {
        Ok(parsed) => Ok((None, parsed)),
        Err(error) => ranges.map_or_else(
            || Err(format!("{error}. Expected a line such as '14', a range such as '10-25', or a location such as 'src/main.rs:14'")),
            |ranges| Ok((Some(head), ranges)),
        ),
    }
}

#[cfg(test)]
mod test_split_spec {
    use super::*;

    fn pairs(ranges: &LineRanges) -> Vec<(usize, usize)> {
        ranges.iter().map(|range| (*range.start(), *range.end())).collect()
    }

    fn split(spec: &str) -> (Option<String>, Vec<(usize, usize)>) {
        let (path, ranges) = split_spec(spec).expect("the spec should parse");
        (path.map(str::to_string), pairs(&ranges))
    }

    #[test]
    fn a_plain_line_has_no_path() {
        assert_eq!(split("14"), (None, vec![(14, 14)]));
    }

    #[test]
    fn a_plain_range_has_no_path() {
        assert_eq!(split("10-25"), (None, vec![(10, 25)]));
    }

    #[test]
    fn a_path_is_split_from_its_line() {
        assert_eq!(
            split("src/main.rs:14"),
            (Some("src/main.rs".to_string()), vec![(14, 14)])
        );
    }

    #[test]
    fn a_path_can_carry_a_range() {
        assert_eq!(
            split("src/main.rs:14-20"),
            (Some("src/main.rs".to_string()), vec![(14, 20)])
        );
    }

    #[test]
    fn a_column_after_the_line_is_dropped() {
        assert_eq!(
            split("client/GameMain.cpp:1414:33"),
            (Some("client/GameMain.cpp".to_string()), vec![(1414, 1414)])
        );
    }

    #[test]
    fn a_trailing_colon_is_tolerated() {
        assert_eq!(
            split("src/main.rs:14:"),
            (Some("src/main.rs".to_string()), vec![(14, 14)])
        );
    }

    #[test]
    fn a_windows_path_keeps_its_drive_letter() {
        assert_eq!(
            split(r"C:\src\main.rs:14"),
            (Some(r"C:\src\main.rs".to_string()), vec![(14, 14)])
        );
    }

    #[test]
    fn a_line_and_column_without_a_path_take_the_line() {
        assert_eq!(split("14:33"), (None, vec![(14, 14)]));
    }

    #[test]
    fn a_path_without_a_line_is_rejected() {
        assert!(split_spec("src/main.rs").is_err());
    }

    #[test]
    fn an_empty_spec_is_rejected() {
        assert!(split_spec("").is_err());
        assert!(split_spec("  ").is_err());
        assert!(split_spec(":").is_err());
    }

    #[test]
    fn a_line_number_that_cannot_be_parsed_is_rejected() {
        assert!(split_spec("0").is_err());
        assert!(split_spec("25-10").is_err());
    }
}

#[cfg(test)]
mod test_line_selection {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    /// A temporary directory holding two files, with their resolved paths.
    fn two_files() -> (TempDir, PathBuf, PathBuf) {
        let directory = tempfile::Builder::new()
            .prefix("slb_selection")
            .tempdir()
            .expect("temporary directory should be created");
        let mut paths = Vec::new();
        for name in ["first.rs", "second.rs"] {
            let path = directory.path().join(name);
            fs::write(&path, "/// Text.\n").expect("file should be written");
            paths.push(cli_tools::resolve_input_path(Some(&path)).expect("path should resolve"));
        }
        let second = paths.pop().expect("two paths");
        let first = paths.pop().expect("two paths");
        (directory, first, second)
    }

    fn specs(values: &[&str]) -> Vec<LineSpec> {
        values
            .iter()
            .map(|value| value.parse().expect("the value should parse"))
            .collect()
    }

    #[test]
    fn no_specs_select_nothing() {
        let selection = LineSelection::parse(&[]).expect("an empty selection should parse");
        assert!(selection.is_empty());
        assert!(selection.paths().is_empty());
        assert!(selection.bare().is_empty());
    }

    #[test]
    fn plain_ranges_are_collected_without_a_path() {
        let selection = LineSelection::parse(&specs(&["10-25", "40"])).expect("the selection should parse");
        assert!(selection.paths().is_empty());
        assert!(selection.bare().contains_line(10));
        assert!(selection.bare().contains_line(40));
        assert!(!selection.bare().contains_line(30));
    }

    #[test]
    fn a_named_file_gets_its_own_ranges() {
        let (_directory, first, second) = two_files();
        let specs = specs(&[&format!("{}:5", first.display()), &format!("{}:9", second.display())]);
        let selection = LineSelection::parse(&specs).expect("the selection should parse");

        assert_eq!(selection.paths(), vec![first.clone(), second.clone()]);
        assert!(selection.ranges_for(&first).contains_line(5));
        assert!(!selection.ranges_for(&first).contains_line(9));
        assert!(selection.ranges_for(&second).contains_line(9));
        assert!(selection.includes_file(&first));
    }

    #[test]
    fn a_range_after_a_location_belongs_to_the_same_file() {
        let (_directory, first, second) = two_files();
        let specs = specs(&[&format!("{}:5,9", first.display())]);
        let selection = LineSelection::parse(&specs).expect("the selection should parse");

        assert_eq!(selection.paths(), vec![first.clone()]);
        assert!(selection.ranges_for(&first).contains_line(5));
        assert!(selection.ranges_for(&first).contains_line(9));
        assert!(!selection.includes_file(&second));
        assert!(selection.bare().is_empty());
    }

    #[test]
    fn a_location_can_carry_several_ranges() {
        let (_directory, first, _second) = two_files();
        let specs = specs(&[&format!("{}:1-2,7,20-25", first.display())]);
        let selection = LineSelection::parse(&specs).expect("the selection should parse");

        let ranges = selection.ranges_for(&first);
        assert!(ranges.contains_line(2));
        assert!(ranges.contains_line(7));
        assert!(ranges.contains_line(22));
        assert!(!ranges.contains_line(5));
    }

    #[test]
    fn one_value_can_name_two_files() {
        let (_directory, first, second) = two_files();
        let specs = specs(&[&format!("{}:5,{}:9", first.display(), second.display())]);
        let selection = LineSelection::parse(&specs).expect("the selection should parse");

        assert!(selection.ranges_for(&first).contains_line(5));
        assert!(!selection.ranges_for(&first).contains_line(9));
        assert!(selection.ranges_for(&second).contains_line(9));
    }

    #[test]
    fn a_range_before_any_location_is_still_a_plain_range() {
        let (_directory, first, _second) = two_files();
        let specs = specs(&[&format!("5,{}:9", first.display())]);
        assert!(LineSelection::parse(&specs).is_err());
    }

    #[test]
    fn a_value_that_cannot_be_parsed_is_refused_before_the_selection_is_built() {
        assert!("abc".parse::<LineSpec>().is_err());
        assert!("0".parse::<LineSpec>().is_err());
        assert!("25-10".parse::<LineSpec>().is_err());
        assert!("src/main.rs".parse::<LineSpec>().is_err());
    }

    #[test]
    fn several_specs_for_one_file_are_merged() {
        let (_directory, first, _second) = two_files();
        let specs = specs(&[&format!("{}:5", first.display()), &format!("{}:20-25", first.display())]);
        let selection = LineSelection::parse(&specs).expect("the selection should parse");

        assert_eq!(selection.paths(), vec![first.clone()]);
        assert!(selection.ranges_for(&first).contains_line(5));
        assert!(selection.ranges_for(&first).contains_line(22));
    }

    #[test]
    fn a_file_without_ranges_is_not_included() {
        let (_directory, first, second) = two_files();
        let specs = specs(&[&format!("{}:5", first.display())]);
        let selection = LineSelection::parse(&specs).expect("the selection should parse");

        assert!(selection.includes_file(&first));
        assert!(!selection.includes_file(&second));
    }

    #[test]
    fn plain_ranges_apply_to_whichever_file_is_processed() {
        let (_directory, first, second) = two_files();
        let selection = LineSelection::parse(&specs(&["5"])).expect("the selection should parse");

        assert!(selection.includes_file(&first));
        assert!(selection.includes_file(&second));
        assert!(selection.ranges_for(&second).contains_line(5));
    }

    #[test]
    fn mixing_plain_ranges_and_paths_is_rejected() {
        let (_directory, first, _second) = two_files();
        let specs = specs(&["5", &format!("{}:9", first.display())]);
        assert!(LineSelection::parse(&specs).is_err());
    }

    #[test]
    fn a_path_that_does_not_exist_is_rejected() {
        assert!(LineSelection::parse(&specs(&["no_such_file.rs:9"])).is_err());
    }

    #[test]
    fn a_named_file_has_to_be_among_the_files_to_process() {
        let (_directory, first, second) = two_files();
        let specs = specs(&[&format!("{}:5", first.display())]);
        let selection = LineSelection::parse(&specs).expect("the selection should parse");

        assert!(selection.validate(&[(first, FileKind::Rust)]).is_ok());
        assert!(selection.validate(&[(second, FileKind::Rust)]).is_err());
        assert!(selection.validate(&[]).is_err());
    }

    #[test]
    fn plain_ranges_need_a_single_file() {
        let (_directory, first, second) = two_files();
        let selection = LineSelection::parse(&specs(&["5"])).expect("the selection should parse");

        assert!(selection.validate(&[(first.clone(), FileKind::Rust)]).is_ok());
        assert!(
            selection
                .validate(&[(first, FileKind::Rust), (second, FileKind::Rust)])
                .is_err()
        );
    }
}

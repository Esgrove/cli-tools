//! Markdown hard break markers and the paragraph built from a run of prose lines.

/// Intentional hard line break marker at the end of a Markdown line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HardBreak {
    /// No hard break.
    #[default]
    None,
    /// Two or more trailing spaces.
    Spaces,
    /// A trailing backslash.
    Backslash,
}

/// A region of a text buffer, either copied verbatim or reflowed as a prose paragraph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Region {
    /// Lines copied without changes, given as a zero based half open line index range.
    Verbatim {
        /// First line index.
        start: usize,
        /// One past the last line index.
        end: usize,
    },
    /// A paragraph of prose that may be reflowed.
    Paragraph(Paragraph),
}

/// A run of prose lines sharing one prefix.
///
/// The lines own their text although they are always cut from the text buffer.
/// Borrowing them would save one allocation per prose line,
/// at the price of a lifetime running through four more modules, their tests, and the benchmarks.
/// A check run never builds the reflowed text at all, so the lines are the only text it allocates,
/// and the split is not what the profile is spent on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paragraph {
    /// Zero based index of the first line in the text buffer.
    pub start_line: usize,
    /// One past the zero based index of the last line.
    pub end_line: usize,
    /// Prefix of the first line, for example `    /// - `.
    pub first_prefix: String,
    /// Prefix of every following line, for example `    ///   `.
    pub rest_prefix: String,
    /// Suffix appended to the last line, for example closing docstring quotes.
    pub last_suffix: String,
    /// Whether the paragraph holds the content of a list item that carries on under its own marker.
    ///
    /// An item continued by a lazy line, which is one that does not line up under the marker, is left out,
    /// since a line the author did not indent may well be meant as a note of its own.
    pub list_item: bool,
    /// Content lines with prefixes stripped and trailing whitespace removed.
    pub lines: Vec<String>,
    /// Hard break marker of each content line.
    pub hard_breaks: Vec<HardBreak>,
}

impl HardBreak {
    /// Marker text appended to the end of a line.
    #[must_use]
    pub const fn marker(self) -> &'static str {
        match self {
            Self::None => "",
            Self::Spaces => "  ",
            Self::Backslash => "\\",
        }
    }
}

impl Paragraph {
    /// Prefix used for the given content line index.
    #[must_use]
    pub fn prefix_for(&self, line_index: usize) -> &str {
        if line_index == 0 {
            &self.first_prefix
        } else {
            &self.rest_prefix
        }
    }
}

#[cfg(test)]
mod test_hard_break {
    use super::*;

    #[test]
    fn hard_break_markers_round_trip() {
        assert_eq!(HardBreak::None.marker(), "");
        assert_eq!(HardBreak::Spaces.marker(), "  ");
        assert_eq!(HardBreak::Backslash.marker(), "\\");
        assert_eq!(HardBreak::default(), HardBreak::None);
    }
}

#[cfg(test)]
mod test_paragraph {
    use super::*;

    #[test]
    fn the_first_line_uses_the_first_prefix() {
        let paragraph = Paragraph {
            start_line: 0,
            end_line: 2,
            first_prefix: "/// - ".to_string(),
            rest_prefix: "///   ".to_string(),
            last_suffix: String::new(),
            list_item: true,
            lines: vec!["one".to_string(), "two".to_string()],
            hard_breaks: vec![HardBreak::None; 2],
        };
        assert_eq!(paragraph.prefix_for(0), "/// - ");
        assert_eq!(paragraph.prefix_for(1), "///   ");
        assert_eq!(paragraph.prefix_for(9), "///   ");
    }
}

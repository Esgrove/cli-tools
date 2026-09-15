//! Rewrite rules for dashes and colons.
//!
//! Each item below is its own paragraph, so the cases do not reflow into one another.

/// The promotion lives in form defaults, so it does NOT mark the form dirty. Save stays disabled until an edit.
fn sentence_extension() {}

/// The value, a plain integer, is read from the header of every record in the archive.
fn parenthetical_pair() {}

/// "PhotoLabs": lowercase s after the name, which is not a word boundary in the matcher.
fn quoted_label() {}

/// `primaryValue`: the form renders the right target for the verification that is being edited.
fn code_label() {}

/// It uses a semicolon, and a dash, both in the same sentence, which keeps the clause together.
fn clause_word_after_the_dash() {}

/// The parser is fast, really.
fn single_trailing_word() {}

/// Pages 1–5 and the --flag argument are left alone, since neither is an em dash in prose.
fn ranges_and_flags() {}

/// — A dash at the start of the text cannot be rewritten automatically.
fn dash_at_the_edge() {}

/// Skip objects without a hierarchyPath:
/// they share the same other fields as a sibling but have no way to disambiguate themselves,
/// so they are just ambiguous.
fn colon_introduction() {}

/// Note: a one word label stays on the line it labels, even when the sentence runs past the limit by a lot.
fn colon_label() {}

/// Runtime safety net: the executor performs the same promotion, so legacy rows keep running unchanged.
fn colon_with_two_words() {}

/// See https://example.com/a:b and the module::item path, where a colon is part of the text it sits in.
fn colon_inside_a_word() {}

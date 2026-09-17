//! Regex literal detection for the semantic line breaks formatter.
//!
//! Decides whether a slash in JavaScript or Ruby code opens a regex literal or divides,
//! which the scanner needs so a slash inside a regex is not read as a comment marker.
//! The decision is made from the code before the slash,
//! and the scanner is told the line is uncertain when neither reading can be ruled out.

use super::scanner::{Backtick, Cursor, NormalStep, RE_BLOCK_SCALAR, ScanState, rest_starts_with};
use super::string_syntax::StringSyntax;

/// Preserve subsequent source when an unsupported regex may hide a multiline construct.
pub(super) fn uncertain_regex(
    rest: &str,
    line_length: usize,
    cursor: &mut Cursor,
    syntax: &StringSyntax,
) -> NormalStep {
    if rest.contains('`') && syntax.backtick != Backtick::None
        || syntax.block_comment.is_some_and(|(open, _)| rest.contains(open))
        || rest.ends_with('\\')
        || syntax.multiline_strings && rest.contains('"')
        || syntax.triple_quotes && (rest.contains("\"\"\"") || rest.contains("'''"))
        || syntax.heredoc && (rest.contains("<<") || cursor.pending_heredoc.is_some())
        || syntax.block_scalars && RE_BLOCK_SCALAR.is_match(rest)
    {
        cursor.index = line_length;
        cursor.uncertain = true;
        NormalStep::Continue(ScanState::Uncertain)
    } else {
        NormalStep::Uncertain
    }
}

/// Recognize definite operands before division so later strings and block comments are still tracked.
///
/// Contextual keywords can introduce regex expressions and must not be treated as operands here.
pub(super) fn division_can_start(prefix: &str) -> bool {
    let prefix = prefix.trim_end();
    let Some(previous) = prefix.chars().next_back() else {
        return false;
    };
    if matches!(previous, '\'' | '"' | '`' | ']') || prefix.ends_with("++") || prefix.ends_with("--") {
        return true;
    }
    if !previous.is_alphanumeric() && previous != '_' && previous != '$' {
        return false;
    }
    let word = prefix
        .rsplit(|character: char| !character.is_alphanumeric() && character != '_' && character != '$')
        .next()
        .unwrap_or_default();
    !matches!(
        word,
        "return"
            | "throw"
            | "case"
            | "delete"
            | "void"
            | "typeof"
            | "else"
            | "do"
            | "in"
            | "instanceof"
            | "yield"
            | "await"
            | "of"
            | "new"
            | "extends"
            | "default"
    )
}

/// Recognize contexts that require an expression rather than a division operator.
///
/// Closing parentheses and braces, line starts, and postfix operators are ambiguous.
/// Leaving those lines alone is safer than interpreting regex contents as comments or strings.
pub(super) fn regex_can_start(prefix: &str) -> bool {
    let prefix = prefix.trim_end();
    let Some(previous) = prefix.chars().next_back() else {
        return false;
    };
    match previous {
        '=' | '(' | '[' | '{' | ',' | ':' | ';' | '?' | '&' | '|' | '~' | '%' | '*' => true,
        '+' | '-' => !prefix
            .strip_suffix(previous)
            .is_some_and(|rest| rest.ends_with(previous)),
        '>' => prefix.ends_with("=>"),
        _ => {
            let word_start = prefix
                .rfind(|character: char| !character.is_alphanumeric() && character != '_' && character != '$')
                .map_or(0, |index| index + 1);
            let word = prefix.get(word_start..).unwrap_or_default();
            matches!(
                word,
                "return"
                    | "throw"
                    | "case"
                    | "delete"
                    | "void"
                    | "typeof"
                    | "else"
                    | "do"
                    | "new"
                    | "extends"
                    | "default"
            ) && !prefix.get(..word_start).unwrap_or_default().trim_end().ends_with('.')
        }
    }
}

/// Find the end of a same-line slash-delimited regex, including its flags.
///
/// Escapes and character classes hide slash characters.
/// Unescaped quotes can start strings after division or within paths, even inside a presumed regex class.
/// Nested classes may use Unicode set syntax, so leave them unchanged rather than guess at their boundaries.
pub(super) fn find_regex_end(chars: &[(usize, char)], mut index: usize) -> Option<usize> {
    let mut in_class = false;
    while let Some(&(_, character)) = chars.get(index) {
        match character {
            '\n' | '\r' | '\u{2028}' | '\u{2029}' | '\'' | '"' | '`' => return None,
            '\\' => {
                let &(_, escaped) = chars.get(index + 1)?;
                if matches!(escaped, '\n' | '\r' | '\u{2028}' | '\u{2029}') {
                    return None;
                }
                index += 2;
                continue;
            }
            '[' if in_class => return None,
            '[' => in_class = true,
            ']' => in_class = false,
            '/' if !in_class => {
                index += 1;
                while chars.get(index).is_some_and(|(_, flag)| flag.is_ascii_alphabetic()) {
                    index += 1;
                }
                return Some(index);
            }
            _ => {}
        }
        index += 1;
    }
    None
}

/// Contextual words may be ordinary identifiers before division.
/// Do not consume a comment opener as a regex delimiter, but allow comments attached after a complete regex.
/// A statement separator makes an attached comment ambiguous, so the caller must leave that source unchanged.
pub(super) fn regex_end_overlaps_comment(
    chars: &[(usize, char)],
    end: usize,
    candidate: &str,
    syntax: &StringSyntax,
) -> bool {
    end.checked_sub(1)
        .is_some_and(|index| comment_starts_at(chars, index, syntax))
        && (candidate.contains(';') || !comment_starts_at(chars, end, syntax))
}

/// Whether a line or block comment opener begins at a character index.
pub(super) fn comment_starts_at(chars: &[(usize, char)], index: usize, syntax: &StringSyntax) -> bool {
    rest_starts_with(chars, index, syntax.line_marker)
        || syntax
            .block_comment
            .is_some_and(|(open, _)| rest_starts_with(chars, index, open))
}

#[cfg(test)]
mod test_regex_literals {
    use crate::semantic_line_breaks::comments::test_helpers::*;

    use super::super::file_kind::FileKind;
    use super::super::scanner::{ScanState, scan_line};
    use super::super::string_syntax::string_syntax;

    #[test]
    fn contextual_identifiers_preserve_real_trailing_comments() {
        for kind in [
            FileKind::Rust,
            FileKind::Go,
            FileKind::CLike,
            FileKind::JavaScript,
            FileKind::Python,
            FileKind::Shell,
            FileKind::Toml,
            FileKind::Yaml,
        ] {
            let syntax = string_syntax(kind).expect("source syntax");
            let marker = syntax.line_marker;
            let suffixes: &[&str] = if marker == "//" { &["", "/", "//"] } else { &[""] };
            for identifier in ["new", "default", "typeof", "void", "delete", "do", "extends", "case"] {
                let code = format!("let value = {identifier} / divisor;");
                for suffix in suffixes {
                    let comment = format!("{marker}{suffix} note");
                    let line = format!("{code} {comment}");
                    let result = scan_line(&line, ScanState::Normal, &syntax);
                    if suffix.is_empty() {
                        assert_eq!(
                            replacement(&line, kind),
                            Some(pair(&comment, &code)),
                            "{kind:?}: {line}"
                        );
                        assert_eq!(result.comment_start, Some(code.len() + 1), "{kind:?}: {line}");
                        assert!(!result.uncertain, "{kind:?}: {line}");
                    } else {
                        assert_eq!(replacement(&line, kind), None, "{kind:?}: {line}");
                        assert_eq!(result.comment_start, None, "{kind:?}: {line}");
                        assert!(result.uncertain, "{kind:?}: {line}");
                    }
                    assert_eq!(result.state, ScanState::Normal, "{kind:?}: {line}");
                }
            }
        }
    }

    #[test]
    fn contextual_semicolon_regexes_leave_ambiguous_attached_comments_unchanged() {
        for kind in [FileKind::Rust, FileKind::Go, FileKind::CLike, FileKind::JavaScript] {
            let syntax = string_syntax(kind).expect("source syntax");
            for prefix in ["return", "new", "throw", "export default"] {
                for comment in ["// comment", "/// comment"] {
                    let line = format!("{prefix} /foo;/{comment}");
                    assert_eq!(replacement(&line, kind), None, "{kind:?}: {line}");
                    let result = scan_line(&line, ScanState::Normal, &syntax);
                    assert!(result.uncertain, "{kind:?}: {line}");
                    assert_eq!(result.comment_start, None, "{kind:?}: {line}");
                    assert_eq!(result.state, ScanState::Normal, "{kind:?}: {line}");
                    assert_eq!(
                        replaced_indices(&[&line, "let next = 1; // note"], kind),
                        vec![1],
                        "{kind:?}: {line}"
                    );
                }
            }
        }
    }

    #[test]
    fn contextual_identifiers_preserve_real_block_comment_state() {
        for kind in [FileKind::Rust, FileKind::Go, FileKind::CLike, FileKind::JavaScript] {
            let syntax = string_syntax(kind).expect("source syntax");
            for identifier in ["new", "default", "typeof", "void", "delete", "do", "extends", "case"] {
                let opening = format!("let value = {identifier} / divisor; /* block");
                let result = scan_line(&opening, ScanState::Normal, &syntax);
                assert_eq!(result.state, ScanState::InBlockComment(1), "{kind:?}: {opening}");
                assert!(!result.uncertain, "{kind:?}: {opening}");
                let lines = [
                    opening.as_str(),
                    "code // literal; not prose",
                    "code /// literal; not prose",
                    "code //// literal; not prose",
                    "*/ let next = 1; // note",
                ];
                assert_eq!(replaced_indices(&lines, kind), vec![4], "{kind:?}: {opening}");
            }
        }
    }

    #[test]
    fn contextual_regexes_preserve_attached_comments_across_syntaxes() {
        for kind in [
            FileKind::Rust,
            FileKind::Go,
            FileKind::CLike,
            FileKind::JavaScript,
            FileKind::Python,
            FileKind::Shell,
            FileKind::Toml,
            FileKind::Yaml,
        ] {
            let syntax = string_syntax(kind).expect("source syntax");
            for prefix in ["return", "new", "throw", "export default"] {
                for pattern in [r"/foo/", r"/path\//", r"/[/*]/", r"/path\//g"] {
                    let code = format!("{prefix} {pattern}");
                    let comment = format!("{} note", syntax.line_marker);
                    assert_eq!(replacement(&code, kind), None, "{kind:?}: {code}");
                    assert_eq!(
                        replacement(&format!("{code} {comment}"), kind),
                        Some(pair(&comment, &code)),
                        "{kind:?}: {code}"
                    );
                    if syntax.line_marker == "//" {
                        assert_eq!(
                            replacement(&format!("{code}{comment}"), kind),
                            Some(pair(&comment, &code)),
                            "{kind:?}: {code}"
                        );
                        let opening = format!("{code}/* block");
                        assert_eq!(
                            scan_line(&opening, ScanState::Normal, &syntax).state,
                            ScanState::InBlockComment(1),
                            "{kind:?}: {opening}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn uncertain_slashes_protect_heredocs_and_block_scalars() {
        for (kind, opening, interior, closing) in [
            (
                FileKind::Shell,
                "DIR=/root cat <<EOF /dev/stdin",
                "literal # not a comment",
                "EOF",
            ),
            (
                FileKind::Shell,
                "DIR=/\"root\" cat <<'EOF'",
                "literal # not a comment",
                "EOF",
            ),
            (
                FileKind::Shell,
                "cat <<'EOF' DIR=/\"root\"",
                "literal # not a comment",
                "EOF",
            ),
            (
                FileKind::Yaml,
                "/path\"key\": |",
                "  literal # not a comment",
                "next: value",
            ),
        ] {
            let syntax = string_syntax(kind).expect("source syntax");
            let lines = [opening, interior, closing];
            assert_eq!(
                scan_line(opening, ScanState::Normal, &syntax).state,
                ScanState::Uncertain,
                "{opening}"
            );
            assert_eq!(lines_inside_strings(&lines, kind), vec![false, true, true], "{opening}");
            assert!(replaced_indices(&lines, kind).is_empty(), "{opening}");
        }
        let syntax = string_syntax(FileKind::Yaml).expect("YAML syntax");
        let lines = ["foo/\"bar\": |", "  literal # not a comment"];
        assert_eq!(
            scan_line(lines[0], ScanState::Normal, &syntax).state,
            ScanState::InBlockScalar(0)
        );
        assert!(replaced_indices(&lines, FileKind::Yaml).is_empty());
    }

    #[test]
    fn quoted_slashes_after_division_and_in_paths_are_not_regex_boundaries() {
        for (kind, code) in [
            (FileKind::Python, r#"value = default / len("/# not a comment")"#),
            (FileKind::Python, r#"value = default / len(["]/# not a comment"])"#),
            (FileKind::Shell, r#"path=/"foo/bar # literal""#),
            (FileKind::Shell, r#"path=/["]foo/bar # literal""#),
        ] {
            assert_eq!(replacement(code, kind), None, "{code}");
            assert_eq!(replaced_indices(&[code, "next = 1 # c"], kind), vec![1], "{code}");
        }
        for kind in [
            FileKind::Python,
            FileKind::Rust,
            FileKind::CLike,
            FileKind::Go,
            FileKind::Shell,
        ] {
            let marker = string_syntax(kind).expect("source syntax").line_marker;
            for identifier in ["default", "typeof", "void", "delete", "do", "new", "extends", "case"] {
                for argument in [
                    format!("\"/{marker} not a comment\""),
                    format!("[\"]/{marker} not a comment\"]"),
                ] {
                    let code = format!("value = {identifier} / len({argument})");
                    assert_eq!(replacement(&code, kind), None, "{kind:?}: {code}");
                }
            }
        }
    }

    #[test]
    fn regexes_with_unescaped_quotes_are_left_unchanged() {
        for kind in [
            FileKind::JavaScript,
            FileKind::CLike,
            FileKind::Rust,
            FileKind::Go,
            FileKind::Python,
            FileKind::Shell,
            FileKind::Toml,
            FileKind::Yaml,
        ] {
            let marker = string_syntax(kind).expect("source syntax").line_marker;
            for code in [
                r#"pattern = /foo"bar/;"#,
                r"pattern = /foo'bar/;",
                r"pattern = /foo`bar/;",
                r#"pattern = /"'`\/\/*/dgimsuy;"#,
                r#"pattern = /["'`/]/;"#,
                r#"pattern = /[#"'/]/;"#,
                r#"pattern = /[/*"'`]/;"#,
            ] {
                assert_eq!(replacement(code, kind), None, "{kind:?}: {code}");
                assert_eq!(
                    replacement(&format!("{code} {marker} c"), kind),
                    None,
                    "{kind:?}: {code}"
                );
            }
        }
    }

    #[test]
    fn swift_bare_regex_and_hash_comment_syntax_are_protected() {
        let swift = FileKind::from_extension("swift").expect("Swift is supported");
        assert_eq!(swift, FileKind::CLike);
        let code = r"let pattern = /node_modules\/(react|react-dom)\//";
        assert_eq!(replacement(code, swift), None);
        assert_eq!(replacement(&format!("{code} // c"), swift), Some(pair("// c", code)));
        for kind in [FileKind::Python, FileKind::Shell, FileKind::Toml, FileKind::Yaml] {
            let code = r"pattern = /[ #/]/";
            assert_eq!(replacement(code, kind), None, "{kind:?}");
            assert_eq!(
                replacement(&format!("{code} # c"), kind),
                Some(pair("# c", code)),
                "{kind:?}"
            );
        }
    }

    #[test]
    fn ordinary_division_preserves_trailing_comments_across_syntaxes() {
        for kind in [
            FileKind::JavaScript,
            FileKind::CLike,
            FileKind::Rust,
            FileKind::Go,
            FileKind::Python,
            FileKind::Shell,
            FileKind::Toml,
            FileKind::Yaml,
        ] {
            let marker = string_syntax(kind).expect("source syntax").line_marker;
            let comment = format!("{marker} c");
            for code in [
                "value = numerator / denominator",
                "value = numerator / denominator / divisor",
                "value = total() / count",
                "value = total /= count",
                "value = count++ / denominator",
                "value = count! / denominator",
                "value = generic<Type> / denominator",
            ] {
                assert_eq!(
                    replacement(&format!("{code} {comment}"), kind),
                    Some(pair(&comment, code)),
                    "{kind:?}: {code}"
                );
            }
        }
    }

    #[test]
    fn floor_division_paths_and_urls_are_not_regex_literals() {
        for code in [
            "value = 12 // 4",
            "value = total() // count",
            "value //= count",
            "value = total / count",
        ] {
            assert_eq!(
                replacement(&format!("{code} # c"), FileKind::Python),
                Some(pair("# c", code))
            );
        }
        for code in [
            "/usr/bin/true",
            "/bin/echo 'hello'",
            "cp /source /destination",
            "path=/root",
            "path=/usr/bin",
        ] {
            assert_eq!(
                replacement(&format!("{code} # c"), FileKind::Shell),
                Some(pair("# c", code)),
                "{code}"
            );
        }
        for kind in [
            FileKind::Rust,
            FileKind::Go,
            FileKind::CLike,
            FileKind::Shell,
            FileKind::Yaml,
        ] {
            let code = "url = https://example.com/path";
            let marker = string_syntax(kind).expect("source syntax").line_marker;
            let comment = format!("{marker} c");
            assert_eq!(
                replacement(&format!("{code} {comment}"), kind),
                Some(pair(&comment, code)),
                "{kind:?}"
            );
        }
    }

    #[test]
    fn division_preserves_multiline_strings_across_syntaxes() {
        for (kind, opening, closing) in [
            (FileKind::Rust, "let value = total / count + \"", "\";"),
            (FileKind::Go, "value := total / count + `", "`"),
            (FileKind::Python, "value = total // count + \"\"\"", "\"\"\""),
            (FileKind::Shell, "cat /root <<'EOF'", "EOF"),
        ] {
            let marker = string_syntax(kind).expect("source syntax").line_marker;
            let interior = format!("{marker} literal; text must stay unchanged");
            let following = format!("next = 1 {marker} c");
            let lines = [opening, interior.as_str(), closing, following.as_str()];
            assert_eq!(
                lines_inside_strings(&lines, kind),
                vec![false, true, true, false],
                "{kind:?}"
            );
            assert_eq!(replaced_indices(&lines, kind), vec![3], "{kind:?}");
        }
    }

    #[test]
    fn unsupported_regexes_protect_following_multiline_source_across_syntaxes() {
        for (kind, delimiter) in [
            (FileKind::Rust, "\""),
            (FileKind::Go, "`"),
            (FileKind::CLike, "\"\"\""),
            (FileKind::Python, "\"\"\""),
            (FileKind::Toml, "\"\"\""),
        ] {
            let syntax = string_syntax(kind).expect("source syntax");
            let opening = format!("value = /[[a]--[/]]/ + {delimiter}");
            let interior = format!("{} literal; not prose", syntax.line_marker);
            let lines = [opening.as_str(), interior.as_str(), delimiter];
            assert_eq!(
                scan_line(&opening, ScanState::Normal, &syntax).state,
                ScanState::Uncertain
            );
            assert_eq!(lines_inside_strings(&lines, kind), vec![false, true, true], "{kind:?}");
            assert!(replaced_indices(&lines, kind).is_empty(), "{kind:?}");
        }
    }

    #[test]
    fn regex_literals_hide_comment_markers_and_escaped_delimiters() {
        for code in [
            r"test: /node_modules\/(react|react-dom)\//,",
            r"const expression = /a\/b/;",
            r"const expression = /a\/\/b/;",
            r"const expression = /a\\\//g;",
            r"const expression = /a\\/g;",
            r"const expression = /[/]/;",
            r"const expression = /[/*]/;",
            r"const expression = /[//]/;",
            r"const expression = /[\]\/]/;",
            r"const expression = /=/;",
            r"const expressions = [/a\//, /b\//];",
            r"const expression = /😀\//u;",
        ] {
            for kind in [
                FileKind::JavaScript,
                FileKind::CLike,
                FileKind::Rust,
                FileKind::Go,
                FileKind::Python,
                FileKind::Shell,
                FileKind::Toml,
                FileKind::Yaml,
            ] {
                let marker = string_syntax(kind).expect("source syntax").line_marker;
                let comment = format!("{marker} real comment");
                assert_eq!(replacement(code, kind), None, "{kind:?}: {code}");
                assert_eq!(
                    replacement(&format!("{code} {comment}"), kind),
                    Some(pair(&comment, code)),
                    "{kind:?}: {code}"
                );
            }
        }
    }

    #[test]
    fn expression_contexts_allow_regex_literals() {
        for prefix in [
            "return ",
            "throw ",
            "case ",
            "typeof ",
            "void ",
            "delete ",
            "new ",
            "export default ",
            "class Example extends ",
            "else ",
            "do ",
            "const expression = ",
            "test(",
            "const expressions = [",
            "test: ",
            "condition ? ",
            "condition && ",
            "condition || ",
            "const create = () => ",
            "value + ",
            "value - ",
            "value * ",
            "value % ",
        ] {
            let code = format!("{prefix}/path\\//g;");
            assert_eq!(
                replacement(&format!("{code} // c"), FileKind::JavaScript),
                Some(pair("// c", &code)),
                "{code}"
            );
        }
    }

    #[test]
    fn ambiguous_or_unclosed_regexes_leave_no_persistent_state() {
        let syntax = string_syntax(FileKind::JavaScript).expect("JavaScript syntax");
        for line in [
            r"const value = numerator / /path\//.source.length; // c",
            r"if (ready) /path\//.test(value); // c",
            r"if (ready) {} /path\//.test(value); // c",
            r"/path\//.test(value); // c",
            r"const expression = /[[a]--[/]]/v; // c",
        ] {
            let result = scan_line(line, ScanState::Normal, &syntax);
            assert!(result.uncertain, "{line}");
            assert_eq!(result.comment_start, None, "{line}");
            assert_eq!(result.state, ScanState::Normal, "{line}");
            assert_eq!(
                replaced_indices(&[line, "const next = 1; // c"], FileKind::JavaScript),
                vec![1],
                "{line}"
            );
        }
    }

    #[test]
    fn regex_boundaries_preserve_comments_and_block_comment_state() {
        assert_eq!(
            replacement(r"const expression = /path\//// c", FileKind::JavaScript),
            Some(pair("// c", r"const expression = /path\//"))
        );
        let lines = [
            r"const expression = /[/*]/; /* real block",
            "inside the block",
            "*/ const next = 1; // c",
        ];
        assert_eq!(replaced_indices(&lines, FileKind::JavaScript), vec![2]);

        // A comment line just above the code keeps the trailing comment in place.
        let with_comment_above = [
            r"const expression = /[/*]/; /* real block",
            "// inside the block",
            "*/ const next = 1; // c",
        ];
        assert!(replaced_indices(&with_comment_above, FileKind::JavaScript).is_empty());
    }

    #[test]
    fn division_keeps_later_multiline_template_and_block_comment_state() {
        let syntax = string_syntax(FileKind::JavaScript).expect("JavaScript syntax");
        for operand in ["total", "42", "values[0]", "'total'", "\"total\"", "`total`", "total++"] {
            let opening = format!("const value = {operand} / count + `");
            let result = scan_line(&opening, ScanState::Normal, &syntax);
            assert!(!result.uncertain, "{opening}");
            assert_eq!(result.state, ScanState::InTemplate, "{opening}");
            let lines = [
                opening.as_str(),
                "// literal; text must stay unchanged",
                "`;",
                "const next = 1; // c",
            ];
            assert_eq!(
                lines_inside_strings(&lines, FileKind::JavaScript),
                vec![false, true, true, false]
            );
            assert_eq!(replaced_indices(&lines, FileKind::JavaScript), vec![3]);
        }
        for opening in [
            "const value = total / count; /* block",
            "const value = total() / count; /* block",
        ] {
            let lines = [
                opening,
                "still inside; // not a trailing comment",
                "*/ const next = 1; // c",
            ];
            let result = scan_line(lines[0], ScanState::Normal, &syntax);
            assert_eq!(result.state, ScanState::InBlockComment(1));
            assert_eq!(replaced_indices(&lines, FileKind::JavaScript), vec![2]);
        }
    }

    #[test]
    fn ambiguous_slashes_protect_possible_multiline_constructs() {
        let syntax = string_syntax(FileKind::JavaScript).expect("JavaScript syntax");
        for opening in [
            "const value = total() / count + `",
            "const value = { total: 1 } / count + `",
            r#"if (ready) /["'`/]/.test(value);"#,
            r#"const value = total() / count + "\"#,
            r#"const expression = /[/"'`* // unclosed"#,
            r"const expression = /unfinished\",
            "const value = /[[a]--[/]]/v + `",
        ] {
            let result = scan_line(opening, ScanState::Normal, &syntax);
            assert!(result.uncertain, "{opening}");
            assert_eq!(result.state, ScanState::Uncertain, "{opening}");
            let lines = [
                opening,
                "// literal; not prose",
                "`; // still uncertain",
                "const next = 1; // c",
            ];
            assert_eq!(
                lines_inside_strings(&lines, FileKind::JavaScript),
                vec![false, true, true, true]
            );
            assert!(replaced_indices(&lines, FileKind::JavaScript).is_empty());
        }
    }
}

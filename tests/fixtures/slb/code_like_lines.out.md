# Code like lines

A paragraph whose line starts with a code keyword and reads like code is kept exactly as it was written.

let configuration = Configuration::load(&project_root).expect("the configuration file should exist in the project root");

fn reflow_paragraph(paragraph: &Paragraph, options: &FormatOptions, produce_fix: bool) -> ReflowOutcome { todo!() }

use cli_tools::semantic_line_breaks::{FileKind, FormatOptions, RuleSet, format, check, LineRanges, SkipNotice, Violation};

impl Display for ViolationKind { fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result { write!(formatter, "{}", self.name()) } }

pub fn collect_files(paths: &[PathBuf], config: &Config) -> Result<Vec<(PathBuf, FileKind)>> { Ok(Vec::with_capacity(paths.len())) }

mod test_helpers { pub fn temporary_directory() -> TempDir { tempfile::Builder::new().prefix("slb_test").tempdir().unwrap() } }

struct Budgets { first: usize, rest: usize, hard_cap: bool, first_prefix: String, rest_prefix: String, tab_width: usize }

enum Refusal { Longer, OverTheLimit, LessBalanced, InsideBrackets, InsideQuotes, LooksLikeCode, NothingFollows(usize) }

import { readFileSync, writeFileSync, existsSync, mkdirSync } from "node:fs"; import { join, dirname, resolve } from "node:path";

from dataclasses import (dataclass, field, replace, asdict, astuple, fields, is_dataclass, make_dataclass, InitVar, KW_ONLY)

def reflow_paragraph(paragraph: Paragraph, options: FormatOptions, produce_fix: bool = True) -> ReflowOutcome: return None

class ReflowOutcome(NamedTuple): lines: Optional[list[str]] = None; changed: bool = False; violations: list = field(default=[])

const DEFAULT_OPTIONS = { width: 120, strict: false, allowWordBreak: false, joinSentences: false, rules: ["too-long", "mid-clause"] };

static RE_CODE_SPAN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"`[^`]*`").expect("the code span pattern should compile"));

return Err(anyhow!("the configuration file {} could not be parsed, see the error above for the line that failed", path.display()));

println!("Formatted {} files, {} of them changed, {} violations could not be fixed automatically", total, changed, unfixable);

eprintln!("Skipping unsupported file type: {}, pass --type to force a kind or add the extension to the config", path.display());

assert!(result.violations.iter().all(|violation| violation.fixable), "every violation should be fixable: {:?}", result.violations);

assert_eq!(format(expected, FileKind::JavaScript, &options), FormatResult::default(), "formatting the output again changed it");

dbg!(&paragraph.lines, &paragraph.hard_breaks, paragraph.first_prefix.len(), paragraph.rest_prefix.len(), options.max_width);

print(f"Formatted {total} files, {changed} of them changed, and {unfixable} violations could not be fixed automatically");

self.violations = [violation for violation in self.violations if violation.kind != ViolationKind.MID_CLAUSE_BREAK and violation.fixable]

$ cargo run --bin slb -- --stdin --type markdown --width 120 --trailing < tests/fixtures/slb/code_like_lines.in.md > /tmp/output.md

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, serde::Serialize, serde::Deserialize, clap::ValueEnum)]

#!/usr/bin/env -S cargo +nightly -Zscript --quiet --manifest-path=scripts/Cargo.toml --release --features=cli,serde,parallel

//TODO(akseli): replace = with a proper parser (the current split breaks on quoted values such as "a = b" inside strings)

#include <stdio.h> /* the header is only needed for printf (the formatter never looks at it) */ int main(void) { return 0; }

## Prose

A paragraph that only starts with one of those words is prose,
so it is reflowed like any other paragraph of the document.

Let the formatter handle the line breaks of every comment in the repository,
and review the changes it makes before you commit them.

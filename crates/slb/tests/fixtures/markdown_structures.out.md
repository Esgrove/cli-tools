# Markdown Structure Preservation

This guide documents structural Markdown constructs.
Each section pairs verbatim syntax with normal prose so formatting fixes prose without corrupting code, tables, or math.

## Pipe tables without outer pipes

Standard GFM pipe tables often omit leading and trailing pipes.
Every row in the table must remain untouched even when cells contain semicolons or dashes.

Flag | Default | Notes
--- | :--- | ---:
`--width` | 120 | Target line length; counts comment prefixes
`--strict` | false | Check mode — treats unfixable lines as errors
`--trailing` | false | Moves trailing comments; keeps directives in place

Prose right after a borderless pipe table.
It must still be formatted cleanly.

## Indented code blocks versus nested lists

A four-space indented line at top level is a CommonMark indented code block,
even when it begins with a bullet or numbered marker.
It must stay verbatim.

    - first code line; semicolons stay verbatim
    - second code line — em dashes stay verbatim
    1. numbered code line; also untouched

	* tab-indented code line; stays untouched as well

Inside a real list, however, a four-space indented item is a nested sub-list.
Its prose must still be formatted normally.

- Top-level list item. It introduces the nested items below.
    - Nested bullet item that was wrapped in the middle of a clause. It formats cleanly.
    1. Nested numbered item. Semicolons become periods here.

## Code spans, fences, and indentation

An inline triple-backtick code span at the start of a paragraph is prose, not a fenced code block.

```cargo test``` runs the workspace suite. ```cargo clippy``` checks lints.

A fenced code block ignores tab-indented backticks inside it.
Only a closing fence indented by at most three spaces ends the block.

```sh
echo "start"
	```
# still inside the fence; semicolons stay untouched
echo "end"
```

After the fence closes, normal prose resumes.
Semicolons are rewritten again.

## Math blocks

Single-line display math blocks open and close on the same line,
so prose immediately afterwards is not swallowed as math.

$$E = mc^2$$

Prose after a single-line math block.
It formats normally.

Multi-line display math blocks stay completely verbatim until their closing delimiter.

$$
\sum_{i=1}^{n} x_i = S; \quad \text{keep verbatim — untouched}
$$

Prose after a multi-line math block.
It also formats normally.

## Blockquotes and ignore directives

An ignore directive immediately before a blockquote suppresses formatting for the whole quote,
and the directive is consumed so the next paragraph still formats.

<!-- slb-ignore -->
> This quoted paragraph is ignored; semicolons and
> arbitrary line wraps remain untouched.

Prose immediately after an ignored blockquote.
It formats cleanly.

  > An indented blockquote with wrapped prose preserves its leading indentation.
  > Semicolons become periods.

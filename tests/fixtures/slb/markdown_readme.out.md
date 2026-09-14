# Project

[![Build](https://github.com/example/project/actions/workflows/ci.yml/badge.svg)](https://github.com/example/project/actions)

A collection of command line utilities. Every tool is a separate binary.
The tools share one configuration file, the sections are named after the binaries.

## Install

Install the tools with `cargo install --locked`, and make sure that the cargo binary directory is on the path.

```shell
# This fenced block must never be reflowed, even though this comment line is very long indeed.
cargo install --locked --path .
```

## Usage

- `dirmove` moves files into the directories that match their names, and it can create the directories.
- `slb` checks prose in comments. It also formats Markdown.
- A short item.

1. First run the check mode, which reports the violations and exits with a non-zero code.
2. Then run the fix mode.

| Tool | Purpose |
| ---- | ------- |
| slb  | prose   |

> The configuration file is optional. The defaults are used when it is missing.

### Notes

The line limit is read from the project configuration, for example `.editorconfig`, and it falls back to 120.

See the [documentation](https://example.com/docs/getting-started/configuration) for the full reference.

<!-- This HTML comment is left alone even when it is long enough to exceed the configured line limit. -->

    An indented code block stays as it is, even with a semicolon; and an em dash — here.

Text with a hard break at the end of the line  
continues on the next line.

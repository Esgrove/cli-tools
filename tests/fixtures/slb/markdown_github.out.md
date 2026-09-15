# Project Name

[![CI](https://github.com/example/project/actions/workflows/ci.yml/badge.svg)](https://github.com/example/project/actions)
[![Crates.io](https://img.shields.io/crates/v/project.svg)](https://crates.io/crates/project)

> [!NOTE]
> Alert blocks are written as blockquotes, and the marker line has to stay on a line of its own.

## Table of contents

- [Install](#install)
- [Usage](#usage)
  - [Options](#options)

## Install

<details>
<summary>Build from source</summary>

Clone the repository and build it with `cargo build --release`, which puts the binary under `target/release`.

</details>

## Usage

Run the tool with no arguments to check the current directory. It walks the tree and reports every violation.

### Options

| Flag | Default | Description                |
| :--- | :-----: | -------------------------: |
| `-f` | `false` | Rewrite the files in place |
| `-w` | `120`   | Maximum line width         |

- [x] Check mode, which reports the violations and exits with a non-zero code when it finds any of them.
- [ ] Watch mode, which is still being designed and will land in a later release once the API has settled.

1. Install the tool.
2. Run it in check mode first, so that you can see what would change
   before anything is written to disk, and only then run the fix mode.
   1. Nested ordered items keep their own indentation.

> A blockquote that was hard wrapped by hand in the middle of a clause is joined
> and broken again at the boundary that reads best.

```rust
/// A doc comment inside a fence is never reflowed, no matter how long the line inside the block happens to be.
fn main() {}
```

````markdown
```
A nested fence inside a wider fence stays verbatim.
```
````

Autolink: <https://example.com/very/long/path/that/would/otherwise/be/wrapped/by/the/formatter/right/here>

Reference style [link][docs] and an image ![logo](docs/logo.png "The logo") in a line that needs a break here.

A footnote reference[^1] sits in a sentence that is long enough to be reflowed by the formatter when it runs.

[^1]: Footnote definitions are left alone.

[docs]: https://example.com/docs

***

Setext heading
==============

Another one
-----------

<div align="center">
  <img src="docs/banner.png" alt="banner">
</div>

Emoji :rocket: and inline HTML <kbd>Ctrl</kbd>+<kbd>C</kbd> in a line that is long enough to need a break in it.

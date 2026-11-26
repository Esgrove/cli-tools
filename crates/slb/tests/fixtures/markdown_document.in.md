---
title: Sample
---

# Heading

[![Badge](https://img.shields.io/x.svg)](https://example.com)

Some intro text that was wrapped by hand at a fairly
narrow column; it also uses a semicolon — and a dash.

- A list item that continues
onto a lazy line.
- Short item.
    - Nested sub-list item; semicolons still format inside real nested lists.

A paragraph that runs past the limit and ends with **a bold phrase that must stay together** so the break lands before it.

1. Numbered item with `code; inside` that should
   stay together.

> A quote that was
> hard wrapped.

  > An indented quote that was
  > hard wrapped keeps its indentation.

<!-- slb-ignore -->
> An ignored quote; semicolons and
> hard wraps stay untouched.

Prose after an ignored quote; it still formats normally because the marker was consumed.

| Column | Other |
|--------|-------|
| a; b   | c — d |

Name | Description
--- | ---
foo | pipe table without leading pipes; stays untouched
bar | second row — also stays untouched

    - indented code block starting with a dash; stays verbatim
    1. indented code block starting with a number; stays verbatim

```foo``` starts with an inline triple-backtick code span; it is prose, not a fence opener.

```rust
let x = 1; // untouched
	```
let y = 2; // tab-indented backticks inside a fence do not close it; still untouched
```

$$x + y = z$$

Prose after a single-line math block; it still formats normally.

Line with hard break  
continues here.

<!-- slb-ignore -->
this paragraph is
left alone; forever.

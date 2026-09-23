# SIMD evaluation with `fearless_simd`

This document records an experiment with [fearless_simd](https://github.com/linebender/fearless_simd) 1.0,
which gives safe, portable SIMD with runtime level detection on x86 and NEON on Apple Silicon.
It covers the process, the kernels, what was measured on a Ryzen 7950X and an Apple M4 Pro,
which changes were kept, and how to repeat the measurements.

## Goal and targets

Find the parts of the project where SIMD byte scanning pays off,
measure them with the existing Criterion benchmarks,
and keep only changes that clearly win on the machines the tools run on.

| Machine                    | ISA used by `fearless_simd` | Native vector | Notes                                                                            |
| -------------------------- | --------------------------- | ------------- | -------------------------------------------------------------------------------- |
| AMD Ryzen 9 7950X, Windows | AVX-512 (Ice Lake level)    | 64 bytes      | Zen 4 runs 512-bit operations as two 256-bit halves, so AVX2 may be as fast      |
| Apple M4 Pro, macOS        | NEON                        | 16 bytes      | No movemask instruction, a mask to bitmask conversion takes several instructions |

The local build uses `target-cpu=native` from `.cargo/config.toml`,
so the scalar code is already auto vectorized by LLVM with every feature of the build machine.
The comparisons below are against that, not against a baseline x86-64 build.
Capping the level with `CLI_TOOLS_SIMD=avx2` limited the vector width the kernels used,
but LLVM may still have picked AVX-512 encodings for the surrounding code.

## Where SIMD could help

Most of the project works on short strings.
Tokens are 3 to 15 bytes, lines 40 to 120 bytes, and file names 20 to 100 bytes.
That is the hard case for SIMD, since dispatch and vector setup cost a few nanoseconds per call.
The candidates, from a read of the code:

| Area                   | Code                                                                                | Idea                                                                                                                      | Status                                                                |
| ---------------------- | ----------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------- |
| slb scanner            | `scan_line_buffered` in `src/semantic_line_breaks/scanner.rs`                       | Skip the character by character scan for a code line without any byte that can start a comment, string, heredoc, or regex | Kept, 5 to 19 percent faster scans on top of the shortcut itself      |
| slb scanner            | `scan_characters` in `src/semantic_line_breaks/scanner.rs`                          | On lines that do need a scan, jump straight to the next byte that can change the scanner state                            | Kept, 15 to 22 percent faster C, Python, and trailing comment scans   |
| slb tokenizer          | `chunk_end` in `src/semantic_line_breaks/tokenizer.rs`                              | Find the next whitespace or em dash lead byte with SIMD, confirm it with `char::is_whitespace`                            | Kept, 21 to 27 percent faster tokenizing                              |
| slb tokenizer          | `backtick_span_end` in `src/semantic_line_breaks/tokenizer.rs`                      | Jump between backtick runs while finding a matching code span delimiter                                                   | Kept, 4 percent faster punctuated tokenizing on the M4 Pro            |
| slb Markdown heuristic | `looks_like_code` in `src/semantic_line_breaks/looks_like_code.rs`                  | Search once for any of the code marker bytes rather than checking the three characters separately                         | Kept, 7 percent faster Markdown formatting and checking on the M4 Pro |
| dupefind               | `collapse_repeated_separators` in `src/lib.rs`                                      | Skip the regex when ASCII text has no two adjacent separators                                                             | Kept, the shortcut is the gain and SIMD adds little on file names     |
| slb widths             | `text_width` in `src/semantic_line_breaks/token.rs` and `chars().count()` in reflow | SIMD character count                                                                                                      | Not applied, the kernel loses to std below about 1 KB                 |
| dirmove                | `count_prefix_chars`, `get_all_n_part_sequences` in `src/dir_move/utils.rs`         | SIMD character count and dot positions                                                                                    | Reverted, short file names got up to twice as slow                    |
| dirmove                | `find_prefix_candidates` and `parts_are_contiguous_with_combined`                   | Not SIMD: a sorted prefix index and precomputed lowercase parts                                                           | Kept, 15 times faster on 500 files, see below                         |
| File hashing           | `src/file_hash.rs`                                                                  | None, BLAKE3 already uses SIMD                                                                                            | Skipped                                                               |
| Regex heavy code       | dates, resolutions, dot rename formatting                                           | None, the regex crate already uses SIMD prefilters                                                                        | Skipped                                                               |

## Implementation

`src/simd.rs` holds all SIMD code, and the rest of the project only calls its safe functions:

- `count_chars` and `count_chars_excluding` count UTF-8 characters, optionally skipping one ASCII byte.
- `find_byte_of` and `contains_byte_of` search for any byte of a small set.
- `has_adjacent_bytes_of` tells whether two bytes of a set stand next to each other.
- `byte_positions` lists every position of one byte.

Each kernel is a `#[simd]` function generic over `fearless_simd::Simd`,
called through `dispatch!` with the level detected once per process.
The kernels share one block walker, and a `ByteClass` trait describes the bytes each kernel looks for:

1. Input shorter than 16 bytes is classified byte by byte into a bitmask,
   because loading a vector costs more than the loop.
2. Input shorter than the native width is read as 128-bit blocks.
3. Longer input is read in native blocks, 64 bytes with AVX-512, 32 with AVX2, and 16 with NEON.
4. The last block overlaps the one before it, so there is no scalar tail loop.
   Its bitmask is shifted to drop the bytes the previous block already covered.
5. Lane masks become `u64` bitmasks, so counting is `count_ones`, the first match is `trailing_zeros`,
   and adjacency is `bits & (bits >> 1)` with one carried bit between blocks.

The project denies `unsafe_code`, and the `fearless_simd` macros did not trip the lint.
The only lint exception is `clippy::inline_always` on the helpers,
which must inline into the `#[simd]` caller to inherit its target features.

### Choosing the implementation at runtime

During the evaluation, `CLI_TOOLS_SIMD` selected the implementation, read once per process:

| Value                              | Effect                                                                                      |
| ---------------------------------- | ------------------------------------------------------------------------------------------- |
| unset or `auto`                    | Best detected level, AVX-512 on the 7950X and NEON on Apple Silicon                         |
| `scalar`                           | Plain byte loops in the same call sites, so the difference to `auto` is the SIMD part alone |
| `avx2`, `sse4.2`, `sse2`, `avx512` | Cap the level on x86, ignored elsewhere                                                     |

The same binary then ran every variant, so Criterion baselines compared like with like.
The scanner shortcut and the separator check skip work in either mode,
so their gain over the code before the experiment is measured separately, against the first baseline.

The switch was removed once the kernels were merged.
The kernels now always run at the detected level.

### Correctness

- Each kernel is tested against a scalar reference on every SIMD level the machine supports,
  on every prefix up to about 200 bytes of mixed ASCII and multibyte text, crossing the 16, 32, and 64 byte blocks.
- The scanner shortcut has a test that scans every source, test fixture,
  and config file in the repository with and without the shortcut
  and requires identical results on every line.
- `chunk_end` is tested against the old character loop on every start position of lines
  with every Unicode whitespace kind, em and en dashes, curly quotes, and long words.

## Process

1. Saved Criterion baselines of the existing benchmarks before any change.
2. Added the kernels and `benches/simd.rs`,
   which runs each kernel against the scalar code it would replace at 4 bytes to 64 KB,
   to find the input length where SIMD starts to pay.
3. Applied the candidates one area at a time and compared against the baseline.
4. Added the `CLI_TOOLS_SIMD` switch and `simd-bench.sh`,
   because comparing against a baseline from an earlier session turned out to be unreliable:
   benchmarks that were never touched came out 5 to 12 percent faster than the day before.
   Running every mode back to back in one session removes most of that drift.

Lessons from the process:

- Always read a few untouched benchmarks as a control before trusting a percentage.
- Never edit sources while a benchmark script runs, because the next `cargo bench` recompiles with the edit.
- A padded copy into a 64-byte buffer made every input below 64 bytes cost about 8 ns with AVX-512,
  which is why the kernels now use 128-bit blocks and a byte loop for short input.

## Results on the Ryzen 7950X

Windows 11, Rust 1.98.1, `target-cpu=native`, `./simd-bench.sh` with full Criterion runs,
all modes back to back in one session.
The median time of each mode is shown, and the change is against `scalar` in the same session.

### Pipeline benchmarks

| Benchmark                                                 |        scalar | auto (AVX-512) |          avx2 | auto vs scalar | avx2 vs scalar |
| --------------------------------------------------------- | ------------: | -------------: | ------------: | -------------: | -------------: |
| `tokenize_line/long_sentence`                             |       2.23 µs |        1.76 µs |       1.75 µs |         −21.4% |         −21.7% |
| `tokenize_line/punctuated_line`                           |       1.47 µs |        1.14 µs |       1.14 µs |         −22.2% |         −22.4% |
| `tokenize_line/unicode_line`                              |       1.58 µs |        1.15 µs |       1.15 µs |         −27.5% |         −27.2% |
| `format/rust_source`                                      |      22.60 µs |       14.57 µs |      14.57 µs |         −35.5% |         −35.5% |
| `format/large_rust_file`                                  |       1.48 ms |        1.37 ms |       1.36 ms |          −7.4% |          −8.0% |
| `format/large_prose_file`                                 |       3.12 ms |        3.03 ms |       3.00 ms |          −2.9% |          −3.9% |
| `format/large_markdown_file`                              |       1.63 ms |        1.58 ms |       1.57 ms |          −2.6% |          −3.1% |
| `format/many_small_files`                                 |       1.49 ms |        1.44 ms |       1.44 ms |          −3.3% |          −3.1% |
| `check/large_rust_file`                                   |       1.43 ms |        1.36 ms |       1.33 ms |          −5.1% |          −7.0% |
| `check/large_prose_file`                                  |       2.99 ms |        2.92 ms |       2.91 ms |          −2.3% |          −2.7% |
| `check/large_markdown_file`                               |       1.57 ms |        1.52 ms |       1.53 ms |          −3.2% |          −3.0% |
| `reflow_paragraph/clauses_to_lines`                       |      10.98 µs |       10.07 µs |       9.92 µs |          −8.3% |          −9.7% |
| `reword_semicolons/many_semicolons`                       |      306.3 µs |       277.1 µs |      308.4 µs |          −9.5% |          +0.7% |
| `reword_semicolons/many_semicolons_with_a_stray_bracket`  |      323.0 µs |       265.7 µs |      264.9 µs |         −17.7% |         −18.0% |
| `scan/fix_trailing_comments`                              |      224.0 µs |       180.8 µs |      180.8 µs |         −19.3% |         −19.3% |
| `scan/split_source_regions` (Rust)                        |      544.3 µs |       511.6 µs |      507.9 µs |          −6.0% |          −6.7% |
| `scan/split_shell_regions`                                |      314.5 µs |       284.6 µs |      286.4 µs |          −9.5% |          −8.9% |
| `scan/split_c_regions`                                    |      505.8 µs |       481.1 µs |      483.0 µs |          −4.9% |          −4.5% |
| `scan/split_python_regions`                               |      630.6 µs |       601.5 µs |      600.1 µs |          −4.6% |          −4.8% |
| `dupe_find/normalize_stem` with a resolution or codec tag | 236 to 244 ns |  206 to 218 ns | 203 to 306 ns |    −8% to −13% |   −12% to +37% |
| `dupe_find/normalize_stem` other cases                    |               |                |               |    −4% to +16% |    −3% to +15% |
| `dupe_find/normalize_stem_batch_16`                       |       6.32 µs |        6.16 µs |       6.32 µs |          −2.6% |          −0.0% |

The `normalize_stem` cases take 130 to 750 ns and are dominated by two regex replacements,
so single cases swing by 10 to 30 percent between modes without a pattern, which is noise.
The batch benchmark is the steadier number.

These percentages are the SIMD part alone, because `scalar` keeps the scanner shortcut and the separator check.
Against the first baseline, taken before any change,
the scanner shortcut made `scan/fix_trailing_comments` 31 percent and `scan/split_source_regions` 15 percent faster,
and the separator check made `normalize_stem` about 26 percent faster.
That baseline was from a slower moment of the same machine, so roughly 7 percent of each number is drift.

### Kernel microbenchmarks

Median time of the `auto` kernel against the scalar code it replaces, by input length in bytes.

| Kernel                                | Scalar reference                   |     4 |     8 |   16 |   32 |    64 |  128 | 1024 | 65536 |
| ------------------------------------- | ---------------------------------- | ----: | ----: | ---: | ---: | ----: | ---: | ---: | ----: |
| `contains_byte_of`, 7 byte set        | lookup table                       | +545% | +295% | −20% | −28% |  −79% | −79% | −77% |  −78% |
| `contains_byte_of`, 7 byte set        | `slice::contains`                  | +401% | +154% | −47% | −57% |  −87% | −89% | −88% |  −89% |
| `find_byte_of`, whitespace candidates | `char_indices` and `is_whitespace` | +365% |  +78% | −68% | −70% |  −91% | −89% | −89% |  −88% |
| `has_adjacent_bytes_of`               | `windows(2)`                       | +198% |   +1% | −87% | −89% |  −96% | −97% | −97% |  −97% |
| `count_chars`, ASCII                  | `is_ascii` then `len`              |  +81% | +256% | +63% | +32% | +140% | +97% | −16% |   −0% |
| `count_chars`, mixed                  | `is_ascii` then `chars().count()`  | +118% |  +95% |  −1% | −59% |  −59% | −60% | −77% |  −72% |
| `byte_positions`                      | `enumerate` and `filter_map`       |  +42% |  −68% | −69% | −10% |  −13% | −22% | −51% |  −71% |

- SIMD pays off from 16 bytes up and loses below that,
  since the kernels spend 2 to 7 ns on dispatch and on building the byte table for short input.
- Character counting only wins for text with multibyte characters,
  because the standard library's ASCII check already runs a word at a time and is auto vectorized.
  Tokens are mostly ASCII and short, so the width code stays scalar.
- Collecting positions into a `Vec` costs more than finding them, so `byte_positions` gains little on file names.

### AVX-512 against AVX2

On the 7950X the two are within noise of each other on every pipeline benchmark.
The lines and tokens here rarely fill a 64-byte vector,
so most of the work runs on the 128-bit and 256-bit paths either way,
and Zen 4 executes 512-bit operations as two 256-bit halves.
There is no reason to cap the level, so the kernels always use the detected level.

### Scanner skip-ahead

The line shortcut only helps lines without any trigger byte.
On the lines that remain, the scanner used to call its state machine on every character,
although most characters can only move the cursor forward by one.
The skip-ahead asks `find_byte_of` for the next byte that can change the current state,
for example a quote or backslash inside a string or the first byte of the closing marker inside a block comment,
and jumps the cursor there with a binary search over the character offsets.
The repository wide test compares it against the plain character loop on every line.

Measured on the 7950X back to back against the previous commit, so machine drift does not enter the numbers.

| Benchmark                                        |   Before | Skip-ahead | Change |
| ------------------------------------------------ | -------: | ---------: | -----: |
| `scan/split_c_regions`                           | 486.0 µs |   377.2 µs | −22.4% |
| `scan/split_python_regions`                      | 595.1 µs |   475.3 µs | −20.1% |
| `scan/fix_trailing_comments`                     | 178.9 µs |   152.0 µs | −15.0% |
| `scan/split_source_regions` (Rust)               | 502.5 µs |   488.1 µs |  −2.9% |
| `scan/split_shell_regions`                       | 285.2 µs |   281.4 µs |  −1.3% |
| `format/large_rust_file`                         | 1.322 ms |   1.304 ms |  −1.4% |
| `check/large_rust_file`                          | 1.281 ms |   1.263 ms |  −1.4% |
| `tokenize_line/long_sentence`, untouched control | 1.455 µs |   1.469 µs |  +1.0% |

## Results on the M4 Pro

macOS on an Apple M4 Pro, `target-cpu=native`, `./simd-bench.sh` with full Criterion runs.
`auto` selects the 16-byte NEON implementation.
The table compares the scalar and auto modes back to back in the same session.

| Benchmark                                                |    scalar | auto (NEON) | auto vs scalar |
| -------------------------------------------------------- | --------: | ----------: | -------------: |
| `tokenize_line/long_sentence`                            |   2.12 µs |     1.62 µs |         −23.4% |
| `tokenize_line/punctuated_line`                          |   1.40 µs |     1.07 µs |         −23.8% |
| `tokenize_line/unicode_line`                             |   1.54 µs |     1.12 µs |         −27.2% |
| `format/rust_source`                                     |  12.92 µs |    12.49 µs |          −3.4% |
| `format/large_rust_file`                                 |   1.30 ms |     1.15 ms |         −10.9% |
| `format/large_prose_file`                                |   2.81 ms |     2.68 ms |          −4.6% |
| `format/large_markdown_file`                             |  1.462 ms |    1.365 ms |          −6.6% |
| `format/many_small_files`                                |   1.21 ms |     1.14 ms |          −6.1% |
| `check/large_rust_file`                                  |   1.30 ms |     1.11 ms |         −14.7% |
| `check/large_prose_file`                                 |   2.81 ms |     2.56 ms |          −9.1% |
| `check/large_markdown_file`                              |  1.419 ms |    1.319 ms |          −7.1% |
| `reflow_paragraph/clauses_to_lines`                      |   9.22 µs |     8.86 µs |          −3.9% |
| `reword_semicolons/many_semicolons`                      | 204.00 µs |   199.89 µs |          −2.0% |
| `reword_semicolons/many_semicolons_with_a_stray_bracket` | 206.46 µs |   199.67 µs |          −3.3% |
| `scan/fix_trailing_comments`                             | 227.77 µs |   156.88 µs |         −31.1% |
| `scan/split_source_regions` (Rust)                       | 509.47 µs |   469.64 µs |          −7.8% |
| `scan/split_shell_regions`                               | 270.59 µs |   253.52 µs |          −6.3% |
| `scan/split_c_regions`                                   | 418.23 µs |   340.07 µs |         −18.7% |
| `scan/split_python_regions`                              | 490.96 µs |   407.85 µs |         −16.9% |
| `dupe_find/normalize_stem_batch_16`                      |   4.51 µs |     4.45 µs |          −1.4% |

NEON keeps the scanner and tokenizer kernels on a second architecture.
The largest gains occur where the scan reaches a long non-triggering run.
The Markdown code heuristic also benefits from replacing its separate code-marker checks with one byte-set search.
The duplicate-name batch remains within noise, which confirms that the separator shortcut,
rather than the SIMD kernel, is the important optimization in that path.

The microbenchmarks have the same crossover as the Ryzen run for the byte-search kernels.
`contains_byte_of` and `find_byte_of` lose for 4 and 8 byte inputs,
then improve by 32 to 65 percent at 16 bytes and above.
`has_adjacent_bytes_of` improves at every measured length,
reaching about 91 percent for inputs of 16 bytes and above.
Character counting remains unsuitable for current token widths.
The SIMD path loses on short ASCII text and is only marginally faster for larger mixed text.

The M4 Pro run already includes the scanner skip-ahead in both modes,
which likely explains why its C and Python scans gain more from SIMD than in the 7950X table above.
The missing movemask instruction did not stop the byte search kernels from paying off,
though their microbenchmark gains at 16 bytes and above are smaller than with AVX-512.

## The dirmove prefix search

This part is an algorithm change rather than SIMD, found while looking at where dirmove spends its time.
`collect_all_prefix_groups` calls `find_prefix_candidates` once per file,
and each call checked every candidate prefix of that file against every file in the directory,
so the first pass grew with the square of the file count.
For every file that matched, `parts_are_contiguous_with_combined` then lowercased all its parts
and built concatenated strings for every start position, which allocated in the innermost loop.

Two changes remove that work without changing any result:

1. `PrefixIndex` in `src/dir_move/prefix_index.rs` sorts the lowercased single,
   two part, and three part combinations of all files once.
   All combinations that start with a candidate prefix form one contiguous range, found with a binary search,
   so each candidate only visits the files that can match it.
   The binary builds the index once per grouping pass and shares it between the parallel workers.
   The second pass queries the same index for every group key,
   instead of checking every file against every group.
2. `FileInfo` now keeps `original_parts_lower`,
   and `parts_are_contiguous_lowered` skips every start position whose lowercased parts cannot spell out the prefix,
   so strings are only built for the rare positions that might match.

Tests compare the index with a full scan of `prefix_matches_normalized`,
and the new contiguity check with the old allocating code,
including characters such as `İ` whose lowercase form has a different byte length.

| Benchmark                                                               |  Before |   After |          Change |
| ----------------------------------------------------------------------- | ------: | ------: | --------------: |
| First pass over 500 files, `find_prefix_candidates/scaled_500`          | 62.2 ms | 4.18 ms | 15 times faster |
| First pass over 40 files, `find_prefix_candidates/all_files/large_set`  |  265 µs |  109 µs |            −59% |
| First pass over 16 files, `find_prefix_candidates/all_files/medium_set` | 69.7 µs | 36.2 µs |            −48% |
| `parts_are_contiguous/no_match`                                         | 1.64 µs | 19.4 ns | 85 times faster |
| `parts_are_contiguous/extended_starts_with`                             | 38.7 ns |  8.9 ns |            −77% |

The one-shot `find_prefix_candidates` still exists for tests and single lookups,
but it builds an index per call, so callers checking many files should use `find_prefix_candidates_indexed`.

## Repeating the measurements

```shell
./simd-bench.sh
```

The script runs the slb and dupefind benchmarks that go through `src/simd.rs`,
saving them as the Criterion baseline `pipeline`.
It then runs the kernel microbenchmarks as the baseline `kernels`
and writes a table of both to `target/simd-bench-<os>-<arch>.md`.
Add `--quick` for a noisier run that takes a few minutes,
and `--summary-only` to rebuild the table from saved baselines.

The tables above were produced by an earlier version of the script,
which also ran every `CLI_TOOLS_SIMD` mode back to back.
That comparison is no longer possible, since the scalar mode and the level cap were removed after the merge.

## Conclusions

The experiment is complete, and `fearless_simd` stays in the project for the byte search kernels.

- Kept on both architectures:
  the scanner line shortcut and skip-ahead, the tokenizer whitespace and backtick search,
  the Markdown code marker search, and the separator check in `collapse_repeated_separators`.
- Measured gains of the SIMD part alone, against `scalar` in the same session:
  21 to 28 percent faster tokenizing, 5 to 31 percent faster comment and string scanning,
  and 2 to 15 percent faster whole file checking and formatting.
- Most of the dupefind gain comes from the separator shortcut, which skips the regex in either mode.
- Character counting and the dirmove file name scans stay scalar,
  because the dispatch cost on short strings exceeds the benefit.
  `count_chars` and `count_chars_excluding` remain in `src/simd.rs` with their tests and benchmarks,
  so the decision can be revisited for longer inputs.
- AVX-512 and AVX2 perform the same on the 7950X, so the detected level is always used.
- The largest single improvement is not SIMD.
  The dirmove prefix index makes grouping 500 files 15 times faster,
  and both grouping passes now avoid their previous all pairs scans.

An M1 Pro run was planned but not done.
Its NEON code path is the same as the M4 Pro's, so the same decisions are expected to hold.

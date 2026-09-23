# cli-tools

Various CLI helper tools in one Rust project.
Produces separate binaries for each tool.

```shell
./build.sh
./install.sh
```

## Configuration

The CLI binaries can be configured with a user config file in addition to the CLI arguments.
The config file goes to `~/.config/cli-tools.toml`,
and has separate sections for each binary.

An example config [cli-tools.toml](./cli-tools.toml) is provided in the repo root.

## Shell Completions

All binaries support shell completion generation via the `completion` subcommand.
The install script automatically detects the platform and installs completions for the appropriate shells:

- **Windows**: Bash and PowerShell
- **macOS**: Zsh
- **Linux**: Zsh and Bash

The binary list is read from `Cargo.toml` automatically to get all binaries.

### Install completions

```shell
./completions.sh
```

### Generate manually for a single binary

```shell
# Print completion script to stdout
dots completion bash
dots completion zsh

# Install completion script to the shell's completion directory
dots completion zsh -I
dots completion powershell -I
```

## Div

```console
Print divider comment with centered text

Usage: div [OPTIONS] [TEXT]...

Arguments:
  [TEXT]...  Optional divider texts

Options:
  -l, --length <LENGTH>   Divider length in number of characters [default: 120]
  -c, --char <CHARACTER>  Divider character to use [default: %]
  -a, --align             Align multiple divider texts to same start position
  -h, --help              Print help
  -V, --version           Print version
```

## Dirmove

```console
Move files to directories based on name

Usage: dirmove [OPTIONS] [PATH]... [COMMAND]

Commands:
  completion  Generate shell completion script
  help        Print this message or the help of the given subcommand(s)

Arguments:
  [PATH]...  Optional input directories or files

Options:
  -O, --output <OUTPUT>           Optional output directory, defaults to the input directory
  -a, --auto                      Auto-confirm all prompts without asking
  -c, --create                    Create directories for files with matching prefixes
  -D, --debug                     Print debug information
  -f, --force                     Overwrite existing files
  -F, --files-only                Only match input files; do not offer whole-directory merges
  -n, --include <INCLUDE>         Include files that match the given pattern
  -e, --exclude <EXCLUDE>         Exclude files that match the given pattern
  -i, --ignore <IGNORE>           Ignore prefix when matching filenames
  -I, --ignore-group <GROUP>      Group name to ignore
  -P, --ignore-group-part <PART>  Ignore groups containing this part (substring match)
  -o, --override <OVERRIDE>       Override prefix to use for directory names
  -u, --unpack <NAME>             Directory name to "unpack" by moving its contents to the parent directory
  -M, --map <MAPPING>             Name to directory mapping pair (pattern:dirname)
  -g, --group <COUNT>             Minimum number of matching files needed to create a group
  -m, --min-chars <CHARS>         Minimum character count for prefixes to be valid group names
  -p, --print                     Only print changes without moving files
  -r, --recurse                   Recurse into subdirectories
  -S, --show-db                   Show database statistics and contents
  -v, --verbose                   Print verbose output
  -h, --help                      Print help
  -V, --version                   Print version
```

## Dots

```console
Rename files to use dot formatting

Usage: dots [OPTIONS] [PATH] [COMMAND]

Commands:
  prefix      Prefix files with a name or parent directory name
  suffix      Suffix files with a name or parent directory name
  completion  Generate shell completion script
  help        Print this message or the help of the given subcommand(s)

Arguments:
  [PATH]  Optional input directory or file

Options:
  -c, --case                                Convert casing
  -D, --debug                               Enable debug prints
  -d, --directory                           Rename directories
  -f, --force                               Overwrite existing files
  -n, --include <INCLUDE>                   Include files that match the given pattern
  -e, --exclude <EXCLUDE>                   Exclude files that match the given pattern
  -i, --increment                           Increment conflicting file name with running index
  -p, --print                               Only print changes without renaming files
  -r, --recurse                             Recurse into subdirectories
  -x, --prefix <PREFIX>                     Append prefix to the start
  -b, --prefix-dir                          Prefix files with directory name
  -B, --prefix-dir-start                    Force prefix name to the start
  -R, --prefix-dir-recursive                Prefix files with their parent directory name
  -j, --suffix-dir                          Suffix files with directory name
  -J, --suffix-dir-recursive                Suffix files with their parent directory name
  -u, --suffix <SUFFIX>                     Append suffix to the end
  -s, --substitute <PATTERN> <REPLACEMENT>  Substitute pattern with replacement in filenames
  -m, --random                              Remove random strings
  -z, --remove <PATTERN>                    Remove pattern from filenames
  -g, --regex <PATTERN> <REPLACEMENT>       Substitute regex pattern with replacement in filenames
  -y, --year                                Assume year is last in short dates
  -v, --verbose                             Print verbose output
  -h, --help                                Print help
  -V, --version                             Print version
```

## Dupefind

Find duplicate video files based on identifier patterns,
and detect files with the same name but different resolutions, codecs, or file extensions.

```console
Find duplicate video files based on identifier patterns

Usage: dupefind [OPTIONS] [PATHS]... [COMMAND]

Commands:
  completion  Generate shell completion script
  help        Print this message or the help of the given subcommand(s)

Arguments:
  [PATHS]...  Input directories to search

Options:
  -g, --pattern <PATTERN>      Identifier patterns to search for (regex)
  -e, --extension <EXTENSION>  File extensions to include
  -m, --move                   Move duplicates to a "Duplicates" directory
  -i, --ignore <IGNORE>        Ignore prefix when matching filenames
  -H, --hash                   Compare file contents using BLAKE3 hashes
  -p, --print                  Only print changes without moving files
  -r, --recurse                Recurse into subdirectories
  -d, --default                Use default paths from config file
  -v, --verbose                Print verbose output
  -D, --debug                  Print debug output (extensions, patterns, etc.)
  -h, --help                   Print help
  -V, --version                Print version
```

## Flipdate

Rename files and directories to use `yyyy.mm.dd` date format for files,
and `yyyy-mm-dd` for directories.

```console
Flip dates in file and directory names to start with year

Usage: flipdate [OPTIONS] [PATH]

Arguments:
  [PATH]  Optional input directory or file

Options:
  -d, --dir                     Use directory rename mode
  -f, --force                   Overwrite existing
  -e, --extensions <EXTENSION>  Specify file extensions
  -y, --year                    Assume year is first in short dates
  -p, --print                   Only print changes without renaming
  -r, --recurse                 Recurse into subdirectories
  -s, --swap                    Swap year and day around
  -v, --verbose                 Print verbose output
  -h, --help                    Print help
  -V, --version                 Print version
```

## Vconvert

Convert video files to HEVC (H.265) format using ffmpeg and NVENC.
Tracks files needing conversion in a local SQLite database for efficient processing.

```console
Convert video files to HEVC (H.265) format using ffmpeg and NVENC

Usage: vconvert [OPTIONS] [PATH] [COMMAND]

Commands:
  completion  Generate shell completion script
  help        Print this message or the help of the given subcommand(s)

Arguments:
  [PATH]  Optional input directory or file

Options:
  -a, --all                          Convert all known video file types
  -b, --bitrate <BITRATE>            Skip files with bitrate lower than LIMIT kbps [default: 8000]
  -c, --count <COUNT>                Limit the number of files to convert
  -d, --delete                       Delete input files immediately instead of moving to trash
  -p, --print                        Print commands without running them
  -f, --force                        Overwrite existing output files
  -n, --include <INCLUDE>            Include files that match the given pattern
  -e, --exclude <EXCLUDE>            Exclude files that match the given pattern
  -t, --extension <EXTENSION>        Override file extensions to convert
  -o, --other                        Convert all known video file types except MP4 files
  -r, --recurse                      Recurse into subdirectories
  -k, --skip-convert                 Skip conversion
  -x, --delete-duplicates            Delete source file if converted x265 file already exists
  -m, --movie                        Movie mode: preserve MKV container, metadata, and selected stream languages
  -M, --skip-remux                   Skip remuxing
  -s, --sort [<ORDER>]               Sort files [possible values: bitrate, size, size-asc, duration, duration-asc, resolution, resolution-asc, impact, name]
  -v, --verbose                      Print verbose output
  -D, --from-db                      Process files from database instead of scanning
  -C, --clear-db                     Clear all entries from the database
  -S, --show-db                      Show database statistics and contents
  -E, --list-extensions              List file extension counts in the database
  -X, --clean-cache                  Remove stale entries from the scan cache
  -B, --max-bitrate <MAX_BITRATE>    Maximum bitrate in kbps
  -u, --min-duration <MIN_DURATION>  Minimum duration in seconds
  -U, --max-duration <MAX_DURATION>  Maximum duration in seconds
  -R, --min-resolution <PIXELS>      Skip files where either width or height is smaller than PIXELS
  -L, --display-limit <LIMIT>        Maximum number of files to display
  -h, --help                         Print help (see more with '--help')
  -V, --version                      Print version
```

### Filter Options

The filter options
(`-b`/`--bitrate`, `-B`/`--max-bitrate`, `-u`/`--min-duration`, `-U`/`--max-duration`, `-R`/`--min-resolution`, `-t`/`--extension`, `-c`/`--count`)
work for both normal scanning mode and database mode (`-D`/`--from-db`, `-S`/`--show-db`).

### Database Commands

```shell
# Normal scan and convert (updates database automatically)
vconvert /path/to/videos --recurse

# Show database contents and statistics
vconvert --show-db

# List file extension counts in database
vconvert --list-extensions

# Show only mkv files in database
vconvert --show-db --extension mkv

# Process files from database (skip rescanning)
vconvert --from-db

# Process only files between 8-20 Mbps from database
vconvert --from-db --bitrate 8000 --max-bitrate 20000

# Process files sorted by bitrate (highest first)
vconvert --from-db --sort bitrate

# Clear the database
vconvert --clear-db

# Remove stale entries from the scan cache (files that no longer exist on disk)
vconvert --clean-cache
```

### Configuration

Filter options can also be set in the config file (`~/.config/cli-tools.toml`):

```toml
[video_convert]
bitrate = 8000           # Minimum bitrate threshold (kbps)
max_bitrate = 50000      # Maximum bitrate threshold (kbps)
min_duration = 60        # Minimum duration (seconds)
max_duration = 7200      # Maximum duration (seconds)
min_resolution = 1000    # Minimum resolution — skip if width or height is below this
count = 10               # Limit number of files to process
sort = "bitrate"         # Sort order (highest bitrate first)
recurse = true           # Recurse into subdirectories
display_limit = 100      # Max files to display (0 = all)
```

CLI arguments take priority over config file values.

## Vres

Add video resolution labels to filenames based on actual video dimensions.

```console
Add video resolution to filenames

Usage: vres [OPTIONS] [PATH]

Arguments:
  [PATH]  Optional input directory or file path

Options:
  -D, --debug              Enable debug prints
  -x, --delete [<DELETE>]  Delete files with width or height smaller than limit (default: 500)
  -f, --force              Overwrite existing files
  -p, --print              Only print file names without renaming or deleting
  -r, --recurse            Recurse into subdirectories
  -v, --verbose            Print verbose output
  -h, --help               Print help
  -V, --version            Print version
```

## Vstats

Collect and print video file statistics.
Finds all video files in a directory, probes them with ffprobe in parallel,
and prints aggregate statistics including resolution distribution,
duration, codec, bitrate, and file size summaries.

```console
Collect and print video file statistics

Usage: vstats [OPTIONS] [PATH] [COMMAND]

Commands:
  completion  Generate shell completion script
  help        Print this message or the help of the given subcommand(s)

Arguments:
  [PATH]  Optional input directory or file

Options:
  -r, --recurse  Recurse into subdirectories
  -v, --verbose  Print verbose per-file output
  -h, --help     Print help
  -V, --version  Print version
```

## Thumbs

Create thumbnail sheets for video files using ffmpeg.

```console
Create thumbnail sheets for video files using ffmpeg

Usage: thumbs [OPTIONS] [PATH] [COMMAND]

Commands:
  completion  Generate shell completion script
  help        Print this message or the help of the given subcommand(s)

Arguments:
  [PATH]  Optional input directory or file

Options:
  -f, --force              Overwrite existing thumbnail files
  -p, --print              Print commands without running them
  -r, --recurse            Recurse into subdirectories
  -c, --cols <COLS>        Number of columns in the thumbnail grid
  -w, --rows <ROWS>        Number of rows in the thumbnail grid
  -s, --scale <WIDTH>      Thumbnail width in pixels
  -a, --padding <PIXELS>   Padding between tiles in pixels
  -t, --fontsize <SIZE>    Font size for timestamp overlay
  -q, --quality <QUALITY>  JPEG quality (1-31, lower is better)
  -v, --verbose            Print verbose output
  -h, --help               Print help
  -V, --version            Print version
```

## Visaparse

Parse Finvoice credit card statements and output items as CSV and Excel sheet.

```console
Parse Finvoice XML credit card statement files

Usage: visaparse [OPTIONS] [PATH]

Arguments:
  [PATH]  Optional input directory or XML file path

Options:
  -o, --output <OUTPUT_PATH>  Optional output path (default is the input directory)
  -p, --print                 Only print information without writing to file
  -n, --number <NUMBER>       How many total sums to print with verbose output
  -v, --verbose               Print verbose output
  -h, --help                  Print help
  -V, --version               Print version
```

## Qtorrent

Add torrents to qBittorrent with automatic file renaming.
Parses `.torrent` files and adds them to qBittorrent,
automatically setting the output filename or folder name based on the torrent filename.
For multi-file torrents,
offers to rename the root folder and supports filtering files by extension, name, or minimum size.

```console
Add torrents to qBittorrent with automatic file renaming

Usage: qtorrent [OPTIONS] [PATH]... [COMMAND]

Commands:
  info        Show info and statistics for existing torrents in qBittorrent
  completion  Generate shell completion script
  help        Print this message or the help of the given subcommand(s)

Arguments:
  [PATH]...  Optional input paths with torrent files or directories

Options:
  -H, --host <HOST>          qBittorrent WebUI host
  -P, --port <PORT>          qBittorrent WebUI port
  -u, --username <USER>      qBittorrent WebUI username
  -w, --password <PASS>      qBittorrent WebUI password
  -s, --save-path <PATH>     Save path for downloaded files
  -c, --category <CATEGORY>  Category for the torrent
  -t, --tags <TAGS>          Tags for the torrent (comma-separated)
  -a, --paused               Add torrent in paused state
  -p, --dryrun               Print what would be done without actually adding torrents
  -o, --offline              Offline mode: skip qBittorrent connection entirely (implies dryrun)
  -y, --yes                  Skip confirmation prompts
  -e, --skip-ext <EXT>       File extensions to skip (e.g., nfo, txt)
  -k, --skip-dir <NAME>      Directory names to skip (case-insensitive full name match)
  -m, --min-size <MB>        Minimum file size in MB (files smaller than this will be skipped)
  -i, --include-images       Include image files (.jpg, .jpeg, .png)
  -M, --min-image-size <KB>  Minimum image file size in KB
  -r, --recurse              Recurse into subdirectories when searching for torrent files
  -x, --skip-existing        Skip rename prompts for existing torrents
  -v, --verbose              Print verbose output
  -h, --help                 Print help (see more with '--help')
  -V, --version              Print version
```

### Info subcommand

Show statistics for existing torrents: total count, total size,
completed size, downloading, and not-yet-started sizes.

Print modes:

- Default: summary statistics only
- `--list`: one line per torrent with progress, size, name, save path, and tags
- `--list --verbose`: same as list but also shows ratio, added date, and completed date
- `--verbose`: full multi-line detail per torrent with all fields

Torrents are sorted by name by default. Use `--sort` to change the order.

```console
Show info and statistics for existing torrents in qBittorrent

Usage: qtorrent info [OPTIONS]

Options:
  -s, --sort <SORT>      Sort torrents [default: name] [possible values: name, size, path]
  -l, --list             Print one line per torrent
  -H, --host <HOST>      qBittorrent WebUI host
  -P, --port <PORT>      qBittorrent WebUI port
  -u, --username <USER>  qBittorrent WebUI username
  -w, --password <PASS>  qBittorrent WebUI password
  -v, --verbose          Print verbose output
  -h, --help             Print help (see more with '--help')
```

## Slb

Check and format prose in code comments, docstrings, and Markdown with semantic line breaks.
Lines are broken at sentence and clause boundaries within a soft 120 character limit,
which may be exceeded by up to 10 characters when that gives a better break than stopping short.
`--strict` turns the limit into a hard cap that is never exceeded.
A sentence that does not fit on one line is spread evenly over the lines it needs.
In long sentences, a colon after an introduction of three words or more takes its own line
whenever the clause it introduces carries on past it,
so the explanation or list starts on a line of its own.
Text inside backticks, Markdown links, and inline formatting such as `**bold**`, `_italic_`,
and `~~strikethrough~~` is never broken, and a span that was split by hand is joined back together.
Semicolons are rewritten as separate sentences.
An em dash becomes a period and a new sentence, or a colon where the text before it names what follows.
A pair of dashes that encloses a short phrase becomes a pair of commas,
but a pair whose enclosed span carries its own verb reads as a full clause,
so it is left for a person to reword instead of becoming a comma splice.
A dash that is kept never ends or starts a line.
Aligned column blocks, such as the environment table of a usage comment, are left as they are.
In Python, the quotes of a docstring that needs more than one line are moved onto lines of their own,
while a docstring holding one sentence that fits is written on a single line with its quotes around it.
A Markdown list item keeps all of its prose on the line its marker starts when the prose fits there,
which keeps the list readable in the raw text,
and an item too long for one line is broken at its sentence and clause boundaries like any other prose.
Slash-delimited regex literals are protected across supported source languages.
When ambiguous slash syntax could hide a multiline string or comment, the remaining source is left unchanged.
With `--trailing`, a comment sharing a line with code is moved onto its own line above it.
That rule rewrites code lines and the result often reads worse than the original, so it is off by default.
A trailing comment whose code line already has a comment above it is reported but not moved,
since the two notes would be reflowed into one sentence.
With `--lines`, only the lines named are checked and fixed,
which keeps a run over an edited file from reflowing the paragraphs that were not touched.
The forms a selection can take are described under Line selection below.
The line limit is read from project config files such as `.editorconfig`, `rustfmt.toml`, and `pyproject.toml`.
Paths ignored by git are skipped, unless `--no-ignore` says otherwise.
Files are checked in parallel, one worker per core unless `--jobs` says otherwise,
and the report is printed in file order so a run is reproducible.
The default mode reports violations and exits with code 1.
Use `--fix` to rewrite files, `--print` to show a diff,
or `--stdin` to format text from stdin for editor and git hook integration.

```console
Check and format prose in comments, docstrings, and Markdown with semantic line breaks

Usage: slb [OPTIONS] [PATHS]... [COMMAND]

Commands:
  completion  Generate shell completion script
  help        Print this message or the help of the given subcommand(s)

Arguments:
  [PATHS]...  Files or directories to check. Defaults to the current directory

Options:
  -f, --fix                     Rewrite files in place
  -p, --print                   Show the changes fix mode would make without writing
  -j, --join-sentences          Also pack consecutive short sentences up to the line limit
  -w, --width <N>               Maximum line length including indentation and comment marker (default: from project config or 120)
  -i, --ignore-project-config   Do not read the line length from project config files such as .editorconfig, rustfmt.toml, or pyproject.toml
  -R, --rules <RULES>           Rules to enable [possible values: too-long, mid-clause, semicolon, em-dash, trailing, docstring]
  -T, --trailing                Also move trailing comments to their own line above the code
  -l, --lines <RANGES>          Only check and fix these lines, for example "10-25" or "src/main.rs:14"
  -e, --extensions <EXTENSION>  Only process files with these extensions
  -x, --exclude <PATTERN>       Skip paths with a directory or file name equal to this text, in addition to the default excludes
  -n, --no-ignore               Do not skip paths ignored by git
  -t, --type <KIND>             Force the file kind, required with --stdin [possible values: rust, c, javascript, go, python, shell, toml, yaml, dockerfile, makefile, cmake, ruby, sql, lua, markdown]
  -s, --stdin                   Read text from stdin and write the formatted result to stdout
  -b, --word-break              Allow plain word boundaries even when a semantic boundary also fits
  -S, --strict                  Treat the width as a hard cap instead of allowing a small overflow past it
  -J, --jobs <N>                Number of worker threads, 0 for one per core [default: 0]
  -q, --quiet                   Only print the summary
  -v, --verbose                 Print processed files and the resolved line width
  -h, --help                    Print help (see more with '--help')
  -V, --version                 Print version
```

### Line selection

`--lines` limits both checking and fixing to the lines it names,
so a run over a file that was just edited leaves the paragraphs that were not touched alone.
This is what makes the tool usable from an editor hook or from a coding agent,
which changes a handful of lines and wants a diff of the same size.

A value is either lines and ranges for a single file,
or a location in the form the violation report prints.

| Value | Selects |
| --- | --- |
| `14` | line 14 |
| `10-25` | lines 10 to 25 |
| `10-25,40` | lines 10 to 25 and line 40 |
| `src/main.rs:14` | line 14 of that file |
| `src/main.rs:14-20` | lines 14 to 20 of that file |
| `src/main.rs:14:33` | line 14 of that file, the column is ignored |
| `src/main.rs:14,20` | lines 14 and 20 of that file |
| `src/a.rs:5,src/b.rs:9` | line 5 of one file and line 9 of another |

Commas separate the parts of a value,
and a part without a path belongs to the file the part before it named,
so the path only has to be written once.
The column is ignored so that a line of the report can be pasted back in as the thing to fix.
Line numbers are read from the end of a value, so a Windows path keeps its drive letter.
`--lines` can be repeated, and each one starts over,
so a plain range in one of them never picks up the file named in another.

A location names the file to work on,
so no path argument is needed when the value already carries one.
A file named that way also skips the extension filter,
which means a location works for a file type the config does not list.

```shell
# Fix only the lines that were edited, in one file
slb --fix --lines 10-25 src/main.rs

# Fix one line of one file, naming no path of its own
slb --fix --lines src/main.rs:14

# Paste a reported violation back in, column and all
slb --fix --lines client/GameMain.cpp:1414:33

# Select several ranges of the same file, writing the path once
slb --fix --lines src/main.rs:14,20-25,40

# Select lines in more than one file
slb --fix --lines src/a.rs:5,src/b.rs:9

# Report the violations of the changed lines without fixing them
slb --lines src/main.rs:14-20

# Format the selected lines of text on stdin
slb --stdin --type rust --lines 10-25 < src/main.rs
```

Selecting a line takes the whole paragraph or comment block holding it,
since reflow joins and splits a paragraph as one unit,
and half of a reflowed paragraph would read worse than either whole.
A trailing comment is matched line by line instead, because that rule works that way.

The selection applies to the default reporting mode as well as to `--fix`,
so a check over the changed lines alone still exits with code 1 when one of them is wrong.
After a fix the selection follows the lines to where they moved,
so a violation that could not be repaired is still reported even when the fix pushed it further down the file.

Two combinations are refused rather than guessed at.
Plain ranges with more than one file would apply the same line numbers to every one of them,
so a location is required once the run covers more than a single file.
Mixing plain ranges with locations in one value leaves it unclear which file the plain ranges belong to.
With `--stdin` only plain ranges are accepted, since the text has no path.

## Vtag

```console
Create git version tags for a project (Rust, C++, Python)

Usage: vtag [OPTIONS] [PATH] [COMMAND]

Commands:
  completion  Generate shell completion script
  help        Print this message or the help of the given subcommand(s)

Arguments:
  [PATH]  Optional git repository path. Defaults to current directory

Options:
  -d, --dryrun   Only print information without creating or pushing tags
  -p, --push     Push tags to remote
  -n, --new      Only push new tags that did not exist locally
  -s, --single   Use a single push to push all tags
  -v, --verbose  Print verbose output
  -h, --help     Print help
  -V, --version  Print version
```

## Development

### Required Tools

```shell
cargo install --locked cargo-nextest
cargo install --locked cargo-llvm-cov
```

### Testing

This project uses [cargo-nextest](https://nexte.st/) for faster test execution with better output.

```shell
# Run tests with nextest
cargo nextest run

# Run tests with standard cargo test
cargo test
```

### Code Coverage

Code coverage is generated using [cargo-llvm-cov](https://github.com/taiki-e/cargo-llvm-cov).

```shell
# Run tests with coverage (text output)
cargo llvm-cov nextest

# Generate HTML coverage report
cargo llvm-cov nextest --html

# Generate and open HTML report in browser
cargo llvm-cov nextest --html --open
```

The HTML report is generated in `target/llvm-cov/html/`.

### Benchmarks

Benchmarks use [Criterion.rs](https://github.com/criterion-rs/criterion.rs) and live in `benches/`.

```shell
# Run all benchmarks
cargo bench

# Run a specific benchmark suite
cargo bench --bench date
cargo bench --bench dir_move
cargo bench --bench dupe_find
cargo bench --bench format
cargo bench --bench lib
cargo bench --bench resolution
cargo bench --bench semantic_line_breaks

# Run benchmarks matching a filter pattern
cargo bench -- normalize_stem

# Quick benchmark run for faster local feedback
cargo bench -- --quick
```

Criterion reports are generated under `target/criterion/`.

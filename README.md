# cli-tools

Various CLI helper tools in one Rust project.
Produces separate binaries for each tool.

```shell
./build.sh
./install.sh
```

## Development

### Testing

This project uses [cargo-nextest](https://nexte.st/) for faster test execution with better output.

```shell
# Run tests with nextest (recommended)
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

### Required Tools

```shell
cargo install cargo-nextest
cargo install cargo-llvm-cov
```

## Configuration

The CLI binaries can be configured with a user config file in addition to the CLI arguments.
The config file goes to `~/.config/cli-tools.toml`,
and has separate sections for each binary.

An example config [cli-tools.toml](./cli-tools.toml) is provided in the repo root.

## Div

```console
Print divider comment with centered text

Usage: div [OPTIONS] [TEXT]...

Arguments:
  [TEXT]...  Optional divider text(s)

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

Usage: dirmove [OPTIONS] [PATH]

Arguments:
  [PATH]  Optional input directory or file

Options:
  -a, --auto                      Auto-confirm all prompts without asking
  -c, --create                    Create directories for files with matching prefixes
  -D, --debug                     Print debug information
  -f, --force                     Overwrite existing files
  -n, --include <INCLUDE>         Include files that match the given pattern
  -e, --exclude <EXCLUDE>         Exclude files that match the given pattern
  -i, --ignore <IGNORE>           Ignore prefix when matching filenames
  -I, --ignore-group <GROUP>      Group name to ignore (exact match, won't be offered as new directory)
  -P, --ignore-group-part <PART>  Ignore groups containing this part (substring match in any part of group name)
  -o, --override <OVERRIDE>       Override prefix to use for directory names
  -u, --unpack <NAME>             Directory name to "unpack" by moving its contents to the parent directory
  -g, --group <COUNT>             Minimum number of matching files needed to create a group
  -m, --min-chars <CHARS>         Minimum character count for prefixes to be valid group names (excluding dots)
  -p, --print                     Only print changes without moving files
  -r, --recurse                   Recurse into subdirectories
  -l, --completion <SHELL>        Generate shell completion [possible values: bash, elvish, fish, powershell, zsh]
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

Usage: dupefind [OPTIONS] [PATHS]...

Arguments:
  [PATHS]...  Input directories to search

Options:
  -g, --pattern <PATTERN>      Identifier patterns to search for (regex)
  -e, --extension <EXTENSION>  File extensions to include
  -m, --move                   Move duplicates to a "Duplicates" directory
  -p, --print                  Only print changes without moving files
  -r, --recurse                Recurse into subdirectories
  -d, --default                Use default paths from config file
  -l, --completion <SHELL>     Generate shell completion [possible values: bash, elvish, fish, powershell, zsh]
  -v, --verbose                Print verbose output
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
  -e, --extensions <EXTENSION>  Specify file extension(s)
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

Usage: vconvert [OPTIONS] [PATH]

Arguments:
  [PATH]  Optional input directory or file

Options:
  -a, --all                          Convert all known video file types
  -b, --bitrate <LIMIT>              Skip files with bitrate lower than LIMIT kbps [default: 8000]
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
  -m, --skip-remux                   Skip remuxing
  -s, --sort [<ORDER>]               Sort files [possible values: bitrate, size, size-asc, duration, duration-asc, resolution, resolution-asc, impact, name]
  -l, --completion <SHELL>           Generate shell completion [possible values: bash, elvish, fish, powershell, zsh]
  -v, --verbose                      Print verbose output
  -D, --from-db                      Process files from database instead of scanning
  -C, --clear-db                     Clear all entries from the database
  -S, --show-db                      Show database statistics and contents
  -E, --list-extensions              List file extension counts in the database
  -B, --max-bitrate <MAX_BITRATE>    Maximum bitrate in kbps
  -u, --min-duration <MIN_DURATION>  Minimum duration in seconds
  -U, --max-duration <MAX_DURATION>  Maximum duration in seconds
  -L, --display-limit <LIMIT>        Maximum number of files to display
  -h, --help                         Print help (see more with '--help')
  -V, --version                      Print version
```

### Filter Options

The filter options (`-b`/`--bitrate`, `-B`/`--max-bitrate`, `-u`/`--min-duration`, `-U`/`--max-duration`, `-t`/`--extension`, `-c`/`--count`) work for both normal scanning mode and database mode (`-D`/`--from-db`, `-S`/`--show-db`).

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
```

### Configuration

Filter options can also be set in the config file (`~/.config/cli-tools.toml`):

```toml
[video_convert]
bitrate = 8000           # Minimum bitrate threshold (kbps)
max_bitrate = 50000      # Maximum bitrate threshold (kbps)
min_duration = 60        # Minimum duration (seconds)
max_duration = 7200      # Maximum duration (seconds)
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

## Thumbs

Create thumbnail sheets for video files using ffmpeg.

```console
Create thumbnail sheets for video files using ffmpeg

Usage: thumbs [OPTIONS] [PATH]

Arguments:
  [PATH]  Optional input directory or file

Options:
  -f, --force               Overwrite existing thumbnail files
  -p, --print               Print commands without running them
  -r, --recurse             Recurse into subdirectories
  -c, --cols <COLS>         Number of columns in the thumbnail grid
  -w, --rows <ROWS>         Number of rows in the thumbnail grid
  -s, --scale <WIDTH>       Thumbnail width in pixels
  -a, --padding <PIXELS>    Padding between tiles in pixels
  -t, --fontsize <SIZE>     Font size for timestamp overlay
  -q, --quality <QUALITY>   JPEG quality (1-31, lower is better)
  -l, --completion <SHELL>  Generate shell completion [possible values: bash, elvish, fish, powershell, zsh]
  -v, --verbose             Print verbose output
  -h, --help                Print help
  -V, --version             Print version
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
For multi-file torrents, offers to rename the root folder and supports filtering files by extension, name, or minimum size.

```console
Add torrents to qBittorrent with automatic file renaming

Usage: qtorrent [OPTIONS] [PATH]... [COMMAND]

Commands:
  info        Show info and statistics for existing torrents in qBittorrent
  completion  Generate shell completion script
  help        Print this message or the help of the given subcommand(s)

Arguments:
  [PATH]...  Optional input path(s) with torrent files or directories

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
  -o, --offline              Offline mode: skip qBittorrent connection entirely (implies --dryrun)
  -y, --yes                  Skip confirmation prompts
  -e, --skip-ext <EXT>       File extensions to skip (e.g., nfo, txt, jpg)
  -k, --skip-dir <NAME>      Directory names to skip (case-insensitive full name match)
  -m, --min-size <MB>        Minimum file size in MB (files smaller than this will be skipped)
  -r, --recurse              Recurse into subdirectories when searching for torrent files
  -x, --skip-existing        Skip rename prompts for existing/duplicate torrents
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
  -s, --sort <SORT>      Sort torrents by the given field [default: name] [possible values: name, size, path]
  -l, --list             List all torrents (one per line)
  -H, --host <HOST>      qBittorrent WebUI host
  -P, --port <PORT>      qBittorrent WebUI port
  -u, --username <USER>  qBittorrent WebUI username
  -w, --password <PASS>  qBittorrent WebUI password
  -v, --verbose          Print verbose output
  -h, --help             Print help
```

## Vtag

```console
Create git version tags for a Rust project

Usage: vtag [OPTIONS] [PATH]

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

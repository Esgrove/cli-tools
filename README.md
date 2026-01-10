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
  -a, --auto                 Auto-confirm all prompts without asking
  -c, --create               Create directories for files with matching prefixes
  -D, --debug                Print debug information
  -f, --force                Overwrite existing files
  -n, --include <INCLUDE>    Include files that match the given pattern
  -e, --exclude <EXCLUDE>    Exclude files that match the given pattern
  -i, --ignore <IGNORE>      Ignore prefix when matching filenames
  -o, --override <OVERRIDE>  Override prefix to use for directory names
  -g, --group <COUNT>        Minimum number of matching files needed to create a group [default: 3]
  -p, --print                Only print changes without moving files
  -r, --recurse              Recurse into subdirectories
  -l, --completion <SHELL>   Generate shell completion [possible values: bash, elvish, fish, powershell, zsh]
  -v, --verbose              Print verbose output
  -h, --help                 Print help
  -V, --version              Print version
```

## Dots

```console
Rename files to use dot formatting

Usage: dots [OPTIONS] [PATH]

Arguments:
  [PATH]  Optional input directory or file

Options:
  -c, --case                                Convert casing
      --debug                               Enable debug prints
  -d, --directory                           Rename directories
  -f, --force                               Overwrite existing files
  -n, --include <INCLUDE>                   Include files that match the given pattern
  -e, --exclude <EXCLUDE>                   Exclude files that match the given pattern
  -i, --increment                           Increment conflicting file name with running index
  -p, --print                               Only print changes without renaming files
  -r, --recurse                             Recurse into subdirectories
  -x, --prefix <PREFIX>                     Append prefix to the start
  -b, --prefix-dir                          Prefix files with directory name
  -j, --suffix-dir                          Suffix files with directory name
  -u, --suffix <SUFFIX>                     Append suffix to the end
  -s, --substitute <PATTERN> <REPLACEMENT>  Substitute pattern with replacement in filenames
  -m, --random                              Remove random strings
  -z, --remove <PATTERN>                    Remove pattern from filenames
  -g, --regex <PATTERN> <REPLACEMENT>       Substitute regex pattern with replacement in filenames
  -y, --year                                Assume year is last in short dates
  -l, --completion <SHELL>                  Create shell completion [possible values: bash, elvish, fish, powershell, zsh]
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
  -e, --extension <EXTENSION>  Video file extensions to include
  -m, --move-files             Move duplicates to a "Duplicates" directory
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
  -a, --all                          Convert all known video file types (default is only .mp4 and .mkv)
  -b, --bitrate <LIMIT>              Minimum bitrate threshold in kbps [default: 8000]
  -B, --max-bitrate <MAX_BITRATE>    Maximum bitrate filter (kbps)
  -c, --count <COUNT>                Limit the number of files to process
  -C, --clear-db                     Clear all entries from the database
  -d, --delete                       Delete input files immediately instead of moving to trash
  -D, --from-db                      Process files from database instead of scanning
  -e, --exclude <EXCLUDE>            Exclude files that match the given pattern
  -f, --force                        Overwrite existing output files
  -k, --skip-convert                 Skip conversion
  -l, --completion <SHELL>           Generate shell completion [possible values: bash, elvish, fish, powershell, zsh]
  -m, --skip-remux                   Skip remuxing
  -n, --include <INCLUDE>            Include files that match the given pattern
  -o, --other                        Convert all known video file types except MP4 files
  -p, --print                        Print commands without running them
  -r, --recurse                      Recurse into subdirectories
  -s, --sort <ORDER>                 Sort files [possible values: bitrate, bitrate-asc, size, size-asc, duration, duration-asc, resolution, resolution-asc, name, name-desc]
  -E, --list-extensions              List file extension counts in the database
  -S, --show-db                      Show database statistics and contents
  -t, --extension <EXTENSION>        Filter by file extension (can be repeated)
  -u, --min-duration <SECONDS>       Minimum duration filter (seconds)
  -U, --max-duration <SECONDS>       Maximum duration filter (seconds)
  -v, --verbose                      Print verbose output
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
  -d, --debug              Enable debug prints
  -x, --delete [<DELETE>]  Delete files with width or height smaller than limit (default: 500)
  -f, --force              Overwrite existing files
  -p, --print              Only print file names without renaming or deleting
  -r, --recurse            Recursive directory iteration
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
  -n, --number <NUMBER>       How many total sums to print with verbose output [default: 20]
  -v, --verbose               Print verbose output
  -h, --help                  Print help
  -V, --version               Print version
```

## Qtorrent

Add torrents to qBittorrent with automatic file renaming.
Parses single-file `.torrent` files and adds them to qBittorrent,
automatically setting the output filename based on the torrent filename.

```console
Add torrents to qBittorrent with automatic file renaming

Usage: qtorrent [OPTIONS] [PATH]...

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
  -y, --yes                  Skip confirmation prompts
  -e, --skip-ext <EXT>       File extensions to skip (e.g., nfo, txt, jpg)
  -k, --skip-name <NAME>     Directory names to skip (case-insensitive full name match)
  -m, --min-size <MB>        Minimum file size in MB (files smaller than this will be skipped)
  -r, --recurse              Recurse into subdirectories when searching for torrent files
  -l, --completion <SHELL>   Generate shell completion [possible values: bash, elvish, fish, powershell, zsh]
  -v, --verbose              Print verbose output
  -h, --help                 Print help (see more with '--help')
  -V, --version              Print version
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

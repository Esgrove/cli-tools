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
  -f, --force               Overwrite existing files
  -n, --include <INCLUDE>   Include files that match the given pattern
  -e, --exclude <EXCLUDE>   Exclude files that match the given pattern
  -p, --print               Only print changes without renaming files
  -r, --recurse             Recurse into subdirectories
  -l, --completion <SHELL>  Generate shell completion [possible values: bash, elvish, fish, powershell, zsh]
  -v, --verbose             Print verbose output
  -h, --help                Print help
  -V, --version             Print version
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

```console
Convert video files to HEVC (H.265) format using ffmpeg and NVENC

Usage: vconvert [OPTIONS] [PATH]

Arguments:
  [PATH]  Optional input directory or file

Options:
  -a, --all                    Convert all known video file types (default is only .mp4 and .mkv)
  -b, --bitrate <LIMIT>        Skip files with bitrate lower than LIMIT kbps [default: 8000]
  -d, --delete                 Delete input files immediately instead of moving to trash
  -p, --print                  Print commands without running them
  -f, --force                  Overwrite existing output files
  -i, --include <INCLUDE>      Include files that match the given pattern
  -e, --exclude <EXCLUDE>      Exclude files that match the given pattern
  -t, --extension <EXTENSION>  Override file extensions to convert
  -n, --number <NUMBER>        Number of files to convert [default: 1]
  -o, --other                  Convert all known video file types except MP4 files
  -r, --recurse                Recurse into subdirectories
  -c, --skip-convert           Skip conversion
  -m, --skip-remux             Skip remuxing
  -s, --sort-by-bitrate        Sort files by bitrate (highest first)
  -l, --completion <SHELL>     Generate shell completion [possible values: bash, elvish, fish, powershell, zsh]
  -v, --verbose                Print verbose output
  -h, --help                   Print help
  -V, --version                Print version
```

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

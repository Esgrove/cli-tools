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
  -c, --char <CHARACTER>  Divider length in number of characters [default: %]
  -a, --align             Align multiple divider texts to same start position
  -h, --help              Print help
  -V, --version           Print version
```

## Dots

```console
Replace whitespaces in filenames with dots

Usage: dots [OPTIONS] [PATH]

Arguments:
  [PATH]  Optional input directory or file

Options:
  -f, --force    Overwrite existing files
  -p, --print    Only print changes without renaming
  -v, --verbose  Verbose output
  -h, --help     Print help
  -V, --version  Print version
```

## Flip-date

Rename files and directories to use `yyyy.mm.dd` date format for files,
and `yyyy-mm-dd` for directories.

```console
Flip dates in file and directory names to start with year

Usage: flipdate [OPTIONS] [PATH]

Arguments:
  [PATH]  Optional input directory or file

Options:
  -d, --dir        Use directory rename mode
  -p, --print      Only print changes without renaming
  -r, --recursive  Use recursive path handling
  -h, --help       Print help
  -V, --version    Print version
```

## Visa-parse

Parse Finvoice credit card statements and output items as CSV and Excel sheet.

```console
Parse credit card Finvoice XML files

Usage: visaparse [OPTIONS] [PATH]

Arguments:
  [PATH]  Optional input directory or XML file path

Options:
  -o, --output <OUTPUT_PATH>  Optional output path (default is same as input dir)
  -p, --print                 Only print info without writing to file
  -v, --verbose               Verbose output
  -h, --help                  Print help
  -V, --version               Print version
```

## Version tag

```console
Create git version tags for a Rust project

Usage: vtag [OPTIONS] [PATH]

Arguments:
  [PATH]  Optional git repository path. Defaults to current directory

Options:
  -d, --dryrun   Only print information without creating or pushing tags
  -p, --push     Push tags to remote
  -v, --verbose  Verbose output
  -h, --help     Print help
  -V, --version  Print version
```

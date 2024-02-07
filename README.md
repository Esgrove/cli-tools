# cli-tools

Various CLI helper tools in one Rust project.
Produces separate binaries for each tool.

```shell
./build.sh
./install.sh
```

## Dots

```console
Replace whitespaces in filenames with dots

Usage: dots [OPTIONS] <PATH>

Arguments:
  <PATH>  Input directory or file

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

Usage: flipdate [OPTIONS] <PATH>

Arguments:
  <PATH>  Input directory or file

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

Usage: visaparse [OPTIONS] <PATH>

Arguments:
  <PATH>  Input directory or XML file path

Options:
  -o, --output <OUTPUT_PATH>  Optional output path (default is same as input dir)
  -p, --print                 Only print items, don't write to file
  -v, --verbose               Verbose output
  -h, --help                  Print help
  -V, --version               Print version
```

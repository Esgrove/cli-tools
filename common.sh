#!/bin/bash
set -eo pipefail

# Absolute path to the repo root, which is the directory of this file.
# It is not taken from the current directory, so running a script from inside another git repository still targets this one.
REPO_ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
export REPO_ROOT

# Check platform: BSD (Mac) and GNU (Linux & Git for Windows) coreutils implementations have some differences
# for example in the available command line options.
case "$(uname -s)" in
    "Darwin")
        BASH_PLATFORM="mac"
        ;;
    "MINGW"*)
        BASH_PLATFORM="windows"
        ;;
    *)
        BASH_PLATFORM="linux"
        ;;
esac
export BASH_PLATFORM

# Print a message with green color
print_green() {
    printf "\e[1;49;32m%s\e[0m\n" "$1"
}

# Print a message with magenta color
print_magenta() {
    printf "\e[1;49;35m%s\e[0m\n" "$1"
}

# Print a message with red color
print_red() {
    printf "\e[1;49;31m%s\e[0m\n" "$1"
}

# Print a message with yellow color
print_yellow() {
    printf "\e[1;49;33m%s\e[0m\n" "$1"
}

# Print an error and exit the program
print_error_and_exit() {
    print_red "ERROR: $1"
    # use exit code if given as second argument, otherwise default to 1
    exit "${2:-1}"
}

# Print usage and exit the program. An optional error message can be given as well.
print_usage_and_exit() {
    if [ $# -eq 1 ]; then
        print_red "ERROR: $1"
    fi
    if [ -z "$USAGE" ]; then
        print_red "No usage text provided in variable USAGE"
    else
        echo "$USAGE"
    fi
    # use exit code if given as second argument, otherwise default to 1
    exit "${2:-1}"
}

# Get all Rust executable names from the workspace crate manifests
get_rust_executable_names() {
    local suffix=""
    if [ "$BASH_PLATFORM" = "windows" ]; then
        suffix=".exe"
    fi
    awk -F'=' -v suffix="$suffix" '/\[\[bin\]\]/,/name/ {
        if ($1 ~ /name/) {
            gsub(/[ "\r]/, "", $2)
            print $2 suffix
        }
    }' "$REPO_ROOT"/crates/*/Cargo.toml
}

# Get the directories of the workspace crates that define at least one binary
get_rust_binary_crate_directories() {
    local manifest
    for manifest in "$REPO_ROOT"/crates/*/Cargo.toml; do
        if grep -q '^\[\[bin\]\]' "$manifest"; then
            dirname "$manifest"
        fi
    done
}

# if DRYRUN or DRY_RUN has been set, only print commands instead of running them
run_command() {
    if [ "$DRY_RUN" = true ] || [ "$DRYRUN" = true ]; then
        echo "DRYRUN: $*"
    else
        echo "Running: $*"
        "$@"
    fi
}

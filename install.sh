#!/bin/bash
set -eo pipefail

# Install the Rust binaries to path.

# Import common functions
DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=./common.sh
source "$DIR/common.sh"

USAGE="Usage: $0 [OPTIONS]

Install all Rust binaries to the Cargo bin directory.

OPTIONS: All options are optional
    -f | --force
        Force recompilation by touching source files.

    --help
        Display these instructions.

    --verbose
        Display commands being executed.
"

FORCE=false

while [ $# -gt 0 ]; do
    case "$1" in
        -f | --force)
            FORCE=true
            ;;
        --help)
            echo "$USAGE"
            exit 1
            ;;
        --verbose)
            set -x
            ;;
        *)
            print_error_and_exit "Unknown option: $1"
            ;;
    esac
    shift
done

if [ -z "$(command -v cargo)" ]; then
    print_error_and_exit "Cargo not found in path. Maybe install rustup?"
fi

print_magenta "Installing binaries..."
cd "$REPO_ROOT"

if [ "$FORCE" = true ]; then
    print_yellow "Forcing recompilation..."

    # Remove existing release binaries to force recompilation with current version number.
    if [ -d "target/release" ]; then
        for executable in $(get_rust_executable_names); do
            rm -f "target/release/${executable}"
        done
    fi

    # Touch source files to ensure recompilation
    touch src/lib.rs
    find src/bin -name "*.rs" -exec touch {} \;
fi

cargo install --force --path "$REPO_ROOT"
echo ""

print_green "Installed binaries:"
for executable in $(get_rust_executable_names); do
    if [ -z "$(command -v "$executable")" ]; then
        print_error_and_exit "Binary not found. Is the Cargo install directory in path?"
    fi
    echo "$($executable --version) from $(which "$executable")"
done

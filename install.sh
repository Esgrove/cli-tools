#!/bin/bash
set -eo pipefail

# Install the Rust binaries to path through the xtask install group,
# which builds every binary in one release build and copies them to the Cargo bin directory.

# Import common functions
DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=./common.sh
source "$DIR/common.sh"

USAGE="Usage: $0 [OPTIONS]

Install all Rust binaries to the Cargo bin directory with the xtask install group from xtask.toml.
One release build compiles every binary, so cargo runs their link steps in parallel.

OPTIONS: All options are optional
    -f | --force
        Force recompilation by touching source files.

    -h | --help
        Display these instructions.

    -v | --verbose
        Display commands being executed.
"

FORCE=false

while [ $# -gt 0 ]; do
    case "$1" in
        -f | --force)
            FORCE=true
            ;;
        -h | --help)
            echo "$USAGE"
            exit 0
            ;;
        -v | --verbose)
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

# Print the names of the long running filefind processes that are currently running.
# A running executable is locked on Windows, so cargo install cannot replace it.
running_background_processes() {
    if [ "$BASH_PLATFORM" = "windows" ]; then
        # Get-Process also lists elevated processes, which the daemon usually is.
        powershell.exe -NoProfile -Command \
            "Get-Process -Name filefindd,filefind-tray -ErrorAction SilentlyContinue | Select-Object -ExpandProperty ProcessName" |
              tr -d '\r'
    else
        local process_name
        for process_name in filefindd filefind-tray; do
            if pgrep -x "$process_name" > /dev/null 2>&1; then
                echo "$process_name"
            fi
        done
    fi
}

RUNNING_PROCESSES=$(running_background_processes)
if [ -n "$RUNNING_PROCESSES" ]; then
    print_error_and_exit "Stop these before installing: $(echo "$RUNNING_PROCESSES" | sort -u | tr '\n' ' ')"
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
    find crates -path "*/src/*" -name "*.rs" -exec touch {} \;
fi

run_xtask install
echo ""

print_green "Installed binaries:"
for executable in $(get_rust_executable_names); do
    if [ -z "$(command -v "$executable")" ]; then
        print_error_and_exit "Binary not found. Is the Cargo install directory in path?"
    fi
    echo "$("$executable" --version) from $(command -v "$executable")"
done

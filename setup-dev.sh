#!/bin/bash
set -eo pipefail

# Install the tools for developing this repository:
# the updated Rust toolchains, the cargo subcommands the checks use, slb from this repository,
# the extra command line tools from Homebrew on macOS, and the prek git hooks.

# Import common functions
DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=./common.sh
source "$DIR/common.sh"

USAGE="Usage: $0 [OPTIONS]

Install the development tools of this repository, updating the installed Rust toolchains first with rustup update.
Cargo tools are installed or upgraded with cargo install --locked, which skips the ones already up to date.
On macOS, the Homebrew packages that are missing are installed with brew install.
Other platforms print the missing tools to install with their package manager.

OPTIONS: All options are optional
    -d | --dryrun
        Only print the commands instead of running them.

    -h | --help
        Display these instructions.

    -v | --verbose
        Display commands being executed.
"

# Cargo packages for the checks, the tests, coverage, fuzzing, and the xtask bump and update groups.
CARGO_PACKAGES=(
    cargo-edit
    cargo-fuzz
    cargo-llvm-cov
    cargo-nextest
)

# Homebrew formulae, each providing a command of the same name.
# ffmpeg also provides the ffprobe that the video tools use.
BREW_PACKAGES=(
    bun
    ffmpeg
    jq
    prek
    shellcheck
    shfmt
    uv
)

while [ $# -gt 0 ]; do
    case "$1" in
        -d | --dryrun)
            export DRYRUN=true
            ;;
        -h | --help)
            echo "$USAGE"
            exit 0
            ;;
        -v | --verbose)
            set -x
            ;;
        *)
            print_usage_and_exit "Unknown option: $1"
            ;;
    esac
    shift
done

if [ -z "$(command -v cargo)" ] || [ -z "$(command -v rustup)" ]; then
    print_error_and_exit "cargo or rustup not found in path. Install Rust first: https://rustup.rs"
fi

# Update the installed Rust toolchains and rustup,
# then install the nightly toolchain for fuzzing and the LLVM tools that cargo-llvm-cov needs.
install_rust_components() {
    print_magenta "Updating Rust toolchains..."
    run_command rustup update
    run_command rustup component add llvm-tools-preview
    run_command rustup toolchain install nightly --profile minimal
}

# Install or upgrade the cargo packages, and slb from this repository for checking semantic line breaks.
install_cargo_packages() {
    print_magenta "Installing cargo packages..."
    local package
    for package in "${CARGO_PACKAGES[@]}"; do
        run_command cargo install --locked "$package"
    done
    run_command cargo install --locked --path "$REPO_ROOT/crates/slb"
}

# Install the missing Homebrew packages on macOS, or list the missing tools on other platforms.
install_extra_tools() {
    local missing=()
    local package
    for package in "${BREW_PACKAGES[@]}"; do
        if [ -z "$(command -v "$package")" ]; then
            missing+=("$package")
        fi
    done

    if [ "${#missing[@]}" -eq 0 ]; then
        print_green "Extra tools already installed"
        return
    fi

    if [ "$BASH_PLATFORM" != "mac" ]; then
        print_yellow "Install these tools with your package manager: ${missing[*]}"
        return
    fi
    if [ -z "$(command -v brew)" ]; then
        print_error_and_exit "brew not found in path. Install Homebrew first: https://brew.sh"
    fi
    print_magenta "Installing Homebrew packages: ${missing[*]}"
    run_command brew install "${missing[@]}"
}

# Install the git hooks from .pre-commit-config.yaml with prek when it is available.
install_git_hooks() {
    if [ -z "$(command -v prek)" ] && [ "$DRYRUN" != true ]; then
        print_yellow "Skipping git hooks: prek not found in path"
        return
    fi
    print_magenta "Installing git hooks..."
    (cd "$REPO_ROOT" && run_command prek install)
}

install_rust_components
install_cargo_packages
install_extra_tools
install_git_hooks

print_green "Development tools installed"

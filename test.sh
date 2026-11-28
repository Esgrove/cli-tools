#!/bin/bash
set -eo pipefail

# Run every check and test in the workspace through the xtask test group:
# formatting, lints, unit, integration, CLI, property, and doc tests, and each cargo-fuzz target for a fixed time.

# Import common functions
DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=./common.sh
source "$DIR/common.sh"

FUZZ_SECONDS_DEFAULT=30
FUZZ_SECONDS=$FUZZ_SECONDS_DEFAULT
FUZZ_REQUIRED=false
XTASK_ARGS=()

USAGE="Usage: $0 [OPTIONS]

Run the xtask test group from xtask.toml: formatting, clippy, all workspace tests including the property tests,
and each cargo-fuzz target in fuzz/, in parallel where they do not share the cargo target directory.
Fuzzing needs the nightly toolchain and cargo-fuzz, and is skipped with a notice when either is missing.

OPTIONS: All options are optional
    -h | --help
        Display these instructions.

    -c | --cases <COUNT>
        Number of cases for each property test, exported as PROPTEST_CASES. Defaults to the proptest default of 256.

    -f | --fuzz
        Require fuzzing: fail instead of skipping when nightly or cargo-fuzz is missing.

    -l | --no-lint
        Skip clippy.

    -n | --no-fuzz
        Skip fuzzing.

    -t | --fuzz-time <SECONDS>
        Seconds to fuzz each target. Defaults to $FUZZ_SECONDS_DEFAULT.

    -v | --verbose
        Display commands being executed.
"

while [ $# -gt 0 ]; do
    case "$1" in
        -h | --help)
            echo "$USAGE"
            exit 0
            ;;
        -c | --cases)
            [[ "${2:-}" =~ ^[0-9]+$ ]] || print_usage_and_exit "$1 needs a whole number of cases"
            export PROPTEST_CASES="$2"
            shift
            ;;
        -f | --fuzz)
            FUZZ_REQUIRED=true
            ;;
        -l | --no-lint)
            XTASK_ARGS+=(--no-lint)
            ;;
        -n | --no-fuzz)
            XTASK_ARGS+=(--no-fuzz)
            ;;
        -t | --fuzz-time)
            [[ "${2:-}" =~ ^[0-9]+$ ]] || print_usage_and_exit "$1 needs a whole number of seconds"
            FUZZ_SECONDS="$2"
            shift
            ;;
        -v | --verbose)
            set -x
            XTASK_ARGS+=(--verbose)
            ;;
        *)
            print_usage_and_exit "Unknown option: $1"
            ;;
    esac
    shift
done

if [ -z "$(command -v cargo)" ]; then
    print_error_and_exit "Cargo not found in path. Maybe install rustup?"
fi

export FUZZ_SECONDS FUZZ_REQUIRED

cd "$REPO_ROOT"
run_xtask "${XTASK_ARGS[@]}" test

print_green "All tests passed"

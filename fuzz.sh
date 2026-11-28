#!/bin/bash
set -eo pipefail

# Fuzz each cargo-fuzz target in fuzz/ for a fixed time, seeding slb with its test fixtures.
# Fuzzing needs the nightly toolchain and cargo-fuzz, and is skipped with a notice when either is missing.

# Import common functions
DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=./common.sh
source "$DIR/common.sh"

FUZZ_SECONDS_DEFAULT=30
FUZZ_SECONDS=${FUZZ_SECONDS:-$FUZZ_SECONDS_DEFAULT}
FUZZ_REQUIRED=${FUZZ_REQUIRED:-false}

USAGE="Usage: $0 [OPTIONS]

Fuzz each cargo-fuzz target in fuzz/ for a fixed time.
Needs the nightly toolchain and cargo-fuzz, and skips fuzzing with a notice when either is missing.
The FUZZ_SECONDS and FUZZ_REQUIRED environment variables set the defaults of --fuzz-time and --require.

OPTIONS: All options are optional
    -h | --help
        Display these instructions.

    -r | --require
        Fail instead of skipping when nightly or cargo-fuzz is missing.

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
        -r | --require)
            FUZZ_REQUIRED=true
            ;;
        -t | --fuzz-time)
            FUZZ_SECONDS="${2:-}"
            shift
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

[[ "$FUZZ_SECONDS" =~ ^[0-9]+$ ]] || print_usage_and_exit "Fuzz time needs a whole number of seconds, got '$FUZZ_SECONDS'"

# Whether the nightly toolchain and cargo-fuzz are both available.
fuzzing_available() {
    rustup run nightly rustc --version > /dev/null 2>&1 && cargo +nightly fuzz --version > /dev/null 2>&1
}

if ! fuzzing_available; then
    if [ "$FUZZ_REQUIRED" = true ]; then
        print_error_and_exit "Fuzzing needs the nightly toolchain and cargo-fuzz: rustup toolchain install nightly && cargo install cargo-fuzz"
    fi
    print_yellow "Skipping fuzzing: install the nightly toolchain and cargo-fuzz to run it"
    exit 0
fi

cd "$REPO_ROOT/fuzz"
targets=$(cargo +nightly fuzz list)
for target in $targets; do
    print_magenta "Fuzzing $target for $FUZZ_SECONDS seconds"
    corpus="corpus/$target"
    mkdir -p "$corpus"
    seeds=()
    if [ "$target" = "slb_format" ]; then
        seeds+=("$REPO_ROOT/crates/slb/tests/fixtures")
    fi
    if ! cargo +nightly fuzz run "$target" "$corpus" "${seeds[@]}" -- -max_total_time="$FUZZ_SECONDS"; then
        print_red "Fuzz target $target failed, crash inputs are in $REPO_ROOT/fuzz/artifacts/$target"
        print_red "Reproduce with: cd fuzz && cargo +nightly fuzz run $target artifacts/$target/<file>"
        exit 1
    fi
done

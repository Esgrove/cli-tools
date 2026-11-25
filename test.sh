#!/bin/bash
set -eo pipefail

# Run every test in the workspace: lints, unit, integration, CLI, property, and doc tests,
# then each cargo-fuzz target for a fixed time.

# Import common functions
DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=./common.sh
source "$DIR/common.sh"

FUZZ_SECONDS_DEFAULT=30
FUZZ_SECONDS=$FUZZ_SECONDS_DEFAULT
LINT=true
FUZZ=auto

USAGE="Usage: $0 [OPTIONS]

Run clippy, then all workspace tests including the property tests, then fuzz each cargo-fuzz target in fuzz/.
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
            [ -n "$2" ] || print_usage_and_exit "$1 needs a case count"
            export PROPTEST_CASES="$2"
            shift
            ;;
        -f | --fuzz)
            FUZZ=required
            ;;
        -l | --no-lint)
            LINT=false
            ;;
        -n | --no-fuzz)
            FUZZ=false
            ;;
        -t | --fuzz-time)
            [[ "$2" =~ ^[0-9]+$ ]] || print_usage_and_exit "$1 needs a whole number of seconds"
            FUZZ_SECONDS="$2"
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

if [ -z "$(command -v cargo)" ]; then
    print_error_and_exit "Cargo not found in path. Maybe install rustup?"
fi

cd "$REPO_ROOT"

# Run clippy over every target with warnings as errors.
run_lints() {
    print_magenta "Clippy"
    cargo clippy --workspace --all-targets -- -Dwarnings
}

# Run the unit, integration, CLI, and property tests, then the doc tests nextest does not run.
run_tests() {
    if [ -n "$PROPTEST_CASES" ]; then
        print_magenta "Tests with $PROPTEST_CASES property test cases"
    else
        print_magenta "Tests"
    fi
    if cargo nextest --version > /dev/null 2>&1; then
        cargo nextest run --workspace
        print_magenta "Doc tests"
        cargo test --workspace --doc
    else
        print_yellow "cargo-nextest not found, using cargo test"
        cargo test --workspace
    fi
}

# Whether the nightly toolchain and cargo-fuzz are both available.
fuzzing_available() {
    rustup run nightly rustc --version > /dev/null 2>&1 && cargo +nightly fuzz --version > /dev/null 2>&1
}

# Fuzz each target for the configured time, seeding slb with its test fixtures.
run_fuzzing() {
    if [ "$FUZZ" = false ]; then
        return
    fi
    if ! fuzzing_available; then
        if [ "$FUZZ" = required ]; then
            print_error_and_exit "Fuzzing needs the nightly toolchain and cargo-fuzz: rustup toolchain install nightly && cargo install cargo-fuzz"
        fi
        print_yellow "Skipping fuzzing: install the nightly toolchain and cargo-fuzz to run it"
        return
    fi

    cd "$REPO_ROOT/fuzz"
    local targets
    targets=$(cargo +nightly fuzz list)
    for target in $targets; do
        print_magenta "Fuzzing $target for $FUZZ_SECONDS seconds"
        local corpus="corpus/$target"
        mkdir -p "$corpus"
        local seeds=()
        if [ "$target" = "slb_format" ]; then
            seeds+=("$REPO_ROOT/crates/slb/tests/fixtures")
        fi
        if ! cargo +nightly fuzz run "$target" "$corpus" "${seeds[@]}" -- -max_total_time="$FUZZ_SECONDS"; then
            print_red "Fuzz target $target failed, crash inputs are in $REPO_ROOT/fuzz/artifacts/$target"
            print_red "Reproduce with: cd fuzz && cargo +nightly fuzz run $target artifacts/$target/<file>"
            exit 1
        fi
    done
    cd "$REPO_ROOT"
}

if [ "$LINT" = true ]; then
    run_lints
fi
run_tests
run_fuzzing

print_green "All tests passed"

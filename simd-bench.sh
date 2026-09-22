#!/bin/bash
set -eo pipefail

# Import common functions
DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=./common.sh
source "$DIR/common.sh"

USAGE="Usage: $0 [OPTIONS]

Benchmark the SIMD code paths against scalar loops, one CLI_TOOLS_SIMD mode at a time.
Each mode is saved as a Criterion baseline named mode-<mode>,
and a summary table comparing every mode to scalar is written to target/simd-bench-<os>-<arch>.md.
See docs/simd.md for how to read the results.

OPTIONS: All options are optional
    --help
        Display these instructions.

    --quick
        Shorter warm up and measurement times, noisier but much faster.

    --modes <list>
        Space separated modes to run. Default: \"scalar auto\", plus \"avx2\" on x86_64.

    --summary-only
        Skip running benchmarks and only print the summary of the saved baselines.

    --verbose
        Display commands being executed.
"

CRITERION_ARGS=(--noplot)
MODES=""
SUMMARY_ONLY=false

while [ $# -gt 0 ]; do
    case "$1" in
        --help)
            echo "$USAGE"
            exit 1
            ;;
        --quick)
            CRITERION_ARGS+=(--warm-up-time 1 --measurement-time 2)
            ;;
        --modes)
            MODES="$2"
            shift
            ;;
        --summary-only)
            SUMMARY_ONLY=true
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

ARCH="$(uname -m)"
if [ -z "$MODES" ]; then
    case "$ARCH" in
        x86_64 | amd64)
            MODES="scalar auto avx2"
            ;;
        *)
            MODES="scalar auto"
            ;;
    esac
fi

# Benchmarks that run through code using cli_tools::simd.
SLB_FILTER="scan/|format/|check/|tokenize_line|reflow_paragraph|reword"
DUPE_FILTER="normalize_stem"

cd "$REPO_ROOT"

if [ "$SUMMARY_ONLY" = false ]; then
    for mode in $MODES; do
        print_magenta "CLI_TOOLS_SIMD=$mode"
        CLI_TOOLS_SIMD="$mode" cargo bench --bench semantic_line_breaks -- \
            --save-baseline "mode-$mode" "${CRITERION_ARGS[@]}" "$SLB_FILTER"
        CLI_TOOLS_SIMD="$mode" cargo bench --bench dupe_find -- \
            --save-baseline "mode-$mode" "${CRITERION_ARGS[@]}" "$DUPE_FILTER"
    done
    print_magenta "Kernel microbenchmarks"
    cargo bench --bench simd -- --save-baseline kernels "${CRITERION_ARGS[@]}"
fi

if [ -z "$(command -v python3)" ] && [ -z "$(command -v python)" ]; then
    print_yellow "Python not found, skipping the summary. Baselines are in target/criterion/*/mode-*"
    exit 0
fi
PYTHON="$(command -v python3 || command -v python)"

SUMMARY="target/simd-bench-$(uname -s)-${ARCH}.md"
"$PYTHON" - "$MODES" > "$SUMMARY" << 'EOF'
import json
import pathlib
import sys

modes = sys.argv[1].split()
root = pathlib.Path("target/criterion")


def median(path):
    return json.loads(path.read_text())["median"]["point_estimate"]


def format_time(nanoseconds):
    for unit, scale in (("ms", 1e6), ("µs", 1e3)):
        if nanoseconds >= scale:
            return f"{nanoseconds / scale:.2f} {unit}"
    return f"{nanoseconds:.1f} ns"


rows = {}
for estimate in root.glob("**/mode-*/estimates.json"):
    mode = estimate.parent.name.removeprefix("mode-")
    benchmark = estimate.parent.parent.relative_to(root).as_posix()
    rows.setdefault(benchmark, {})[mode] = median(estimate)

header = ["benchmark"] + [f"{mode}" for mode in modes] + [f"{mode} vs scalar" for mode in modes if mode != "scalar"]
print("| " + " | ".join(header) + " |")
print("|" + "|".join(["---"] + ["---:"] * (len(header) - 1)) + "|")
for benchmark in sorted(rows):
    times = rows[benchmark]
    cells = [benchmark] + [format_time(times[mode]) if mode in times else "" for mode in modes]
    for mode in modes:
        if mode == "scalar":
            continue
        if mode in times and "scalar" in times:
            cells.append(f"{(times[mode] / times['scalar'] - 1) * 100:+.1f}%")
        else:
            cells.append("")
    print("| " + " | ".join(cells) + " |")

print()
print("Kernel microbenchmarks, median of the auto level against the scalar code in the same group:")
print()
print("| group | input | scalar variant | scalar | simd | change |")
print("|---|---|---|---:|---:|---:|")
for group in sorted(root.glob("simd_*")):
    simd_dir = group / "simd"
    if not simd_dir.is_dir():
        continue
    for variant in sorted(path for path in group.iterdir() if path.is_dir() and path.name not in ("simd", "report")):
        for scalar_estimate in sorted(variant.glob("*/kernels/estimates.json")):
            parameter = scalar_estimate.parent.parent.name
            simd_estimate = simd_dir / parameter / "kernels" / "estimates.json"
            if not simd_estimate.exists():
                continue
            scalar_time = median(scalar_estimate)
            simd_time = median(simd_estimate)
            print(
                f"| {group.name} | {parameter} | {variant.name} | {format_time(scalar_time)} | "
                f"{format_time(simd_time)} | {(simd_time / scalar_time - 1) * 100:+.1f}% |"
            )
EOF

cat "$SUMMARY"
print_green "Summary written to $SUMMARY"

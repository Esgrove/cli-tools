#!/bin/bash
set -eo pipefail

# Import common functions
DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=./common.sh
source "$DIR/common.sh"

USAGE="Usage: $0 [OPTIONS]

Benchmark the code paths that use the SIMD kernels, and the kernels against the scalar code they replace.
The pipeline benchmarks are saved as the Criterion baseline named pipeline and the kernels as kernels,
and a summary table is written to target/simd-bench-<os>-<arch>.md.
See docs/simd.md for how to read the results.

OPTIONS: All options are optional
    --help
        Display these instructions.

    --quick
        Shorter warm up and measurement times, noisier but much faster.

    --summary-only
        Skip running benchmarks and only print the summary of the saved baselines.

    --verbose
        Display commands being executed.
"

CRITERION_ARGS=(--noplot)
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

# Benchmarks that run through code using cli_tools::simd.
SLB_FILTER="scan/|format/|check/|tokenize_line|reflow_paragraph|reword"
DUPE_FILTER="normalize_stem"

cd "$REPO_ROOT"

if [ "$SUMMARY_ONLY" = false ]; then
    print_magenta "Pipeline benchmarks"
    cargo bench --bench semantic_line_breaks -- --save-baseline pipeline "${CRITERION_ARGS[@]}" "$SLB_FILTER"
    cargo bench --bench dupe_find -- --save-baseline pipeline "${CRITERION_ARGS[@]}" "$DUPE_FILTER"
    print_magenta "Kernel microbenchmarks"
    cargo bench --bench simd -- --save-baseline kernels "${CRITERION_ARGS[@]}"
fi

if [ -z "$(command -v python3)" ] && [ -z "$(command -v python)" ]; then
    print_yellow "Python not found, skipping the summary. Baselines are in target/criterion/*/pipeline and */kernels"
    exit 0
fi
PYTHON="$(command -v python3 || command -v python)"

SUMMARY="target/simd-bench-$(uname -s)-$(uname -m).md"
"$PYTHON" - > "$SUMMARY" << 'EOF'
import json
import pathlib

root = pathlib.Path("target/criterion")


def median(path):
    return json.loads(path.read_text())["median"]["point_estimate"]


def format_time(nanoseconds):
    for unit, scale in (("ms", 1e6), ("µs", 1e3)):
        if nanoseconds >= scale:
            return f"{nanoseconds / scale:.2f} {unit}"
    return f"{nanoseconds:.1f} ns"


print("Pipeline benchmarks, median time:")
print()
print("| benchmark | median |")
print("|---|---:|")
for estimate in sorted(root.glob("**/pipeline/estimates.json")):
    benchmark = estimate.parent.parent.relative_to(root).as_posix()
    print(f"| {benchmark} | {format_time(median(estimate))} |")

print()
print("Kernel microbenchmarks, median of the SIMD kernel against the scalar code in the same group:")
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

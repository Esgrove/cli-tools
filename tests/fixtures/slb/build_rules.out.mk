# Builds and tests the project. Every target is phony because there are no file outputs to track.
.PHONY: build test

# Environment:
#   CARGO_FLAGS   extra flags passed to every cargo invocation
#   TARGET_DIR    directory the artifacts are written to

build:
	cargo build --release $(CARGO_FLAGS)

test:
	cargo nextest run

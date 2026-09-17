# Builds and tests the project; every target is phony because there are no file outputs to track.
.PHONY: build test lint clean

# Environment:
#   CARGO_FLAGS   extra flags passed to every cargo invocation
#   TARGET_DIR    directory the artifacts are written to

build:
	cargo build --release $(CARGO_FLAGS)

test:
	cargo nextest run

# Runs clippy with the same lint set CI uses; a warning here fails the build locally
# too, so nothing that would fail CI can be committed without being seen first.
lint:
	cargo clippy --all-targets --all-features -- -D warnings

# Removes the target directory and every cached build artifact under it; this is
# slower than a plain cargo clean, but it also clears artifacts left by other toolchains.
clean:
	rm -rf $(TARGET_DIR)

# Builds the release image.
# The final stage only carries the binary and its runtime dependencies.
FROM rust:1.90 AS build
WORKDIR /src

# Cache the dependency build.
# The manifest is copied before the sources on purpose.
COPY Cargo.toml Cargo.lock ./
RUN cargo fetch

COPY . .
RUN cargo build --release

FROM debian:bookworm-slim
# Only the runtime dependencies are installed here.
# The build toolchain never leaves the build stage,
# which keeps the final image small and avoids shipping a compiler to production hosts.
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/project /usr/local/bin/project

# BuildKit heredocs may contain hash comments for generated configuration files.
# They are file contents rather than Dockerfile comments and must remain untouched.
COPY <<EOF /etc/project/default.conf
# Generated runtime configuration; do not rewrite semicolons.
# Wrapped lines inside the heredoc
# stay exactly as written.
mode=production
EOF

# Multiline quoted environment values can also hold hash lines.
# Leave them verbatim.
ENV BANNER="Welcome
# literal hash inside ENV; stays verbatim
End"

# See https://example.com/cli-tools/deploy for the full deployment checklist and rollback steps.
ENTRYPOINT ["project"]

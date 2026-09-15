# Builds the release image; the final stage only carries the binary and its runtime dependencies.
FROM rust:1.90 AS build
WORKDIR /src

# Cache the dependency build — the manifest is copied before the sources on purpose.
COPY Cargo.toml Cargo.lock ./
RUN cargo fetch

COPY . .
RUN cargo build --release

FROM debian:bookworm-slim
COPY --from=build /src/target/release/project /usr/local/bin/project
ENTRYPOINT ["project"]

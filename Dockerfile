# syntax=docker/dockerfile:1
# VIKRAM — terminal AI runtime. Multi-stage build producing a small runtime image.

# ---------- builder ----------
FROM rust:1.86-bookworm AS builder
WORKDIR /src
# Cache dependencies: copy manifests first.
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
# Build the release binary (vikram-cli produces the `vikram` binary).
RUN cargo build --release -p vikram-cli \
    && strip target/release/vikram

# ---------- runtime ----------
FROM debian:bookworm-slim AS runtime
# ca-certificates for HTTPS providers; tini for signal handling.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates tini \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --create-home --shell /bin/bash --uid 1000 vikram

# Persistent state lives here. Mount a volume to keep it across restarts.
ENV VIKRAM_HOME=/data/vikram
RUN mkdir -p /data/vikram && chown -R vikram:vikram /data

COPY --from=builder /src/target/release/vikram /usr/local/bin/vikram
USER vikram
WORKDIR /workspace

# Default: launch the TUI. Override with `docker run ... vikram chat "..."` for headless.
ENTRYPOINT ["/usr/bin/tini", "--", "vikram"]
CMD []
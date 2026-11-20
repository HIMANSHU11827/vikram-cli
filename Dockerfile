# syntax=docker/dockerfile:1
# VIKRAM — terminal AI runtime. Multi-stage build producing a small runtime image.

# ---------- builder ----------
FROM rust:1.89-bookworm AS builder
WORKDIR /src
# Cache dependencies: copy manifests first.
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
# Build the release binary (vikram-cli produces the `vikram` binary).
RUN cargo build --release --locked -p vikram-cli \
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

# The binary itself is the only health signal: no port is open until the operator
# sets GATEWAY_WEBHOOK_BIND, so an HTTP probe would lie.
HEALTHCHECK --interval=60s --timeout=10s --start-period=10s --retries=3 \
    CMD ["vikram", "--version"]

# Default: launch the TUI. Override with `docker run ... vikram chat "..."` for headless.
ENTRYPOINT ["/usr/bin/tini", "--", "vikram"]
CMD []
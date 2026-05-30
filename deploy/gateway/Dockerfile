# Multi-stage build for the Triton gateway — DEMO image.
#
# Build context is the repo root (the binary needs the whole Cargo
# workspace). Built/pushed by the substrate CI, pinned by SHA — NEVER
# :latest:
#
#   docker build -f deploy/gateway/Dockerfile \
#     -t ghcr.io/datazoo/dz-triton:<version> .
#
# DEMO flavour: `dev-token` is in triton-bin's DEFAULT features, so a
# plain `cargo build --release` keeps it ON — the deployed binary
# accepts `Bearer dev-token` and registers `demo_panel` + the dev
# tools (the A2UI showcase). This is a deliberate, tailnet-only,
# nonprod affordance: this image MUST NOT get a Fabio `urlprefix-` tag
# and MUST NOT run in prod. A production image is built with
# `--no-default-features` (ADR-10 / FR-T-1) — see deploy/README.md.

# ----- stage 1: build -----
FROM rust:1.95-bookworm AS build
WORKDIR /src
COPY . .
# Default features include `dev-token` (demo). Use --locked for a
# reproducible build against the committed Cargo.lock.
RUN cargo build --release --locked --bin triton
RUN install -D target/release/triton /out/triton

# ----- stage 2: runtime -----
# Binary links only libc (NFR-PT-2); debian-slim provides glibc + CA
# certs (needed for the outbound HTTPS to api.telegram.org under the
# long_poll Telegram adapter). No Python/Node (NFR-O-2).
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /out/triton /usr/local/bin/triton
# The demo manifest ships in the image; the Nomad job points
# TRITON_MANIFEST_PATH at it.
COPY deploy/gateway/adapter.yaml /etc/triton/adapter.yaml
EXPOSE 8001 8002 8003
ENTRYPOINT ["/usr/local/bin/triton"]

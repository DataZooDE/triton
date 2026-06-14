# Multi-stage build for the Triton agent-ingress gateway binary.
#
# Stage 1 — `cargo build --release -p triton-bin`. Mirrors the
# auth-portal shape (substrate-platform reference 18 — GAR-only):
# the workspace's pinned rust-toolchain (1.95.x) compiles a single
# binary that is then dropped into a distroless runtime.
#
# Stage 2 — `gcr.io/distroless/cc-debian12:nonroot`. The binary is
# dynamically linked against libgcc/libc; distroless/cc gives us
# those without the bigger debian image. No shell, no package
# manager, runs as the nonroot user (uid 65532).
#
# Image target (ghcr — GAR retired, Phase 5):
#   ghcr.io/datazoode/dz-triton:<sha>

# ----- stage 1: build -----
FROM rust:1.95-slim AS build
WORKDIR /src
RUN apt-get update && apt-get install -y --no-install-recommends pkg-config \
    && rm -rf /var/lib/apt/lists/*

# Copy only what the Rust build needs. (No rust-toolchain.toml in this
# workspace — `rust-version = "1.95"` in Cargo.toml + the FROM tag pin
# above is the equivalent.)
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates

# Default features include `dev-token`; the demo uses oauth2-proxy
# in front for auth (substrate ADR-0011), so the Bearer-JWT verifier
# inside Triton stays unused. We keep dev-token on so an operator
# can also paste a token in the SPA's Settings page for ad-hoc
# diagnostics — production-hardened (no dev-token) builds come when
# we wire real OIDC verification end-to-end.
RUN cargo build --release -p triton-bin

# ----- stage 2: runtime -----
FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /src/target/release/triton /usr/local/bin/triton
# Secret-free chat manifest (env:// refs only). OPT-IN: loaded only when the
# deploy sets TRITON_MANIFEST_PATH=/etc/triton/adapter.yaml; a pure-REST
# deploy leaves it unset and ignores the file (backward compatible).
COPY deploy/triton/adapter.yaml /etc/triton/adapter.yaml
USER nonroot
# REST, MCP, A2A. /metrics stays on 127.0.0.1:9090 (internal-only).
EXPOSE 8001 8002 8003
# Kamal asserts a `service` label matching the deploy service before booting a
# prebuilt image. The GAR→ghcr mirror used to stamp it; we publish to ghcr
# directly now (GAR retired, Phase 5), so bake it here.
LABEL service="dz-triton"
ENTRYPOINT ["/usr/local/bin/triton"]

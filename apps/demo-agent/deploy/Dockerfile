# Multi-stage build for the Triton demo upstream agent.
#
# Build context is the repo root (so the COPY paths below resolve),
# mirroring apps/explorer/deploy/Dockerfile. The Rust SDK image is
# pinned at deploy time by the substrate CI (substrate doctrine:
# NEVER :latest).
#
#   docker build -f apps/demo-agent/deploy/Dockerfile \
#     -t ghcr.io/datazoo/dz-triton-demo-agent:<version> .

# ----- stage 1: build -----
FROM rust:1.95-bookworm AS build
WORKDIR /src
# Copy only the standalone crate so changes elsewhere don't bust cache.
COPY apps/demo-agent/Cargo.toml apps/demo-agent/Cargo.lock* ./
COPY apps/demo-agent/src ./src
RUN cargo build --release --locked || cargo build --release
RUN install -D target/release/triton-demo-agent /out/triton-demo-agent

# ----- stage 2: runtime -----
# Binary links only libc (NFR-PT-2); debian-slim provides glibc + CA
# certs. No Python/Node.
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /out/triton-demo-agent /usr/local/bin/triton-demo-agent
ENV AGENT_PORT=8080
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/triton-demo-agent"]

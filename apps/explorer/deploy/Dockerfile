# Multi-stage build for the Triton Explorer Flutter web app.
#
# Stage 1 — `flutter build web --release`. We pin to a specific Flutter
# SDK image tag (substrate doctrine: NEVER `:latest`). `ghcr.io/cirruslabs`
# is the maintained mirror of the old cirrusci/flutter images. The tag
# must match the workstation's Flutter / Dart pair — `pubspec.yaml`
# requires Dart ^3.12.0 (Flutter 3.44.0), so we pin 3.44.0 here too.
#
# Stage 2 — `nginxinc/nginx-unprivileged` serves the compiled assets on
# :8080. Plain HTTP — fabio-internal terminates TLS upstream with the
# `*.<env>.int.data-zoo.de` wildcard cert (ADR-0010).
#
# Image target (ghcr — GAR retired, Phase 5):
#   ghcr.io/datazoode/dz-triton-explorer:<sha>
#
# The Dockerfile copies only `apps/explorer/{lib,web,pubspec.*,
# analysis_options.yaml}` into the build stage so changes outside the
# explorer don't bust the layer cache.

# ----- stage 1: build -----
FROM ghcr.io/cirruslabs/flutter:3.44.0 AS build
WORKDIR /src

# Copy only what the build needs (cache-stable).
COPY apps/explorer/pubspec.yaml apps/explorer/pubspec.lock* ./
RUN flutter pub get

COPY apps/explorer/lib ./lib
COPY apps/explorer/web ./web
COPY apps/explorer/analysis_options.yaml ./

RUN flutter build web --release --base-href=/

# ----- stage 2: runtime -----
FROM nginxinc/nginx-unprivileged:1.27-alpine
COPY --from=build /src/build/web /usr/share/nginx/html
COPY apps/explorer/deploy/nginx.conf /etc/nginx/conf.d/default.conf
EXPOSE 8080
# Kamal `service` label (published to ghcr directly; GAR retired, Phase 5).
LABEL service="dz-triton-explorer"
USER nginx

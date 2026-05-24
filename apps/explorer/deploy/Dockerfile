# Multi-stage build for the Triton Explorer Flutter web app.
#
# Stage 1 — `flutter build web --release`. We pin to a specific
# Flutter SDK image SHA at deploy time (substrate doctrine: NEVER
# `:latest`). The placeholder `cirrusci/flutter:3.24.5` below is
# replaced by the substrate repo's CI when it produces the deploy
# image SHA. Use `--web-renderer canvaskit` so SVG/Material icons
# render consistently across browsers.
#
# Stage 2 — `nginxinc/nginx-unprivileged` serves the compiled assets
# on :8080. The Flutter SPA talks to Triton cross-origin; Triton's
# CORS layer (PR #16) clears the preflight when this explorer's
# tailnet FQDN is in TRITON_CORS_ALLOWED_ORIGINS.

# ----- stage 1: build -----
FROM cirrusci/flutter:3.24.5 AS build
WORKDIR /src

# Copy only what the build needs so changes outside apps/explorer/
# don't bust the layer cache.
COPY apps/explorer/pubspec.yaml apps/explorer/pubspec.lock* ./
RUN flutter pub get

COPY apps/explorer/lib ./lib
COPY apps/explorer/web ./web
COPY apps/explorer/analysis_options.yaml ./

RUN flutter build web --release --web-renderer canvaskit --base-href=/

# ----- stage 2: runtime -----
FROM nginxinc/nginx-unprivileged:1.27-alpine
COPY --from=build /src/build/web /usr/share/nginx/html
COPY apps/explorer/deploy/nginx.conf /etc/nginx/conf.d/default.conf
EXPOSE 8080
USER nginx

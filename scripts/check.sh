#!/usr/bin/env bash
# scripts/check.sh — Triton's local gate, shared by CI and the pre-push hook.
#
#   scripts/check.sh              # the pre-push set (below)
#   scripts/check.sh fmt clippy   # named steps, in the order given
#   scripts/check.sh --list
#
# CI (.github/workflows/ci.yml) calls the same steps one by one, so a command
# changes here or nowhere. CI-only tuning (CARGO_BUILD_JOBS, caches, swap)
# stays in the workflow. Everything here is offline apart from dependency
# downloads: no deploy, no cloud credentials, no live services.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

# Formatting first: it is the cheapest failure and the most common one.
PRE_PUSH=(fmt mcp-widget clippy consumer-smoke bins test capture flutter-a2ui flutter-explorer)
# Not in the hook: the release dart2js compile is slow and memory-hungry
# (CI adds 14 GB of swap for it) and it is a build, not a test.
EXTRA=(flutter-web)

step_fmt() {
  cargo fmt --all -- --check || { echo "fix with: cargo fmt --all" >&2; return 1; }
}

step_clippy() {
  cargo clippy --all-targets --workspace -- -D warnings
}

# ACC-13: the harness consumed from a SEPARATE workspace, so its `pub` surface
# stays usable downstream. In CI this runs before `bins` on purpose (see
# ci.yml); locally target/debug/triton usually exists already.
step_consumer_smoke() {
  (cd examples/consumer-smoke && cargo test --locked)
}

# The spawn-based integration tests need these; `cargo test --workspace` does
# not build separate workspace binaries.
step_bins() {
  cargo build --locked --bin triton --bin triton-rasterizer
}

step_test() {
  cargo test --workspace --locked
}

# The dev-only `capture` feature is off by default; test it explicitly.
step_capture() {
  cargo test --locked -p triton-embed --features capture
}

FLUTTER_PIN=3.44.4 # keep in step with ci.yml's flutter-version

flutter_version_note() {
  local v
  v="$(flutter --version 2>/dev/null | awk 'NR==1 {print $2}')"
  if [ "$v" != "$FLUTTER_PIN" ]; then
    echo "note: local Flutter $v, CI pins $FLUTTER_PIN — analyzer results may differ" >&2
  fi
}

# Newer Flutter SDKs rewrite tracked files on `pub get` (3.47 adds analyzer
# excludes to analysis_options.yaml). A check must not leave the tree dirty,
# so restore any tracked file under the package that was clean beforehand.
# Restore on failure too: a failed analyze must not leave the rewrite behind.
flutter_package() {
  local before after f rc=0
  before="$(git diff --name-only -- "$1")"
  (cd "$1" && flutter pub get && flutter analyze && flutter test) || rc=$?
  after="$(git diff --name-only -- "$1")"
  for f in $after; do
    if ! grep -qxF "$f" <<<"$before"; then
      echo "note: flutter rewrote $f; restored it" >&2
      git checkout -- "$f"
    fi
  done
  return "$rc"
}

# The MCP Apps widget (#368): the official A2UI renderer and Triton's glue,
# built into the one HTML file `triton-core` `include_str!`s. The built file
# is committed (cargo stays Node-free); this rebuilds it from the lockfile and
# fails when the committed copy differs, so it can never go stale.
step_mcp_widget() {
  (cd packages/mcp-widget && npm ci --ignore-scripts --no-audit --no-fund && npm test && npm run build)
  git diff --exit-code -- crates/triton-core/src/a2ui/mcp_widget.html || {
    echo "mcp_widget.html is stale: run 'cd packages/mcp-widget && npm run build' and commit it" >&2
    return 1
  }
}

step_flutter_a2ui() {
  flutter_version_note
  flutter_package packages/a2ui_flutter
}

step_flutter_explorer() {
  flutter_version_note
  flutter_package apps/explorer
}

# `--no-wasm-dry-run`: skips the extra wasm compile that OOMs the CI runner.
step_flutter_web() {
  (cd apps/explorer && flutter build web --release --no-wasm-dry-run)
}

usage() {
  echo "usage: $0 [--list | step...]"
  echo "  pre-push (default): ${PRE_PUSH[*]}"
  echo "  also available:     ${EXTRA[*]}"
}

steps=()
case "${1:-pre-push}" in
  --list | -h | --help) usage; exit 0 ;;
esac
for arg in "${@:-pre-push}"; do
  if [ "$arg" = pre-push ]; then
    steps+=("${PRE_PUSH[@]}")
  elif declare -F "step_${arg//-/_}" >/dev/null; then
    steps+=("$arg")
  else
    echo "unknown step: $arg" >&2; usage >&2; exit 2
  fi
done

start=$SECONDS
for s in "${steps[@]}"; do
  t=$SECONDS
  echo "==> check: $s"
  "step_${s//-/_}"
  echo "<== check: $s ok ($((SECONDS - t))s)"
done
echo "check: ${#steps[@]} step(s) passed in $((SECONDS - start))s"

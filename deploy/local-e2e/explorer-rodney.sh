#!/usr/bin/env bash
# Browser smoke test for the Triton Explorer SPA, driven by `rodney`
# (https://github.com/simonw/rodney — a Go CLI over a persistent
# headless Chrome). Boots a local Triton + the built SPA, then walks
# every page taking screenshots and asserting page identity via the
# Flutter accessibility (semantics) tree.
#
# WHY semantics + screenshots, not CSS selectors: the Explorer is
# Flutter Web on the CanvasKit renderer — the UI paints into a
# <canvas>, so rodney's `click`/`text`/`exists` can't see widgets.
# Flutter exposes a parallel DOM semantics tree (<flt-semantics> nodes
# with roles/labels) only after accessibility is enabled; we click the
# injected "Enable accessibility" placeholder, then drive/assert
# through that tree (rodney `ax-find`) and capture screenshots as the
# primary visual evidence.
#
# Usage:
#   deploy/local-e2e/explorer-rodney.sh [--show] [--no-build]
#     --show      run Chrome with a visible window (default: headless)
#     --no-build  reuse an existing apps/explorer/build/web
#
# Requirements: rodney, chromium/chrome, flutter, python3, cargo.
# Exit code 0 = all assertions passed; non-zero = a failure (or setup
# error). Screenshots land in deploy/local-e2e/.rodney-out/ (gitignored).
set -euo pipefail

# ---- locations ------------------------------------------------------
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
EXPLORER_DIR="$REPO_ROOT/apps/explorer"
OUT_DIR="$SCRIPT_DIR/.rodney-out"
WEB_DIR="$EXPLORER_DIR/build/web"

# ---- config ---------------------------------------------------------
SPA_PORT=5000
REST_PORT=8003
MCP_PORT=8001
A2A_PORT=8002
# The adk-hello-agent (an upstream agent Triton dispatches into) and the
# tiny Consul/Vault fakes that let Triton reach it. See PR "wire the live
# hello agent into the browser demo".
AGENT_PORT=8090
SPA_URL="http://localhost:${SPA_PORT}"
REST_URL="http://127.0.0.1:${REST_PORT}"
AGENT_DIR="$REPO_ROOT/examples/adk-hello-agent"
export ROD_CHROME_BIN="${ROD_CHROME_BIN:-$(command -v chromium || command -v chromium-browser || command -v google-chrome || true)}"

SHOW_FLAG=""
DO_BUILD=1
for arg in "$@"; do
  case "$arg" in
    --show) SHOW_FLAG="--show" ;;
    --no-build) DO_BUILD=0 ;;
    *) echo "unknown arg: $arg" >&2; exit 2 ;;
  esac
done

PASS=0
FAIL=0
TRITON_PID=""
HTTP_PID=""
AGENT_PID=""

pass() { PASS=$((PASS+1)); echo "  PASS: $1"; }
fail() { FAIL=$((FAIL+1)); echo "  FAIL: $1" >&2; }

cleanup() {
  rodney stop >/dev/null 2>&1 || true
  for p in "$HTTP_PID" "$TRITON_PID" "$AGENT_PID"; do
    [ -n "$p" ] && kill "$p" >/dev/null 2>&1 || true
  done
}
trap cleanup EXIT

# ---- preflight ------------------------------------------------------
for tool in rodney flutter python3 cargo; do
  command -v "$tool" >/dev/null 2>&1 || { echo "missing required tool: $tool" >&2; exit 2; }
done
[ -n "$ROD_CHROME_BIN" ] || { echo "no chromium/chrome found; set ROD_CHROME_BIN" >&2; exit 2; }

mkdir -p "$OUT_DIR"

# ---- build the SPA --------------------------------------------------
if [ "$DO_BUILD" -eq 1 ]; then
  echo "==> flutter build web"
  ( cd "$EXPLORER_DIR" && flutter build web --release >/dev/null )
fi
[ -f "$WEB_DIR/index.html" ] || { echo "no build at $WEB_DIR (drop --no-build)" >&2; exit 2; }

# ---- build everything ----------------------------------------------
echo "==> building triton-bin and the adk-hello-agent"
( cd "$REPO_ROOT" && cargo build -p triton-bin --bin triton >/dev/null 2>&1 )
( cd "$AGENT_DIR" && cargo build --bin adk-hello-agent >/dev/null 2>&1 )

# ---- start the live upstream agent ----------------------------------
# The agent runs its deterministic StaticBrain (no ANTHROPIC_API_KEY), so
# the greeting is fixed and assertable. Triton reaches it via the static
# TRITON_STATIC_UPSTREAMS map (no Consul, no Vault); with no JWT signer
# configured Triton dispatches the static `dev-token` bearer, which the
# agent accepts in its dev-token mode.
echo "==> starting adk-hello-agent on :$AGENT_PORT"
AGENT_PORT=$AGENT_PORT "$AGENT_DIR/target/debug/adk-hello-agent" >/dev/null 2>&1 &
AGENT_PID=$!

# ---- start Triton (dev-token inbound; static upstream → the agent) --
# `local` env so the loopback static-upstream endpoint is accepted (the
# NFR-S-4 SSRF guard only enforces tailnet/private targets outside local).
echo "==> starting triton-bin"
TRITON_HOST=127.0.0.1 TRITON_REST_PORT=$REST_PORT TRITON_MCP_PORT=$MCP_PORT \
  TRITON_A2A_PORT=$A2A_PORT TRITON_METRICS_PORT=0 TRITON_CHAT_WEBHOOK_PORT=0 \
  TRITON_ENV=local \
  TRITON_STATIC_UPSTREAMS="hello=127.0.0.1:$AGENT_PORT" \
  "$REPO_ROOT/target/debug/triton" \
    --cors-allowed-origins "$SPA_URL" \
    --explorer-client-id triton-explorer-dev >/dev/null 2>&1 &
TRITON_PID=$!

echo -n "==> waiting for /healthz"
for _ in $(seq 1 50); do
  if curl -fsS "$REST_URL/healthz" >/dev/null 2>&1; then echo " ok"; break; fi
  echo -n "."; sleep 0.2
done
curl -fsS "$REST_URL/healthz" >/dev/null 2>&1 || { echo " FAILED" >&2; exit 1; }

# ---- headless proof: the live hello agent is reachable through Triton
# (rodney can't reliably type into a CanvasKit text field to drive the
# Console's custom-tool entry, so we assert the live upstream path here
# over real HTTP, then let the browser walk cover the Console UI).
echo "==> asserting the live hello agent through REST/MCP/A2A"
hello_rest="$(curl -fsS -X POST "$REST_URL/v1/tools/hello" \
  -H 'Authorization: Bearer dev-token' -H 'Content-Type: application/json' \
  -d '{"subject":"Rodney"}' 2>/dev/null || true)"
case "$hello_rest" in
  *"Welcome to Triton"*) pass "REST → upstream hello agent renders a greeting" ;;
  *) fail "REST → hello agent (got: $hello_rest)" ;;
esac
hello_a2a="$(curl -fsS -X POST "http://127.0.0.1:${A2A_PORT}/message:send" \
  -H 'Authorization: Bearer dev-token' -H 'Content-Type: application/json' \
  -d '{"parts":[{"data":{"tool":"hello","args":{"subject":"Rodney"}}}],"metadata":{"a2ui_version":"v0.8"}}' 2>/dev/null || true)"
case "$hello_a2a" in
  *"Welcome to Triton"*) pass "A2A → upstream hello agent renders a greeting" ;;
  *) fail "A2A → hello agent (got: $hello_a2a)" ;;
esac

# ---- serve the built SPA -------------------------------------------
echo "==> serving SPA on $SPA_URL"
python3 -m http.server "$SPA_PORT" --directory "$WEB_DIR" >/dev/null 2>&1 &
HTTP_PID=$!
sleep 1

# ---- rodney helpers -------------------------------------------------
# Click the first tappable semantics node whose text starts with $1.
nav_click() {
  rodney js "(function(){var b=Array.from(document.querySelectorAll('flt-semantics[role=button],flt-semantics[flt-tappable]')).find(function(e){return (e.textContent||'').indexOf('$1')===0;}); if(!b) return 'nf'; b.click(); return 'ok';})()"
}
# Click a tappable semantics node whose trimmed text equals $1.
click_exact() {
  rodney js "(function(){var b=Array.from(document.querySelectorAll('flt-semantics[role=button],flt-semantics[flt-tappable]')).find(function(e){return (e.textContent||'').trim()==='$1';}); if(!b) return 'nf'; b.click(); return 'ok';})()"
}
# Assert the rendered page exposes text $1, polling up to ~8s. We read
# Chrome's accessibility tree via `ax-tree` (a CDP fetch) rather than
# querying <flt-semantics> directly: Flutter only keeps the semantics
# DOM fresh for newly-rendered subtrees while an a11y client is
# actively reading it, and ax-tree IS that client. CanvasKit also
# paints a beat before mirroring text, and content waits on /v1/*.
assert_text() {
  for _ in $(seq 1 16); do
    if rodney ax-tree --depth 80 2>/dev/null | grep -qF "$1"; then
      pass "$2"; return
    fi
    sleep 0.5
  done
  fail "$2 (text '$1' not found in a11y tree)"
}
shot() { rodney screenshot "$OUT_DIR/$1.png" >/dev/null 2>&1 && echo "  shot: $1.png"; }

# ---- boot the browser + app ----------------------------------------
echo "==> rodney start ${SHOW_FLAG:-(headless)}"
rodney stop >/dev/null 2>&1 || true
rodney start $SHOW_FLAG >/dev/null
rodney open "$SPA_URL" >/dev/null
sleep 6; rodney waitstable >/dev/null 2>&1 || true

# Seed the dev bearer + base URL (shared_preferences web = JSON under
# `flutter.`-prefixed localStorage keys), then reload so the app boots
# past the login gate pointed at the local Triton.
rodney js "(function(){localStorage.setItem('flutter.triton.bearer', JSON.stringify('dev-token')); localStorage.setItem('flutter.triton.baseUrl', JSON.stringify('$REST_URL')); return 'seeded';})()" >/dev/null
rodney open "$SPA_URL" >/dev/null
sleep 6; rodney waitstable >/dev/null 2>&1 || true

# Enable Flutter semantics so the DOM tree is populated.
sem="$(rodney js "(function(){var b=document.querySelector('flt-semantics-placeholder'); if(b){b.click(); return 'on';} return 'noph';})()" 2>/dev/null || echo err)"
[ "$sem" = "on" ] && pass "Flutter semantics enabled" || fail "could not enable semantics ($sem)"
sleep 2; rodney waitstable >/dev/null 2>&1 || true
assert_text "registered" "Dashboard renders runtime + tools"
shot "01-dashboard"

# ---- walk the pages -------------------------------------------------
echo "==> walking pages"
declare -a PAGES=(
  "Console|Pick a tool|02-console"
  "A2UI diff|Invoke|03-a2ui-diff"
  "Manifest|Manifest|04-manifest"
  "Audit|Audit|05-audit"
  "Trace|Available traces|05b-trace"
  "Metrics|Metrics|06-metrics"
  "Settings|Settings|07-settings"
)
for spec in "${PAGES[@]}"; do
  IFS='|' read -r nav marker file <<< "$spec"
  echo "-- $nav"
  [ "$(nav_click "$nav")" = "ok" ] && pass "nav → $nav" || fail "nav → $nav (button not found)"
  sleep 1; rodney waitstable >/dev/null 2>&1 || true
  assert_text "$marker" "$nav shows '$marker'"
  shot "$file"
done

# ---- Console: select a tool, edit-and-send, round-trip --------------
# The Console merges the old Playground + Adapters: pick a tool, see the
# editable per-protocol envelope, Send, and the MCP-handshake +
# error-taxonomy sections live below the response (FR-D-3, FR-D-4).
echo "==> Console round-trip + sections"
nav_click "Console" >/dev/null; sleep 1; rodney waitstable >/dev/null 2>&1 || true
nav_click "demo_panel" >/dev/null; sleep 1; rodney waitstable >/dev/null 2>&1 || true
click_exact "v0.9" >/dev/null; sleep 1
click_exact "Send" >/dev/null; sleep 2; rodney waitstable >/dev/null 2>&1 || true
shot "08-console-rendered"
assert_text "Triton demo panel" "Console renders the A2UI surface (REST)"
assert_text "MCP handshake" "Console shows the MCP handshake section"
assert_text "Error taxonomy" "Console shows the error-taxonomy section"
# Pick a Selection option → re-invokes the tool over the active protocol;
# narrate's surface (its unique "generated narration about <tone>" text)
# replaces the panel, proving a real round-trip occurred.
click_exact "Friendly" >/dev/null; sleep 2; rodney waitstable >/dev/null 2>&1 || true
shot "09-console-roundtrip"
assert_text "generated narration" "Console Selection re-invoked the tool (round-trip)"

# ---- summary --------------------------------------------------------
echo
echo "================ rodney smoke summary ================"
echo "  passed: $PASS    failed: $FAIL"
echo "  screenshots: $OUT_DIR"
echo "======================================================"
[ "$FAIL" -eq 0 ]

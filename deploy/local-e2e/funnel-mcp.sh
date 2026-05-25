#!/usr/bin/env bash
# Expose a locally-running Triton's MCP endpoint over Tailscale
# Funnel so a REAL remote MCP host (MCP Inspector, Claude Desktop,
# an SDK client) can drive it end to end.
#
# ┌─ SECURITY ────────────────────────────────────────────────────┐
# │ Tailscale FUNNEL is PUBLIC-internet exposure (unlike `serve`,  │
# │ which is tailnet-only). This script boots Triton in dev-token  │
# │ mode, so while the funnel is up ANYONE on the internet who     │
# │ sends `Authorization: Bearer dev-token` can call your tools.   │
# │ Use it for a short, supervised test window and Ctrl-C when     │
# │ done. For anything longer, configure a real OIDC issuer        │
# │ (TRITON_OIDC_ISSUER) so dev-token is rejected, or use          │
# │ `tailscale serve` (tailnet-only) instead of funnel.            │
# └────────────────────────────────────────────────────────────────┘
#
# Usage:  ./deploy/local-e2e/funnel-mcp.sh
# Requires: triton binary (built if missing), tailscale (logged in,
#           Funnel enabled for the tailnet in the admin ACLs).

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

MCP_PORT="${TRITON_MCP_PORT:-8101}"
REST_PORT="${TRITON_REST_PORT:-8103}"
BIN="$REPO_ROOT/target/debug/triton"

[[ -x "$BIN" ]] || { echo "→ building triton…"; cargo build -p triton-bin; }

echo "→ starting triton (mcp=$MCP_PORT, env=local, dev-token)…"
TRITON_MCP_PORT="$MCP_PORT" \
TRITON_A2A_PORT=8102 \
TRITON_REST_PORT="$REST_PORT" \
TRITON_CHAT_WEBHOOK_PORT=0 \
TRITON_METRICS_PORT=0 \
  "$BIN" >/tmp/triton-funnel.log 2>&1 &
TRITON_PID=$!

cleanup() {
  echo ""
  echo "→ tearing down funnel + triton…"
  tailscale funnel --https=443 off 2>/dev/null || true
  kill "$TRITON_PID" 2>/dev/null || true
}
trap cleanup EXIT INT TERM

for _ in $(seq 1 50); do
  curl -sf "http://127.0.0.1:$REST_PORT/healthz" >/dev/null 2>&1 && break
  sleep 0.1
done
curl -sf "http://127.0.0.1:$REST_PORT/healthz" >/dev/null \
  || { echo "✗ triton unhealthy"; tail -20 /tmp/triton-funnel.log; exit 1; }

echo "→ opening Tailscale Funnel on :443 → localhost:$MCP_PORT …"
# Funnel proxies the public :443 to the local MCP port. `--bg`
# keeps it running in the background; we turn it off in cleanup.
tailscale funnel --bg --https=443 "http://localhost:$MCP_PORT" \
  || { echo "✗ funnel failed — is Funnel enabled in the tailnet ACL?"; exit 1; }

NODE_URL="$(tailscale funnel status 2>/dev/null | grep -oE 'https://[^ ]+' | head -1 || true)"
[[ -n "$NODE_URL" ]] || NODE_URL="https://<your-node>.<tailnet>.ts.net"

cat <<EOF

✓ Triton MCP is live at:  $NODE_URL

Drive it with a real MCP host:

  • MCP Inspector (interactive UI):
      npx @modelcontextprotocol/inspector
    then in the UI choose transport "Streamable HTTP", URL
      $NODE_URL
    and add header  Authorization: Bearer dev-token

  • curl wire check:
      curl -s -X POST "$NODE_URL" \\
        -H "Authorization: Bearer dev-token" \\
        -H "Content-Type: application/json" \\
        -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}' | python3 -m json.tool

  • Claude Desktop / Code MCP config (HTTP transport):
      { "mcpServers": { "triton": {
          "url": "$NODE_URL",
          "headers": { "Authorization": "Bearer dev-token" } } } }

Press Ctrl-C to tear down the funnel and stop Triton.
EOF

# Hold open until Ctrl-C.
wait "$TRITON_PID"

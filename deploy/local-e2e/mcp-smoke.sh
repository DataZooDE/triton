#!/usr/bin/env bash
# Real end-to-end MCP + A2UI smoke against a locally-spawned Triton.
#
# Boots the `triton` binary with the HTTP trio only (no manifest, no
# Consul/Vault/OIDC — minimum-viable boot per FR-T-1), drives a real
# MCP JSON-RPC 2.0 sequence over HTTP with the `dev-token` bearer,
# and asserts the A2UI surface round-trips through the dispatcher in
# both v0.8 and v0.9. Tears the process down on exit.
#
# This is the wire-level proof that an MCP host (Claude Desktop,
# MCP Inspector, an SDK client) can drive Triton end to end. For a
# remote/real-client run, see `funnel-mcp.sh` which exposes the same
# MCP port over Tailscale Funnel.
#
# Usage:  ./deploy/local-e2e/mcp-smoke.sh
# Requires: a release/debug `triton` binary (built if missing),
#           curl, python3.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

MCP_PORT="${TRITON_MCP_PORT:-8101}"
REST_PORT="${TRITON_REST_PORT:-8103}"
BIN="$REPO_ROOT/target/debug/triton"

if [[ ! -x "$BIN" ]]; then
  echo "→ building triton (debug, dev-token default)…"
  cargo build -p triton-bin
fi

echo "→ starting triton (mcp=$MCP_PORT rest=$REST_PORT, env=local, dev-token)…"
TRITON_MCP_PORT="$MCP_PORT" \
TRITON_A2A_PORT=8102 \
TRITON_REST_PORT="$REST_PORT" \
TRITON_CHAT_WEBHOOK_PORT=0 \
TRITON_METRICS_PORT=0 \
  "$BIN" >/tmp/triton-mcp-smoke.log 2>&1 &
TRITON_PID=$!
trap 'kill "$TRITON_PID" 2>/dev/null || true' EXIT

# Wait for healthz.
for _ in $(seq 1 50); do
  if curl -sf "http://127.0.0.1:$REST_PORT/healthz" >/dev/null 2>&1; then break; fi
  sleep 0.1
done
curl -sf "http://127.0.0.1:$REST_PORT/healthz" >/dev/null || {
  echo "✗ triton did not become healthy"; tail -20 /tmp/triton-mcp-smoke.log; exit 1; }

MCP="http://127.0.0.1:$MCP_PORT/"
AUTH=(-H "Authorization: Bearer dev-token" -H "Content-Type: application/json")

rpc() { curl -s -X POST "$MCP" "${AUTH[@]}" -d "$1"; }

fail() { echo "✗ $1"; exit 1; }

echo "→ initialize"
rpc '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"mcp-smoke","version":"0.1"}}}' \
  | python3 -c "import json,sys; d=json.load(sys.stdin); assert d['result']['protocolVersion']=='2025-06-18', d; assert d['result']['serverInfo']['name']=='triton', d; print('  ok: protocolVersion + serverInfo')" \
  || fail "initialize"

echo "→ tools/list"
rpc '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' \
  | python3 -c "import json,sys; d=json.load(sys.stdin); names=[t['name'] for t in d['result']['tools']]; assert 'demo_panel' in names, names; assert all('inputSchema' in t for t in d['result']['tools']), 'missing inputSchema'; print('  ok: tools =', names)" \
  || fail "tools/list"

echo "→ tools/call demo_panel (A2UI v0.9)"
rpc '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"demo_panel","arguments":{},"_meta":{"a2ui_version":"v0.9"}}}' \
  | python3 -c "
import json,sys
r=json.load(sys.stdin)['result']
sc=r['structuredContent']['result']
assert sc['version']=='0.9', sc.get('version')
types=[c['type'] for c in sc['stream']]
for want in ('text','narration','dashboard','selection','form','button'):
    assert want in types, ('missing '+want, types)
assert r['isError'] is False
assert r['_meta']['trace_id']
print('  ok: v0.9 stream types =', types)
" || fail "tools/call v0.9"

echo "→ tools/call demo_panel (A2UI v0.8)"
rpc '{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"demo_panel","arguments":{},"_meta":{"a2ui_version":"v0.8"}}}' \
  | python3 -c "import json,sys; sc=json.load(sys.stdin)['result']['structuredContent']['result']; assert sc['version']=='0.8', sc.get('version'); print('  ok: v0.8 envelope')" \
  || fail "tools/call v0.8"

echo "→ tools/call echo (plain dispatch, no A2UI)"
rpc '{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"echo","arguments":{"message":"hello triton"}}}' \
  | python3 -c "import json,sys; r=json.load(sys.stdin)['result']; sc=r['structuredContent']['result']; assert sc=={'echo':'hello triton'}, sc; print('  ok: echo round-trip')" \
  || fail "tools/call echo"

echo "→ unknown method rejected (-32601-ish)"
rpc '{"jsonrpc":"2.0","id":6,"method":"bogus/method"}' \
  | python3 -c "import json,sys; d=json.load(sys.stdin); assert 'error' in d, d; print('  ok: error =', d['error']['message'][:60])" \
  || fail "unknown method"

echo ""
echo "✓ MCP + A2UI end-to-end smoke passed against a real Triton process."

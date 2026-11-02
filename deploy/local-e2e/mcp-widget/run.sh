#!/usr/bin/env bash
# The answer-card MCP Apps widget in headless Chromium (rodney).
#
# host.html plays an MCP Apps host: it answers ui/initialize with a theme,
# sends each golden `<name>.mcp.json` as a tool result, and records what
# the widget asks of it (ui/message, ui/open-link, size-changed). The run
# checks the review findings (one transport per click, stale card cleared,
# fill vectors equal to the Rust parser + resolve, booleans as true/false)
# and the user flows, then saves light and dark PNGs.
#
#   deploy/local-e2e/mcp-widget/run.sh [png-out-dir]
set -euo pipefail
W=$(cd "$(dirname "$0")/../../.." && pwd)
OUT=${1:-$W/target/mcp-widget-png}
H=$(mktemp -d)
PORT=${PORT:-18977}
mkdir -p "$OUT"
cp "$W/deploy/local-e2e/mcp-widget/host.html" "$H/"
cp "$W/crates/triton-core/src/a2ui/mcp_app_widget.html" "$H/widget.html"
(cd "$W" && WIDGET_FILL_VECTORS_OUT=$H/vectors.json cargo test -q -p triton-tests --offline \
  --test it mcp_app_widget::fill_vectors >/dev/null)
python3 - "$W" "$H" <<'PY'
import json, glob, os, sys
w, h = sys.argv[1], sys.argv[2]
d = {os.path.basename(p)[:-9]: json.load(open(p))
     for p in sorted(glob.glob(f"{w}/crates/triton-tests/fixtures/conversation/golden/*.mcp.json"))}
v = json.load(open(f"{h}/vectors.json"))
open(f"{h}/data.js", "w").write("window.WIDGETS=" + json.dumps(d) + ";window.FILL_VECTORS=" + json.dumps(v) + ";")
PY
python3 -m http.server "$PORT" --bind 127.0.0.1 --directory "$H" >/dev/null 2>&1 &
SERVER=$!
cd "$H"
R="uvx rodney --local"
$R start >/dev/null
trap '$R stop >/dev/null 2>&1 || true; kill $SERVER 2>/dev/null || true' EXIT
F='document.getElementById("frame").contentWindow'
fail=0
check() { if [ "$2" = "$3" ]; then echo "PASS $1"; else echo "FAIL $1: got $2 want $3"; fail=1; fi; }
load() { $R open "http://127.0.0.1:$PORT/host.html?data=$1&theme=${3:-light}&mode=${2:-ok}" >/dev/null; $R waitload >/dev/null; $R wait 'body[data-h]' >/dev/null; }
last() { $R js "JSON.stringify(window.hostLog.filter(m=>m.method==='ui/message'||m.method==='ui/open-link').map(m=>m.params).slice(-1)[0])"; }

load 3a noreply
$R js "$F.document.querySelectorAll('.bar button')[1].click()" >/dev/null
$R sleep 9.5 >/dev/null
check "one click sends one ui/message" "$($R js "window.hostLog.filter(m=>m.method==='ui/message').length")" "1"
check "an unanswered ui/message is not re-sent" "$($R js "window.followups.length")" "0"

load 3a
$R js "window.sendResult({content:[{type:'text',text:'prose'}],structuredContent:{result:{}}})" >/dev/null
$R sleep 0.3 >/dev/null
check "a result without a card clears it" "$($R js "$F.document.querySelectorAll('#card button').length")" "0"
$R js "window.sendResult({content:[],structuredContent:{result:{}},_meta:{'triton/widget':WIDGETS['3e']}})" >/dev/null
$R sleep 0.3 >/dev/null
check "data under result _meta renders" "$($R js "$F.document.querySelector('.title').textContent")" "Retire Agent 20: filed for review"

res=$($R js "(()=>{const fill=$F.tritonWidgetFill; const bad=[]; for (const v of FILL_VECTORS){ const r=fill(v.parts, Object.assign(Object.create(null), v.values)); const ok = v.text!==undefined ? (r.missing.length===0 && r.text===v.text) : JSON.stringify(r.missing)===JSON.stringify(v.missing); if(!ok) bad.push(JSON.stringify([v.template,r])); } return bad.length? bad.join(' ; ') : 'all'; })()")
check "fill vectors" "$res" "all"

load 3a
$R js "window.sendResult({structuredContent:{widget:{version:1,body:[{type:'form',submit_label:'Go',groups:[{fields:[{name:'f',label:'Flag',kind:'boolean',required:true}],template:[{text:'Mark it '},{field:'f'}]}]}],actions:[]}}})" >/dev/null
$R sleep 0.3 >/dev/null
$R js "(()=>{const d=$F.document;const s=d.querySelector('select');s.value=[...s.options].find(o=>o.textContent==='Yes').value;s.dispatchEvent(new Event('change',{bubbles:true}));d.querySelector('button[type=submit]').click();})()" >/dev/null
$R sleep 0.3 >/dev/null
check "a boolean is sent as true/false" "$($R js "window.hostLog.filter(m=>m.method==='ui/message').map(m=>m.params.content[0].text).pop()")" "Mark it true"

load 3a
$R js "$F.document.querySelector('tbody tr').click()" >/dev/null; $R sleep 0.3 >/dev/null
check "a row sends its request" "$(last)" '{"role":"user","content":[{"type":"text","text":"Show the agent \"Agent 3\""}]}'
$R js "$F.document.querySelector('.bar button.primary').click()" >/dev/null; $R sleep 0.3 >/dev/null
check "a deep link opens through the host" "$(last)" '{"url":"https://app.example.com/acme/open?view=planned"}'

load 3c
$R js "(()=>{const d=$F.document;[...d.querySelectorAll('[role=tab]')][1].click();const p=[...d.querySelectorAll('form')][1];const s=p.querySelector('select');s.value='Retired';s.dispatchEvent(new Event('change',{bubbles:true}));const t=p.querySelector('textarea');t.value='  replaced by Agent 31 ';t.dispatchEvent(new Event('input',{bubbles:true}));p.querySelector('button[type=submit]').click();})()" >/dev/null
$R sleep 0.3 >/dev/null
check "a change form composes the complete request" "$(last)" '{"role":"user","content":[{"type":"text","text":"Set the lifecycle of Agent 28 to \"Retired\". Reason: replaced by Agent 31 (version 08cbcd825d41)"}]}'

for name in 3a 3b 3c 3d 3d-list 3e 3f 3g 2a-map 1l; do
  for theme in light dark; do
    load "$name" ok "$theme"
    $R sleep 0.3 >/dev/null
    $R screenshot-el '#frame' "$OUT/$name.$theme.png" >/dev/null
  done
done
echo "PNGs in $OUT"
exit $fail

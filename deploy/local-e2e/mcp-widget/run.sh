#!/usr/bin/env bash
# The answer-card MCP Apps widget (official A2UI v0.9 renderer, #368) in
# headless Chromium (rodney).
#
# host.html plays an MCP Apps host: it answers ui/initialize with a theme,
# sends each golden `<name>.a2ui.json` (Triton's card as basic-catalog v0.9
# messages) as a tool result's `_meta["triton/a2ui"]`, and records what the
# widget asks of it (ui/message, ui/open-link, size-changed). The run checks
# the user flows (one send per click, deep links, forms, a producer's own
# surface, a broken message not breaking the rest, the ChatGPT fallback, a
# late handshake), the shared fill vectors, and saves light and dark PNGs.
#
#   deploy/local-e2e/mcp-widget/run.sh [png-out-dir]
#
# Build the widget first if you changed it: (cd packages/mcp-widget && npm run build)
set -euo pipefail
W=$(cd "$(dirname "$0")/../../.." && pwd)
OUT=${1:-$W/target/mcp-widget-png}
H=$(mktemp -d)
PORT=${PORT:-18977}
mkdir -p "$OUT"
cp "$W/deploy/local-e2e/mcp-widget/host.html" "$H/"
cp "$W/crates/triton-core/src/a2ui/mcp_widget.html" "$H/widget.html"
python3 - "$W" "$H" <<'PY'
import json, glob, os, sys
w, h = sys.argv[1], sys.argv[2]
g = f"{w}/crates/triton-tests/fixtures/conversation"
d = {os.path.basename(p)[:-len(".a2ui.json")]: json.load(open(p))
     for p in sorted(glob.glob(f"{g}/golden/*.a2ui.json"))}
v = json.load(open(f"{g}/fill_vectors.parts.json"))
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
load() { $R open "http://127.0.0.1:$PORT/host.html?data=$1&theme=${3:-light}&mode=${2:-ok}" >/dev/null; $R waitload >/dev/null; $R wait 'body[data-h]' >/dev/null; $R sleep 0.5 >/dev/null; }
sent() { $R js "window.hostLog.filter(m=>m.method==='$1').length"; }
lastp() { $R js "JSON.stringify((window.hostLog.filter(m=>m.method==='$1').slice(-1)[0]||{}).params)"; }
has() { $R js "window.deepText().includes($(printf '%s' "$1" | python3 -c 'import json,sys;print(json.dumps(sys.stdin.read()))')) ? 'yes' : 'no'"; }
card() { $R js "window.sendResult({content:[],structuredContent:{result:{}},_meta:{'triton/a2ui':$1}})" >/dev/null; $R sleep 0.5 >/dev/null; }
S='"surfaceId":"triton-card"'
CREATE='{"version":"v0.9","createSurface":{'$S',"catalogId":"https://a2ui.org/specification/v0_9/catalogs/basic/catalog.json"}}'

# ── a card renders, a click sends once ─────────────────────────────────
load 3a noreply
check "the card renders its header" "$(has '6 planned Agents')" "yes"
check "a row click" "$($R js "window.clickText('Agent 3')")" "ok"
$R js "window.clickText('Agent 3')" >/dev/null
$R sleep 1 >/dev/null
check "one click sends one ui/message (and a second, in flight, none)" "$(sent ui/message)" "1"
check "the message is the row's request" "$($R js "(window.hostLog.find(m=>m.method==='ui/message')||{params:{content:[{}]}}).params.content[0].text")" 'Show the agent "Agent 3"'
check "an unanswered ui/message is not re-sent through window.openai" "$($R js "window.followups.length")" "0"

# ── links ──────────────────────────────────────────────────────────────
load 3a
$R js "window.clickText('Open in the portfolio')" >/dev/null; $R sleep 0.5 >/dev/null
check "a deep link opens through the host" "$(sent ui/open-link)" "1"
check "an https link" "$($R js "((window.hostLog.find(m=>m.method==='ui/open-link')||{}).params||{}).url.startsWith('https://')")" "true"
load 3a nolinks
$R js "window.clickText('Open in the portfolio')" >/dev/null; $R sleep 0.5 >/dev/null
check "no openLinks capability: no ui/open-link" "$(sent ui/open-link)" "0"

# ── results ────────────────────────────────────────────────────────────
load 3a
$R js "window.sendResult({content:[{type:'text',text:'prose'}],structuredContent:{result:{}}})" >/dev/null; $R sleep 0.3 >/dev/null
check "a result without a card clears it" "$($R js "window.deep('a2ui-surface').length")" "0"
card "WIDGETS['3e']"
check "a later result renders" "$(has 'Retire Agent 20')" "yes"

# A producer's own basic-catalog surface renders beside the card, and a
# message the renderer rejects drops itself, not the rest.
card '[{"version":"v0.9","createSurface":{"surfaceId":"triton-a2ui-p","catalogId":"https://a2ui.org/specification/v0_9/catalogs/basic/catalog.json"}},{"version":"v0.9","updateComponents":{"surfaceId":"triton-a2ui-p","components":[{"id":"root","component":"Text","text":"Pick a class."}]}},{"version":"v0.9","updateComponents":{"surfaceId":"nope","components":[{"id":"root","component":"NoSuchWidget"}]}}].concat(WIDGETS["3e"])'
check "a producer surface renders" "$(has 'Pick a class.')" "yes"
check "a broken message does not break the card" "$(has 'Retire Agent 20')" "yes"

# ── forms ──────────────────────────────────────────────────────────────
res=$($R js "(()=>{const fill=$F.tritonWidgetFill; const bad=[]; for (const v of FILL_VECTORS){ const r=fill(v.parts, Object.assign(Object.create(null), v.values)); const ok = v.text!==undefined ? (r.missing.length===0 && r.text===v.text) : JSON.stringify(r.missing)===JSON.stringify(v.missing); if(!ok) bad.push(JSON.stringify([v.template,r])); } return bad.length? bad.join(' ; ') : 'all'; })()")
check "fill vectors" "$res" "all"
FORM='['$CREATE',{"version":"v0.9","updateComponents":{'$S',"components":[{"id":"root","component":"Card","child":"col"},{"id":"col","component":"Column","children":["f","u","b"]},{"id":"f","component":"TextField","label":"Reason *","value":{"path":"/f0/why"}},{"id":"u","component":"CheckBox","label":"Urgent","value":{"path":"/f0/urgent"}},{"id":"bl","component":"Text","text":"Go"},{"id":"b","component":"Button","child":"bl","variant":"primary","action":{"event":{"name":"triton.send_message","context":{"parts":"[{\"text\":\"Note \"},{\"field\":\"why\"},{\"text\":\" urgent=\"},{\"field\":\"urgent\"}]","v:why":{"path":"/f0/why"},"v:urgent":{"path":"/f0/urgent"}}}}}]}},{"version":"v0.9","updateDataModel":{'$S',"path":"/","value":{"f0":{"why":"","urgent":false}}}}]'
load 3e
card "$FORM"
$R js "window.clickText('Go')" >/dev/null; $R sleep 0.5 >/dev/null
check "a form with a blank required field sends nothing" "$(sent ui/message)" "0"
$R js "(()=>{const i=window.deep('input,textarea').find(e=>e.type!=='checkbox'); i.value='moved'; i.dispatchEvent(new Event('input',{bubbles:true,composed:true})); const c=window.deep('input[type=checkbox]')[0]; c.click(); })()" >/dev/null
$R sleep 0.3 >/dev/null
$R js "window.clickText('Go')" >/dev/null; $R sleep 0.5 >/dev/null
check "a filled form sends its request, a boolean as true/false" "$($R js "(window.hostLog.find(m=>m.method==='ui/message')||{params:{content:[{}]}}).params.content[0].text")" "Note moved urgent=true"

# The canonical change form (3c), as Triton projects it: pick an option in
# the first tab, type a reason, submit. The request is the producer's
# template filled exactly as the server would.
load 3c
$R js "window.clickText('File for review')" >/dev/null; $R sleep 0.5 >/dev/null
check "3c: nothing chosen, nothing sent" "$(sent ui/message)" "0"
$R js "(()=>{const o=window.deep('*').find(e=>!e.children.length&&e.textContent.trim()==='IT Operations'); (o.closest('label,button,[role=option],[role=radio]')||o).click(); const t=window.deep('textarea,input').find(e=>e.type!=='radio'&&e.type!=='checkbox'); t.value='moved in October'; t.dispatchEvent(new Event('input',{bubbles:true,composed:true})); })()" >/dev/null
$R sleep 0.3 >/dev/null
$R js "window.clickText('File for review')" >/dev/null; $R sleep 0.5 >/dev/null
check "3c: the filled request" "$($R js "(window.hostLog.find(m=>m.method==='ui/message')||{params:{content:[{}]}}).params.content[0].text")" 'Set the owner of Agent 28 to "IT Operations". Reason: moved in October (version 08cbcd825d41)'

# A producer surface firing Triton's own event is ignored by the widget (the
# dispatcher drops such surfaces before they get here).
load 3e
card '[{"version":"v0.9","createSurface":{"surfaceId":"triton-a2ui-x","catalogId":"https://a2ui.org/specification/v0_9/catalogs/basic/catalog.json"}},{"version":"v0.9","updateComponents":{"surfaceId":"triton-a2ui-x","components":[{"id":"root","component":"Button","child":"l","action":{"event":{"name":"triton.send_message","context":{"text":"spoofed"}}}},{"id":"l","component":"Text","text":"Spoof"}]}}]'
$R js "window.clickText('Spoof')" >/dev/null; $R sleep 0.5 >/dev/null
check "a producer cannot fire triton.send_message" "$(sent ui/message)" "0"

# ── hosts ──────────────────────────────────────────────────────────────
load 3e slowinit
$R sleep 7 >/dev/null
check "a late handshake still renders" "$(has 'Retire Agent 20')" "yes"
$R open "http://127.0.0.1:$PORT/host.html?data=3e&mode=noinit" >/dev/null; $R waitload >/dev/null; $R sleep 0.5 >/dev/null
$R js "(()=>{$F.openai.toolResponseMetadata={'triton/a2ui':WIDGETS['3e']}; $F.dispatchEvent(new Event('openai:set_globals'));})()" >/dev/null
$R sleep 0.5 >/dev/null
check "ChatGPT's window.openai drives without an MCP Apps host" "$(has 'Retire Agent 20')" "yes"
$R js "(()=>{$F.openai.toolResponseMetadata={}; $F.dispatchEvent(new Event('openai:set_globals'));})()" >/dev/null
$R sleep 0.5 >/dev/null
check "ChatGPT: a later result without a card clears it" "$($R js "window.deep('a2ui-surface').length")" "0"

# ── pictures ───────────────────────────────────────────────────────────
for n in 1a 1e 1l 2a 2b 3a 3b 3c 3d 3e 3f 3g; do
  for t in light dark; do
    load "$n" ok "$t"
    $R screenshot "$OUT/$n-$t.png" >/dev/null
  done
done
echo "pictures: $OUT"
exit $fail

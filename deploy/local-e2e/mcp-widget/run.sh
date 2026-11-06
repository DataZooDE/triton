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
(cd "$W" && WIDGET_FILL_VECTORS_OUT=$H/vectors.json WIDGET_OVERSIZED_OUT=$H/oversized.json \
  cargo test -q -p triton-tests --offline --test it -- \
  mcp_app_widget::fill_vectors mcp_app_widget::an_oversized_answer >/dev/null)
python3 - "$W" "$H" <<'PY'
import json, glob, os, sys
w, h = sys.argv[1], sys.argv[2]
d = {os.path.basename(p)[:-9]: json.load(open(p))
     for p in sorted(glob.glob(f"{w}/crates/triton-tests/fixtures/conversation/golden/*.mcp.json"))}
d["oversized"] = json.load(open(f"{h}/oversized.json"))
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
load() { $R open "http://127.0.0.1:$PORT/host.html?data=$1&theme=${3:-light}&mode=${2:-ok}&locale=${4:-en-US}" >/dev/null; $R waitload >/dev/null; $R wait 'body[data-h]' >/dev/null; }
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
$R js "window.sendResult({_meta:{'triton/widget':{version:1,body:[{type:'form',submit_label:'Go',groups:[{fields:[{name:'f',label:'Flag',kind:'boolean',required:true}],template:[{text:'Mark it '},{field:'f'}]}]}],actions:[]}}})" >/dev/null
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

# Crew review 2026-11-02 (security, usability): duplicate sends, link
# policy, silent failures, late handshake, keyboard, locale.
msgs() { $R js "window.hostLog.filter(m=>m.method==='ui/message').length"; }
load 3a noreply
$R js "(()=>{const r=$F.document.querySelector('tbody tr');r.click();r.click();})()" >/dev/null; $R sleep 0.3 >/dev/null
check "a double-clicked row sends once" "$(msgs)" "1"
load 3c noreply
$R js "(()=>{const d=$F.document;const p=d.querySelector('form');const t=p.querySelector('textarea, input[type=text]');const s=p.querySelector('select');if(s){s.value=[...s.options].map(o=>o.value).filter(Boolean)[0];s.dispatchEvent(new Event('change',{bubbles:true}));} if(t){t.value='x';t.dispatchEvent(new Event('input',{bubbles:true}));} p.querySelector('button[type=submit]').click();})()" >/dev/null; $R sleep 0.3 >/dev/null
$R js "(()=>{const p=$F.document.querySelector('form');const t=p.querySelector('textarea, input[type=text]');if(t){t.value='xy';t.dispatchEvent(new Event('input',{bubbles:true}));} p.querySelector('button[type=submit]').click();})()" >/dev/null; $R sleep 0.3 >/dev/null
check "editing a form while its request is pending does not send it again" "$(msgs)" "1"
check "the pending form's submit stays disabled" "$($R js "$F.document.querySelector('form button[type=submit]').disabled")" "true"
load 3a nolinks
$R js "(()=>{window.popups=0;$F.open=()=>{window.popups++;return null;};$F.document.querySelector('.bar button.primary').click();})()" >/dev/null; $R sleep 0.3 >/dev/null
check "a host without openLinks gets no ui/open-link" "$($R js "window.hostLog.filter(m=>m.method==='ui/open-link').length")" "0"
check "a host without openLinks gets no popup" "$($R js "window.popups")" "0"
check "a link not opened is not reported as opened" "$($R js "$F.document.getElementById('status').textContent.startsWith('Opened')")" "false"
$R open "http://127.0.0.1:$PORT/host.html?data=3a&mode=slowinit" >/dev/null; $R waitload >/dev/null
$R sleep 7 >/dev/null
check "a late handshake answer still renders the card" "$($R js "!!$F.document.querySelector('#card .title')")" "true"
$R open "http://127.0.0.1:$PORT/host.html?data=3a&mode=noinit" >/dev/null; $R waitload >/dev/null
$R sleep 0.5 >/dev/null
$R js "(()=>{window.unhandled=0;$F.addEventListener('unhandledrejection',()=>{window.unhandled++;});$F.openai={toolOutput:{result:{}},toolResponseMetadata:{'triton/widget':WIDGETS['3a']},sendFollowUpMessage:()=>Promise.reject(new Error('refused'))};const w=$F;w.dispatchEvent(new w.Event('openai:set_globals'));})()" >/dev/null
$R sleep 0.3 >/dev/null
$R js "$F.document.querySelector('tbody tr').click()" >/dev/null; $R sleep 0.5 >/dev/null
check "a refused follow-up is no unhandled rejection" "$($R js "window.unhandled")" "0"
check "a refused follow-up is announced" "$($R js "$F.document.getElementById('status').textContent")" "Not sent — try again"
load 3a
check "an actionable row keeps its table role" "$($R js "$F.document.querySelector('tbody tr').hasAttribute('role')")" "false"
check "an actionable row has a native control" "$($R js "$F.document.querySelectorAll('tbody tr td:first-child button').length === $F.document.querySelectorAll('tbody tr').length")" "true"
load 3c
$R js "(()=>{const t=$F.document.querySelector('[role=tab]');t.focus();t.dispatchEvent(new KeyboardEvent('keydown',{key:'ArrowRight',bubbles:true}));})()" >/dev/null
check "arrow keys move between form tabs" "$($R js "(()=>{const t=[...$F.document.querySelectorAll('[role=tab]')];return t[1].getAttribute('aria-selected')+','+(t[1]===$F.document.activeElement)+','+t.map(x=>x.tabIndex).join('/');})()")" "true,true,-1/0/-1/-1"
load 3a ok light de-DE
$R js "window.sendResult({_meta:{'triton/widget':{version:1,body:[{type:'form',groups:[{fields:[{name:'f',label:'Flag',kind:'boolean',required:true}],template:[{text:'Mark it '},{field:'f'}]}]}],actions:[]}}})" >/dev/null
$R sleep 0.3 >/dev/null
check "the widget's own words follow the host locale" "$($R js "[...$F.document.querySelectorAll('option')].map(o=>o.textContent).join('|')+'|'+$F.document.querySelector('button[type=submit]').textContent")" "|Ja|Nein|Senden"

# Author decisions 2026-11-02: the exact message before the click (PD48),
# a visible status line, the link host only in the description, budgets.
txt() { $R js "(()=>{const e=$F.document.querySelector('$1');return e?e.textContent:'none';})()"; }
load 1e
check "a form previews its request before any input" "$(txt '.preview')" 'Will be sent as: Retire the agent "‹Agent›".'
$R js "(()=>{const d=$F.document;const r=[...d.querySelectorAll('input[type=radio]')].find(x=>x.value==='Agent 28');r.click();const t=d.querySelector('input[type=text]');t.value=' gone ';t.dispatchEvent(new Event('input',{bubbles:true}));})()" >/dev/null
check "the preview follows the fields" "$(txt '.preview')" 'Will be sent as: Retire the agent "Agent 28". Reason: gone'
$R js "$F.document.querySelector('button[type=submit]').click()" >/dev/null; $R sleep 0.3 >/dev/null
check "the form sends exactly the previewed text" "$($R js "window.hostLog.filter(m=>m.method==='ui/message').map(m=>m.params.content[0].text).pop()")" 'Retire the agent "Agent 28". Reason: gone'
load 3c
check "a form without a producer caption previews too" "$(txt 'form:not([hidden]) .preview')" 'Will be sent: Set the owner of Agent 28 to "‹New owner›". (version 08cbcd825d41)'
load 3c ok light de-DE
check "the preview caption follows the host locale" "$($R js "$F.document.querySelector('form .preview').textContent.split(':')[0]")" "Wird gesendet"
load 3a
check "a row shows the message it sends" "$(txt 'tbody tr .msg')" 'Show the agent "Agent 3"'
check "a button shows the message it sends" "$($R js "[...$F.document.querySelectorAll('.bar button')].map(b=>(b.querySelector('.msg')||{}).textContent||'-').join('|')")" '-|Which agents are planned with AI Act class high?|Which agents are planned without an owner?'
$R js "window.sendResult({_meta:{'triton/widget':{version:1,body:[{type:'chips',chips:[{label:'Show it',action:{type:'send_message',text:'Show it'}},{label:'More',action:{type:'send_message',text:'Show more of it'}}]}],actions:[]}}})" >/dev/null; $R sleep 0.3 >/dev/null
check "a chip shows its message only when it differs from its label" "$($R js "[...$F.document.querySelectorAll('.btns button')].map(b=>b.querySelectorAll('.msg').length).join(',')")" "0,1"
load 3a
check "no status before a click" "$(txt '#status')" ""
$R js "$F.document.querySelector('tbody tr').click()" >/dev/null; $R sleep 0.3 >/dev/null
check "a sent request says so visibly" "$($R js "(()=>{const s=$F.document.getElementById('status');return s.textContent+'|'+(s.getBoundingClientRect().height>4)})()")" "Sent|true"
$R js "$F.document.querySelector('.bar button.primary').click()" >/dev/null; $R sleep 0.3 >/dev/null
check "an opened link says so" "$(txt '#status')" "Opened"
check "a link names its host in the description, not the label" "$($R js "(()=>{const b=$F.document.querySelector('.bar button.primary');return b.textContent+'|'+b.title+'|'+b.getAttribute('aria-description')})()")" "Open in the portfolio|app.example.com|Opens app.example.com"
load 3a ok light de-DE
$R js "$F.document.querySelector('tbody tr').click()" >/dev/null; $R sleep 0.3 >/dev/null
check "the status follows the host locale" "$(txt '#status')" "Gesendet"
load 3a nolinks
$R js "$F.document.querySelector('.bar button.primary').click()" >/dev/null; $R sleep 0.3 >/dev/null
check "a link not opened asks to try again" "$(txt '#status')" "Not opened — try again"
load oversized
check "a cut table says how many rows are left out" "$(txt '.scroll + .more .n')" "… 70 more"
check "the cut names the open link" "$(txt '.scroll + .more button')" "Open"
check "a cut tree says how many nodes are left out" "$($R js "$F.document.querySelector('.tree').parentNode.querySelector('.more .n').textContent")" "… 224 more"
load oversized ok light de-DE
check "the cut follows the host locale" "$(txt '.more .n')" "… 70 weitere"

# Crew review 2026-11-02 (W1-W4 diff).
form1() { $R js "window.sendResult({_meta:{'triton/widget':{version:1,body:[{type:'form',groups:[{fields:[{name:'x',label:'Value',kind:'string',required:true}],template:[{text:'Set '},{field:'x'},{optional:[{text:' again '},{field:'x'}]}]}]}],actions:[]}}})" >/dev/null; $R sleep 0.3 >/dev/null; }
load 3a
form1
check "a missing field does not complete an optional segment in the preview" "$(txt '.preview')" "Will be sent: Set ‹Value›"
check "the preview is not a live region" "$($R js "$F.document.querySelector('.preview').getAttribute('aria-live')")" "null"
$R js "window.sendResult({structuredContent:{result:{},widget:WIDGETS['3e']}})" >/dev/null; $R sleep 0.3 >/dev/null
check "widget data in structured content alone draws no card" "$($R js "$F.document.querySelectorAll('#card *').length")" "0"
form1
$R js "window.sendContext({locale:'de-DE'})" >/dev/null; $R sleep 0.3 >/dev/null
check "a locale change re-renders the widget's own words" "$(txt 'button[type=submit]')" "Senden"
load 3a noreply
$R js "$F.document.querySelector('tbody tr').click()" >/dev/null; $R sleep 0.3 >/dev/null
check "a request in flight says so" "$(txt '#status')" "Sending…"
load 3a delayreply
$R js "$F.document.querySelector('tbody tr').click()" >/dev/null; $R sleep 0.1 >/dev/null
$R js "window.sendResult({_meta:{'triton/widget':WIDGETS['3e']}})" >/dev/null; $R sleep 1.5 >/dev/null
check "a request from the previous card does not report on the new one" "$(txt '#status')" ""
load 3a nolinks
$R js "$F.document.querySelector('.bar button.primary').click()" >/dev/null; $R sleep 0.3 >/dev/null
check "a failure looks different from a success" "$($R js "$F.document.getElementById('status').className")" "sent bad"
load 3a
$R js "(()=>{const b=$F.document.querySelectorAll('.bar button')[1];b.focus();b.click();})()" >/dev/null; $R sleep 1.6 >/dev/null
check "the clicked control keeps the focus" "$($R js "$F.document.activeElement===$F.document.querySelectorAll('.bar button')[1]")" "true"
$R js "window.sendResult({_meta:{'triton/widget':{version:1,body:[{type:'tree',nodes:[{label:'Node',action_label:'Open it',action:{type:'send_message',text:'Show Node'}}]}],actions:[]}}})" >/dev/null; $R sleep 0.3 >/dev/null
check "a tree node's action caption is its own control" "$($R js "[...$F.document.querySelectorAll('.tree button')].map(b=>b.firstChild.textContent).join('|')")" "Open it"
$R js "window.sendResult({_meta:{'triton/widget':{version:1,header:{title:'T'},body:[],actions:[{label:'Open',primary:true,action:{type:'open_url',url:'https://app.example.com/x'}}],more:3}}})" >/dev/null; $R sleep 0.3 >/dev/null
check "dropped blocks are counted as sections" "$(txt '.more .n')" "… 3 more sections"
load oversized
check "the open link appears once among the cuts" "$($R js "$F.document.querySelectorAll('.more button').length")" "1"

for name in 3a 3b 3c 3d 3d-list 3e 3f 3g 2a-map 1l 1e oversized; do
  for theme in light dark; do
    load "$name" ok "$theme"
    $R sleep 0.3 >/dev/null
    $R screenshot-el '#frame' "$OUT/$name.$theme.png" >/dev/null
  done
done
echo "PNGs in $OUT"
exit $fail

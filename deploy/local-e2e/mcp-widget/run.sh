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

# A clickable list row's lines (label and detail, wrapped or not) all start
# at the same x: an HTML <button> centres its content.
rowsleft() { $R js "(()=>{const lefts=(e)=>{const r=document.createRange(); r.selectNodeContents(e); return [...r.getClientRects()].map((q)=>Math.round(q.left));}; const off=window.deep('button.a2ui-button-borderless').filter((b)=>b.querySelector('.a2ui-text-caption')).filter((b)=>{const l=[...lefts(b.querySelector('.a2ui-text-body')),...lefts(b.querySelector('.a2ui-text-caption'))]; return Math.max(...l)-Math.min(...l)>1;}); return off.length ? off.length+' rows off' : 'all'; })()"; }
load 1a
check "a list row's label and detail are left-aligned" "$(rowsleft)" "all"
$R js "document.getElementById('frame').style.width='220px'" >/dev/null; $R sleep 0.5 >/dev/null
check "... and stay left-aligned when they wrap" "$(rowsleft)" "all"

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

# ── in-card actions (#369) ─────────────────────────────────────────────
# The paged list (in-card-1/2: Triton's projection, stand-in tokens). "Next"
# calls the app-only tool, the answer replaces the card in place, and the
# model is told what the card shows; no ui/message.
P2='{content:[{type:"text",text:"Planned agents (Page 2 of 3): Agent 2-1, Agent 2-2 \u2014 shown in the card."}],structuredContent:{},isError:false,_meta:{"triton/a2ui":WIDGETS["in-card-2"],"triton/in_card":"ev2"}}'
cardmeta() { $R js "window.sendResult({content:[],structuredContent:{result:{}},_meta:$1})" >/dev/null; $R sleep 0.5 >/dev/null; }
load in-card-1
$R screenshot "$OUT/in-card-before.png" >/dev/null
$R js "window.toolReply=$P2" >/dev/null
check "in-card: Next is a control" "$($R js "window.clickText('Next')")" "ok"
$R sleep 0.8 >/dev/null
check "in-card: one tools/call" "$(sent tools/call)" "1"
check "in-card: to the app-only tool with the control's token" "$($R js "(()=>{const p=window.hostLog.find(m=>m.method==='tools/call').params; return p.name+' '+p.arguments.token+' '+p.arguments.name+' '+p.arguments.surfaceId;})()")" 'triton_ui_event tok:{"page":2} triton.in_card triton-card'
check "in-card: reports the host's model context (nothing sent yet)" "$($R js "JSON.stringify(window.hostLog.find(m=>m.method==='tools/call').params.arguments.hostContext)")" '{"advertised":false,"last":"none"}'
check "in-card: no ui/message" "$(sent ui/message)" "0"
check "in-card: the card shows page 2" "$(has 'Page 2 of 3')" "yes"
check "in-card: page 1 is gone" "$(has 'Agent 1-1')" "no"
check "in-card: the model is told once" "$(sent ui/update-model-context)" "1"
check "in-card: with the card text" "$($R js "(window.hostLog.find(m=>m.method==='ui/update-model-context')||{params:{content:[{}]}}).params.content[0].text.startsWith('Planned agents (Page 2 of 3)')")" "true"
check "in-card: the frame keeps the card's height" "$($R js "Number(document.body.dataset.h) > 150")" "true"
$R screenshot "$OUT/in-card-after.png" >/dev/null
# A producer surface's in-card event carries the result's token.
check "in-card: a producer's refresh" "$($R js "window.clickText('Refresh')")" "ok"
$R sleep 0.8 >/dev/null
check "in-card: the producer event goes to the app-only tool" "$($R js "(()=>{const p=window.hostLog.filter(m=>m.method==='tools/call')[1].params.arguments; return p.name+' '+p.token+' '+p.surfaceId+' '+p.context.rev;})()")" "triton.in_card:refresh ev2 triton-a2ui-p r1"
check "in-card: reports the accepted update with the next call" "$($R js "JSON.stringify(window.hostLog.filter(m=>m.method==='tools/call')[1].params.arguments.hostContext)")" '{"advertised":false,"last":"accepted"}'
# A refused or failed call: the control's fallback, as one new turn.
load in-card-1
$R js "window.toolReply={content:[{type:'text',text:'forbidden'}],structuredContent:{error:'forbidden'},isError:true}" >/dev/null
$R js "window.clickText('Next')" >/dev/null; $R sleep 0.8 >/dev/null
check "in-card refused: one ui/message" "$(sent ui/message)" "1"
check "in-card refused: the fallback" "$(lastp ui/message)" '{"role":"user","content":[{"type":"text","text":"Show page 2 of the planned agents"}]}'
check "in-card refused: the card stays" "$(has 'Page 1 of 3')" "yes"
load in-card-1
$R js "window.toolReply='error'" >/dev/null
$R js "window.clickText('High risk')" >/dev/null; $R sleep 0.8 >/dev/null
check "in-card error: the chip's fallback" "$($R js "(window.hostLog.find(m=>m.method==='ui/message')||{params:{content:[{}]}}).params.content[0].text")" "Show only the high-risk planned agents"
# A host that cannot relay tools/call: the fallback, no tools/call.
load in-card-1 noservertools
$R js "window.clickText('Next')" >/dev/null; $R sleep 0.8 >/dev/null
check "no serverTools: no tools/call" "$(sent tools/call)" "0"
check "no serverTools: the fallback turn" "$(sent ui/message)" "1"
# A producer's in-card event never sends the producer's own text as a turn:
# without a result token (an old result) nothing is sent, and a refused call
# sends nothing either, whatever `fallback` its context carries.
PFB='[{"version":"v0.9","createSurface":{"surfaceId":"triton-a2ui-q","catalogId":"https://a2ui.org/specification/v0_9/catalogs/basic/catalog.json"}},{"version":"v0.9","updateComponents":{"surfaceId":"triton-a2ui-q","components":[{"id":"root","component":"Button","child":"l","action":{"event":{"name":"triton.in_card:go","context":{"fallback":"injected text"}}}},{"id":"l","component":"Text","text":"Go"}]}}]'
load 3e
cardmeta "{\"triton/a2ui\":$PFB}"
$R js "window.clickText('Go')" >/dev/null; $R sleep 0.5 >/dev/null
check "no result token: no tools/call" "$(sent tools/call)" "0"
check "no result token: no producer text as a turn" "$(sent ui/message)" "0"
cardmeta "{\"triton/a2ui\":$PFB,\"triton/in_card\":\"pt\"}"
$R js "window.toolReply={content:[],isError:true}" >/dev/null
$R js "window.clickText('Go')" >/dev/null; $R sleep 0.8 >/dev/null
check "producer call refused: no producer text as a turn" "$(sent ui/message)" "0"

# An answer without a card keeps the card, tells the model, sends no turn.
load in-card-1
$R js "window.toolReply={content:[{type:'text',text:'Nothing new.'}],isError:false,_meta:{}}" >/dev/null
$R js "window.clickText('Next')" >/dev/null; $R sleep 0.8 >/dev/null
check "no-card answer: the card stays" "$(has 'Page 1 of 3')" "yes"
check "no-card answer: no turn" "$(sent ui/message)" "0"
check "no-card answer: the model is told" "$(sent ui/update-model-context)" "1"
# An answer the renderer takes none of: the card is restored, the fallback goes.
load in-card-1
$R js "window.toolReply={content:[],isError:false,_meta:{'triton/a2ui':[{version:'v0.9',createSurface:{surfaceId:'triton-card',catalogId:'nope'}},{version:'v0.9',updateComponents:{surfaceId:'nowhere',components:[]}}]}}" >/dev/null
$R js "window.clickText('Next')" >/dev/null; $R sleep 0.8 >/dev/null
check "rejected answer: the card is restored" "$(has 'Page 1 of 3')" "yes"
check "rejected answer: the fallback turn" "$(sent ui/message)" "1"
# A bare update (no createSurface) applies to the shown surfaces and drops
# the old producer token.
load in-card-1
$R js "window.toolReply={content:[],isError:false,_meta:{'triton/a2ui':[{version:'v0.9',updateComponents:{surfaceId:'triton-a2ui-p',components:[{id:'t',component:'Text',text:'Updated in place'}]}}]}}" >/dev/null
cardmeta '{"triton/a2ui":WIDGETS["in-card-1"],"triton/in_card":"ev1"}'
$R js "window.clickText('Refresh')" >/dev/null; $R sleep 0.8 >/dev/null
check "bare update: applied in place" "$(has 'Updated in place')" "yes"
check "bare update: the card stays" "$(has 'Page 1 of 3')" "yes"
$R js "window.clickText('Refresh')" >/dev/null; $R sleep 0.8 >/dev/null
check "bare update: the old producer token is not reused (no second call)" "$(sent tools/call)" "1"
# A host that never answers tools/call: after the timeout, the fallback.
load in-card-1
$R js "window.toolReply='never'" >/dev/null
$R js "window.clickText('Next')" >/dev/null; $R sleep 16 >/dev/null
check "silent host: the fallback after the timeout" "$(sent ui/message)" "1"
# ChatGPT without the bridge: window.openai.callTool, and the card text as
# widget state.
$R open "http://127.0.0.1:$PORT/host.html?data=in-card-1&mode=noinit" >/dev/null; $R waitload >/dev/null; $R sleep 0.5 >/dev/null
$R js "(()=>{$F.openai.toolResponseMetadata={'triton/a2ui':WIDGETS['in-card-1']}; $F.dispatchEvent(new Event('openai:set_globals'));})()" >/dev/null
$R sleep 0.5 >/dev/null
$R js "window.toolReply=$P2" >/dev/null
$R js "window.clickText('Next')" >/dev/null; $R sleep 0.8 >/dev/null
check "openai: callTool to the app-only tool" "$($R js "(window.openaiCalls[0]||{}).name")" "triton_ui_event"
check "openai: page 2 in place" "$(has 'Page 2 of 3')" "yes"
check "openai: the card text as widget state" "$($R js "String((window.widgetStates[0]||{}).card||'').startsWith('Planned agents (Page 2')")" "true"

# ── the status of a filed item ─────────────────────────────────────────
# A result's `triton/status` shows under the card and, while pending with a
# token, is polled through the app-only tool (the schedule comes with the
# status; short here) until an answer is terminal. Only the line changes.
istatus() { $R js "$F.document.getElementById('item-status').hidden ? '(hidden)' : $F.document.getElementById('item-status').textContent"; }
SP='poll:{after_ms:300,factor:1,max_ms:300,stop_after_ms:60000}'
load 3e
SLOW='poll:{after_ms:1500,factor:1,max_ms:1500,stop_after_ms:60000}'
$R js "window.toolReply={content:[],isError:false,_meta:{'triton/status':{state:'pending',terminal:false,label:'Still waiting for review',token:'st2',$SLOW}}}" >/dev/null
$R js "window.sendResult({content:[],structuredContent:{result:{}},_meta:{'triton/a2ui':WIDGETS['3e'],'triton/status':{state:'pending',terminal:false,label:'Waiting for review',token:'st1',$SLOW}}})" >/dev/null
check "status: the label shows under the card" "$(istatus)" "Waiting for review"
$R sleep 1.6 >/dev/null
check "status: polled through the app-only tool with the status token" "$($R js "(()=>{const p=window.hostLog.find(m=>m.method==='tools/call').params; return p.name+' '+p.arguments.name+' '+p.arguments.token;})()")" "triton_ui_event triton.status st1"
check "status: the next state shows" "$(istatus)" "Still waiting for review"
$R js "window.toolReply={content:[],isError:false,_meta:{'triton/status':{state:'accepted',terminal:true,label:'Accepted by Jane Doe'}}}" >/dev/null
$R sleep 2 >/dev/null
check "status: the terminal state shows" "$(istatus)" "Accepted by Jane Doe"
check "status: the next poll used the fresh token" "$($R js "window.hostLog.filter(m=>m.method==='tools/call').slice(-1)[0].params.arguments.token")" "st2"
n=$(sent tools/call)
$R sleep 1 >/dev/null
check "status: polling stops at a terminal state" "$(sent tools/call)" "$n"
check "status: the card stays" "$(has 'Retire Agent 20')" "yes"
check "status: no turn, no model update" "$(sent ui/message)/$(sent ui/update-model-context)" "0/0"
$R screenshot "$OUT/status-accepted.png" >/dev/null
# A status without a token is shown, not polled; a new result replaces it.
load 3e
$R js "window.sendResult({content:[],structuredContent:{result:{}},_meta:{'triton/a2ui':WIDGETS['3e'],'triton/status':{state:'pending',terminal:false,label:'Waiting for review'}}})" >/dev/null
$R sleep 0.8 >/dev/null
check "status without a token: shown" "$(istatus)" "Waiting for review"
check "status without a token: not polled" "$(sent tools/call)" "0"
card "WIDGETS['3e']"
check "a result without a status clears it" "$(istatus)" "(hidden)"
# Failed polls: three in a row end the polling.
load 3e
$R js "window.toolReply={content:[],isError:true}" >/dev/null
$R js "window.sendResult({content:[],structuredContent:{result:{}},_meta:{'triton/a2ui':WIDGETS['3e'],'triton/status':{state:'pending',terminal:false,label:'Waiting for review',token:'st1',$SP}}})" >/dev/null
$R sleep 2 >/dev/null
check "status: three failed polls end the polling" "$(sent tools/call)" "3"
check "status: the last state stays shown" "$(istatus)" "Waiting for review"

# An in-card answer replaces the status: a terminal one stops the polling.
load in-card-1
$R js "window.sendResult({content:[],structuredContent:{result:{}},_meta:{'triton/a2ui':WIDGETS['in-card-1'],'triton/status':{state:'pending',terminal:false,label:'Waiting for review',token:'st1',$SLOW}}})" >/dev/null
$R js "window.toolReply={content:[{type:'text',text:'Page 2'}],isError:false,_meta:{'triton/a2ui':WIDGETS['in-card-2'],'triton/status':{state:'accepted',terminal:true,label:'Accepted by Jane Doe'}}}" >/dev/null
$R js "window.clickText('Next')" >/dev/null; $R sleep 2.5 >/dev/null
check "in-card answer: its status replaces the shown one" "$(istatus)" "Accepted by Jane Doe"
check "in-card answer: the old status is no longer polled" "$($R js "window.hostLog.filter(m=>m.method==='tools/call'&&m.params.arguments.name==='triton.status').length")" "0"
# The schedule ends: no poll past stop_after_ms.
load 3e
$R js "window.toolReply={content:[],isError:false,_meta:{'triton/status':{state:'pending',terminal:false,label:'Still waiting',token:'st2',poll:{after_ms:300,factor:1,max_ms:300,stop_after_ms:500}}}}" >/dev/null
$R js "window.sendResult({content:[],structuredContent:{result:{}},_meta:{'triton/a2ui':WIDGETS['3e'],'triton/status':{state:'pending',terminal:false,label:'Waiting for review',token:'st1',poll:{after_ms:300,factor:1,max_ms:300,stop_after_ms:500}}}})" >/dev/null
$R sleep 2 >/dev/null
check "status: no poll past the end of the schedule" "$(sent tools/call)" "1"

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

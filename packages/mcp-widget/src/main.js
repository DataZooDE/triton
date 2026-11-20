// Triton's MCP Apps answer card: the official A2UI v0.9 renderer
// (@a2ui/lit, basic catalog) driven by the A2UI messages a tool result
// carries in `_meta["triton/a2ui"]`, plus the MCP Apps host bridge
// (JSON-RPC over postMessage, spec 2026-01-26) and ChatGPT's `window.openai`.
//
// Controls are Triton's stateless events (crates/triton-core/src/a2ui/basic.rs):
//   triton.send_message {text} or {parts, v:<field>...} -> ui/message
//   triton.open_url {url}                                -> ui/open-link
//   triton.in_card {token, fallback}                     -> tools/call (#369)
// and a producer surface's own `triton.in_card:<name>` events -> tools/call.
// An in-card call (`triton_ui_event`, an app-only tool) re-runs the producing
// tool; its result replaces the card in place and the widget tells the model
// what the card now shows (ui/update-model-context). Where the host cannot
// call the tool, or the call fails, the control's fallback is sent as a new
// turn instead (only read-only controls are in-card, so that is safe).
import { MessageProcessor } from '@a2ui/web_core/v0_9';
import { injectBasicCatalogStyles } from '@a2ui/web_core/v0_9/basic_catalog';
import { basicCatalog } from '@a2ui/lit/v0_9';
import { fill, inputs } from './fill.js';

const META_KEY = 'triton/a2ui';
const SEND_MESSAGE = 'triton.send_message';
const OPEN_URL = 'triton.open_url';
const IN_CARD = 'triton.in_card';
const IN_CARD_PREFIX = 'triton.in_card:';
const IN_CARD_TOOL = 'triton_ui_event';
const IN_CARD_META = 'triton/in_card';
const IN_CARD_TIMEOUT_MS = 15000;
const PROTOCOL = '2026-01-26';
// Triton's own card: the only surface whose `triton.*` events are honoured.
const CARD_SURFACE = 'triton-card';

const app = document.getElementById('app');
const status = document.getElementById('status');
injectBasicCatalogStyles();

function say(text, error) {
  status.textContent = text || '';
  status.classList.toggle('err', !!error);
}

// ── host bridge ─────────────────────────────────────────────────────────
let bridge = false;
let openLinks = false;
let serverTools = false;
let nextId = 1;
const pending = new Map();

function post(msg) {
  window.parent.postMessage({ jsonrpc: '2.0', ...msg }, '*');
}

// No timeout: Claude holds a `ui/message` behind its confirmation banner for
// as long as the person takes.
function request(method, params, timeoutMs) {
  const id = nextId++;
  post({ id, method, params });
  return new Promise((resolve, reject) => {
    pending.set(id, { resolve, reject });
    // Only for calls a person is not deciding on (an in-card call): a host
    // that never answers must not hold the card forever.
    if (timeoutMs) {
      setTimeout(() => {
        if (pending.delete(id)) reject(new Error('timeout'));
      }, timeoutMs);
    }
  });
}

function notify(method, params) {
  post({ method, params });
}

window.addEventListener('message', (e) => {
  if (e.source !== window.parent) return;
  const m = e.data;
  if (!m || m.jsonrpc !== '2.0') return;
  if (m.id !== undefined && pending.has(m.id) && !m.method) {
    const p = pending.get(m.id);
    pending.delete(m.id);
    if (m.error) p.reject(new Error(m.error.message || 'error'));
    else p.resolve(m.result);
    return;
  }
  switch (m.method) {
    case 'ui/notifications/tool-result':
      render(m.params?._meta?.[META_KEY], m.params?._meta);
      return;
    case 'ui/notifications/host-context-changed':
      applyContext(m.params);
      return;
    case 'ui/resource-teardown':
      if (m.id !== undefined) post({ id: m.id, result: {} });
      return;
    default:
      if (m.id !== undefined && m.method) {
        post({ id: m.id, error: { code: -32601, message: 'method not found' } });
      }
  }
});

function applyContext(ctx) {
  if (!ctx) return;
  if (ctx.theme === 'dark' || ctx.theme === 'light') {
    document.documentElement.style.colorScheme = ctx.theme;
  }
  const vars = ctx.styles?.variables;
  if (vars && typeof vars === 'object') {
    for (const [k, v] of Object.entries(vars)) {
      if (/^--[a-z0-9-]+$/i.test(k) && typeof v === 'string') {
        document.documentElement.style.setProperty(k, v);
      }
    }
  }
  if (typeof ctx.locale === 'string') document.documentElement.lang = ctx.locale;
}

// ── rendering ───────────────────────────────────────────────────────────
let processor = null;
let generation = 0;
// The token a producer surface's in-card events carry (the result's).
let eventToken = null;
// What the card shows now, to restore it when an in-card answer draws nothing.
let shown = { messages: null, meta: null };

/** Replace the card with the surfaces `messages` build; anything else
 * (no card for this result) clears it. A malformed message drops itself,
 * never the rest. */
function render(messages, meta) {
  generation++;
  shown = { messages, meta };
  eventToken = typeof meta?.[IN_CARD_META] === 'string' ? meta[IN_CARD_META] : null;
  inflight = false;
  say('');
  processor?.dispose();
  processor = null;
  app.replaceChildren();
  if (!Array.isArray(messages) || messages.length === 0) {
    resize();
    return 0;
  }
  processor = new MessageProcessor([basicCatalog], onAction);
  processor.onSurfaceCreated((surface) => {
    const el = document.createElement('a2ui-surface');
    el.surface = surface;
    app.append(el);
  });
  const applied = apply(messages);
  settled();
  return applied;
}

/** Feed `messages` to the processor; how many it took without an error. */
function apply(messages) {
  let applied = 0;
  for (const m of messages) {
    const errs = processor.processMessagesWithErrors([m]);
    for (const err of errs) console.warn('triton widget: message dropped', err);
    if (!errs.length) applied++;
  }
  return applied;
}

// ── actions ─────────────────────────────────────────────────────────────
let inflight = false;

function safeLink(u) {
  try {
    const url = new URL(u);
    return url.protocol === 'https:' && !!url.hostname && !url.username && !url.password
      ? url.href
      : null;
  } catch {
    return null;
  }
}

// Triton's own events (send message, open link, rerun in place) are honoured
// on Triton's card only, whose controls passed Triton's checks (basic.rs); a
// producer surface cannot fire them (the dispatcher also drops such surfaces).
// A producer surface may fire `triton.in_card:<name>` events, which go to the
// app-only tool with the answer's token (#369).
async function onAction(a) {
  if (a.surfaceId === CARD_SURFACE && a.name === SEND_MESSAGE) return sendMessage(a.context || {});
  if (a.surfaceId === CARD_SURFACE && a.name === OPEN_URL) return openUrl(a.context?.url);
  if (a.surfaceId === CARD_SURFACE && a.name === IN_CARD) {
    const ctx = a.context || {};
    return inCard(
      { surfaceId: a.surfaceId, name: a.name, sourceComponentId: a.sourceComponentId || '',
        context: {}, token: ctx.token },
      ctx.fallback,
    );
  }
  // A producer's in-card event has no fallback turn: the producer's own
  // text is never sent as the user's (only Triton's vetted card may).
  if (a.surfaceId !== CARD_SURFACE && typeof a.name === 'string' && a.name.startsWith(IN_CARD_PREFIX)) {
    const ctx = a.context && typeof a.context === 'object' ? a.context : {};
    return inCard(
      { surfaceId: a.surfaceId, name: a.name, sourceComponentId: a.sourceComponentId || '',
        context: ctx, token: eventToken },
      null,
    );
  }
  say('This action is not available here.', true);
}

// ── in-card calls (#369) ────────────────────────────────────────────────
/** Call the app-only tool; the result, or null when the host cannot. */
async function callInCard(args) {
  if (bridge && serverTools) {
    return request('tools/call', { name: IN_CARD_TOOL, arguments: args }, IN_CARD_TIMEOUT_MS);
  }
  if (!bridge && window.openai?.callTool) {
    return window.openai.callTool(IN_CARD_TOOL, args);
  }
  return null;
}

/** Re-run the card's tool in place; on any failure, send `fallback` as a
 * new turn (or say so when there is none). One call at a time. */
async function inCard(args, fallback) {
  if (inflight) return;
  const gen = generation;
  let res = null;
  if (typeof args.token === 'string' && args.token) {
    inflight = true;
    say('Updating\u2026');
    try {
      res = await callInCard(args);
    } catch {
      res = null;
    }
    if (gen !== generation) return; // a newer result replaced the card
    inflight = false;
  }
  const meta = res?._meta ?? res?.meta;
  const messages = meta?.[META_KEY];
  if (res && !res.isError) {
    // An answer without a card keeps the card shown; one the renderer
    // takes none of is a failure (the card is restored).
    if (!Array.isArray(messages) || !messages.length || applyInPlace(messages, meta)) {
      say('');
      updateModelContext(res);
      return;
    }
  }
  say('');
  if (typeof fallback === 'string' && fallback.trim()) return sendMessage({ text: fallback });
  say('This action is not available here.', true);
}

/** An in-card result: a complete answer (it builds its surfaces) replaces
 * the card; bare updates apply to the surfaces already shown. */
function applyInPlace(messages, meta) {
  if (!processor || messages.some((m) => m && m.createSurface)) {
    const prev = shown;
    if (render(messages, meta) > 0) return true;
    render(prev.messages, prev.meta);
    return false;
  }
  const applied = apply(messages);
  eventToken = typeof meta?.[IN_CARD_META] === 'string' ? meta[IN_CARD_META] : null;
  settled();
  return applied > 0;
}

/** Tell the model what the card shows now (its one-line card text), so a
 * later question about it is answered from the current state. */
function updateModelContext(res) {
  const content = (res.content || []).filter((c) => c && c.type === 'text' && typeof c.text === 'string');
  if (!content.length) return;
  if (bridge) {
    request('ui/update-model-context', { content }).catch(() => {});
  } else if (window.openai?.setWidgetState) {
    // ChatGPT without the MCP Apps bridge: widget state is what its model
    // sees of the widget.
    Promise.resolve(window.openai.setWidgetState({ card: content.map((c) => c.text).join('\n') }))
      .catch(() => {});
  }
}

async function sendMessage(ctx) {
  let text = typeof ctx.text === 'string' ? ctx.text : null;
  if (text === null && typeof ctx.parts === 'string') {
    let parts;
    try {
      parts = JSON.parse(ctx.parts);
    } catch {
      say('Could not send this request.', true);
      return;
    }
    const r = fill(parts, inputs(ctx));
    if (r.missing.length) {
      say(`Please fill in: ${r.missing.join(', ')}`, true);
      return;
    }
    text = r.text;
  }
  if (!text || !text.trim()) return;
  // One click sends once: a second click while the first is in flight, or
  // a double click, sends nothing.
  if (inflight) return;
  inflight = true;
  const gen = generation;
  let ok = false;
  try {
    if (bridge) {
      const res = await request('ui/message', {
        role: 'user',
        content: [{ type: 'text', text }],
      });
      ok = !res?.isError;
    } else if (window.openai?.sendFollowUpMessage) {
      await window.openai.sendFollowUpMessage({ prompt: text });
      ok = true;
    }
  } catch {
    ok = false;
  }
  if (gen !== generation) return;
  inflight = false;
  say(ok ? 'Sent.' : 'Could not send this request.', !ok);
}

let opening = false;
async function openUrl(raw) {
  const url = safeLink(raw);
  // One click opens once.
  if (!url || opening) return;
  opening = true;
  let ok = false;
  try {
    if (bridge && openLinks) {
      const res = await request('ui/open-link', { url });
      ok = !res?.isError;
    } else if (window.openai?.openExternal) {
      await window.openai.openExternal({ href: url });
      ok = true;
    } else if (!bridge) {
      ok = !!window.open(url, '_blank', 'noopener,noreferrer');
    }
  } catch {
    ok = false;
  }
  opening = false;
  if (!ok) say('Could not open the link.', true);
}

// ── size ────────────────────────────────────────────────────────────────
let lastSize = '';
/** Report the page's size when it changed; `force` once the host bridge is
 * up (a change measured before it was could not be sent). */
function resize(force) {
  const width = document.documentElement.clientWidth;
  const height = Math.ceil(document.documentElement.getBoundingClientRect().height);
  const size = `${width}x${height}`;
  if (size === lastSize && force !== true) return;
  lastSize = size;
  if (bridge) notify('ui/notifications/size-changed', { width, height });
  window.openai?.notifyIntrinsicHeight?.(height);
}
new ResizeObserver(() => resize()).observe(document.body);
// After the renderer drew new surfaces. Measured synchronously, an in-place
// update of a card of the same height reads the emptied page, and the
// observer never fires again (the body ends at the size it started at).
function settled() {
  requestAnimationFrame(() => requestAnimationFrame(() => resize()));
}

// ── start ───────────────────────────────────────────────────────────────
// ChatGPT hands the result in `window.openai` (and on later results through
// `openai:set_globals`); an MCP Apps host answers `ui/initialize`.
let lastOpenAiMeta;
function fromOpenAi() {
  if (bridge) return;
  const m = window.openai?.toolResponseMetadata;
  // Only a new result re-renders (other globals, a theme say, change too);
  // a new result without the key is no card, so the old one goes.
  if (m === lastOpenAiMeta) return;
  lastOpenAiMeta = m;
  if (!m) return;
  const meta = m._meta ?? m.call_tool_result?._meta ?? m;
  render(meta?.[META_KEY], meta);
}
window.addEventListener('openai:set_globals', fromOpenAi);
fromOpenAi();

request('ui/initialize', {
  protocolVersion: PROTOCOL,
  appInfo: { name: 'triton-answer-card', version: '2' },
  appCapabilities: { availableDisplayModes: ['inline'] },
})
  .then((res) => {
    bridge = true;
    // A host that reports no capabilities at all is not refusing links.
    openLinks = !res?.hostCapabilities || !!res.hostCapabilities.openLinks;
    // In-card calls need the host to relay tools/call (MCP Apps `serverTools`).
    serverTools = !!res?.hostCapabilities?.serverTools;
    applyContext(res?.hostContext);
    notify('ui/notifications/initialized', {});
    resize(true);
  })
  .catch(() => {
    // No MCP Apps host answered: ChatGPT's `window.openai` (if any) drives.
  });

// For the browser harness: the pure filler, as every channel fills.
window.tritonWidgetFill = fill;

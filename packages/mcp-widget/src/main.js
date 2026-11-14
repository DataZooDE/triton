// Triton's MCP Apps answer card: the official A2UI v0.9 renderer
// (@a2ui/lit, basic catalog) driven by the A2UI messages a tool result
// carries in `_meta["triton/a2ui"]`, plus the MCP Apps host bridge
// (JSON-RPC over postMessage, spec 2026-01-26) and ChatGPT's `window.openai`.
//
// Controls are Triton's stateless events (crates/triton-core/src/a2ui/basic.rs):
//   triton.send_message {text} or {parts, v:<field>...} -> ui/message
//   triton.open_url {url}                                -> ui/open-link
// Any other event (a producer surface's own) is not handled here yet (#369).
import { MessageProcessor } from '@a2ui/web_core/v0_9';
import { injectBasicCatalogStyles } from '@a2ui/web_core/v0_9/basic_catalog';
import { basicCatalog } from '@a2ui/lit/v0_9';
import { fill, inputs } from './fill.js';

const META_KEY = 'triton/a2ui';
const SEND_MESSAGE = 'triton.send_message';
const OPEN_URL = 'triton.open_url';
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
let nextId = 1;
const pending = new Map();

function post(msg) {
  window.parent.postMessage({ jsonrpc: '2.0', ...msg }, '*');
}

// No timeout: Claude holds a `ui/message` behind its confirmation banner for
// as long as the person takes.
function request(method, params) {
  const id = nextId++;
  post({ id, method, params });
  return new Promise((resolve, reject) => pending.set(id, { resolve, reject }));
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
      render(m.params?._meta?.[META_KEY]);
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

/** Replace the card with the surfaces `messages` build; anything else
 * (no card for this result) clears it. A malformed message drops itself,
 * never the rest. */
function render(messages) {
  generation++;
  inflight = false;
  say('');
  processor?.dispose();
  processor = null;
  app.replaceChildren();
  if (!Array.isArray(messages) || messages.length === 0) {
    resize();
    return;
  }
  processor = new MessageProcessor([basicCatalog], onAction);
  processor.onSurfaceCreated((surface) => {
    const el = document.createElement('a2ui-surface');
    el.surface = surface;
    app.append(el);
  });
  for (const m of messages) {
    for (const err of processor.processMessagesWithErrors([m])) {
      console.warn('triton widget: message dropped', err);
    }
  }
  resize();
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

// `triton.*` events come from Triton's own card only, whose controls passed
// Triton's checks (basic.rs); a producer surface cannot fire them (the
// dispatcher also drops such surfaces). A producer's own events are not
// handled in this widget yet (#369).
async function onAction(a) {
  if (a.surfaceId === CARD_SURFACE && a.name === SEND_MESSAGE) return sendMessage(a.context || {});
  if (a.surfaceId === CARD_SURFACE && a.name === OPEN_URL) return openUrl(a.context?.url);
  say('This action is not available here.', true);
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
  render(meta?.[META_KEY]);
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
    applyContext(res?.hostContext);
    notify('ui/notifications/initialized', {});
    resize(true);
  })
  .catch(() => {
    // No MCP Apps host answered: ChatGPT's `window.openai` (if any) drives.
  });

// For the browser harness: the pure filler, as every channel fills.
window.tritonWidgetFill = fill;

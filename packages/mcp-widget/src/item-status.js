// The status of a filed item (`_meta["triton/status"]`, crates/triton-core/
// src/a2ui/item_status.rs): what the card shows under the answer, and when it
// asks again. Pure, so `npm test` covers it; main.js does the calls.

export const STATUS_META = 'triton/status';
export const STATUS_EVENT = 'triton.status';
// Consecutive failed polls after which the card stops asking.
export const MAX_FAILURES = 3;

/** The status in a result `_meta`, or null. `token` and `poll` only while
 * the status is not terminal and Triton signed a poll. */
export function readStatus(meta) {
  const s = meta && typeof meta === 'object' ? meta[STATUS_META] : null;
  if (!s || typeof s !== 'object' || typeof s.state !== 'string' || !s.state) return null;
  const terminal = s.terminal === true;
  const label = typeof s.label === 'string' && s.label.trim() ? s.label.trim() : null;
  const token = !terminal && typeof s.token === 'string' && s.token ? s.token : null;
  const p = s.poll && typeof s.poll === 'object' ? s.poll : {};
  const num = (v, d) => (Number.isFinite(v) && v > 0 ? v : d);
  const poll = token
    ? {
        afterMs: num(p.after_ms, 5000),
        factor: Math.max(1, num(p.factor, 2)),
        maxMs: num(p.max_ms, 60000),
        stopAfterMs: num(p.stop_after_ms, 30 * 60 * 1000),
      }
    : null;
  return { state: s.state, terminal, label, token, poll };
}

/** The line the card shows for a status: its label, else its state. */
export function statusText(status) {
  return status ? status.label || status.state : '';
}

/** The delay before poll number `n` (0 = the first), or null once the
 * schedule is over (`elapsedMs` since the answer arrived). */
export function nextDelay(poll, n, elapsedMs) {
  if (!poll) return null;
  const delay = Math.min(poll.maxMs, poll.afterMs * poll.factor ** n);
  return elapsedMs + delay > poll.stopAfterMs ? null : delay;
}

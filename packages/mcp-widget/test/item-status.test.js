import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readStatus, statusText, nextDelay, STATUS_META } from '../src/item-status.js';

const pending = {
  state: 'pending', terminal: false, label: 'Waiting for review', token: 'tok',
  poll: { after_ms: 5000, factor: 2, max_ms: 60000, stop_after_ms: 1800000 },
};

test('a pending status with a token is polled on its schedule', () => {
  const s = readStatus({ [STATUS_META]: pending });
  assert.equal(s.state, 'pending');
  assert.equal(s.token, 'tok');
  assert.equal(statusText(s), 'Waiting for review');
  assert.deepEqual(s.poll, { afterMs: 5000, factor: 2, maxMs: 60000, stopAfterMs: 1800000 });
  assert.deepEqual([0, 1, 2, 3, 4, 5].map((n) => nextDelay(s.poll, n, 0)),
    [5000, 10000, 20000, 40000, 60000, 60000]);
  // Past the stop: no further poll.
  assert.equal(nextDelay(s.poll, 9, 1800000 - 1000), null);
});

test('a terminal status is never polled, even with a token', () => {
  const s = readStatus({ [STATUS_META]: { ...pending, terminal: true, label: 'Accepted by Jane' } });
  assert.equal(s.terminal, true);
  assert.equal(s.token, null);
  assert.equal(s.poll, null);
  assert.equal(nextDelay(s.poll, 0, 0), null);
  assert.equal(statusText(s), 'Accepted by Jane');
});

test('a status without a token is shown, not polled', () => {
  const s = readStatus({ [STATUS_META]: { state: 'pending', terminal: false } });
  assert.equal(s.token, null);
  assert.equal(s.poll, null);
  assert.equal(statusText(s), 'pending');
});

test('no or malformed status is null', () => {
  assert.equal(readStatus(undefined), null);
  assert.equal(readStatus({}), null);
  assert.equal(readStatus({ [STATUS_META]: 'pending' }), null);
  assert.equal(readStatus({ [STATUS_META]: { state: '' } }), null);
  assert.equal(statusText(null), '');
});

test('a malformed schedule falls back to the defaults', () => {
  const s = readStatus({ [STATUS_META]: { state: 'pending', token: 't', poll: { after_ms: -1, factor: 'x' } } });
  assert.deepEqual(s.poll, { afterMs: 5000, factor: 2, maxMs: 60000, stopAfterMs: 1800000 });
});

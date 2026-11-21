import { test } from 'node:test';
import assert from 'node:assert/strict';
import { hostContext } from '../src/host-context.js';

test('nothing sent yet: the capability as advertised, last none', () => {
  const hc = hostContext();
  assert.deepEqual(hc.report(), { advertised: false, last: 'none' });
  hc.advertised({ updateModelContext: { text: {} } });
  assert.deepEqual(hc.report(), { advertised: true, last: 'none' });
  hc.advertised({ serverTools: {} });
  assert.equal(hc.report().advertised, false);
  hc.advertised(undefined);
  assert.equal(hc.report().advertised, false);
});

test('an update is pending until the host answers, then accepted or rejected', async () => {
  const hc = hostContext();
  let answer;
  const sent = hc.track(new Promise((resolve, reject) => { answer = { resolve, reject }; }));
  assert.equal(hc.report().last, 'pending');
  answer.resolve({});
  await sent;
  assert.equal(hc.report().last, 'accepted');

  const refused = hc.track(Promise.reject(new Error('unsupported')));
  await refused;
  assert.equal(hc.report().last, 'rejected');
});

test('only the latest update counts', async () => {
  const hc = hostContext();
  let first;
  const a = hc.track(new Promise((resolve) => { first = resolve; }));
  const b = hc.track(Promise.resolve({}));
  await b;
  first({});
  await a;
  assert.equal(hc.report().last, 'accepted');
  let late;
  const c = hc.track(new Promise((_, reject) => { late = reject; }));
  const d = hc.track(new Promise(() => {}));
  late(new Error('x'));
  await c;
  assert.equal(hc.report().last, 'pending', 'an older answer does not overwrite the newer pending one');
  void d;
});

test("ChatGPT's widget state is its own outcome", () => {
  const hc = hostContext();
  hc.widgetState();
  assert.deepEqual(hc.report(), { advertised: false, last: 'widget_state' });
});

test('the report is a copy', () => {
  const hc = hostContext();
  const r = hc.report();
  r.last = 'accepted';
  assert.equal(hc.report().last, 'none');
});

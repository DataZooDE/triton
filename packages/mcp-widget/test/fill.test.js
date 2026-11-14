// The widget fills a form's request exactly as every other channel's
// server-side `resolve` does: the shared vectors, with the parts the
// widget receives (pinned by the Rust test `fill_vectors_match_...`).
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fill, inputs } from '../src/fill.js';

const vectors = JSON.parse(
  readFileSync(
    new URL(
      '../../../crates/triton-tests/fixtures/conversation/fill_vectors.parts.json',
      import.meta.url,
    ),
  ),
);

test('the shared fill vectors', () => {
  assert.ok(vectors.length > 0);
  for (const v of vectors) {
    const got = fill(v.parts, v.values);
    if (v.text !== undefined) {
      assert.deepEqual(got.missing, [], v.template);
      assert.equal(got.text, v.text, v.template);
    } else {
      assert.deepEqual(got.missing, v.missing, v.template);
    }
  }
});

test('a submit context yields its inputs; a choice is its first value', () => {
  assert.deepEqual(
    { ...inputs({ parts: '[]', 'v:who': ['ops'], 'v:why': 'x', 'v:urgent': false, other: 1 }) },
    { who: 'ops', why: 'x', urgent: false },
  );
  assert.deepEqual({ ...inputs({ 'v:who': [] }) }, { who: '' });
  assert.equal(fill([{ text: 'Urgent: ' }, { field: 'u' }], { u: false }).text, 'Urgent: false');
});

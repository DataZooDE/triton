// A form's request, filled on the client from the producer's template parts
// exactly as the server's `fill_template` + `resolve` would (the shared
// vectors in crates/triton-tests/fixtures/conversation pin both sides).
//
// Parts: {text}, {field}, {optional: [...]}. An optional segment is kept only
// when all its fields are non-blank; a blank field outside one is missing.
// Trimming is Rust's `str::trim` (Unicode White_Space), not JS `trim`: the
// two differ on U+0085 and U+FEFF.
const WS =
  '\\t\\n\\v\\f\\r \\u0085\\u00a0\\u1680\\u2000-\\u200a\\u2028\\u2029\\u202f\\u205f\\u3000';
const TRIM = new RegExp('^[' + WS + ']+|[' + WS + ']+$', 'g');
export const trim = (x) => x.replace(TRIM, '');

const has = (o, k) => Object.prototype.hasOwnProperty.call(o, k);

/** `{text, missing}` for `parts` and the inputs `values` (name → value). */
export function fill(parts, values) {
  const missing = [];
  const one = (list, out, miss) => {
    for (const p of list) {
      if (has(p, 'text')) out.push(p.text);
      else if (has(p, 'field')) {
        const raw = has(values, p.field) ? values[p.field] : null;
        const v = trim(raw == null ? '' : String(raw));
        if (v) out.push(v);
        else if (!miss.includes(p.field)) miss.push(p.field);
      }
    }
  };
  const out = [];
  for (const p of parts) {
    if (p.optional) {
      const o = [];
      const m = [];
      one(p.optional, o, m);
      if (!m.length) out.push(...o);
    } else one([p], out, missing);
  }
  return { text: trim(out.join('')), missing };
}

/** The inputs a submit event's context carries: `v:<name>` → value. A
 * choice is a one-element list (a ChoicePicker's value). */
export function inputs(context) {
  const values = Object.create(null);
  for (const [k, v] of Object.entries(context || {})) {
    if (!k.startsWith('v:')) continue;
    values[k.slice(2)] = Array.isArray(v) ? (v.length ? v[0] : '') : v;
  }
  return values;
}

import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { decode, encode, displayHints, validateExchange, validateBoundRead, validateBoundResponse } from '../dist/index.js';
const corpus = JSON.parse(readFileSync(new URL('../fixtures/corpus.json', import.meta.url), 'utf8'));
assert.equal(corpus.schema_version, 1);
const cases = new Map(corpus.cases.map(c => [c.id, c]));
assert.equal(cases.size, corpus.cases.length);
function input(c) {
  if (c.input) return structuredClone(c.input);
  const v = input(cases.get(c.base));
  for (const p of c.patches) {
    const parent = p.path.slice(0, -1).reduce((a,k) => a[k], v), key = p.path.at(-1);
    if (p.op === 'remove') { assert.ok(Object.hasOwn(parent, key)); if (Array.isArray(parent)) parent.splice(Number(key), 1); else delete parent[key]; }
    else parent[key] = structuredClone(p.value);
  }
  return v;
}
const raw = c => c.raw_hex ? new Uint8Array(Buffer.from(c.raw_hex,'hex')) : c.raw ?? JSON.stringify(input(c));
function qualify(c) {
  const d = decode(raw(c));
  if (c.against) {
    const q = decode(raw(cases.get(c.against)));
    if (q.type === 'request' && d.type === 'response') validateExchange(q.value, d.value);
    else { assert.equal(q.type, 'bound_read'); assert.equal(d.type, 'bound_response'); validateBoundResponse(q.value, d.value); }
  }
  if (c.view) {
    const view = decode(raw(cases.get(c.view))); assert.equal(view.type, 'view_state'); assert.equal(d.type, 'bound_read'); validateBoundRead(view.value, d.value);
  }
  return d;
}
export function run() {
  return corpus.cases.map(c => {
    if (!c.valid) { assert.throws(() => qualify(c), {message:'invalid Remote V1 wire value'}, c.id); return {id:c.id,valid:false}; }
    let d; try { d = qualify(c); } catch (e) { throw new Error(`positive fixture ${c.id}`, {cause:e}); }
    const actual = JSON.parse(encode(d));
    assert.deepEqual(actual, c.expected ?? input(c), `canonical fixture ${c.id}`);
    assert.deepEqual(decode(encode(d)), d, `round trip ${c.id}`);
    const hints = displayHints(d);
    if(c.source_tool_ids){assert.notEqual(c.source_tool_ids[0],c.source_tool_ids[1]);for(const s of c.source_tool_ids)assert.ok(Buffer.byteLength(s)>256);assert.equal(c.source_tool_ids[0].slice(0,256),c.source_tool_ids[1].slice(0,256));}
    for (const text of c.visible) assert.ok(hints.includes(text), `positive display identity ${c.id}: ${text}`);
    return {id:c.id,valid:true,value:actual,hints};
  });
}

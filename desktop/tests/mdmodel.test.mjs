import { test } from 'node:test';
import assert from 'node:assert/strict';
import { MarkdownCache, cacheKey, splitFold, takeBatch } from '../dist/mdmodel.js';

test('splitFold keeps short text whole and cuts long text at a line boundary', () => {
  assert.deepEqual(splitFold('short', 10), ['short', '']);
  const text = 'aaaa\nbbbb\ncccc\ndddd';
  const [head, tail] = splitFold(text, 12);
  assert.equal(head + tail, text);
  assert.equal(head, 'aaaa\nbbbb\n');
  const [h2, t2] = splitFold('x'.repeat(30), 10);
  assert.equal(h2.length, 10);
  assert.equal(h2 + t2, 'x'.repeat(30));
});

test('cache hits only while the source text is unchanged', () => {
  const c = new MarkdownCache();
  const k = cacheKey('s', { id: 1, sequence: 2 }, 0);
  assert.equal(c.get(k, 'a'), undefined);
  c.set(k, 'a', '<p>a</p>');
  assert.equal(c.get(k, 'a'), '<p>a</p>');
  assert.equal(c.get(k, 'b'), undefined);
  assert.notEqual(k, cacheKey('s', { id: 1, sequence: 2 }, 1));
});

test('cache is bounded', () => {
  const c = new MarkdownCache();
  for (let i = 0; i < 20000; i++) c.set(`k${i}`, 's', 'h');
  assert.ok(c.size <= 6000);
  assert.equal(c.get('k19999', 's'), 'h');
});

test('takeBatch takes newest first within count and byte bounds', () => {
  const q = Array.from({ length: 10 }, (_, i) => ({ src: String(i).repeat(10) }));
  const b = takeBatch(q, 4, 1000);
  assert.deepEqual(b.map((j) => j.src[0]), ['9', '8', '7', '6']);
  assert.equal(q.length, 6);
  const big = [{ src: 'a'.repeat(100) }, { src: 'b'.repeat(100) }];
  assert.equal(takeBatch(big, 10, 150).length, 1);
  assert.equal(takeBatch([{ src: 'z'.repeat(999) }], 10, 5).length, 1);
});

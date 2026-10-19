// Slice 14 (hub #3016): the Windows and macOS icon containers are generated
// from the committed PNGs and stay in sync with them.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { buildIcns, buildIco, pngSize, readSources, staleIcons } from '../scripts/icons.mjs';

test('icon.ico and icon.icns match the committed PNGs', () => {
  assert.deepEqual(staleIcons(), []);
});

test('the ICO directory points at each PNG entry with its size', () => {
  const pngs = readSources();
  const ico = buildIco(pngs);
  assert.equal(ico.readUInt16LE(2), 1);
  assert.equal(ico.readUInt16LE(4), 3);
  const sizes = [];
  for (let i = 0; i < 3; i += 1) {
    const entry = 6 + 16 * i;
    const length = ico.readUInt32LE(entry + 8);
    const offset = ico.readUInt32LE(entry + 12);
    const { width } = pngSize(ico.subarray(offset, offset + length));
    assert.equal(ico.readUInt8(entry) || 256, width);
    sizes.push(width);
  }
  assert.deepEqual(sizes, [32, 128, 256]);
});

test('the ICNS file lists its PNG elements and their total length', () => {
  const icns = buildIcns(readSources());
  assert.equal(icns.toString('latin1', 0, 4), 'icns');
  assert.equal(icns.readUInt32BE(4), icns.length);
  const found = [];
  for (let at = 8; at < icns.length; at += icns.readUInt32BE(at + 4)) {
    const type = icns.toString('latin1', at, at + 4);
    const { width } = pngSize(icns.subarray(at + 8, at + icns.readUInt32BE(at + 4)));
    found.push(`${type}:${width}`);
  }
  assert.deepEqual(found, ['icp5:32', 'ic07:128', 'ic08:256', 'ic09:512']);
});

test('a PNG of the wrong size is refused', () => {
  const pngs = readSources();
  assert.throws(() => buildIco({ ...pngs, 128: pngs[32] }), /the 128 px PNG is 32x32/);
  assert.throws(() => pngSize(Buffer.from('not a png')), /not a PNG/);
});

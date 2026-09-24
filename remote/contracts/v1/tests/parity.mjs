import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
const [rustFile, tsFile] = process.argv.slice(2);
const rust=JSON.parse(readFileSync(rustFile,'utf8')), ts=JSON.parse(readFileSync(tsFile,'utf8'));
assert.deepEqual(rust,ts,'Rust and TypeScript emitted fixture values and display hints');
console.log(`Emitted parity: ${rust.length} cases (${rust.filter(c=>c.valid).length} positive / ${rust.filter(c=>!c.valid).length} negative)`);

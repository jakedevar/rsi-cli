// What to print when PARITY.md is not what the generator produces (#278): a parity test failed once in a
// full `npm test` run and passed on re-run, and nobody had recorded which tests failed or why. The state
// below is what tells a truncated or concurrently rewritten PARITY.md (size 0, a short file, an mtime
// within the run) from a real drift (a different line at the same size, an unchanged old mtime).
import { existsSync, readdirSync, statSync, readFileSync } from 'node:fs';
import { join } from 'node:path';

const stamp = (p) => {
  if (!existsSync(p)) return `${p}: missing`;
  const st = statSync(p);
  return `${p}: ${st.size} bytes, mtime ${st.mtime.toISOString()}`;
};

/** The first line where two texts differ, 1-based, each side clipped. */
export function firstDifference(actual, expected) {
  const a = actual.split('\n');
  const e = expected.split('\n');
  for (let i = 0; i < Math.max(a.length, e.length); i++) {
    if (a[i] !== e[i]) return `line ${i + 1}: file has ${JSON.stringify((a[i] ?? '(end of file)').slice(0, 160))}, generator wants ${JSON.stringify((e[i] ?? '(end of text)').slice(0, 160))}`;
  }
  return 'no differing line';
}

/**
 * A multi-line report of the shared state a PARITY.md comparison depends on: the file under test, the
 * inputs it is generated from, the first differing line, the process id and the clock. Pure reads; never
 * throws (a diagnostic must not hide the failure it describes).
 */
export function describeParityState({ out, root, sidecarDir, expected }) {
  const lines = [`parity diagnostics (pid ${process.pid}, now ${new Date().toISOString()}):`];
  try {
    lines.push(`  ${stamp(out)}`);
    lines.push(`  ${stamp(join(root, 'docs/keybindings.md'))}`);
    const files = existsSync(sidecarDir) ? readdirSync(sidecarDir).filter((f) => f.endsWith('.json')) : [];
    const newest = files.map((f) => [statSync(join(sidecarDir, f)).mtimeMs, f]).sort((x, y) => y[0] - x[0])[0];
    lines.push(`  ${sidecarDir}: ${files.length} status files${newest ? `, newest ${newest[1]} at ${new Date(newest[0]).toISOString()}` : ''}`);
    if (expected !== undefined) {
      const current = existsSync(out) ? readFileSync(out, 'utf8') : '';
      lines.push(`  expected ${expected.length} chars, file has ${current.length}; ${firstDifference(current, expected)}`);
      if (current.length < expected.length) lines.push('  the file is shorter than the generator output: another process may have been rewriting it (truncate-then-write) while this ran');
    }
  } catch (err) {
    lines.push(`  (diagnostics failed: ${err.message})`);
  }
  return lines.join('\n');
}

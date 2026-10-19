// Vendor xterm.js (slice 12, the terminal overlay) into dist/vendor/: the ES
// module builds, the stylesheet and the MIT licenses, copied from the
// lockfile-pinned npm packages, so the app ships them inside its own bundle
// (no CDN, no bundler). Source-map comments are dropped; the maps are not
// shipped. actions/terminal.ts imports these files the first time the
// overlay opens.
import { existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';

/** [package-relative source, file name under dist/vendor/]. */
export const VENDORED = [
  ['@xterm/xterm/lib/xterm.mjs', 'xterm.mjs'],
  ['@xterm/xterm/css/xterm.css', 'xterm.css'],
  ['@xterm/xterm/LICENSE', 'LICENSE-xterm.txt'],
  ['@xterm/addon-fit/lib/addon-fit.mjs', 'addon-fit.mjs'],
  ['@xterm/addon-fit/LICENSE', 'LICENSE-addon-fit.txt'],
];

export function stripSourceMap(text) {
  return text.replace(/\n?\/[/*][#@] sourceMappingURL=[^\n]*\s*$/, '\n');
}

export function vendorTerminal(root, dist) {
  const out = join(dist, 'vendor');
  mkdirSync(out, { recursive: true });
  for (const [src, name] of VENDORED) {
    const from = join(root, 'node_modules', src);
    if (!existsSync(from)) throw new Error(`${from} is missing: run npm ci in desktop/`);
    const text = readFileSync(from, 'utf8');
    writeFileSync(join(out, name), /\.(mjs|css)$/.test(name) ? stripSourceMap(text) : text);
  }
}

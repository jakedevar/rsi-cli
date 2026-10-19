// Copy the static shell (HTML, CSS) next to the compiled JS in dist/, and
// vendor xterm.js for the terminal overlay (scripts/vendor-terminal.mjs).
import { copyFileSync, mkdirSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { vendorTerminal } from './vendor-terminal.mjs';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const dist = join(root, 'dist');
mkdirSync(dist, { recursive: true });
for (const f of ['index.html', 'styles.css', 'detail.css', 'lifecycle.css', 'terminal.css']) {
  copyFileSync(join(root, 'static', f), join(dist, f));
}
vendorTerminal(root, dist);

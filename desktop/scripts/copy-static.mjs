// Copy the static shell (HTML, CSS) next to the compiled JS in dist/.
import { copyFileSync, mkdirSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const dist = join(root, 'dist');
mkdirSync(dist, { recursive: true });
for (const f of ['index.html', 'styles.css']) {
  copyFileSync(join(root, 'static', f), join(dist, f));
}

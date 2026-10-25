#!/usr/bin/env node
// Print the desktop action reachability table and exit 1 on a problem.
//   node scripts/reachability.mjs [--json]                (needs `npm run build` first: reads dist/)
//   node scripts/reachability.mjs --by-section [--json]   one row per PARITY.md `##` section
import { readFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import {
  FORM_COMPLETION, bySection, formCompletion, parityRows, reachability, renderSectionTable, renderTable, smokeStepIds, stemActionMap,
} from './reachability-lib.mjs';
import { splitRow, unwrap } from './parity-lib.mjs';
import { generate as generateParity } from './parity.mjs';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const read = (p) => readFileSync(resolve(root, p), 'utf8');

export function load() {
  return import(pathToFileURL(resolve(root, 'dist/registry.js')).href).then((reg) =>
    reachability(reg, {
      html: read('static/index.html'),
      palette: read('src/palette.ts'),
      pageSources: ['src/menus.ts', 'src/issues.ts', 'src/actions/nav-run.ts'].map(read),
    }));
}

/** Hub #3280: overlay editors a click opens are finished and cancelled by click (FORM_COMPLETION). */
export function loadForms() {
  return formCompletion(FORM_COMPLETION, read, {
    smoke: smokeStepIds(read('scripts/smoke.mjs')),
    e2e: smokeStepIds(read('scripts/smoke-e2e-sections.mjs')),
  });
}

/** The by-section audit: PARITY.md sections against the registry's reachability rows. */
export async function loadBySection() {
  const dist = (p) => import(pathToFileURL(resolve(root, 'dist', p)).href);
  const base = await load();
  const kb = await dist('keybindings.js');
  const stems = [...read('src/actions/index.generated.ts').matchAll(/from '\.\/([a-z-]+)\.js'/g)].map((m) => m[1]);
  const slices = {};
  for (const s of stems) slices[s] = (await dist(`actions/${s}.js`)).SLICE;
  // PARITY.md is a build product (#60): read the generator's text, never a stale file.
  const parity = generateParity({ root: resolve(root, '..'), sidecarDir: resolve(root, 'parity-status.d') }).text;
  const rows = parityRows(parity, splitRow, unwrap);
  const by = bySection(rows, base.rows, kb.KEY_BINDINGS, kb.COMMAND_BINDINGS, stemActionMap(stems, slices));
  return { ...by, rows: base.rows, problems: [...base.problems, ...by.problems], parityRows: rows.length };
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  if (process.argv.includes('--by-section')) {
    const { sections, rows, problems, parityRows: n } = await loadBySection();
    console.log(process.argv.includes('--json') ? JSON.stringify({ sections, problems }, null, 2) : renderSectionTable(sections));
    for (const p of problems) console.error(`problem: ${p}`);
    console.error(`${sections.length} PARITY sections, ${n} rows, ${rows.length} actions (${rows.filter((r) => r.surfaces.length).length} reachable), ${problems.length} problems`);
    process.exit(problems.length ? 1 : 0);
  }
  const { rows, problems: actionProblems } = await load();
  const forms = loadForms();
  const problems = [...actionProblems, ...forms.problems];
  console.log(process.argv.includes('--json') ? JSON.stringify({ rows, forms: forms.rows, problems }, null, 2) : renderTable(rows));
  for (const p of problems) console.error(`problem: ${p}`);
  console.error(`${rows.length} actions, ${rows.filter((r) => r.surfaces.length).length} reachable; ${forms.rows.filter((f) => f.ok).length}/${forms.rows.length} overlay forms finished and cancelled by mouse; ${problems.length} problems`);
  process.exit(problems.length ? 1 : 0);
}

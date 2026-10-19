// Real-rsid end-to-end interactions for the desktop (hub #3105, local #115), on the #109 harness.
//
//   node scripts/smoke.mjs --isolated-write [--rsid PATH] [--only ID[,ID]] [--out DIR] [--chrome PATH]
//
// It starts an ISOLATED rsid (smoke-isolated.mjs: own HOME, database and socket under /tmp/rsi-e2e-*, scrubbed
// environment), seeds a synthetic git project, then drives the built desktop in headless Chrome over CDP key
// events. The page's Tauri invokes reach that daemon through the bridge in its explicit isolated-write mode
// (smoke-bridge.mjs): the read allowlist plus a short table of writes, all refused unless the socket is the one
// this run started. `--live` stays read-only and never uses any of this.
//
// One interaction per section of the generated desktop/PARITY.md (E2E_SECTIONS), plus the Issue lifecycle and
// the Vim-keys toggle. Each result records source, command, result, runner SHA and daemon SHA.
import { spawnSync } from 'node:child_process';
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { createBridge, rpcCall } from './smoke-bridge.mjs';
import { judgeEgress, sampleDirectConnections } from './smoke-egress.mjs';
import { BUILD_COMMAND, findRsid, findSeeder, seedSynthetic, startIsolatedRsid } from './smoke-isolated.mjs';
import { connectPage, launchChrome, newToken, serve } from './smoke-lib.mjs';

const here = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(here, '..', '..');
export const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const DIALOG = `(document.querySelector('dialog[open]')||{}).innerText||''`;

/** The H2 sections of the generated PARITY.md, in file order. */
export function paritySections(markdown) {
  return [...markdown.matchAll(/^## (.+)$/gm)].map((m) => m[1].trim());
}

const pass = (note, evidence = {}) => ({ status: 'pass', note, evidence });
const fail = (note, evidence = {}) => ({ status: 'fail', note, evidence });

// ---------- page helpers shared by the steps ----------

export function helpers({ page, bridgeLog, handle, seed }) {
  const rpc = (m, p) => rpcCall(handle.socket, m, p);
  const h = {
    page, rpc, seed, bridgeLog,
    writes: (cmd) => bridgeLog.filter((e) => e.write && e.outcome === 'forwarded' && (!cmd || e.cmd === cmd)),
    refused: (cmd) => bridgeLog.filter((e) => e.outcome === 'refused' && (!cmd || e.cmd === cmd)),
    text: (sel) => page.eval(`(document.querySelector(${JSON.stringify(sel)})||{}).innerText||''`),
    dialog: () => page.eval(DIALOG),
    async fresh() {
      await page.goto(`${page.origin}/`);
      await sleep(300);
      if (!(await page.waitFor(`document.querySelectorAll('#session-list li').length > 0`, 12000))) throw new Error('the session list never rendered');
      await sleep(400);
    },
    async closeAll() { for (let i = 0; i < 4; i++) { await page.key('Escape'); await sleep(120); } },
    async command(text) { await page.keys(`:${text}`); await sleep(150); await page.key('Enter'); await sleep(900); },
    async palette(text) {
      await page.element('#open-palette', '(e) => e.click()', { step: 'open Actions palette' });
      await page.element('#palette-input', undefined, { step: 'Actions palette accepts input' });
      await sleep(400);
      await page.keys(text);
      await sleep(300);
      await page.key('Enter');
      await sleep(900);
    },
    /** Focus the first matching field of the open form and type into it (real key events). */
    async typeInto(selector, text, { replace = true } = {}) {
      await page.element(selector, `(e) => { e.focus(); ${replace ? 'e.select();' : ''} }`, { step: 'type into field' });
      if (replace) await page.key('Backspace');
      await page.keys(text);
    },
    async issue(id) {
      const r = await rpc('GetIssueInProject', { project_id: seed.projectId, issue_id: id });
      return { ...r.issue, readiness: r.readiness };
    },
    async issuesByTitle(title, archive = 'All') {
      const page_ = await rpc('ListIssuesPage', { project_id: seed.projectId, archive, limit: 64, sort: 'updated_desc', direction: 'forward' });
      return page_.rows.map((r) => r.issue).filter((i) => i.title === title);
    },
    async openIssues() {
      await h.command('issues');
      if (!(await page.waitFor(`!document.getElementById('issues-view').hidden && document.querySelectorAll('#issues-list li').length > 0`, 8000))) throw new Error('the Issues workspace did not list Issues');
      await sleep(300);
    },
    /** Move the Issues cursor with j/k until the selected row's text contains `title`. */
    async selectIssue(title) {
      for (let i = 0; i < 40; i++) {
        const at = await page.eval(`(() => { const rows = [...document.querySelectorAll('#issues-list li')]; const i = rows.findIndex((r) => r.classList.contains('selected') || r.getAttribute('aria-selected') === 'true'); return { i, hit: rows.findIndex((r) => r.innerText.includes(${JSON.stringify(title)})) }; })()`);
        if (at.hit < 0) throw new Error(`no listed Issue titled ${title}`);
        if (at.i === at.hit) return;
        await page.key(at.i < at.hit ? 'j' : 'k');
        await sleep(150);
      }
      throw new Error(`could not select ${title}`);
    },
  };
  return h;
}

// ---------- the Issue lifecycle (hub #3105: create, edit, cancel, archive, restore, with retained end states) ----------

export const LIFECYCLE_TITLE = 'E2E lifecycle Issue';
export const LIFECYCLE_EDITED = 'E2E lifecycle Issue (edited)';

export async function issueLifecycle(h) {
  const { page } = h;
  const steps = [];
  const record = async (id, what, run) => {
    let r;
    try { r = await run(); } catch (e) { r = fail(`threw: ${e.message}`); }
    r.shot = await page.shot(join(h.out, `issue-${id}.png`));
    steps.push({ id: `issue-${id}`, title: what, ...r });
    return r;
  };

  await h.fresh();
  await h.openIssues();

  let issueId = null;
  const created = await record('create', 'n opens the New Issue form; typed fields and Ctrl+Enter create it in rsid', async () => {
    await page.key('n');
    if (!(await page.waitFor(`document.querySelector('#issue-dialog[open]') !== null`, 4000))) return fail('n did not open the Issue form');
    await h.typeInto('#issue-form [name=title]', LIFECYCLE_TITLE);
    await h.typeInto('#issue-form [name=body]', 'Created by the end-to-end smoke.');
    await h.typeInto('#issue-form [name=labels]', 'e2e,lifecycle');
    await page.key('Enter', { ctrl: true });
    if (!(await page.waitFor(`document.querySelector('#issue-dialog[open]') === null`, 6000))) return fail(`the form stayed open: ${await page.eval(`(document.querySelector('#issue-form .form-error')||{}).innerText||''`)}`);
    const rows = await h.issuesByTitle(LIFECYCLE_TITLE);
    if (rows.length !== 1) return fail(`rsid holds ${rows.length} Issues titled ${LIFECYCLE_TITLE}`);
    issueId = rows[0].id;
    const ok = rows[0].status === 'Open' && rows[0].body === 'Created by the end-to-end smoke.' && rows[0].labels.join() === 'e2e,lifecycle';
    return ok ? pass(`rsid holds #${rows[0].display_number} Open with body and labels e2e,lifecycle`, { issue: rows[0].display_number, row_version: rows[0].row_version })
      : fail(`unexpected stored Issue ${JSON.stringify(rows[0])}`);
  });
  if (created.status !== 'pass') return steps;

  await record('edit', 'e edits the title; the stored row_version moves and the list shows the new title', async () => {
    await h.selectIssue(LIFECYCLE_TITLE);
    const before = await h.issue(issueId);
    await page.key('e');
    if (!(await page.waitFor(`document.querySelector('#issue-dialog[open]') !== null`, 4000))) return fail('e did not open the edit form');
    await h.typeInto('#issue-form [name=title]', LIFECYCLE_EDITED);
    await page.key('Enter', { ctrl: true });
    if (!(await page.waitFor(`document.querySelector('#issue-dialog[open]') === null`, 6000))) return fail(`the edit form stayed open: ${await page.eval(`(document.querySelector('#issue-form .form-error')||{}).innerText||''`)}`);
    const after = await h.issue(issueId);
    const listed = await page.waitFor(`document.getElementById('issues-list').innerText.includes(${JSON.stringify(LIFECYCLE_EDITED)})`, 4000);
    return after.title === LIFECYCLE_EDITED && after.row_version > before.row_version && listed
      ? pass(`title "${after.title}", row_version ${before.row_version} -> ${after.row_version}`, { row_version: after.row_version })
      : fail(`stored title ${after.title}, row_version ${before.row_version} -> ${after.row_version}, listed ${listed}`);
  });

  await record('cancel', 'dd asks for confirmation; the confirmed form sets status Cancelled (never deletes)', async () => {
    await h.selectIssue(LIFECYCLE_EDITED);
    await page.keys('dd');
    if (!(await page.waitFor(`document.querySelector('#issue-dialog[open]') !== null`, 4000))) return fail('dd did not open the cancel confirmation');
    const confirmText = await h.text('#issue-dialog');
    // Refusal preserved: Escape on the untouched confirmation changes nothing in rsid.
    await page.key('Escape'); await sleep(500);
    const untouched = await h.issue(issueId);
    if (untouched.status !== 'Open') return fail(`Escape on the confirmation still changed the Issue to ${untouched.status}`);
    await page.keys('dd');
    await page.waitFor(`document.querySelector('#issue-dialog[open]') !== null`, 4000);
    await page.click(`[...document.querySelectorAll('#issue-form button')].find((b) => b.textContent.trim() === 'Cancel Issue')`);
    if (!(await page.waitFor(`document.querySelector('#issue-dialog[open]') === null`, 6000))) return fail('the confirmed cancel left the form open');
    const after = await h.issue(issueId);
    return after.status === 'Cancelled' && after.archived_at === null
      ? pass('Escape kept the Issue Open; the confirmed form left it Cancelled (not archived, not deleted)', { confirm: confirmText.split('\n')[0] })
      : fail(`stored status ${after.status}, archived_at ${after.archived_at}`);
  });

  await record('archive', 'A asks for confirmation; the confirmed form archives the Issue, which leaves the default list', async () => {
    await h.selectIssue(LIFECYCLE_EDITED);
    await page.key('A', { shift: true });
    if (!(await page.waitFor(`document.querySelector('#issue-dialog[open]') !== null`, 4000))) return fail('A did not open the archive confirmation');
    await page.click(`[...document.querySelectorAll('#issue-form button')].find((b) => b.textContent.trim() === 'Archive')`);
    if (!(await page.waitFor(`document.querySelector('#issue-dialog[open]') === null`, 6000))) return fail('the confirmed archive left the form open');
    const after = await h.issue(issueId);
    const active = await h.issuesByTitle(LIFECYCLE_EDITED, 'Active');
    const archived = await h.issuesByTitle(LIFECYCLE_EDITED, 'Archived');
    return after.archived_at && active.length === 0 && archived.length === 1
      ? pass(`archived_at ${after.archived_at}; absent from Active, present in Archived`, { archived_at: after.archived_at })
      : fail(`archived_at ${after.archived_at}, active ${active.length}, archived ${archived.length}`);
  });

  await record('restore', 'U restores the archived Issue; it is back in the default list with its Cancelled status', async () => {
    // The default list hides it: show archived Issues through the filters form, as the user would.
    await page.keys('F'.toLowerCase());
    await sleep(300);
    const formOpen = await page.eval(`document.querySelector('#issue-dialog[open]') !== null`);
    if (!formOpen) return fail('f did not open the filters form');
    const sel = await page.eval(`(() => { const s = document.querySelector('#issue-form [name=archive]'); return s ? [...s.options].map((o) => o.value) : null; })()`);
    if (!sel) return fail('the filters form has no archive field');
    await page.element('#issue-form [name=archive]', `(s) => { s.focus(); s.value = 'Archived'; s.dispatchEvent(new Event('input', { bubbles: true })); s.dispatchEvent(new Event('change', { bubbles: true })); }`, { step: 'archive Issue' });
    await page.key('Enter', { ctrl: true });
    await page.waitFor(`document.querySelector('#issue-dialog[open]') === null`, 4000);
    await page.waitFor(`document.getElementById('issues-list').innerText.includes(${JSON.stringify(LIFECYCLE_EDITED)})`, 6000);
    await h.selectIssue(LIFECYCLE_EDITED);
    await page.key('U', { shift: true });
    await sleep(300);
    // A restore may ask for confirmation; accept it when it does.
    if (await page.eval(`document.querySelector('#issue-dialog[open]') !== null`)) await page.key('Enter', { ctrl: true });
    const restored = await (async () => {
      for (let i = 0; i < 30; i++) { const s = await h.issue(issueId); if (!s.archived_at) return s; await sleep(200); }
      return null;
    })();
    if (!restored) return fail('rsid still holds the Issue archived');
    const active = await h.issuesByTitle(LIFECYCLE_EDITED, 'Active');
    return active.length === 1 && restored.status === 'Cancelled'
      ? pass(`restored: archived_at null, status ${restored.status}, back in the Active list`, { status: restored.status })
      : fail(`active ${active.length}, status ${restored.status}`);
  });
  return steps;
}

// ---------- orchestration ----------

function gitHead(cwd) {
  const r = spawnSync('git', ['-C', cwd, 'rev-parse', 'HEAD'], { encoding: 'utf8' });
  return r.status === 0 ? r.stdout.trim() : null;
}

export async function runIsolated(opts, { dist, chromeBin, sections, signal }) {
  const rsid = findRsid(opts.rsid);
  if (!rsid) {
    console.log('runner_unavailable: no built rsid found (pass --rsid PATH, set RSI_E2E_RSID, or `cargo build -p rsid`); 0 steps executed');
    return 3;
  }
  if (!findSeeder(rsid)) {
    console.log(`runner_unavailable: no rsi-e2e-seed next to ${rsid} (build both with \`${BUILD_COMMAND}\`, or set RSI_E2E_SEEDER); 0 steps executed`);
    return 3;
  }
  mkdirSync(opts.out, { recursive: true });
  const handle = await startIsolatedRsid(rsid);
  const bridgeLog = [];
  let server = null;
  let chrome = null;
  let page = null;
  const results = [];
  const directSeen = new Set();
  let socketsSampled = false;
  const sampleEgress = () => {
    const sm = sampleDirectConnections(handle.pid);
    socketsSampled ||= sm.sampled;
    for (const d of sm.direct) directSeen.add(d);
  };
  const runner = { sha: gitHead(REPO), chrome: chromeBin, node: process.version, platform: process.platform };
  try {
    const seed = await seedSynthetic(handle);
    const bridge = createBridge({ call: (m, p) => rpcCall(handle.socket, m, p), record: (e) => bridgeLog.push(e), isolated: { socket: handle.socket } });
    const token = newToken();
    const served = await serve({ dist, bridge, token, onRefusal: (e) => bridgeLog.push(e), signal });
    server = served.server;
    chrome = await launchChrome(chromeBin, { signal });
    page = await connectPage(chrome.cdpPort, { signal });
    page.origin = `http://127.0.0.1:${served.port}`;
    const h = helpers({ page, bridgeLog, handle, seed });
    h.out = opts.out;
    const only = opts.only ? new Set(opts.only) : null;
    for (const s of sections) {
      if (only && !only.has(s.id)) continue;
      let r;
      try { await h.fresh(); r = await s.run(h); } catch (e) { r = fail(`threw: ${e.message}`); }
      const steps = r.steps ?? [];
      const shot = await page.shot(join(opts.out, `${s.id}.png`)).catch(() => null);
      results.push({
        id: s.id, section: s.section, parityRow: s.parityRow, source: s.source, command: `node scripts/smoke.mjs --isolated-write --only ${s.id}`,
        interaction: s.interaction, status: r.status, note: r.note, evidence: r.evidence ?? {}, steps, screenshot: shot,
        runner, daemon: { socket: 'isolated (temporary)', pid: handle.pid, ...handle.binary },
      });
      await h.closeAll().catch(() => {});
      sampleEgress();
    }
    // The no-outbound guard: judged on the proxy log and the socket samples, and part of the pass/fail verdict.
    sampleEgress();
    const keyRow = results.find((r) => r.id === 'overlay-ref' && r.status === 'pass');
    const egress = judgeEgress({ attempts: handle.egress.attempts, direct: [...directSeen], keyCheck: { set: Boolean(keyRow), detail: keyRow?.evidence?.keyCheck ?? null }, sampled: socketsSampled });
    results.push({
      id: 'egress-guard', section: '(extra) No outbound network', parityRow: 'n/a (harness guard, local #123, #130)', source: 'desktop/scripts/smoke-egress.mjs, crates/rsid-core/src/offline.rs',
      command: 'node scripts/smoke.mjs --isolated-write', interaction: 'the isolated rsid runs offline (RSI_OFFLINE=1) with every proxy variable on a local refusing listener; from its first boot no attempt may reach the listener, no direct connection may exist, and a provider-key check must read offline_unchecked',
      status: egress.ok ? 'pass' : 'fail',
      note: egress.ok
        ? `no outbound connection: 0 attempts reached the guard from rsid's first boot on; provider-key check ${egress.keyCheck.set ? egress.keyCheck.detail : 'not exercised'}; socket sample ${egress.socketSample}`
        : egress.problems.join('; '),
      evidence: { ...egress }, steps: [], screenshot: null, runner, daemon: { socket: 'isolated (temporary)', pid: handle.pid, ...handle.binary },
    });
    const refusals = bridgeLog.filter((e) => e.outcome === 'refused').map((r) => `${r.cmd}: ${r.reason}`);
    const forwardedWrites = [...new Set(bridgeLog.filter((e) => e.write && e.outcome === 'forwarded').map((e) => e.method))].sort();
    const parity = paritySections(readFileSync(join(REPO, 'desktop', 'PARITY.md'), 'utf8'));
    const covered = new Set(sections.filter((s) => !s.extra).map((s) => s.section));
    const skips = results.flatMap((r) => [r, ...r.steps]).filter((r) => r.evidence?.skipped).map((r) => ({ id: r.id, reason: r.evidence.skipped }));
    const report = {
      mode: 'isolated-write', runner, daemon: { pid: handle.pid, ...handle.binary }, results, forwardedWrites, refusals,
      parity: { sections: parity, uncovered: only ? [] : parity.filter((p) => !covered.has(p)) }, skips,
      egress,
      observations: [
        `rsid ran offline (RSI_OFFLINE=1) behind a local refusing proxy (${handle.egress.url}), which recorded ${handle.egress.attempts.length} attempt(s) from first boot on; ${egress.directConnections.length} direct non-loopback connection(s); socket sample ${egress.socketSample}.`,
      ],
    };
    writeFileSync(join(opts.out, 'e2e-report.json'), `${JSON.stringify(report, null, 2)}\n`);
    writeFileSync(join(opts.out, 'e2e-report.md'), renderMarkdown(report));
    for (const r of results) {
      console.log(`${r.status.toUpperCase().padEnd(8)} ${r.id.padEnd(18)} ${r.note}`);
      for (const st of r.steps) console.log(`  ${st.status.toUpperCase().padEnd(8)} ${st.id.padEnd(16)} ${st.note}`);
    }
    console.log(`writes forwarded to the isolated daemon: [${forwardedWrites.join(', ')}]; refused: ${refusals.length}`);
    console.log(`parity sections covered: ${parity.length - report.parity.uncovered.length}/${parity.length}${report.parity.uncovered.length ? ` (uncovered: ${report.parity.uncovered.join('; ')})` : ''}; skips: ${skips.length ? skips.map((k) => `${k.id}: ${k.reason}`).join('; ') : 'none'}`);
    console.log(`report: ${join(opts.out, 'e2e-report.json')}`);
    return report.parity.uncovered.length || results.some((r) => r.status === 'fail' || r.steps.some((s) => s.status === 'fail')) ? 1 : 0;
  } finally {
    page?.close();
    server?.closeAllConnections();
    server?.close();
    try { await chrome?.stop(); } finally { await handle.stop(); }
  }
}

export function renderMarkdown(report) {
  const d = report.daemon;
  const daemonSha = `${d.checkoutHead?.slice(0, 12) ?? 'unknown'}${d.checkoutDirty ? '+rust-dirty' : ''}; last Rust change ${d.lastRustCommit?.slice(0, 12) ?? 'unknown'}; binary sha256 ${d.sha256.slice(0, 12)}`;
  const cell = (v) => String(v ?? '').replace(/\|/g, '/').replace(/\n/g, ' ');
  const rows = ['| section | PARITY row | source | command | result | note | runner SHA | daemon SHA |', '| --- | --- | --- | --- | --- | --- | --- | --- |'];
  const line = (r, extra = {}) => `| ${cell(extra.section ?? r.section ?? r.id)} | ${cell(r.parityRow)} | ${cell(r.source)} | \`${cell(r.command)}\` | ${r.status} | ${cell(r.note)} | ${report.runner.sha?.slice(0, 12)} | ${daemonSha} |`;
  for (const r of report.results) {
    rows.push(line(r));
    for (const st of r.steps ?? []) rows.push(line({ ...st, source: r.source, command: r.command, parityRow: r.parityRow }, { section: `↳ ${st.id}` }));
  }
  const tail = [
    '', `runner: ${report.runner.sha} (${report.runner.node}, ${report.runner.platform}); Chrome: ${report.runner.chrome}`,
    `daemon: ${d.path}`, `skips: ${report.skips.length ? report.skips.map((k) => `${k.id}: ${k.reason}`).join('; ') : 'none'}`,
    `uncovered PARITY sections: ${report.parity.uncovered.length ? report.parity.uncovered.join('; ') : 'none'}`,
    ...report.observations.map((o) => `observation: ${o}`),
  ];
  return `${rows.join('\n')}\n${tail.join('\n')}\n`;
}

// First-run and boot-completion check (hub #3016, Issue #117) on the #109 headless Chrome/CDP harness.
//
//   node scripts/smoke.mjs --first-run [--out DIR] [--chrome PATH]
//
// Boots the built dist/ five times against a bridge that behaves like each first-run situation and
// reads what a newcomer sees plus the boot evidence the app writes (html[data-boot], data-boot-report,
// #boot-banner). Needs no GUI permission and no real daemon: the bridge is the harness's own, the fake
// rsid answers reads, and nothing is written anywhere but --out.
import { mkdirSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { createBridge, rpcCall } from './smoke-bridge.mjs';
import { startFakeDaemon } from './smoke-fake.mjs';
import { connectPage, launchChrome, newToken, serve } from './smoke-lib.mjs';

const DOWN = 'cannot reach rsid: /tmp/rsi-demo/daemon.sock: No such file or directory (os error 2)';

/** What the page shows, read back as data. */
export const PAGE_STATE = `(() => {
  const e = document.getElementById('empty');
  const b = document.getElementById('boot-banner');
  const r = document.documentElement.dataset.bootReport;
  return {
    boot: document.documentElement.dataset.boot ?? null,
    report: r ? JSON.parse(r) : null,
    onboarding: e && !e.hidden ? e.dataset.onboarding ?? null : null,
    emptyText: e ? e.innerText : '',
    emptyButtons: e ? [...e.querySelectorAll('button')].map((x) => x.textContent) : [],
    banner: b && !b.hidden ? b.textContent : null,
    conn: document.getElementById('conn')?.textContent ?? null,
    rows: document.querySelectorAll('#session-list li').length,
  };
})()`;

/** The scenarios: how the bridge answers and what must be on the page. */
export function scenarios() {
  const has = (t, s) => t.includes(s);
  return [
    {
      id: 'daemon-missing', title: 'rsid is not running: the newcomer is told what it is, where it looked and how to start it',
      call: () => { throw new Error(DOWN); },
      ready: (s) => s.boot !== null,
      check: (s) => [
        [s.boot === 'offline', `data-boot is offline (got ${s.boot})`],
        [s.report?.daemon === 'unreachable', 'the report names the unreachable daemon'],
        [s.onboarding === 'daemon-down', `guidance is daemon-down (got ${s.onboarding})`],
        [has(s.emptyText, 'Cannot reach the rsid daemon'), 'title says it cannot reach rsid'],
        [has(s.emptyText, '/tmp/rsi-demo/daemon.sock') || has(s.emptyText, 'daemon.sock'), 'it names the socket it looked at'],
        [has(s.emptyText, 'rsid') && has(s.emptyText, 'make release-install'), 'it says how to start the daemon'],
        [s.emptyButtons.some((b) => b.startsWith('Retry now')), 'a Retry now button'],
        [s.conn === 'daemon unreachable', 'the top bar says daemon unreachable'],
        [s.banner === null, 'no failure banner: offline is expected, not broken'],
      ],
    },
    {
      id: 'connected-empty', title: 'rsid is up with no sessions: the first steps are offered',
      call: (m, p, sock) => (m === 'ListSessions' ? [] : rpcCall(sock, m, p)),
      ready: (s) => s.boot !== null,
      check: (s) => [
        [s.boot === 'ready', `data-boot is ready (got ${s.boot})`],
        [s.report?.steps?.every((x) => x.ok), 'every boot step ok'],
        [s.report?.steps?.some((x) => x.name === 'settings' && x.ok), 'the Settings config read is one of them'],
        [s.onboarding === 'no-sessions', `guidance is no-sessions (got ${s.onboarding})`],
        [has(s.emptyText, 'Connected to rsid. No sessions yet.'), 'title says connected, no sessions'],
        [s.emptyButtons.includes('New session') && s.emptyButtons.some((b) => b.startsWith('Actions')), 'New session and Actions… buttons'],
        [s.conn === 'daemon connected', 'the top bar says daemon connected'],
      ],
      async after(page) {
        await page.click(`document.querySelector('#empty [data-action="launch"]')`, { step: 'connected-empty: New session' });
        const open = await page.element('#launch-dialog[open]', '(e) => e.open', { step: 'connected-empty: launch dialog opens' });
        await page.key('Escape');
        return [[open === true, 'the New session button opens the launch dialog']];
      },
    },
    {
      id: 'connected-sessions', title: 'rsid is up with sessions: a plain prompt, no banner',
      call: (m, p, sock) => rpcCall(sock, m, p),
      ready: (s) => s.boot !== null && s.rows > 0,
      check: (s) => [
        [s.boot === 'ready', `data-boot is ready (got ${s.boot})`],
        [s.onboarding === 'select', `guidance is select (got ${s.onboarding})`],
        [s.rows > 0, 'the session list has rows'],
        [s.banner === null, 'no banner'],
      ],
    },
    {
      id: 'settings-fails', title: 'the Settings config read fails at boot: it is visible, not silent',
      call: (m, p, sock) => {
        if (m === 'GetDaemonConfig') throw new Error('Method not found: GetDaemonConfig');
        return rpcCall(sock, m, p);
      },
      ready: (s) => s.boot !== null,
      check: (s) => [
        [s.boot === 'failed', `data-boot is failed (got ${s.boot})`],
        [s.report?.steps?.some((x) => x.name === 'settings' && !x.ok && /Method not found/.test(x.error ?? '')), 'the settings step failed with rsid\'s error'],
        [s.banner !== null && has(s.banner, 'settings') && has(s.banner, 'Method not found'), 'a visible banner names settings and the error'],
      ],
    },
    {
      id: 'daemon-recovers', title: 'rsid starts after the app: Retry now reconnects and the report turns ready',
      call: (m, p, sock, ctl) => { if (!ctl.up) throw new Error(DOWN); return rpcCall(sock, m, p); },
      ready: (s) => s.boot !== null,
      check: (s) => [
        [s.boot === 'offline', `starts offline (got ${s.boot})`],
        [s.onboarding === 'daemon-down', 'with the daemon-down guidance'],
      ],
      async after(page, ctl) {
        ctl.up = true;
        await page.click(`document.querySelector('#empty [data-action="refresh"]')`, { step: 'daemon-recovers: Retry now' });
        const ok = await page.ready(`document.documentElement.dataset.boot === 'ready'`, { step: 'daemon-recovers: boot completes after Retry now' });
        const s = await page.eval(PAGE_STATE);
        return [
          [ok, `data-boot becomes ready after Retry now (got ${s.boot})`],
          [s.conn === 'daemon connected', 'the top bar now says daemon connected'],
          [s.report?.steps?.every((x) => x.ok), 'all steps ok again (the data steps were re-run)'],
          [s.onboarding !== 'daemon-down', `the daemon-down guidance is gone (got ${s.onboarding})`],
        ];
      },
    },
  ];
}

export async function runFirstRun({ out, chromeBin, dist, signal }) {
  mkdirSync(out, { recursive: true });
  const fake = await startFakeDaemon();
  const results = [];
  try {
    for (const sc of scenarios()) {
      const ctl = { up: false };
      const log = [];
      const bridge = createBridge({ call: (m, p) => sc.call(m, p, fake.socket, ctl), record: (e) => log.push(e) });
      const token = newToken();
      const { server, port } = await serve({ dist, bridge, token, onRefusal: (e) => log.push(e), signal });
      let page, chrome;
      const checks = [];
      try {
        // These are independent first launches: do not carry a renderer, module
        // cache or page state from one daemon situation into the next.
        chrome = await launchChrome(chromeBin, { signal });
        page = await connectPage(chrome.cdpPort, { signal });
        await page.goto(`http://127.0.0.1:${port}/`);
        await page.element('#conn', undefined, { step: `${sc.id}: page shell renders` });
        let s = await page.ready(`(() => { const s = ${PAGE_STATE}; return (${sc.ready.toString()})(s) ? s : null; })()`, {
          step: `${sc.id}: boot and scenario state render`, action: '(s) => s',
        });
        const errs = page.events.filter((e) => e.method === 'Runtime.exceptionThrown');
        checks.push([errs.length === 0, 'no uncaught page error']);
        checks.push(...sc.check(s));
        const shot = await page.shot(join(out, `firstrun-${sc.id}.png`));
        if (sc.after) {
          checks.push(...await sc.after(page, ctl));
          s = await page.eval(PAGE_STATE);
          await page.shot(join(out, `firstrun-${sc.id}-after.png`));
        }
        results.push({
          id: sc.id, title: sc.title, status: checks.every(([ok]) => ok) ? 'pass' : 'fail',
          checks: checks.map(([ok, what]) => ({ ok: Boolean(ok), what })), screenshot: shot, finalBoot: s.boot, finalReport: s.report,
        });
      } finally {
        page?.close();
        server.closeAllConnections();
        server.close();
        await chrome?.stop();
      }
    }
  } finally {
    await fake.stop();
  }
  writeFileSync(join(out, 'firstrun-report.json'), `${JSON.stringify({ results }, null, 2)}\n`);
  for (const r of results) {
    console.log(`${r.status.toUpperCase().padEnd(5)} ${r.id.padEnd(20)} ${r.title}`);
    for (const c of r.checks) console.log(`        ${c.ok ? 'ok  ' : 'FAIL'} ${c.what}`);
    console.log(`        ${r.screenshot}`);
  }
  console.log(`report: ${join(out, 'firstrun-report.json')}`);
  return results.some((r) => r.status === 'fail') ? 1 : 0;
}

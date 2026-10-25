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
import { HOME_PROJECT, startFakeDaemon } from './smoke-fake.mjs';
import { bootWithRetries, connectPage, domSnapshot, launchChrome, newToken, serve } from './smoke-lib.mjs';

const DOWN = 'cannot reach rsid: /tmp/rsi-demo/daemon.sock: No such file or directory (os error 2)';
const INSTALLED_LAUNCHD = { kind: 'installed', manager: 'launchd', name: 'dev.rsi.rsid', path: '/Users/demo/Library/LaunchAgents/dev.rsi.rsid.plist' };
const RUNNING_LAUNCHD = { ...INSTALLED_LAUNCHD, state: 'running', pid: 4242, socket: { app: '/tmp/rsi-demo/daemon.sock', service: '/tmp/rsi-demo/daemon.sock', matches: true } };
const OTHER_SOCKET = { ...INSTALLED_LAUNCHD, state: 'running', pid: 4242, socket: { app: '/tmp/rsi-demo/none.sock', service: '/Users/demo/.rsi/daemon.sock', matches: false } };
const KICKSTART_ERR = 'Could not find service "dev.rsi.rsid" in domain for user gui: 501';

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
    focus: (() => { const a = document.activeElement; return a?.dataset?.startDaemon !== undefined ? 'start' : a?.closest?.('#empty') ? 'card' : (a?.tagName ?? null); })(),
    start: (() => { const x = e?.querySelector('button[data-start-daemon]'); return x ? { label: x.textContent, disabled: x.disabled } : null; })(),
    startStatus: e?.querySelector('.onboard-start-status')?.textContent ?? null,
    devText: e?.querySelector('details.onboard-developer')?.textContent ?? '',
    banner: b && !b.hidden ? b.textContent : null,
    daemonBanner: (() => {
      const d = document.getElementById('daemon-banner');
      if (!d || d.hidden || d.offsetParent === null) return null;
      const r = d.getBoundingClientRect();
      const q = (sel) => document.querySelector(sel);
      return {
        text: d.innerText, buttons: [...d.querySelectorAll('button')].map((x) => ({ label: x.textContent, disabled: x.disabled })),
        top: Math.round(r.top), height: Math.round(r.height), width: Math.round(r.width), focused: d.contains(document.activeElement),
        stale: q('#s-meta .stale-note')?.textContent ?? null,
      };
    })(),
    session: (() => {
      const q = (sel) => document.querySelector(sel);
      const v = q('#session-view');
      return {
        open: Boolean(v && !v.hidden), emptyShown: Boolean(e && !e.hidden), title: q('#s-title')?.textContent ?? null,
        status: q('#s-meta .status-chip')?.textContent ?? null, stale: q('#s-meta .stale-note')?.textContent ?? null,
        stop: q('#interrupt')?.disabled ?? null, pause: q('#hard-pause')?.disabled ?? null, archive: q('#archive')?.disabled ?? null,
        send: q('#composer button[type=submit]')?.disabled ?? null, draft: q('#message')?.value ?? null,
        pillRole: document.getElementById('conn')?.getAttribute('role') ?? null, events: document.querySelectorAll('#events > *').length,
      };
    })(),
    conn: document.getElementById('conn')?.textContent ?? null,
    rows: document.querySelectorAll('#session-list li').length,
  };
})()`;

/** The scenarios: how the bridge answers and what must be on the page. */
export function scenarios() {
  const has = (t, s) => t.includes(s);
  return [
    {
      id: 'daemon-missing', title: 'rsid is not running: the newcomer is told what it is, where it looked and offered Start rsid (local #232)',
      call: () => { throw new Error(DOWN); },
      // The installed launchd agent, and a start the service manager refuses: the card shows the exit status and stderr.
      local: () => ({
        daemon_service: () => INSTALLED_LAUNCHD,
        start_daemon: () => ({ ok: false, manager: 'launchd', command: 'launchctl kickstart gui/501/dev.rsi.rsid', status: 113, stderr: KICKSTART_ERR }),
      }),
      ready: (s) => s.boot !== null && s.start?.disabled === false,
      check: (s) => [
        [s.boot === 'offline', `data-boot is offline (got ${s.boot})`],
        [s.report?.daemon === 'unreachable', 'the report names the unreachable daemon'],
        [s.onboarding === 'daemon-down', `guidance is daemon-down (got ${s.onboarding})`],
        [has(s.emptyText, 'Cannot reach the rsid daemon'), 'title says it cannot reach rsid'],
        [has(s.emptyText, '/tmp/rsi-demo/daemon.sock') || has(s.emptyText, 'daemon.sock'), 'it names the socket it looked at'],
        [s.focus === 'start', `the Start rsid button has focus after boot (local #244; got ${s.focus})`],
        [s.emptyButtons[0] === 'Start rsid' && s.start?.disabled === false, `the first button is an enabled Start rsid (got ${JSON.stringify(s.emptyButtons)})`],
        [has(s.emptyText, 'login service (dev.rsi.rsid)'), 'it says rsid is installed as a login service'],
        [has(s.devText, 'For developers') && has(s.devText, 'make release-install') && has(s.devText, 'RSI_DAEMON_SOCKET'), 'the terminal commands are under For developers'],
        [s.emptyButtons.some((b) => b.startsWith('Retry now')), 'a Retry now button'],
        [s.conn === 'daemon unreachable', 'the top bar says daemon unreachable'],
        [s.banner === null, 'no failure banner: offline is expected, not broken'],
      ],
      async after(page) {
        await page.click(`document.querySelector('#empty button[data-start-daemon]')`, { step: 'daemon-missing: Start rsid' });
        const failed = await page.waitFor(`(document.querySelector('#empty .onboard-start-status')?.textContent ?? '').includes('exit status 113')`);
        const s = await page.eval(PAGE_STATE);
        const raw = await page.eval(`document.querySelector('#empty .onboard-start-status .err-raw')?.textContent ?? ''`);
        return [
          [failed, `a refused start says so with its exit status (got ${s.startStatus})`],
          [raw.includes(KICKSTART_ERR) && raw.includes('launchctl kickstart'), `Details holds the command and its stderr (got ${raw})`],
          [s.start?.label === 'Start rsid' && s.start?.disabled === false, 'Start rsid can be pressed again'],
          [s.onboarding === 'daemon-down', 'the card stays: rsid is still down'],
        ];
      },
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
      id: 'connected-no-projects', title: 'rsid is up with no projects of the person\'s own, only the built-in RSI home every rsid lists: what a project is and a visible way to create one (desktop #210, #319)',
      call: (m, p, sock) => (m === 'ListSessions' ? [] : m === 'ListProjects' ? [HOME_PROJECT] : rpcCall(sock, m, p)),
      ready: (s) => s.boot !== null && s.onboarding === 'no-projects',
      check: (s) => [
        [s.onboarding === 'no-projects', `guidance is no-projects (got ${s.onboarding})`],
        [has(s.emptyText, 'A project is a folder with your code that sessions work in'), 'it says what a project is'],
        [s.emptyButtons[0] === 'Create your first project', 'the first button is Create your first project'],
        [s.emptyButtons.includes('New session'), 'New session is still offered'],
      ],
      async after(page) {
        await page.click(`document.querySelector('#empty [data-action="project-new"]')`, { step: 'no-projects: Create your first project' });
        const form = await page.eval(`(() => {
          const d = document.querySelector('dialog.pick-form-dialog[open]');
          return d ? { text: d.innerText, hints: [...d.querySelectorAll('.field-hint')].map((h) => h.textContent) } : null;
        })()`);
        const submit = (name, path) => page.eval(`(() => {
          const d = document.querySelector('dialog.pick-form-dialog[open]');
          d.querySelector('[name=name]').value = ${JSON.stringify(name)};
          d.querySelector('[name=path]').value = ${JSON.stringify(path)};
          d.querySelector('button[type=submit]').click();
          return d.querySelector('.form-error')?.textContent ?? '';
        })()`);
        const relative = await submit('demo', 'relative/dir');
        const empty = await submit('', '');
        await page.eval(`document.querySelector('dialog.pick-form-dialog[open] .actions .btn').click()`);
        await page.click(`document.querySelector('#empty [data-action="launch"]')`, { step: 'no-projects: New session' });
        await page.waitFor(`document.querySelector('#launch-dialog[open]') !== null`);
        const launch = await page.eval(`(() => {
          const sel = document.querySelector('#launch-form select[name=project_id]');
          const hint = document.getElementById('launch-project-hint');
          return { last: sel.options[sel.options.length - 1].textContent, hint: hint ? hint.textContent : '' };
        })()`);
        const picked = await page.eval(`(() => {
          const sel = document.querySelector('#launch-form select[name=project_id]');
          sel.value = '__new_project__';
          sel.dispatchEvent(new Event('change', { bubbles: true }));
          const d = document.querySelector('dialog.pick-form-dialog[open]');
          return { form: d ? d.querySelector('h2').textContent : null, kept: sel.value };
        })()`);
        await page.eval(`document.querySelector('dialog.pick-form-dialog[open] .actions .btn')?.click()`);
        await page.eval(`document.getElementById('launch-dialog').close()`);
        return [
          [picked.form === 'New project' && picked.kept === '', `choosing New project… in the select opens the form and keeps the old choice (got ${JSON.stringify(picked)})`],
          [form !== null && form.text.includes('Folder'), 'the project form labels the path field Folder'],
          [form !== null && form.hints.some((h) => h.includes('New sessions in this project start here')), 'the Folder field says what it is for'],
          [/full path/.test(relative), `a relative folder is refused in plain words (got ${relative})`],
          [/Name is required/.test(empty), `an empty name is refused in plain words (got ${empty})`],
          [launch.last === 'New project…', `the New session dialog offers New project… (got ${JSON.stringify(launch)})`],
          [launch.hint.includes('folder with your code') && launch.hint.includes('New project…'), 'with no project the dialog explains one and has a New project… button'],
        ];
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
    {
      id: 'daemon-starts', title: 'Start rsid asks the service manager, shows Starting rsid… and reconnects on its own (local #232)',
      call: (m, p, sock, ctl) => { if (!ctl.up) throw new Error(DOWN); return rpcCall(sock, m, p); },
      local: (ctl) => ({
        daemon_service: () => ({ kind: 'installed', manager: 'systemd', name: 'rsid.service', path: '/home/demo/.config/systemd/user/rsid.service' }),
        // The service manager accepts the start; rsid answers a moment later.
        start_daemon: () => new Promise((resolve) => setTimeout(() => {
          ctl.started = (ctl.started ?? 0) + 1;
          setTimeout(() => { ctl.up = true; }, 1500);
          resolve({ ok: true, manager: 'systemd', command: 'systemctl --user start rsid.service', status: 0, stderr: '' });
        }, 300)),
      }),
      ready: (s) => s.boot !== null && s.start?.disabled === false,
      check: (s) => [
        [s.onboarding === 'daemon-down', 'starts with the daemon-down guidance'],
        [s.emptyButtons[0] === 'Start rsid', 'Start rsid is the first button'],
        [s.emptyText.includes('systemd user service rsid.service'), 'it names the systemd user service'],
      ],
      async after(page, ctl) {
        await page.click(`document.querySelector('#empty button[data-start-daemon]')`, { step: 'daemon-starts: Start rsid' });
        const starting = await page.waitFor(`(document.querySelector('#empty .onboard-start-status')?.textContent ?? '').includes('Starting rsid')`, 3000);
        const ok = await page.waitFor(`document.documentElement.dataset.boot === 'ready'`, 15000);
        const s = await page.eval(PAGE_STATE);
        return [
          [starting, 'the card says Starting rsid… while it waits'],
          [ctl.started === 1, `start_daemon ran once (got ${ctl.started})`],
          [ok, `data-boot becomes ready with no Retry press (got ${s.boot})`],
          [s.conn === 'daemon connected', 'the top bar now says daemon connected'],
          [s.onboarding !== 'daemon-down', `the daemon-down guidance is gone (got ${s.onboarding})`],
        ];
      },
    },
    {
      id: 'daemon-running', title: 'the login service is already running but does not answer: no Start rsid, a confirmed Restart rsid instead (local #258)',
      call: () => { throw new Error(DOWN); },
      local: (ctl) => ({
        daemon_service: () => RUNNING_LAUNCHD,
        start_daemon: () => { ctl.started = (ctl.started ?? 0) + 1; throw new Error('Start must not be offered for a running service'); },
        restart_daemon: () => { ctl.restarted = (ctl.restarted ?? 0) + 1; return { ok: true, manager: 'launchd', command: 'launchctl kickstart -k gui/501/dev.rsi.rsid', status: 0, stderr: '', refused: null }; },
      }),
      ready: (s) => s.boot !== null && s.start?.disabled === false,
      check: (s) => [
        [s.onboarding === 'daemon-down', 'the daemon-down guidance shows'],
        [s.emptyButtons[0] === 'Restart rsid' && !s.emptyButtons.includes('Start rsid'), `the service button is Restart rsid, never Start rsid (got ${JSON.stringify(s.emptyButtons)})`],
        [has(s.emptyText, 'already running (pid 4242)') && has(s.emptyText, 'not answering'), 'it says the service is running (pid 4242) and not answering'],
        [has(s.emptyText, 'interrupts any sessions it is running'), 'it says what a restart costs'],
        [s.emptyButtons.some((b) => b.startsWith('Retry now')), 'a Retry now button'],
      ],
      async after(page, ctl) {
        await page.click(`document.querySelector('#empty button[data-start-daemon]')`, { step: 'daemon-running: Restart rsid' });
        const asked = await page.waitFor(`document.querySelector('dialog.confirm[open]') !== null`, 3000);
        const text = await page.eval(`document.querySelector('dialog.confirm[open]')?.textContent ?? ''`);
        await page.click(`[...document.querySelectorAll('dialog.confirm[open] button')].find((b) => b.textContent === 'Keep it running')`, { step: 'daemon-running: keep it running' });
        const closed = await page.waitFor(`document.querySelector('dialog.confirm[open]') === null`, 3000);
        return [
          [asked && text.includes('Restart rsid?') && text.includes('interrupted'), `Restart rsid asks first (got ${text})`],
          [closed && !ctl.restarted && !ctl.started, `declining runs nothing (restarted ${ctl.restarted ?? 0}, started ${ctl.started ?? 0})`],
        ];
      },
    },
    {
      id: 'daemon-other-socket', title: 'the window looks at a socket the service does not hold: Start rsid is off with the reason (local #258)',
      call: () => { throw new Error(DOWN); },
      local: () => ({ daemon_service: () => OTHER_SOCKET }),
      ready: (s) => s.boot !== null && s.start !== null,
      check: (s) => [
        [s.start?.label === 'Start rsid' && s.start?.disabled === true, `Start rsid is disabled (got ${JSON.stringify(s.start)})`],
        [has(s.startStatus ?? '', '/tmp/rsi-demo/none.sock') && has(s.startStatus ?? '', '/Users/demo/.rsi/daemon.sock'), 'it names both sockets'],
        [!s.emptyButtons.includes('Restart rsid'), 'no Restart rsid either: it could not help'],
      ],
    },
    {
      // Local #266: the card lives in the empty pane, so with a session open only the pill changed. A strip above the layout now carries it.
      id: 'daemon-down-open-session', ownFake: true,
      title: 'rsid is killed while a session is open: a strip with Start rsid and Retry now shows over the session, which is marked last known; rsid back, it clears and the session resumes (local #266)',
      call: (m, p, sock, ctl) => { if (!ctl.fake) throw new Error(DOWN); return rpcCall(ctl.fake.socket, m, p); },
      local: (ctl) => ({
        daemon_service: () => INSTALLED_LAUNCHD,
        // The service manager starts a new fake rsid a moment later: Start rsid from the strip brings the session back.
        start_daemon: () => new Promise((resolve) => setTimeout(() => {
          ctl.started = (ctl.started ?? 0) + 1;
          setTimeout(() => { void ctl.revive(); }, 800);
          resolve({ ok: true, manager: 'launchd', command: 'launchctl kickstart gui/501/dev.rsi.rsid', status: 0, stderr: '', refused: null });
        }, 200)),
      }),
      ready: (s) => s.boot === 'ready' && s.rows > 0,
      check: (s) => [
        [s.boot === 'ready', `starts connected (got ${s.boot})`],
        [s.daemonBanner === null, 'no strip while rsid answers'],
      ],
      async after(page, ctl) {
        await page.click(`[...document.querySelectorAll('#session-list .session-item')].find((li) => li.textContent.includes('Fake worker'))`, { step: 'daemon-down-open-session: open Fake worker' });
        await page.waitFor(`document.querySelector('#s-meta .status-chip')?.textContent === 'Running'`, 5000);
        await page.eval(`(() => { const m = document.getElementById('message'); m.value = 'draft that must survive'; m.dispatchEvent(new Event('input', { bubbles: true })); m.focus(); })()`);
        const before = await page.eval(PAGE_STATE);
        await page.shot(join(ctl.out, 'firstrun-daemon-down-open-session-connected.png'));
        // Kill the fake rsid with the session open.
        const dead = ctl.fake;
        ctl.fake = null;
        await dead.stop();
        const shown = await page.waitFor(`(() => { const d = document.getElementById('daemon-banner'); return d && !d.hidden && d.offsetParent !== null; })()`, 20000);
        // The strip appears before the install probe answers; Start rsid stays disabled ("Checking whether rsid is installed…") until it settles. Wait for that, bounded, then assert the settled state below.
        await page.waitFor(`(() => { const d = document.getElementById('daemon-banner'); const t = d?.querySelector('.onboard-start-status')?.textContent ?? ''; return d !== null && !d.hidden && !t.includes('Checking whether rsid is installed') && d.querySelector('button[data-start-daemon]') !== null; })()`, 10000);
        const down = await page.eval(PAGE_STATE);
        await page.shot(join(ctl.out, 'firstrun-daemon-down-open-session-down.png'));
        const b = down.daemonBanner;
        const labels = b?.buttons.map((x) => x.label) ?? [];
        // Send with rsid down: the draft stays, the toast says it failed.
        await page.eval(`document.querySelector('#composer button[type=submit]').click()`);
        const toasted = await page.waitFor(`(document.getElementById('toast')?.textContent ?? '').includes('Send failed')`, 4000);
        const afterSend = await page.eval(PAGE_STATE);
        // The pill is a button that moves to the help.
        await page.key('Escape');
        await page.eval(`document.getElementById('conn').click()`);
        const pillFocus = await page.eval(`document.activeElement?.closest?.('#daemon-banner') !== null && document.activeElement?.tagName === 'BUTTON'`);
        // Start rsid from the strip; the service manager brings rsid back.
        await page.click(`document.querySelector('#daemon-banner button[data-start-daemon]')`, { step: 'daemon-down-open-session: Start rsid in the strip' });
        const starting = await page.waitFor(`(document.querySelector('#daemon-banner .onboard-start-status')?.textContent ?? '').includes('Starting rsid')`, 3000);
        const gone = await page.waitFor(`document.getElementById('daemon-banner').hidden === true`, 20000);
        await page.waitFor(`document.querySelector('#interrupt')?.disabled === false`, 10000);
        const up = await page.eval(PAGE_STATE);
        await page.shot(join(ctl.out, 'firstrun-daemon-down-open-session-back.png'));
        return [
          [before.session.status === 'Running' && before.session.open, `the session is open and Running before the kill (got ${JSON.stringify(before.session)})`],
          [shown, 'the strip appears within the health poll after the fake is killed'],
          [b && b.text.includes('rsid is not running') && b.text.includes('last known state'), `it says rsid is not running and the state is last known (got ${b?.text})`],
          [labels[0] === 'Start rsid' && labels.includes('Retry now') && b.buttons[0].disabled === false, `Start rsid (enabled) and Retry now are offered (got ${JSON.stringify(labels)})`],
          [b && b.top < 120 && b.height > 30 && b.width > 400, `the strip is on screen above the session (top ${b?.top}, ${b?.width}x${b?.height})`],
          [down.session.open && !down.session.emptyShown, 'the session stays open; the empty-pane card is not what shows'],
          [down.session.status === 'Running' && down.session.stale === 'last known: rsid is not running', `the header marks Running as last known (got ${down.session.status} / ${down.session.stale})`],
          [down.session.stop === true && down.session.pause === true && down.session.archive === true, `Stop, Pause and Archive are off while rsid is down (got ${JSON.stringify([down.session.stop, down.session.pause, down.session.archive])})`],
          [down.session.send === false && down.session.draft === 'draft that must survive', 'Send stays enabled and the draft is kept'],
          [down.session.pillRole === 'button', 'the top-bar pill is a button'],
          [b && !b.focused, 'the strip did not take focus from the message box'],
          [toasted && afterSend.session.draft === 'draft that must survive', 'a failed Send toasts and keeps the draft'],
          [pillFocus, 'pressing the pill moves focus to a button in the strip'],
          [starting, 'the strip shows Starting rsid… after Start rsid'],
          [ctl.started === 1, `start_daemon ran once (got ${ctl.started})`],
          [gone, 'the strip clears when rsid answers again'],
          [up.session.open && up.session.title === before.session.title && up.session.status === 'Running' && up.session.stale === null, `the same session is still open, no longer marked last known (got ${JSON.stringify(up.session)})`],
          [up.session.stop === false && up.session.pause === false && up.session.archive === false, 'Stop, Pause and Archive work again'],
          [up.session.draft === 'draft that must survive' && up.session.pillRole === null, 'the draft is still there and the pill is plain text again'],
          [up.conn === 'daemon connected', 'the top bar says daemon connected'],
        ];
      },
    },
  ];
}

/** Keep a missing module from stranding the first-run suite in the static connecting shell. */
export async function freshFirstRunPage(page, url, sc, log) {
  const from = page.events.length;
  const bridgeStart = log.length;
  try {
    return await bootWithRetries(page, url, async (stop) => {
      await page.element('#conn', undefined, { step: `${sc.id}: page shell renders`, stop });
      return page.ready(`(() => { const s = ${PAGE_STATE}; return (${sc.ready.toString()})(s) ? s : null; })()`, {
        step: `${sc.id}: boot and scenario state render`, action: '(s) => s', stop,
      });
    });
  } catch (error) {
    const diagnostics = { attempts: error.bootAttempts, dom: await domSnapshot(page),
      network: page.events.slice(from).filter((e) => e.method === 'Network.loadingFailed'),
      bridge: log.slice(bridgeStart),
    };
    throw new Error(`${error.message}; startup diagnostics: ${JSON.stringify(diagnostics)}`, { cause: error });
  }
}

export async function runFirstRun({ out, chromeBin, dist, signal }) {
  mkdirSync(out, { recursive: true });
  const fake = await startFakeDaemon();
  const results = [];
  try {
    for (const sc of scenarios()) {
      const ctl = { up: false, out };
      // A scenario that kills and revives its rsid owns a fake (the shared one is not touched).
      if (sc.ownFake) {
        ctl.fake = await startFakeDaemon();
        ctl.revive = async () => { ctl.fake ??= await startFakeDaemon(); };
      }
      const log = [];
      const bridge = createBridge({ call: (m, p) => sc.call(m, p, fake.socket, ctl), record: (e) => log.push(e), local: sc.local ? sc.local(ctl) : {} });
      const token = newToken();
      const { server, port } = await serve({ dist, bridge, token, onRefusal: (e) => log.push(e), signal });
      let page, chrome;
      const checks = [];
      try {
        // These are independent first launches: do not carry a renderer, module
        // cache or page state from one daemon situation into the next.
        chrome = await launchChrome(chromeBin, { signal });
        page = await connectPage(chrome.cdpPort, { signal });
        let s = await freshFirstRunPage(page, `http://127.0.0.1:${port}/`, sc, log);
        // A failed import may emit an exception; judge the final document's app errors.
        const errs = page.events.slice(page.events.findLastIndex((e) => e.method === 'Runtime.executionContextsCleared') + 1)
          .filter((e) => e.method === 'Runtime.exceptionThrown');
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
        if (ctl.fake) await ctl.fake.stop();
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

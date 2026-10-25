// Smoke steps for the views that read manager, dashboard, budget, Trash, approval and model-switch data (local #234).
//
// The fake daemon (smoke-fake.mjs) answers each of these reads with a populated, daemon-shaped fixture, so each step proves
// the view shows that content, not its error state: it names the text it saw and fails on the view's error line. Against
// --live the real daemon's data is the operator's own, so those steps are substituted and the fake run covers them.
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const pass = (note) => ({ status: 'pass', note });
const fail = (note) => ({ status: 'fail', note });
const DIALOG = `(document.querySelector('dialog[open]')||{}).innerText||''`;
const text = (selector) => `(document.querySelector(${JSON.stringify(selector)})||{}).innerText||''`;

/** The Managers overlay: its title, status line (an error there carries `.err`), list rows and the whole text. */
const MANAGERS = {
  open: `(() => { const v = document.getElementById('managers-view'); return !!v && !v.hidden; })()`,
  title: text('#managers-view .mgr-title'),
  status: text('#managers-view .mgr-status'),
  failed: `!!document.querySelector('#managers-view .mgr-status.err')`,
  rows: `document.querySelectorAll('#managers-view .mgr-item').length`,
  all: text('#managers-view'),
};

/**
 * Steps for the views the fake daemon fills. `closeAll`, `palette` and `command` are the smoke's own key helpers.
 * @returns {Array<{id: string, title: string, run: (p: any) => Promise<{status: string, note: string}>, after?: (p: any) => Promise<void>}>}
 */
export function managerSteps({ live, closeAll, palette, command }) {
  /** One step: open a view, wait for rows, check it holds `needles` and shows no error. */
  const viewStep = ({ id, group, refuses, title, open, ready, read, needles, absent = [] }) => ({
    id, group, refuses, title,
    async run(p) {
      if (live) return { status: 'substituted', note: 'the live daemon holds the operator\'s own data; the fake-daemon run checks the populated view' };
      await open(p);
      if (!(await p.waitFor(ready, 8000))) return fail(`${id} never showed rows; saw ${JSON.stringify((await p.eval(read)).slice(0, 160))}`);
      await sleep(400);
      const seen = await p.eval(read);
      for (const n of needles) if (!seen.includes(n)) return fail(`${id} is missing "${n}"; saw ${JSON.stringify(seen.slice(0, 240))}`);
      for (const n of absent) if (seen.includes(n)) return fail(`${id} shows "${n}": ${JSON.stringify(seen.slice(0, 240))}`);
      return pass(`${id} shows ${needles.map((n) => `"${n}"`).join(', ')}`);
    },
    after: closeAll,
  });
  const managers = (what, actionTitle, needles, extra = {}) => viewStep({
    id: what.id, group: what.group, refuses: what.refuses, title: what.title,
    open: (p) => palette(p, actionTitle),
    ready: `${MANAGERS.open} && ${MANAGERS.rows} > 0 && !${MANAGERS.failed}`,
    read: MANAGERS.all, needles, absent: ['No manager is appointed', 'Nothing here', 'Method not found'], ...extra,
  });

  // Walk the session list's cursor onto the row naming `title`, then Enter opens it.
  const openSessionRow = async (p, title) => {
    const target = await p.eval(`[...document.querySelectorAll('#session-list li')].findIndex(l => l.innerText.includes(${JSON.stringify(title)}))`);
    if (target < 0) return false;
    const cursor = `[...document.querySelectorAll('#session-list li')].findIndex(l => l.classList.contains('cursor'))`;
    for (let guard = 0; guard < 60; guard++) {
      const at = await p.eval(cursor);
      if (at === target) break;
      await p.key(at < target ? 'j' : 'k'); await sleep(120);
    }
    await p.key('Enter');
    return true;
  };

  return [
    managers({ id: 'manager-tree', group: 'manager', title: 'the manager tree lists the global manager, the project manager and its Epic' }, 'Manager tree',
      ['GLOBAL', 'Global manager', 'PROJECT', 'Fake project', 'EPIC', 'Fake lead', 'dec 1']),
    managers({ id: 'manager-board', group: 'manager', title: 'the manager board shows KPIs and the pending decision' }, 'Manager board',
      ['program accepted 3', 'Land the list-view fix now', 'needs answer', 'Fake lead']),
    managers({ id: 'manager-policy', group: 'manager', refuses: ['mgr_policy_classify'], title: 'the manager policy form shows the saved policy' }, 'Manager policy',
      ['policy saved (revision 4)', 'Mode', 'execute', 'Max active sessions', 'Created sessions quota']),
    managers({ id: 'manager-workspace', group: 'manager', title: 'the global manager workspace shows the seat and the project manager' }, 'Global manager workspace',
      ['Fake project', 'Running']),
    managers({ id: 'manager-budget', group: 'manager', title: 'the budget panel shows the caps of the global node and the project manager' }, 'Manager budget panel',
      ['Fake project', 'active sessions', 'spend (USD)']),
    viewStep({
      id: 'dashboard', group: 'manager', title: 'the global dashboard lists a row per project',
      open: (p) => command(p, 'dashboard'),
      ready: `(() => { const v = document.getElementById('dashboard-view'); return !!v && !v.hidden && document.querySelectorAll('#dashboard-view .dash-row').length > 1 && !document.querySelector('#dashboard-view .dash-status.err'); })()`,
      read: text('#dashboard-view'), needles: ['alpha', 'Zeta', 'Window 24h'], absent: ['could not be read'],
    }),
    {
      id: 'trash', group: 'manager', title: 'Trash lists the deleted session',
      async run(p) {
        if (live) return { status: 'substituted', note: 'the live daemon holds the operator\'s own trash; the fake-daemon run checks the populated view' };
        await palette(p, 'Trash');
        if (!(await p.waitFor(`${DIALOG}.includes('Trash')`, 4000))) return fail(`no Trash dialog; saw ${JSON.stringify((await p.eval(DIALOG)).slice(0, 120))}`);
        const rows = await p.eval(`document.querySelectorAll('dialog[open] .trash-row').length`);
        const seen = await p.eval(DIALOG);
        if (rows < 1 || !seen.includes('Fake deleted worker')) return fail(`Trash lists ${rows} sessions; saw ${JSON.stringify(seen.slice(0, 160))}`);
        return pass(`Trash lists ${rows} deleted session ("Fake deleted worker"), with Restore and Purge`);
      },
      after: closeAll,
    },
    {
      id: 'approval', group: 'manager', title: 'a WaitingApproval session shows its pending question',
      async run(p) {
        if (live) return { status: 'substituted', note: 'the live daemon holds the operator\'s own sessions; the fake-daemon run checks the banner' };
        if (!(await openSessionRow(p, 'Fake waiting worker'))) return fail('the session list has no "Fake waiting worker" row');
        if (!(await p.waitFor(`!!document.querySelector('.approval')`, 6000))) return fail('opening the waiting session showed no approval banner');
        const seen = await p.eval(text('.approval'));
        for (const n of ['Waiting for you', 'Which checks should run before the fix lands?', 'tests', 'smoke']) {
          if (!seen.includes(n)) return fail(`the approval banner is missing "${n}": ${JSON.stringify(seen.slice(0, 200))}`);
        }
        // Local #270: the skip button names what the agent receives; no red "Decline" that reads as a "no".
        const buttons = await p.eval(`JSON.stringify([...document.querySelectorAll('.approval button')].map((b) => ({ text: b.textContent.trim(), danger: b.classList.contains('danger') })))`);
        const bs = JSON.parse(buttons);
        if (!bs.some((b) => b.text === 'Let the agent decide' && !b.danger)) return fail(`the banner has no neutral "Let the agent decide" button: ${buttons}`);
        if (bs.some((b) => /^decline/i.test(b.text) || b.danger)) return fail(`the banner still offers a Decline or red button: ${buttons}`);
        if (!seen.includes('To say no, pick an option or type your answer')) return fail(`the banner does not explain how to say no: ${JSON.stringify(seen.slice(0, 300))}`);
        return pass('the banner shows "Waiting for you", the question, its two options (tests, smoke), a neutral "Let the agent decide" button and the "To say no" note');
      },
      after: closeAll,
    },
    {
      id: 'model-switch', group: 'manager', refuses: ['discover_models'], title: 'the model switcher offers the session\'s models',
      async run(p) {
        if (live) return { status: 'substituted', note: 'the live daemon holds the operator\'s own sessions; the fake-daemon run checks the picker' };
        await palette(p, 'Switch model or effort');
        if (!(await p.waitFor(`document.querySelectorAll('#switch-dialog .pick-item').length > 0`, 4000))) return fail(`no model picker; saw ${JSON.stringify((await p.eval(DIALOG)).slice(0, 160))}`);
        const seen = await p.eval(`document.getElementById('switch-dialog').innerText`);
        // The model list comes from discover_models (a provider CLI call the read-only bridge never makes), so the picker offers
        // the session's own model from GetSessionModelSwitchOptions and its effort row.
        for (const n of ['Now opus · high', 'keeps the conversation', 'opus', 'Effort']) if (!seen.includes(n)) return fail(`the model picker is missing "${n}": ${JSON.stringify(seen.slice(0, 200))}`);
        return pass('the picker shows "Now opus · high", the switch note, the session\'s model and the effort row');
      },
      after: closeAll,
    },
    {
      id: 'budget-policies', group: 'manager', title: 'the Budgets list shows the daemon\'s budget policy',
      async run(p) {
        if (live) return { status: 'substituted', note: 'the live daemon holds the operator\'s own policies; the fake-daemon run checks the list' };
        await p.eval(`import('./budgets.js').then((m) => m.openBudgets())`);
        if (!(await p.waitFor(`${DIALOG}.includes('Budget policies') && document.querySelectorAll('dialog[open] .pick-list li').length > 0`, 4000))) return fail(`no budget policy listed; saw ${JSON.stringify((await p.eval(DIALOG)).slice(0, 160))}`);
        const seen = await p.eval(DIALOG);
        if (/Failed|error|Method not found/i.test(seen)) return fail(`the Budgets list shows an error: ${JSON.stringify(seen.slice(0, 160))}`);
        return pass(`the Budgets list shows a policy: ${JSON.stringify(seen.split('\n').filter(Boolean)[2] ?? '')}`);
      },
      after: closeAll,
    },
    {
      id: 'usage', group: 'manager', title: 'Settings shows usage and model-control status',
      async run(p) {
        if (live) return { status: 'substituted', note: 'the live daemon reports the operator\'s own usage; the fake-daemon run checks the row' };
        await command(p, 'set');
        await p.eval(`import('./settings.js').then((m) => m.openSettings('usage'))`);
        const cells = `[...document.querySelectorAll('#settings-view .sv-value')].map((n) => n.textContent).join('|')`;
        if (!(await p.waitFor(`${cells}.includes('$123.45')`, 5000))) return fail(`the usage row never showed the spend; saw ${JSON.stringify((await p.eval(cells)).slice(0, 200))}`);
        return pass(`the usage row reads "${(await p.eval(cells)).split('|').find((c) => c.includes('$123.45'))}"`);
      },
      after: closeAll,
    },
  ];
}

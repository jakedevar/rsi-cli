// Smoke step for local #243: with Vim keys off no pane or dialog takes Tab, so Tab from each view's initial focus reaches
// the view's named buttons by the browser's own focus order. The views are opened with Vim keys on (their commands), then
// the switch is turned off and Tab is pressed for real (a trusted CDP key event), at most MAX presses.
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const pass = (note) => ({ status: 'pass', note });
const fail = (note) => ({ status: 'fail', note });
const MAX = 80;

/** The element `selector` holds whose text starts with `label`, as an expression. */
const buttonIn = (selector, label) =>
  `[...document.querySelectorAll(${JSON.stringify(`${selector} button`)})].find((b) => b.textContent.trim().startsWith(${JSON.stringify(label)}))`;

/**
 * @returns {Array<{id: string, title: string, run: (p: any) => Promise<{status: string, note: string}>, after?: (p: any) => Promise<void>}>}
 */
export function tabSteps({ closeAll, palette, command, bridgeLog = [] }) {
  const setVim = async (p, on) => {
    await p.eval(`(() => { const c = document.getElementById('vim-keys'); if (c.checked !== ${on}) { c.checked = ${on}; c.dispatchEvent(new Event('change')); } })()`);
    await sleep(250);
  };
  /** Press Tab until the focused element is `expr`'s element; the number of presses, or -1. */
  const tabTo = async (p, expr) => {
    for (let n = 1; n <= MAX; n++) {
      await p.key('Tab'); await sleep(30);
      if (await p.eval(`(() => { const t = ${expr}; return !!t && document.activeElement === t; })()`)) return n;
    }
    return -1;
  };
  /** One view: open it, switch Vim keys off, Tab to each named button. */
  const view = ({ id, group, title, open, ready, root, buttons }) => ({
    id, group, title,
    async run(p) {
      await setVim(p, true);
      await open(p);
      if (!(await p.waitFor(ready, 8000))) return fail(`${id}: the view did not open`);
      await setVim(p, false);
      await p.eval(`document.activeElement && document.activeElement.blur && document.activeElement.blur()`);
      const seen = [];
      for (const label of buttons) {
        const n = await tabTo(p, buttonIn(root, label));
        if (n < 0) return fail(`${id}: Tab never reached "${label}" in ${MAX} presses (focus ended on ${await p.eval(`(document.activeElement||{}).tagName + '.' + ((document.activeElement||{}).className||'')`)})`);
        seen.push(`${label} (${n})`);
      }
      return pass(`with Vim keys off Tab reached ${seen.join(', ')}`);
    },
    async after(p) { await setVim(p, true); await closeAll(p); },
  });
  const open = (id) => `(() => { const v = document.getElementById(${JSON.stringify(id)}); return !!v && !v.hidden; })()`;

  return [
    view({
      id: 'tab-settings', group: 'keyboard', title: 'with Vim keys off Tab reaches Settings Refresh and Close',
      open: (p) => command(p, 'set'), ready: open('settings-view'), root: '#settings-view', buttons: ['Refresh', 'Close'],
    }),
    view({
      id: 'tab-issues', group: 'keyboard', title: 'with Vim keys off Tab reaches the Issues workspace New Issue',
      open: (p) => command(p, 'issues'), ready: open('issues-view'), root: '#issues-view', buttons: ['New Issue'],
    }),
    view({
      id: 'tab-dashboard', group: 'keyboard', title: 'with Vim keys off Tab reaches the dashboard Refresh',
      open: (p) => command(p, 'dashboard'), ready: open('dashboard-view'), root: '#dashboard-view', buttons: ['Refresh'],
    }),
    view({
      id: 'tab-manager-board', group: 'keyboard', title: 'with Vim keys off Tab reaches the manager board Refresh and Close',
      open: (p) => palette(p, 'Manager board'), ready: open('managers-view'), root: '#managers-view', buttons: ['Refresh', 'Close'],
    }),
    view({
      id: 'tab-dag', group: 'keyboard', title: 'with Vim keys off Tab reaches the DAG browser buttons',
      open: (p) => command(p, 'dag'), ready: open('dag-view'), root: '#dag-view', buttons: ['Close'],
    }),
    {
      id: 'tab-help', group: 'keyboard', title: 'with Vim keys off Tab reaches Help Getting started and keeps focus on it when the view changes',
      async run(p) {
        await setVim(p, false);
        await palette(p, 'Keyboard shortcuts');
        if (!(await p.waitFor(`!!document.querySelector('#keys-help[open]')`, 4000))) return fail('Help did not open');
        await p.eval(`document.getElementById('keys-help').focus()`);
        const target = buttonIn('#keys-help', 'Getting started');
        const n = await tabTo(p, target);
        if (n < 0) return fail('Tab never reached Getting started in the Help view tabs');
        // Pick the view with Enter: the tab buttons are not rebuilt, so focus stays on the same button.
        await p.eval(`(() => { const t = ${buttonIn('#keys-help', 'All Commands')}; t.click(); })()`);
        await sleep(200);
        const target2 = buttonIn('#keys-help', 'Getting started');
        await p.eval(`(() => { ${target2}.focus(); })()`);
        await p.key('Enter'); await sleep(250);
        const kept = await p.eval(`(() => { const t = ${target2}; return document.activeElement === t && t.classList.contains('active'); })()`);
        if (!kept) return fail('Getting started lost focus or did not become the active view after Enter');
        return pass(`Tab reached Getting started in ${n} presses; Enter showed it and kept the focus on it`);
      },
      async after(p) { await setVim(p, true); await closeAll(p); },
    },
    // Local #265: the Issue detail pane has plain Edit and Change status buttons. With Vim keys off Tab reaches them, Enter opens
    // the status form, and Change status sends the same update_issue_status the S key does (the read-only bridge refuses it,
    // which the log still records as an attempt).
    {
      id: 'issue-status-button', group: 'keyboard', refuses: ['update_issue_status'], title: 'the Issue detail pane has Edit and Change status buttons that Tab reaches and that send the status write',
      async run(p) {
        await setVim(p, true);
        await command(p, 'issues');
        if (!(await p.waitFor(`${open('issues-view')} && document.querySelectorAll('#issues-list li').length > 0`, 8000))) return fail('the Issues workspace did not list Issues');
        await setVim(p, false);
        await p.eval(`document.activeElement && document.activeElement.blur && document.activeElement.blur()`);
        const inspector = '#issues-inspector';
        if (!(await p.waitFor(`!!${buttonIn(inspector, 'Edit')} && !!${buttonIn(inspector, 'Change status')}`, 6000))) {
          return fail(`the detail pane shows no Edit and Change status buttons: ${JSON.stringify(await p.eval(`document.getElementById('issues-inspector').innerText.slice(0, 160)`))}`);
        }
        const nEdit = await tabTo(p, buttonIn(inspector, 'Edit'));
        if (nEdit < 0) return fail(`Tab never reached Edit… in ${MAX} presses`);
        const nStatus = await tabTo(p, buttonIn(inspector, 'Change status'));
        if (nStatus < 0) return fail(`Tab never reached Change status… in ${MAX} presses`);
        await p.key('Enter');
        const dialog = `document.querySelector('#issue-dialog[open]')`;
        if (!(await p.waitFor(`!!${dialog} && ${dialog}.innerText.includes('Move to')`, 4000))) return fail('Enter on Change status… did not open the status form');
        const writes = () => bridgeLog.filter((e) => e.cmd === 'update_issue_status').length;
        const before = writes();
        await p.eval(`[...${dialog}.querySelectorAll('button')].find((b) => b.textContent.trim() === 'Change status').click()`);
        const sent = await (async () => { for (let i = 0; i < 40; i++) { if (writes() > before) return true; await sleep(100); } return false; })();
        if (!sent) return fail('Change status in the form attempted no update_issue_status');
        return pass(`Tab reached Edit… (${nEdit}) and Change status… (${nStatus}); Enter opened the form and Change status attempted update_issue_status`);
      },
      async after(p) { await setVim(p, true); await closeAll(p); },
    },
    // Local #268: an archived session has a plain Restore button in its header. With Vim keys off Tab reaches it and Enter
    // sends the same unarchive_session the U key does (the read-only bridge refuses it, which the log still records).
    {
      id: 'archive-restore', group: 'keyboard', refuses: ['unarchive_session'], title: 'an archived session shows Restore instead of Archive, Tab reaches it, and Enter sends the unarchive write',
      async run(p) {
        await setVim(p, true);
        await p.click(`document.getElementById('view-archived')`); await sleep(700);
        const row = `[...document.querySelectorAll('#session-list li')].find((l) => l.innerText.includes('Fake archived session'))`;
        if (!(await p.waitFor(`!!${row}`, 6000))) return fail('the Archived tab does not list "Fake archived session"');
        await p.click(row); await sleep(500);
        await setVim(p, false);
        await p.eval(`document.activeElement && document.activeElement.blur && document.activeElement.blur()`);
        const seen = await p.eval(`JSON.stringify({ restore: !document.getElementById('restore').hidden, archive: !document.getElementById('archive').hidden, hint: document.getElementById('message').placeholder })`);
        const s = JSON.parse(seen);
        if (!s.restore || s.archive || s.hint !== 'Archived: Restore it to message it') return fail(`the header shows ${seen}`);
        const n = await tabTo(p, `document.getElementById('restore')`);
        if (n < 0) return fail(`Tab never reached Restore in ${MAX} presses`);
        const writes = () => bridgeLog.filter((e) => e.cmd === 'unarchive_session').length;
        const before = writes();
        await p.key('Enter');
        const sent = await (async () => { for (let i = 0; i < 40; i++) { if (writes() > before) return true; await sleep(100); } return false; })();
        if (!sent) return fail('Enter on Restore attempted no unarchive_session');
        return pass(`Restore replaces Archive, the composer reads "${s.hint}", Tab reached Restore (${n}) and Enter attempted unarchive_session`);
      },
      async after(p) { await p.click(`document.getElementById('view-active')`); await setVim(p, true); await closeAll(p); },
    },
  ];
}

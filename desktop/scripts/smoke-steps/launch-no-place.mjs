// Local #289: launching with Project none and an empty Folder asks once, in plain words, and the summary line says what will start.
// Esc and "Choose a folder" go back to the dialog; Enter ("Start anyway") launches (the read-only smoke bridge refuses launch_session and logs it).
export default function steps({ pass, fail, sleep, closeAll }) {
  const vimKeys = (p, on) => p.eval(`(() => { const c = document.getElementById('vim-keys'); if (c.checked !== ${on}) { c.checked = ${on}; c.dispatchEvent(new Event('change')); } })()`);
  const askText = (p) => p.eval(`(() => { const d = document.querySelector('dialog.confirm[open]'); return d ? { text: d.textContent, buttons: [...d.querySelectorAll('button')].map((b) => b.textContent) } : null; })()`);
  /** Submit the launch form (the Launch button) and wait for the ask. */
  const submit = async (p) => {
    await p.element('#launch-form button[type=submit]', '(e) => e.click()', { step: 'press Launch' });
    return p.waitFor(`!!document.querySelector('dialog.confirm[open]')`, 3000);
  };
  return [
    {
      id: 'launch-no-place', group: 'keyboard', refuses: ['launch_session', 'launch_session'],
      title: 'Launching with no project and no folder asks first in plain words (Esc and Choose a folder go back, Enter starts it, Vim keys on or off), and the summary line reads in words',
      async run(p) {
        await vimKeys(p, false);
        await closeAll(p);
        await p.eval(`(() => { const f = document.getElementById('project-filter'); f.value = ''; f.dispatchEvent(new Event('change')); })()`);
        await p.element('#new-session', '(e) => e.click()', { step: 'open the launch dialog' });
        if (!(await p.waitFor(`!!document.querySelector('#launch-dialog[open]')`, 3000))) return fail('the New session dialog did not open');
        await p.eval(`(() => { const f = document.getElementById('launch-form'); const s = f.elements.namedItem('project_id'); s.value = ''; s.dispatchEvent(new Event('change', { bubbles: true }));
          const q = f.elements.namedItem('query'); q.value = 'smoke: where does this run'; q.dispatchEvent(new Event('input', { bubbles: true })); })()`);
        await sleep(150);
        const summary = await p.eval(`document.getElementById('launch-summary')?.textContent ?? ''`);
        if (!/^Starts .+ with .+\. Project: none\. Folder: your home folder or the daemon’s default\. Own git worktree: no\.$/.test(summary) || /no working directory|[⊡▮▯]/.test(summary)) return fail(`the summary line is not in plain words: "${summary}"`);
        // Esc goes back to the dialog, with the focus on Choose…
        if (!(await submit(p))) return fail('launching with no project and no folder did not ask');
        const ask = await askText(p);
        if (!ask || !/Start without a project or folder\?/.test(ask.text) || !/home folder or the daemon’s default/.test(ask.text) || !ask.buttons.includes('Start anyway') || !ask.buttons.includes('Choose a folder')) return fail(`the ask is not in plain words: ${JSON.stringify(ask)}`);
        await p.key('Escape'); await sleep(250);
        const back = await p.eval(`({ ask: !!document.querySelector('dialog.confirm[open]'), dialog: !!document.querySelector('#launch-dialog[open]'), focus: document.activeElement && document.activeElement.id })`);
        if (back.ask || !back.dialog || back.focus !== 'launch-folder-pick') return fail(`Esc did not go back to the dialog with Choose… focused: ${JSON.stringify(back)}`);
        // Choose a folder (the button) goes back the same way.
        if (!(await submit(p))) return fail('the second launch did not ask again');
        await p.element('dialog.confirm[open] .actions .btn:not(.primary)', '(e) => e.click()', { step: 'press Choose a folder' }); await sleep(250);
        const chose = await p.eval(`({ ask: !!document.querySelector('dialog.confirm[open]'), focus: document.activeElement && document.activeElement.id })`);
        if (chose.ask || chose.focus !== 'launch-folder-pick') return fail(`Choose a folder did not go back to the dialog: ${JSON.stringify(chose)}`);
        // Enter starts it, with the Vim keys off and on.
        for (const on of [false, true]) {
          await vimKeys(p, on);
          if (!(await submit(p))) return fail(`the launch did not ask (Vim keys ${on ? 'on' : 'off'})`);
          await p.key('Enter'); await sleep(400);
          const started = await p.eval(`({ ask: !!document.querySelector('dialog.confirm[open]'), toast: document.getElementById('toast')?.textContent ?? '' })`);
          if (started.ask) return fail(`Enter did not answer the ask (Vim keys ${on ? 'on' : 'off'})`);
          if (!/Launch failed/.test(started.toast)) return fail(`Enter did not try to launch (Vim keys ${on ? 'on' : 'off'}): toast "${started.toast}"`);
        }
        return pass('the ask appeared once per launch in plain words; Esc and Choose a folder went back to Choose…; Enter tried the launch with Vim keys off and on; the summary line read in words');
      },
      async after(p) {
        await vimKeys(p, true);
        await closeAll(p);
      },
    },
  ];
}

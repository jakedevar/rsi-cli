// Local #292: New session's `More options` button shows the settings side by mouse (Vim keys off): aria-expanded flips, the
// worktree is a real checkbox that the summary line follows, Ctrl-O still flips it, Esc inside it returns to the prompt with the draft.
export default function steps({ pass, fail, sleep, closeAll }) {
  return [
    {
      id: 'launch-more-options', group: 'keyboard',
      title: 'New session: More options (a button, aria-expanded) shows the worktree and manager choices by mouse; the worktree box changes the summary; Esc and Ctrl-O return to the prompt with the draft kept',
      async run(p) {
        await p.eval(`(() => { const c = document.getElementById('vim-keys'); c.checked = false; c.dispatchEvent(new Event('change')); })()`);
        await closeAll(p);
        await p.element('#new-session', '(e) => e.click()', { step: 'open the launch dialog' });
        if (!(await p.waitFor(`!!document.querySelector('#launch-dialog[open]')`, 3000))) return fail('the New session dialog did not open');
        await p.eval(`(() => { const q = document.querySelector('#launch-form [name=query]'); q.value = 'a draft to keep'; q.dispatchEvent(new Event('input', { bubbles: true })); })()`);
        const btn = await p.eval(`(() => { const b = document.getElementById('launch-more'); const r = b ? b.getBoundingClientRect() : { width: 0 }; return b ? { tag: b.tagName, text: b.textContent.trim(), tab: b.tabIndex, shown: r.width > 0, expanded: b.getAttribute('aria-expanded') } : null; })()`);
        if (!btn || btn.tag !== 'BUTTON' || btn.text !== 'More options' || btn.tab !== 0 || !btn.shown || btn.expanded !== 'false') return fail(`More options is not a visible closed Tab-stop button: ${JSON.stringify(btn)}`);
        await p.element('#launch-more', '(e) => e.click()', { step: 'press More options' });
        await sleep(200);
        const open = await p.eval(`(() => { const s = document.getElementById('launch-side'); const box = s && s.querySelector('input[type=checkbox]'); return {
          expanded: document.getElementById('launch-more').getAttribute('aria-expanded'), side: !!s, promptHidden: document.querySelector('#launch-form [name=query]').closest('label').hidden,
          checks: s ? [...s.querySelectorAll('input[type=checkbox]')].map((c) => c.parentElement.textContent.trim()) : [], tab: box ? box.tabIndex : -1,
          hint: s ? [...s.querySelectorAll('.pick-hint')].pop().textContent : '' }; })()`);
        if (open.expanded !== 'true' || !open.side || !open.promptHidden) return fail(`More options did not show the settings side: ${JSON.stringify(open)}`);
        if (!open.checks.some((c) => /own git worktree/i.test(c)) || !open.checks.some((c) => /manager/i.test(c))) return fail(`the side has no worktree and manager boxes: ${JSON.stringify(open.checks)}`);
        if (open.tab < 0) return fail('the worktree box is not a Tab stop');
        if (!/Tick a box or press a button/.test(open.hint)) return fail(`the hint does not name the buttons with the Vim keys off: "${open.hint}"`);
        await p.element('#launch-side input[type=checkbox]', '(e) => e.click()', { step: 'tick the worktree box' });
        await sleep(200);
        const summary = await p.eval(`document.getElementById('launch-summary').textContent`);
        if (!/Own git worktree: yes/.test(summary)) return fail(`ticking the worktree box left the summary at: "${summary}"`);
        // Esc inside the side returns to the prompt, not out of the dialog; the draft is kept.
        await p.eval(`document.querySelector('#launch-side input[type=checkbox]').focus()`);
        await p.key('Escape'); await sleep(200);
        const back = await p.eval(`({ open: !!document.querySelector('#launch-dialog[open]'), side: !!document.getElementById('launch-side'), expanded: document.getElementById('launch-more').getAttribute('aria-expanded'), draft: document.querySelector('#launch-form [name=query]').value })`);
        if (!back.open || back.side || back.expanded !== 'false' || back.draft !== 'a draft to keep') return fail(`Esc in the side did not return to the prompt with the draft: ${JSON.stringify(back)}`);
        // Ctrl-O (the key route) still opens and closes it.
        await p.eval(`document.querySelector('#launch-form [name=query]').focus()`);
        await p.key('o', { ctrl: true }); await sleep(200);
        const viaKey = await p.eval(`document.getElementById('launch-more').getAttribute('aria-expanded')`);
        if (viaKey !== 'true') return fail(`Ctrl-O did not open the settings side (aria-expanded ${viaKey})`);
        await p.element('#launch-more', '(e) => e.click()', { step: 'press More options again' });
        await sleep(200);
        const closed = await p.eval(`({ side: !!document.getElementById('launch-side'), expanded: document.getElementById('launch-more').getAttribute('aria-expanded') })`);
        if (closed.side || closed.expanded !== 'false') return fail(`a second press did not close it: ${JSON.stringify(closed)}`);
        // Leave the dialog as found: untick the worktree box (the choice is kept while the side is closed).
        await p.element('#launch-more', '(e) => e.click()', { step: 'reopen to untick' });
        await sleep(200);
        await p.element('#launch-side input[type=checkbox]', '(e) => e.click()', { step: 'untick the worktree box' });
        await sleep(200);
        const unticked = await p.eval(`document.getElementById('launch-summary').textContent`);
        if (!/Own git worktree: no/.test(unticked)) return fail(`unticking the worktree box left the summary at: "${unticked}"`);
        await p.element('#launch-more', '(e) => e.click()', { step: 'close the side' });
        return pass('More options is a visible Tab-stop button; pressing it showed the worktree and manager boxes, the worktree box made the summary say yes, Esc and a second press returned to the prompt with the draft, and Ctrl-O still opens it');
      },
      async after(p) {
        await p.eval(`(() => { const q = document.querySelector('#launch-form [name=query]'); q.value = ''; const s = document.getElementById('launch-side'); const b = s && s.querySelector('input[type=checkbox]'); if (b && b.checked) b.click(); const c = document.getElementById('vim-keys'); if (!c.checked) { c.checked = true; c.dispatchEvent(new Event('change')); } })()`);
        await closeAll(p);
      },
    },
  ];
}

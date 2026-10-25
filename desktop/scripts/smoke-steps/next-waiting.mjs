// Local #312: after an answer the pane offers the next session that waits ("Next waiting (N)"); taking it opens that session
// with its question's first option focused. In a real browser (its focus fixup decides where focus is), Vim keys off and on.
// The fake rsid has one waiting session, so the step adds a second to the list; the write itself is refused by the bridge, so
// the answer is simulated the way focus-after-actions does it: offer, then redraw the pane with the question gone.
export default function steps({ pass, fail, sleep, closeAll }) {
  return [
    {
      id: 'next-waiting', group: 'keyboard',
      title: 'after an answer the pane offers "Next waiting (1)" with focus on it; Enter opens the other waiting session at its first option (Vim keys off and on)',
      async run(p) {
        await closeAll(p);
        const setVim = (on) => p.eval(`import('./vimkeys.js').then((m) => { m.setVimKeys(${on}); return true; })`);
        const draw = (fn) => p.eval(`import('./approvals.js').then((a) => import('./state.js').then((st) => import('./sessions.js').then((ss) => { ${fn}; return true; })))`);
        const notes = [];
        for (const vim of [false, true]) {
          const mode = vim ? 'Vim keys on' : 'Vim keys off';
          await setVim(vim); await sleep(150);
          const opened = await p.eval(`(() => { const li = [...document.querySelectorAll('#session-list li')].find((l) => l.innerText.includes('Fake waiting worker')); if (!li) return false; li.click(); return true; })()`);
          if (!opened || !(await p.waitFor(`!!document.querySelector('.approval')`, 6000))) return fail(`${mode}: could not open "Fake waiting worker" with its question`);
          // A second waiting session in the list, the answer and the reads in one go: the session poll rewrites the list.
          await p.eval(`document.querySelector('.approval input, .approval textarea').focus()`);
          const bar = await p.eval(`Promise.all([import('./approvals.js'), import('./state.js'), import('./sessions.js')]).then(([a, st, ss]) => {
            const first = st.selected();
            st.state.sessions.push({ ...first, id: '00000000-0000-4000-8000-000000000312', title: 'Second waiting worker', parent_id: null });
            ss.renderSessionList();
            a.offerNextWaiting(first.id);
            a.renderApproval({ ...first, status: 'Running', pending_question: null });
            const g = document.getElementById('next-waiting-go');
            return { shown: !document.getElementById('next-waiting').hidden && !g.hidden, label: g.textContent, active: document.activeElement.id };
          })`);
          if (!bar.shown || bar.label !== 'Next waiting (1)') return fail(`${mode}: after the answer the pane shows ${JSON.stringify(bar)}, not "Next waiting (1)"`);
          if (bar.active !== 'next-waiting-go') return fail(`${mode}: after the answer focus is on #${bar.active}, not the Next waiting button`);
          await p.key('Enter');
          const after = await p.eval(`import('./state.js').then((st) => ({ sel: st.state.selectedId, tag: document.activeElement.tagName, id: document.activeElement.id }))`);
          if (after.sel !== '00000000-0000-4000-8000-000000000312') return fail(`${mode}: Enter on Next waiting opened ${after.sel}, not the other waiting session`);
          if (after.tag !== 'INPUT' && after.tag !== 'TEXTAREA') return fail(`${mode}: the question's first field is not focused (${JSON.stringify(after)})`);
          const at = after;
          notes.push(`${mode}: Next waiting (1) offered and focused; Enter opened the other session at ${at.tag.toLowerCase()}`);
          await draw(`st.state.sessions = st.state.sessions.filter((s) => s.id !== '00000000-0000-4000-8000-000000000312'); ss.renderSessionList()`);
        }
        await setVim(true);
        return pass(notes.join('; '));
      },
      after: closeAll,
    },
  ];
}

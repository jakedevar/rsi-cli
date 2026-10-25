// Local #297: keyboard focus is never left on the page after an action removes or redraws the focused control. In a real
// browser (its focus fixup is what drops focus to the body), with the Vim keys off and on: Copy ID survives a header redraw,
// and Submit answer / Let the agent decide hand focus to the message box when the question banner goes, never to the page.
export default function steps({ pass, fail, sleep, closeAll }) {
  return [
    {
      id: 'focus-after-actions', group: 'keyboard',
      title: 'focus stays on Copy ID across a header redraw, and moves to the message box when a question banner is answered (Vim keys off and on)',
      async run(p) {
        await closeAll(p);
        const active = () => p.eval(`(() => { const a = document.activeElement; return { tag: a ? a.tagName : '', id: a ? a.id : '', text: a && a.textContent ? a.textContent.trim().slice(0, 24) : '' }; })()`);
        const setVim = (on) => p.eval(`import('./vimkeys.js').then((m) => { m.setVimKeys(${on}); return true; })`);
        const draw = (fn) => p.eval(`import('./approvals.js').then((a) => import('./state.js').then((st) => import('./sessions.js').then((ss) => { ${fn}; return true; })))`);
        const open = async () => {
          const ok = await p.eval(`(() => { const li = [...document.querySelectorAll('#session-list li')].find((l) => l.innerText.includes('Fake waiting worker')); if (!li) return false; li.click(); return true; })()`);
          if (!ok) return false;
          return p.waitFor(`!!document.querySelector('.approval') && !!document.querySelector('#s-secondary button')`, 6000);
        };
        const notes = [];
        for (const vim of [false, true]) {
          const mode = vim ? 'Vim keys on' : 'Vim keys off';
          await setVim(vim); await sleep(150);
          if (!(await open())) return fail(`${mode}: could not open the "Fake waiting worker" session with its question`);
          // Copy ID: the header is rebuilt on every refresh; focus must follow the button.
          await p.eval(`[...document.querySelectorAll('#s-secondary button')].find((b) => b.textContent === 'Copy ID').focus()`);
          await draw(`ss.renderHeader()`); await sleep(100);
          let at = await active();
          if (at.text !== 'Copy ID') return fail(`${mode}: after a header redraw focus is on ${JSON.stringify(at)}, not Copy ID`);
          // Submit answer / Let the agent decide: the banner goes away (the answer went through) and focus moves on.
          for (const button of ['Submit answer', 'Let the agent decide']) {
            await p.eval(`[...document.querySelectorAll('.approval button')].find((b) => b.textContent === ${JSON.stringify(button)}).focus()`);
            at = await active();
            if (at.text !== button) return fail(`${mode}: could not focus ${button} (${JSON.stringify(at)})`);
            await draw(`a.renderApproval({ ...st.selected(), status: 'Running', pending_question: null })`); await sleep(100);
            at = await active();
            if (at.id !== 'message') return fail(`${mode}: after ${button} the banner closed and focus is on ${JSON.stringify(at)}, not the message box`);
            await draw(`a.renderApproval(st.selected())`); await sleep(100); // the banner comes back for the next round
            if (!(await p.eval(`!!document.querySelector('.approval')`))) return fail(`${mode}: the banner did not come back`);
          }
          notes.push(`${mode}: Copy ID keeps focus; the message box takes it after Submit answer and Let the agent decide`);
        }
        await setVim(true);
        return pass(notes.join('; '));
      },
      after: closeAll,
    },
  ];
}

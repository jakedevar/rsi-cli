// Local #307: finding text in the open conversation. A real page, a 1200-event conversation served in place of the open
// session's own (the page asks for it through __TAURI__.core.invoke, which the step wraps), so most of it sits behind
// "Show earlier". Ctrl-F (Cmd-F on macOS) with Vim keys off opens a find bar above the transcript, searches every event,
// reveals and scrolls to a match behind "Show earlier", counts "1 of 3", steps with Enter / Shift+Enter, leaves the
// session list unfiltered, keeps the rendered text intact, and gives focus back on Esc. With Vim keys on, `/` with the
// transcript keyed does the same and n / N carry on after Esc.
import { longConversation } from '../smoke-fake.mjs';

export default function steps({ pass, fail, sleep, closeAll }) {
  return [
    {
      id: 'conversation-find', group: 'core',
      title: 'Cmd/Ctrl-F opens a find bar for the open conversation: every event is searched (a match behind Show earlier is revealed), the count reads 1 of 3, Enter steps, the session list is never filtered, Esc restores focus; / and n do the same with Vim keys on',
      async run(p) {
        await closeAll(p);
        const setVim = (on) => p.eval(`import('./vimkeys.js').then((m) => { m.setVimKeys(${on}); return true; })`);
        await setVim(false);
        await sleep(150);
        const rowCount = () => p.eval(`document.querySelectorAll('#session-list li.session-item').length`);
        const filterText = () => p.eval(`document.getElementById('search').value`);
        const bar = () => p.eval(`(() => { const b = document.getElementById('find-bar'); return b && !b.hidden ? { count: document.getElementById('find-count').textContent, input: document.getElementById('find-input').value, focused: document.activeElement === document.getElementById('find-input'), marks: document.querySelectorAll('#events mark.find-hit').length, current: document.querySelectorAll('#events mark.find-hit.current').length } : null; })()`);
        const NEEDLE = 'needle-zeta';
        // The synthetic conversation: three events contain the needle, the first far behind the 400-event window.
        const events = longConversation('00000000-0000-4000-8000-000000000001', 1200);
        const put = (seq, content) => { const e = events[seq - 1]; e.event_type = 'Message'; e.role = 'Assistant'; e.content = content; };
        put(3, `An early note with ${NEEDLE} inside.`);
        put(1150, `A reply with **${NEEDLE} in bold** and more text.`);
        put(1190, `The last one: ${NEEDLE.toUpperCase()} again.`);
        await p.eval(`(() => {
          const events = ${JSON.stringify(events)};
          const core = globalThis.__TAURI__.core;
          const real = core.invoke;
          core.invoke = (cmd, args) => {
            if (cmd !== 'get_conversation') return real(cmd, args);
            const since = args && args.sinceSequence != null ? args.sinceSequence : -1;
            return Promise.resolve(events.filter((e) => e.sequence > since).map((e) => ({ ...e, session_id: args.sessionId })));
          };
          globalThis.__findRealInvoke = real;
          return true;
        })()`);
        // Open any session: its conversation is the synthetic one.
        await p.eval(`(() => { const li = [...document.querySelectorAll('#session-list li.session-item')].find((l) => /completed/i.test(l.querySelector('.label').textContent)); li.click(); return true; })()`);
        if (!(await p.waitFor(`document.querySelectorAll('#events .ev').length >= 400 && !!document.querySelector('#events button.more')`, 8000))) {
          return fail('the synthetic 1200-event conversation did not render with a Show earlier button');
        }
        const rowsBefore = await rowCount();
        const shown0 = await p.eval(`document.querySelectorAll('#events .ev').length`);
        if (await p.eval(`!!document.querySelector('#events .ev[data-seq="3"]')`)) return fail('event 3 should start behind Show earlier');

        // Cmd-F on macOS, Ctrl-F elsewhere, with Vim keys off.
        const mac = (await p.eval(`import('./vimkeys.js').then((m) => m.currentPlatform())`)) === 'mac';
        const chord = () => p.key('f', mac ? { meta: true } : { ctrl: true });
        await chord();
        if (!(await p.waitFor(`!!document.getElementById('find-bar') && !document.getElementById('find-bar').hidden`, 3000))) return fail('Cmd/Ctrl-F did not open the find bar with Vim keys off');
        await p.keys(NEEDLE);
        await sleep(300);
        let b = await bar();
        if (!b) return fail('the find bar closed while typing');
        if (b.count !== '1 of 3') return fail(`the count reads "${b.count}", expected "1 of 3" (the match behind Show earlier counts and comes first)`);
        // The match behind Show earlier was revealed and scrolled to.
        const shown1 = await p.eval(`document.querySelectorAll('#events .ev').length`);
        const probe = () => p.eval(`(() => { const n = document.querySelector('#events .ev[data-seq="3"]'); if (!n) return null; const r = n.getBoundingClientRect(); const pr = document.getElementById('events').getBoundingClientRect(); return { hit: n.classList.contains('search-hit'), mark: !!n.querySelector('mark.find-hit.current'), inView: r.top >= pr.top - 1 && r.bottom <= pr.bottom + 1 }; })()`);
        // Markdown fills in after the rebuild and moves things; the find keeps the match in view and re-marks it as it settles, so poll
        // until the match is revealed, marked and in view together (the node can be in view a frame before its highlight is back, local #608).
        const settled = (n) => n && n.hit && n.mark && n.inView;
        let first = await probe();
        for (let k = 0; k < 15 && first && !settled(first); k++) { await sleep(200); first = await probe(); }
        if (!first) return fail(`event 3 (behind Show earlier) was not revealed; rendered ${shown0} -> ${shown1}`);
        if (!first.hit || !first.mark) return fail(`event 3 is revealed but not marked as the current match: ${JSON.stringify(first)}`);
        if (!first.inView) return fail('event 3 was revealed but not scrolled into view');
        if (shown1 <= shown0) return fail(`the render window did not grow to include the match (${shown0} -> ${shown1})`);
        // The session list is untouched.
        if ((await filterText()) !== '' || (await rowCount()) !== rowsBefore) return fail(`the session list changed: filter "${await filterText()}", rows ${rowsBefore} -> ${await rowCount()}`);
        if (await p.eval(`/No sessions match/.test(document.getElementById('session-list').textContent)`)) return fail('the session list says No sessions match');
        // The bar sits above the transcript and never over the Send button.
        const geom = await p.eval(`(() => { const r = (s) => document.querySelector(s).getBoundingClientRect(); const bar = r('#find-bar'); const ev = r('#events'); const send = r('#composer button.primary'); return { barBottom: bar.bottom, eventsTop: ev.top, over: !(bar.right <= send.left || bar.left >= send.right || bar.bottom <= send.top || bar.top >= send.bottom) }; })()`);
        if (geom.barBottom > geom.eventsTop + 1 || geom.over) return fail(`the find bar overlaps the transcript or Send: ${JSON.stringify(geom)}`);

        // Enter steps (2 of 3 is the Markdown one), Shift+Enter steps back, the last wraps.
        const textBefore = await p.eval(`document.querySelector('#events .ev[data-seq="1150"]') ? document.querySelector('#events .ev[data-seq="1150"]').textContent : null`);
        await p.key('Enter'); await sleep(250);
        b = await bar();
        if (b.count !== '2 of 3') return fail(`Enter should reach "2 of 3", saw "${b.count}"`);
        const md = await p.eval(`(() => { const n = document.querySelector('#events .ev[data-seq="1150"]'); const m = n && n.querySelector('mark.find-hit.current'); return m ? { inParagraph: !!m.closest('.md p, .md'), text: m.textContent } : null; })()`);
        if (!md) return fail('the second match (a Markdown message) has no current highlight');
        await p.key('Enter', { shift: true }); await sleep(250);
        b = await bar();
        if (b.count !== '1 of 3') return fail(`Shift+Enter should return to "1 of 3", saw "${b.count}"`);
        await p.key('Enter'); await p.key('Enter'); await sleep(250);
        b = await bar();
        if (b.count !== '3 of 3') return fail(`two more Enters should reach "3 of 3", saw "${b.count}"`);
        await p.key('Enter'); await sleep(250);
        b = await bar();
        if (b.count !== '1 of 3') return fail(`Enter after the last match should wrap to "1 of 3", saw "${b.count}"`);

        // Esc: the bar goes, the highlights go, the rendered text is unchanged, focus goes back into the page's list.
        await p.key('Escape'); await sleep(250);
        if (await bar()) return fail('Esc did not close the find bar');
        const left = await p.eval(`({ marks: document.querySelectorAll('#events mark').length, text: document.querySelector('#events .ev[data-seq="1150"]') ? document.querySelector('#events .ev[data-seq="1150"]').textContent : null, active: document.activeElement ? (document.activeElement.closest('#session-list') ? 'list' : document.activeElement.id || document.activeElement.tagName) : '' })`);
        if (left.marks !== 0) return fail(`${left.marks} highlights remain after Esc`);
        if (left.text !== null && textBefore !== null && left.text !== textBefore) return fail('the Markdown message text changed after highlighting and clearing');
        if (left.active === 'BODY' || left.active === 'find-input') return fail(`focus was not restored on Esc: ${left.active}`);

        // Vim keys on: Ctrl-F stays the transcript's page-down (no find bar), / with the keys in the transcript opens it, n carries on after Esc.
        await setVim(true);
        await sleep(150);
        // (On macOS Cmd-F is find with Vim keys on too, so the page-down check is for the other platforms.)
        if (!mac) { await chord(); await sleep(200); }
        if (await bar()) return fail('Ctrl-F opened the find bar with Vim keys on (it is the transcript page-down there)');
        await p.eval(`import('./actions/detail.js').then((m) => { m.setDetailFocus('transcript'); return true; })`);
        await p.key('/'); await sleep(250);
        if (!(await bar())) return fail('/ with the keys in the transcript did not open the find bar');
        if ((await p.eval(`document.activeElement === document.getElementById('search')`))) return fail('/ with the keys in the transcript focused the session filter instead');
        await p.keys(NEEDLE); await sleep(300);
        await p.key('Escape'); await sleep(250);
        const keyed = await p.eval(`import('./actions/detail.js').then((m) => m.detailSnapshot().focus)`);
        if (keyed !== 'transcript') return fail(`the keys did not go back to the transcript after Esc (focus: ${keyed})`);
        await p.key('n'); await sleep(300);
        b = await bar();
        if (!b || b.count !== '1 of 3') return fail(`n after Esc should reopen the find at "1 of 3", saw ${JSON.stringify(b)}`);
        await p.key('n'); await sleep(250);
        b = await bar();
        if (b.count !== '2 of 3') return fail(`a second n should reach "2 of 3", saw "${b.count}"`);
        if ((await filterText()) !== '' || (await rowCount()) !== rowsBefore) return fail('n / N changed the session list');
        await p.eval(`globalThis.__TAURI__.core.invoke = globalThis.__findRealInvoke; true`);
        return pass(`1200 events, ${shown0} rendered: Ctrl-F found 3 (the first behind Show earlier, window ${shown0} -> ${shown1}), 1 of 3 / 2 of 3 / 3 of 3 and wrapping, ${rowsBefore} list rows and an empty filter throughout, marks cleared and text intact on Esc; Vim keys on: / opens it from the transcript and n carries on`);
      },
      async after(p) {
        await p.eval(`(() => { if (globalThis.__findRealInvoke) globalThis.__TAURI__.core.invoke = globalThis.__findRealInvoke; return true; })()`).catch(() => {});
        await closeAll(p);
      },
    },
  ];
}

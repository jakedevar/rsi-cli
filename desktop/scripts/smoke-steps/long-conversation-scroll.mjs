// Local #306: a long conversation keeps the reader's place. In a real browser (layout decides every number here) with a
// 1,200-event conversation and a 400-event window: "Show earlier" leaves the event at the top of the viewport where it was,
// new events never move a reader who scrolled up (a "Jump to latest (N new)" button counts them, reachable by mouse and
// keyboard, gone at the bottom), a reader at the bottom follows the tail while the window trims from the top, and
// Home / End jump to the first loaded / newest event with the Vim keys off. The numbers go in the step's note.
export default function steps({ pass, fail, sleep, closeAll }) {
  return [
    {
      id: 'long-conversation-scroll', group: 'keyboard',
      title: 'Show earlier and new events keep the reading position in a 1,200-event conversation; Jump to latest and Home / End work',
      async run(p) {
        await closeAll(p);
        const mod = (name) => `import('./${name}.js')`;
        const run = (body) => p.eval(`(async () => { const st = await ${mod('state')}; const cv = await ${mod('conversation')}; const dt = await ${mod('actions/detail')}; const vk = await ${mod('vimkeys')}; const pane = document.getElementById('events'); ${body} })()`);
        const opened = await p.eval(`(() => { const li = document.querySelector('#session-list li'); if (!li) return false; li.click(); return true; })()`);
        if (!opened) return fail('no session row to open');
        if (!(await p.waitFor(`!!document.querySelector('#events .ev')`, 6000))) return fail('the session did not render any event');
        await run(`vk.setVimKeys(false);`);
        // The app polls the open session's conversation, and the fake daemon answers with its own few events, which count as a rewrite of the
        // synthetic ones below and rebuild the whole transcript (new nodes, trimmed from a different place) whenever a poll lands mid-step: an
        // intermittent failure that only shows under load (local #608). The step feeds the page its events directly, so a poll gets nothing new,
        // and the step waits for a poll already in flight to land before it loads them.
        await p.eval(`(() => { const core = globalThis.__TAURI__.core; const real = core.invoke; globalThis.__scrollRealInvoke = real; core.invoke = (cmd, args) => (cmd === 'get_conversation' ? Promise.resolve([]) : real(cmd, args)); return true; })()`);
        if (!(await p.waitFor(`import('./state.js').then((m) => !m.state.eventsInFlight)`, 6000))) return fail('a conversation poll was still in flight after the page stopped being fed events');
        // 1,200 collapsed tool events: tall enough to scroll, cheap enough to measure.
        const load = (n) => run(`
          const id = st.selected().id;
          st.state.events = Array.from({ length: ${n} }, (_, k) => ({ id: k + 1, session_id: id, sequence: k + 1, created_at: '2026-10-20T10:00:00.000000000Z',
            event_type: k % 2 ? 'ToolResult' : 'ToolUse', role: k % 2 ? 'Tool' : 'Assistant', tool_name: 'Bash', content: 'event ' + (k + 1) + ' ' + 'x'.repeat(40) }));
          st.state.renderLimit = 400; st.state.renderedCount = 0; st.state.renderedFirstSeq = undefined; cv.resetScroll(); cv.renderEvents();
          return pane.querySelectorAll('.ev').length;`);
        const nodes = await load(1200);
        if (nodes !== 400) return fail(`opening paints ${nodes} events, not the 400-event window`);
        // What the reader looks at: the first event whose bottom edge is below the pane top, and where its top edge is.
        const look = `(() => { const pane = document.getElementById('events'); const t = pane.getBoundingClientRect().top; for (const n of pane.querySelectorAll('.ev')) { const r = n.getBoundingClientRect(); if (r.bottom > t) return { seq: Number(n.dataset.seq), y: Math.round(r.top - t) }; } return null; })()`;
        const state = () => run(`const t = pane.getBoundingClientRect().top; let seq = null, y = null; for (const n of pane.querySelectorAll('.ev')) { const r = n.getBoundingClientRect(); if (r.bottom > t) { seq = Number(n.dataset.seq); y = Math.round(r.top - t); break; } }
          const j = document.getElementById('jump-latest'); const m = pane.querySelector('.more');
          return { seq, y, top: Math.round(pane.scrollTop), max: pane.scrollHeight - pane.clientHeight, nodes: pane.querySelectorAll('.ev').length, jump: j.hidden ? null : j.textContent, more: m ? m.textContent : null, unseen: st.state.unseenEvents };`);
        const push = (n) => run(`const id = st.selected().id; const base = st.state.events.length;
          st.state.events = st.state.events.concat(Array.from({ length: ${n} }, (_, k) => ({ id: base + k + 1, session_id: id, sequence: base + k + 1, created_at: '2026-10-20T10:00:01.000000000Z', event_type: 'ToolUse', role: 'Assistant', tool_name: 'Bash', content: 'new ' + (base + k + 1) })));
          cv.renderEvents(); return true;`);
        const notes = [];
        let s = await state();
        if (s.max - s.top > 2 || s.jump !== null) return fail(`a freshly opened conversation is not at the bottom with no button: ${JSON.stringify(s)}`);
        if (s.more !== 'Show earlier (800 hidden)') return fail(`unexpected Show earlier label: ${s.more}`);
        notes.push(`opened: ${s.nodes} nodes, bottom, "${s.more}"`);

        // 1. Show earlier at the very top keeps the event under the top edge in place.
        await run(`pane.scrollTop = 0;`); await sleep(100);
        const before = await p.eval(look);
        await p.click(`document.querySelector('#events .more')`);
        await sleep(200);
        s = await state();
        // Older events now sit above it, so the first event touching the viewport may be an earlier one: follow the same event.
        const yOf = (seq) => p.eval(`(() => { const pane = document.getElementById('events'); const n = pane.querySelector('.ev[data-seq="${seq}"]'); return n ? Math.round(n.getBoundingClientRect().top - pane.getBoundingClientRect().top) : null; })()`);
        const after = await yOf(before.seq);
        if (after === null || Math.abs(after - before.y) > 2) return fail(`Show earlier moved the reader: event ${before.seq} was ${before.y}px below the top edge and is now ${after}px (scrollTop ${s.top})`);
        if (s.nodes !== 800 || s.more !== 'Show earlier (400 hidden)') return fail(`after Show earlier ${s.nodes} nodes, "${s.more}"`);
        if (s.top < 1000) return fail(`Show earlier left the pane at the top (scrollTop ${s.top}) instead of at the same event`);
        notes.push(`Show earlier: event ${before.seq} stays at ${before.y}px (now ${after}px; scrollTop 0 -> ${s.top}), 800 nodes, "${s.more}"`);

        // 2. New events while scrolled up: position fixed, button counts them.
        await run(`pane.scrollTop = Math.round((pane.scrollHeight - pane.clientHeight) / 2);`); await sleep(100);
        const mid = await p.eval(look);
        const midTop = (await state()).top;
        await push(3);
        s = await state();
        if (s.seq !== mid.seq || Math.abs(s.y - mid.y) > 2 || Math.abs(s.top - midTop) > 2) return fail(`3 new events moved a reader in the middle: ${JSON.stringify(mid)} scrollTop ${midTop} -> ${JSON.stringify(s)}`);
        if (s.jump !== 'Jump to latest (3 new)') return fail(`button after 3 new events: ${JSON.stringify(s.jump)}`);
        await push(2);
        s = await state();
        if (s.jump !== 'Jump to latest (5 new)' || s.seq !== mid.seq) return fail(`button after 5 new events: ${JSON.stringify(s.jump)}, event ${s.seq}`);
        notes.push(`mid-conversation (scrollTop ${midTop}): 5 new events leave event ${mid.seq} at ${mid.y}px; button "${s.jump}"`);

        // 3. The button is a real, tabbable control: keyboard reaches it with the Vim keys off; Enter jumps and hides it.
        await run(`document.getElementById('jump-latest').focus();`);
        if (!(await p.eval(`document.activeElement && document.activeElement.id === 'jump-latest'`))) return fail('the Jump to latest button cannot take focus');
        await p.key('Enter'); await sleep(150);
        s = await state();
        if (s.max - s.top > 2 || s.jump !== null || s.unseen !== 0) return fail(`Enter on Jump to latest left ${JSON.stringify(s)}`);
        if (!(await p.eval(`document.activeElement && document.activeElement.id === 'events'`))) return fail('after Jump to latest focus did not return to the conversation');
        notes.push('Enter on the button: bottom, button hidden, focus back on the conversation');

        // 4. At the bottom the pane follows the tail; the window trims from the top without rebuilding.
        await push(1); s = await state();
        if (s.max - s.top > 2 || s.jump !== null) return fail(`a reader at the bottom lost the tail: ${JSON.stringify(s)}`);
        const stamp = await run(`const n = pane.querySelector('.ev[data-seq="900"]'); n.dataset.mark = 'kept'; return true;`);
        await push(450); s = await state(); // 800 + 456 > 400 + 400: trims to 400
        const kept = await run(`return pane.querySelector('.ev[data-seq="900"]') ? pane.querySelector('.ev[data-seq="900"]').dataset.mark : 'gone';`);
        if (s.max - s.top > 2 || s.jump !== null || s.nodes > 800) return fail(`tail follow with trim: ${JSON.stringify(s)}`);
        if (kept !== 'kept' && kept !== 'gone') return fail(`unexpected mark state ${kept}: ${JSON.stringify(await run(`const evs = [...pane.querySelectorAll('.ev')]; return { first: evs[0] && evs[0].dataset.seq, last: evs.at(-1) && evs.at(-1).dataset.seq, n: evs.length, stamped: evs.filter((e) => e.dataset.mark).length, total: st.state.events.length, first1: st.state.renderedFirstSeq, count: st.state.renderedCount, sel: st.selected().id };`))}`);
        notes.push(`at the bottom: 451 new events follow the tail, ${s.nodes} nodes after the trim, event 900 ${kept === 'kept' ? 'node untouched (no rebuild)' : 'trimmed from the top'}`);

        // 5. A trim never moves a reader who is scrolled up: it stops 50 events above the event they are looking at.
        await load(1200); // a fresh window: events 801..1200
        await run(`pane.scrollTop = 0;`); await sleep(50);
        await p.click(`document.querySelector('#events .more')`); await sleep(200); // 800..1200 + earlier => 400..1200 (800 nodes)
        await run(`pane.scrollTop = Math.round((pane.scrollHeight - pane.clientHeight) * 0.9);`); await sleep(100);
        const near = await p.eval(look); const nearTop = (await state()).top;
        await push(30); // 830 events rendered: no trim yet (limit 800 + slack)
        await push(400); // 1230 rendered > 800 + 400: the oldest are trimmed, the reader's event stays
        s = await state();
        if (s.seq !== near.seq || Math.abs(s.y - near.y) > 2) return fail(`trimming moved the reader: ${JSON.stringify(near)} -> ${JSON.stringify(s)}`);
        if (s.jump !== 'Jump to latest (430 new)') return fail(`unseen after 430 pushes while scrolled up: ${s.jump}`);
        notes.push(`scrolled up near the tail: 430 new events and a trim leave event ${near.seq} at ${near.y}px, button "${s.jump}", ${s.nodes} nodes`);

        // 6. Home / End with the transcript holding the keys and the Vim keys off.
        await run(`dt.setDetailFocus('transcript'); document.activeElement && document.activeElement.blur && document.activeElement.blur();`);
        await p.key('Home'); await sleep(150);
        s = await state();
        const first = await run(`return Number(pane.querySelector('.ev').dataset.seq);`);
        if (s.top > 2 || s.seq !== first) return fail(`Home did not reach the first loaded event: ${JSON.stringify(s)}`);
        await p.key('End'); await sleep(150);
        s = await state();
        if (s.max - s.top > 2 || s.jump !== null) return fail(`End did not reach the newest event and clear the button: ${JSON.stringify(s)}`);
        notes.push(`Home -> first loaded event ${first}, End -> bottom and button cleared (Vim keys off)`);
        await run(`vk.setVimKeys(true);`);
        return pass(notes.join('; '));
      },
      async after(p) {
        await p.eval(`(() => { if (globalThis.__scrollRealInvoke) globalThis.__TAURI__.core.invoke = globalThis.__scrollRealInvoke; return true; })()`).catch(() => {});
        await closeAll(p);
      },
    },
  ];
}

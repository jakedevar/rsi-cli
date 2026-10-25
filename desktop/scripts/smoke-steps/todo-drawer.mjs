// Local #291: the operator to-do panel is a 320 px column on a wide window and a drawer over the session pane below 1200 px, so the
// conversation keeps a readable width (45ch or more) at 760x480 and 1024x640. Esc and Hide close the drawer and focus returns; with Vim keys off Tab
// reaches Caught up and Hide; a narrow launch starts with the drawer closed even when it was left open, a wide one reopens it.
export default function steps({ pass, fail, sleep, closeAll }) {
  return [
    {
      id: 'todo-drawer', group: 'core',
      title: 'with the to-do panel open the conversation keeps 45ch or more at 760x480 and 1024x640 (the panel is a drawer that Esc and Hide close, focus returns) and the 1280 px column is unchanged',
      async run(p) {
        const resize = (w, h) => p.send('Emulation.setDeviceMetricsOverride', { width: w, height: h, deviceScaleFactor: 1, mobile: false });
        const panelOpen = () => p.eval(`!document.getElementById('todo-panel').hidden`);
        const setPanel = async (open) => {
          if ((await panelOpen()) === open) return;
          await p.eval(`import('./todo.js').then((m) => { m.runTodo('todo-toggle', { source: 'key' }); return true; })`);
          await sleep(250);
        };
        const measure = () => p.eval(`(() => {
          const rect = (s) => { const e = document.querySelector(s); const b = e.getBoundingClientRect(); return { x: b.x, w: b.width, r: b.right }; };
          const probe = document.createElement('div'); probe.style.cssText = 'width:45ch;position:absolute;visibility:hidden'; document.getElementById('events').append(probe);
          const ch45 = probe.getBoundingClientRect().width; probe.remove();
          const panel = document.getElementById('todo-panel'), pr = panel.getBoundingClientRect();
          const btn = (t) => { const b = [...panel.querySelectorAll('button')].find((x) => x.textContent === t); if (!b) return null; const r = b.getBoundingClientRect(); const at = document.elementFromPoint(r.x + r.width / 2, r.y + r.height / 2); return { inView: r.right <= innerWidth && r.left >= 0, uncovered: b === at || b.contains(at), tab: b.tabIndex >= 0 }; };
          const heads = [...document.querySelectorAll('.head-actions .btn')].map((b) => b.getBoundingClientRect().height);
          return { events: rect('#events'), detail: rect('.detail'), ch45, position: getComputedStyle(panel).position, panel: { x: pr.x, w: pr.width, r: pr.right }, ack: btn('Caught up'), hide: btn('Hide'), headMax: Math.max(...heads), docW: document.documentElement.scrollWidth, vw: innerWidth };
        })()`);
        await closeAll(p);
        if (await p.eval(`document.getElementById('terminal-overlay')?.hidden === false`)) { await p.click(`document.querySelector('#terminal-overlay button')`); await sleep(300); }
        await p.click(`document.querySelector('#session-list .session-item')`);
        await sleep(400);
        const notes = [];
        for (const [w, h] of [[760, 480], [1024, 640]]) {
          await resize(w, h); await sleep(300);
          await setPanel(false);
          const closed = await measure();
          await p.eval(`document.getElementById('message').focus()`);
          await setPanel(true);
          const m = await measure();
          const where = `${w}x${h}`;
          if (m.position !== 'absolute') return fail(`${where}: the to-do panel is ${m.position}, not a drawer over the pane`);
          if (Math.abs(m.detail.w - closed.detail.w) > 1) return fail(`${where}: the conversation pane is ${Math.round(m.detail.w)} px with the panel open, ${Math.round(closed.detail.w)} px without`);
          if (m.events.w < m.ch45) return fail(`${where}: the conversation is ${Math.round(m.events.w)} px, under 45ch (${Math.round(m.ch45)} px)`);
          if (m.headMax > 32) return fail(`${where}: a header button is ${Math.round(m.headMax)} px tall: its label wrapped`);
          if (m.docW > m.vw) return fail(`${where}: the page scrolls sideways (${m.docW} px)`);
          for (const [name, b] of [['Caught up', m.ack], ['Hide', m.hide]]) {
            if (!b || !b.inView || !b.uncovered || !b.tab) return fail(`${where}: ${name} is not reachable (${JSON.stringify(b)})`);
          }
          if (!(await p.eval(`document.getElementById('todo-panel').contains(document.activeElement)`))) return fail(`${where}: opening the drawer did not move focus into it`);
          // Vim keys off: real Tab presses walk the open drawer and reach Caught up and Hide; Esc on Hide closes it.
          await p.eval(`import('./vimkeys.js').then((m) => { m.setVimKeys(false); return true; })`);
          const reached = [];
          for (let i = 0; i < 16 && !(reached.includes('Caught up') && reached.includes('Hide')); i++) {
            await p.key('Tab'); await sleep(80);
            const at = await p.eval(`(() => { const a = document.activeElement; return { text: a && a.textContent ? a.textContent.trim().slice(0, 20) : '', inside: !!a && document.getElementById('todo-panel').contains(a) }; })()`);
            if (!at.inside) return fail(`${where}: Tab left the open drawer after ${JSON.stringify(reached)} (focus is on "${at.text}")`);
            reached.push(at.text);
          }
          if (!reached.includes('Caught up') || !reached.includes('Hide')) return fail(`${where}: Tab reached ${JSON.stringify(reached)}, not both Caught up and Hide`);
          await p.eval(`import('./vimkeys.js').then((m) => { m.setVimKeys(true); return true; })`);
          await p.key('Escape'); await sleep(250);
          if (await panelOpen()) return fail(`${where}: Esc did not close the drawer`);
          if (!(await p.eval(`document.activeElement === document.getElementById('message')`))) return fail(`${where}: focus did not return to the message box (it is on ${await p.eval(`document.activeElement?.id || document.activeElement?.tagName`)})`);
          await setPanel(true);
          await p.click(`[...document.querySelectorAll('#todo-panel button')].find((b) => b.textContent === 'Hide')`);
          await sleep(250);
          if (await panelOpen()) return fail(`${where}: Hide did not close the drawer`);
          notes.push(`${where}: conversation ${Math.round(m.events.w)} px (${Math.round(m.events.w / (m.ch45 / 45))}ch) beside a ${Math.round(m.panel.w)} px drawer`);
        }
        await resize(1280, 800); await sleep(300);
        await setPanel(true);
        const wide = await measure();
        if (wide.position === 'absolute') return fail('1280x800: the to-do panel is a drawer, not a column');
        if (wide.events.w < 480) return fail(`1280x800: the conversation is ${Math.round(wide.events.w)} px with the column open`);
        if (wide.panel.w < 300) return fail(`1280x800: the column is ${Math.round(wide.panel.w)} px wide`);
        await p.key('Escape'); await sleep(200);
        if (!(await panelOpen())) return fail('1280x800: Esc closed the column (only a drawer closes on Esc)');
        notes.push(`1280x800: column ${Math.round(wide.panel.w)} px, conversation ${Math.round(wide.events.w)} px`);
        await setPanel(false);
        // A narrow launch starts with the drawer closed even if it was left open; a wide launch opens it again (the choice is kept).
        const url = await p.eval(`location.href`);
        const launch = async (w, h) => {
          await resize(w, h);
          await p.goto(url);
          await p.ready(`document.querySelectorAll('#session-list .session-item').length > 0 && !!document.getElementById('todo-panel')`, { step: `reload at ${w}x${h}` });
          await sleep(300);
          return panelOpen();
        };
        await p.eval(`localStorage.setItem('rsi-desktop.todo', JSON.stringify({ open: true, lastVisit: null }))`);
        if (await launch(760, 480)) return fail('760x480: the drawer was open at launch although the window is narrow');
        if (await launch(1280, 800) === false) return fail('1280x800: the column did not open at launch although it was left open');
        notes.push('a narrow launch starts with the drawer closed, a wide one reopens the remembered column');
        await setPanel(false);
        return pass(notes.join('; '));
      },
      async after(p) {
        await p.send('Emulation.clearDeviceMetricsOverride');
        await p.eval(`import('./vimkeys.js').then((m) => { m.setVimKeys(true); return true; })`);
        if (await p.eval(`!document.getElementById('todo-panel').hidden`)) await p.eval(`import('./todo.js').then((m) => { m.runTodo('todo-toggle', { source: 'key' }); return true; })`);
      },
    },
  ];
}

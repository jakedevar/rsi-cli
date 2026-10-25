// Local #308 / #309: Archive on a Running session cannot archive (rsid refuses ArchiveSession for an active session), so it
// marks the session to archive when it finishes, as the TUI's <Space>a does. In a real browser: the button says so, the question
// says "archives itself when this turn finishes", the write goes out as mark_pending_archive (never archive_session; the read-only
// bridge refuses and logs it, the page says so and keeps the button), and a session marked pending shows the marker in the header
// and on its row with a Cancel button. A session that left the Active list as archived is drawn read-only with Restore.
export default function steps({ pass, fail, sleep, closeAll, bridgeLog }) {
  const writes = (cmd) => bridgeLog.filter((e) => e.cmd === cmd).length;
  return [
    {
      id: 'archive-when-finished', group: 'core', refuses: ['mark_pending_archive'],
      title: 'Archive on a Running session reads "Archive when finished", asks, sends mark_pending_archive, and a pending session shows the marker and a Cancel button; a departed session is read-only with Restore',
      async run(p) {
        await closeAll(p);
        const opened = await p.eval(`(() => { const li = [...document.querySelectorAll('#session-list li')].find((l) => l.innerText.includes('Fake worker')); if (!li) return false; li.click(); return true; })()`);
        if (!opened) return fail('could not open the "Fake worker" session');
        await sleep(400);
        const label = () => p.eval(`document.getElementById('archive').textContent.trim()`);
        const firstLabel = await label();
        if (firstLabel !== 'Archive when finished') return fail(`the Archive button on a Running session reads ${JSON.stringify(firstLabel)}, not "Archive when finished"`);
        const sentBefore = writes('mark_pending_archive');
        const archiveBefore = writes('archive_session');
        await p.eval(`document.getElementById('archive').click()`);
        if (!(await p.waitFor(`!!document.querySelector('dialog[open]')`, 4000))) return fail('Archive when finished asked nothing');
        const ask = await p.eval(`document.querySelector('dialog[open]').innerText.replace(/\\s+/g, ' ')`);
        if (!/archives itself when this turn finishes/.test(ask)) return fail(`the question does not say what happens: ${ask}`);
        await p.eval(`[...document.querySelectorAll('dialog[open] button')].find((b) => b.type === 'submit').click()`);
        await sleep(600);
        if (writes('mark_pending_archive') !== sentBefore + 1) return fail('confirming sent no mark_pending_archive');
        if (writes('archive_session') !== archiveBefore) return fail('a Running session was sent archive_session, which rsid refuses');
        const toast = await p.eval(`document.getElementById('toast').textContent`);
        if (!/Mark pending archive failed/.test(toast)) return fail(`the refused write was not reported: ${JSON.stringify(toast)}`);
        if ((await label()) !== 'Archive when finished') return fail('after the refusal the button changed');
        // The pending state, as rsid reports it on the next poll. Set, draw and read in one go, so a poll cannot land in between.
        const pending = await p.eval(`import('./state.js').then((st) => import('./sessions.js').then((ss) => {
          st.state.sessions = st.state.sessions.map((s) => (s.id === st.state.selectedId ? { ...s, pending_archive: true } : s));
          ss.renderSessionList(); ss.renderHeader();
          return { label: document.getElementById('archive').textContent.trim(),
            chip: document.querySelector('#s-meta .pending-archive-chip')?.textContent ?? null,
            row: document.querySelector('#session-list li.selected .sub')?.textContent ?? '',
            mark: document.querySelector('#session-list li.selected .mark-pending-archive')?.textContent ?? null }; }))`);
        if (pending.label !== 'Cancel auto-archive') return fail(`a pending session's button reads ${JSON.stringify(pending.label)}`);
        if (pending.chip !== 'Archives when finished') return fail(`the header shows no pending marker: ${JSON.stringify(pending.chip)}`);
        if (!/archives when finished/.test(pending.row) || !pending.mark) return fail(`the row shows no pending marker: ${JSON.stringify(pending)}`);
        // #309: the open session leaves the list as archived: read-only, Restore, no message box.
        const gone = await p.eval(`import('./state.js').then((st) => import('./sessions.js').then((ss) => {
          const s = st.state.sessions.find((x) => x.id === st.state.selectedId);
          st.state.departed = { ...s, status: 'Archived', pending_archive: false };
          st.state.sessions = st.state.sessions.filter((x) => x.id !== s.id);
          ss.renderSessionList(); ss.renderHeader();
          return { status: document.querySelector('#s-meta .status-chip')?.textContent ?? null,
            restore: !document.getElementById('restore').hidden, archive: !document.getElementById('archive').hidden,
            message: document.getElementById('message').disabled, stop: document.getElementById('interrupt').disabled }; }))`);
        if (gone.status !== 'Archived' || !gone.restore || gone.archive || !gone.message || !gone.stop) return fail(`a session that left the list is drawn wrong: ${JSON.stringify(gone)}`);
        return pass('Running session: "Archive when finished", asked, mark_pending_archive sent (refused, reported, button kept); pending shows header and row marker with "Cancel auto-archive"; a departed session reads Archived with Restore, no Archive, no message box, no Stop');
      },
      async after(p) {
        await p.eval(`import('./state.js').then((st) => { st.state.departed = null; return true; })`);
        await closeAll(p);
        // The step removed a session from the page's list and marked another; read the list again now, so the next poll
        // does not bring them back in the middle of a later step (conversation-find counts the rows).
        await p.eval(`import('./sessions.js').then((ss) => ss.refreshSessions()).then(() => true)`);
        await p.waitFor(`document.querySelectorAll('#session-list li.session-item').length >= 5 && !document.querySelector('#session-list .mark-pending-archive')`, 5000);
      },
    },
  ];
}

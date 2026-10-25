// Local #275: a filter keystroke keeps the session rows that stay (and the ones that return) as the same nodes instead of
// rebuilding the list, and the open session stays selected through the filter. Real page, fake daemon's five sessions.
export default function steps({ pass, fail, sleep, closeAll }) {
  return [
    {
      id: 'session-list-reuse', group: 'core',
      title: 'Filtering the session list keeps the same row nodes, the open session stays selected, and clearing the filter restores every row',
      async run(p) {
        await closeAll(p);
        const rows = `[...document.querySelectorAll('#session-list li.session-item')]`;
        const setFilter = (v) => p.eval(`(() => { const b = document.getElementById('search'); b.value = ${JSON.stringify(v)}; b.dispatchEvent(new Event('input', { bubbles: true })); })()`);
        await setFilter(''); await sleep(150);
        const all = await p.eval(`${rows}.length`);
        if (all < 4) return fail(`expected the fake daemon's sessions in the list, saw ${all} rows`);
        // Open the completed session (the only one whose prompt mentions csv; opening it re-marks its row), then tag every row node and filter.
        await p.eval(`${rows}.find((li) => /completed/i.test(li.querySelector('.label').textContent)).click()`);
        await sleep(400);
        await p.eval(`(() => { for (const li of ${rows}) li.__tag = li.dataset.id; })()`);
        await setFilter('csv'); await sleep(200);
        const filtered = await p.eval(`JSON.stringify(${rows}.map((li) => ({ tag: li.__tag ?? null, selected: li.classList.contains('selected'), current: li.getAttribute('aria-current') })))`);
        const f = JSON.parse(filtered);
        if (f.length !== 1) return fail(`the csv filter should leave one row, left ${f.length}: ${filtered}`);
        if (!f[0].tag) return fail('the kept row was rebuilt (a new node) instead of reused');
        if (!f[0].selected || f[0].current !== 'true') return fail(`the open session lost its selection under the filter: ${filtered}`);
        const active = await p.eval(`document.getElementById('session-list').getAttribute('aria-activedescendant')`);
        if (!active || !active.startsWith('session-row-')) return fail(`aria-activedescendant is "${active}" while the open session is listed`);
        await setFilter(''); await sleep(200);
        const back = JSON.parse(await p.eval(`JSON.stringify(${rows}.map((li) => ({ id: li.dataset.id, tag: li.__tag ?? null, selected: li.classList.contains('selected') })))`));
        if (back.length !== all) return fail(`clearing the filter left ${back.length} rows, expected ${all}`);
        if (back.some((r) => r.tag !== r.id)) return fail(`rows came back as new nodes: ${JSON.stringify(back)}`);
        if (back.filter((r) => r.selected).length !== 1) return fail(`expected exactly one selected row, saw ${JSON.stringify(back)}`);
        return pass(`${all} rows; a filter kept the one matching row's node and its selection; clearing it brought back the same ${all} nodes`);
      },
      after: closeAll,
    },
  ];
}

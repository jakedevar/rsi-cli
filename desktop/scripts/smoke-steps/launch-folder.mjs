// Local #271: the New session dialog opens with a project chosen and its folder shown, and Choose… beside Folder is a Tab stop
// (Vim keys off) that fills the box from the system folder chooser (the smoke bridge's choose_folder stub answers a fixed path).
export default function steps({ pass, fail, sleep, closeAll }) {
  return [
    {
      id: 'launch-folder', group: 'keyboard',
      title: 'New session opens on a project with its folder shown; Choose… beside Folder is reached by Tab and fills the box, which stays editable',
      async run(p) {
        await p.eval(`(() => { const c = document.getElementById('vim-keys'); c.checked = false; c.dispatchEvent(new Event('change')); })()`);
        await closeAll(p);
        await p.eval(`(() => { const f = document.getElementById('project-filter'); f.value = ''; f.dispatchEvent(new Event('change')); })()`);
        await p.element('#new-session', '(e) => e.click()', { step: 'open the launch dialog' });
        if (!(await p.waitFor(`!!document.querySelector('#launch-dialog[open]')`, 3000))) return fail('the New session dialog did not open');
        const opened = await p.eval(`(() => { const f = document.getElementById('launch-form');
          const s = f.elements.namedItem('project_id'); return { project: s.options[s.selectedIndex]?.textContent, folder: f.elements.namedItem('working_dir').value, options: [...s.options].map((o) => o.textContent) }; })()`);
        if (opened.project !== 'Fake project') return fail(`the Project box opened on "${opened.project}", not the only project: ${JSON.stringify(opened)}`);
        if (opened.folder !== '/tmp/fake-project') return fail(`the Folder box shows "${opened.folder}", not the project's folder`);
        if (!opened.options.includes('none')) return fail(`"none" is no longer a choice: ${JSON.stringify(opened.options)}`);
        const btn = await p.eval(`(() => { const b = document.getElementById('launch-folder-pick'); const r = b.getBoundingClientRect(); return { text: b.textContent.trim(), tab: b.tabIndex, shown: r.width > 0 }; })()`);
        if (btn.text !== 'Choose…' || btn.tab !== 0 || !btn.shown) return fail(`Choose… is not a visible Tab stop: ${JSON.stringify(btn)}`);
        await p.eval(`document.querySelector('#launch-form [name=working_dir]').focus()`);
        await p.key('Tab'); await sleep(150);
        if ((await p.eval(`document.activeElement && document.activeElement.id`)) !== 'launch-folder-pick') return fail('Tab from the Folder box did not reach Choose…');
        await p.key('Enter'); await sleep(300);
        const picked = await p.eval(`document.querySelector('#launch-form [name=working_dir]').value`);
        if (picked !== '/tmp/rsi-smoke-picked-folder') return fail(`Choose… left "${picked}" in the Folder box`);
        await p.eval(`(() => { const i = document.querySelector('#launch-form [name=working_dir]'); i.value += '/sub'; i.dispatchEvent(new Event('input', { bubbles: true })); })()`);
        const edited = await p.eval(`document.querySelector('#launch-form [name=working_dir]').value`);
        if (edited !== '/tmp/rsi-smoke-picked-folder/sub') return fail(`the picked path could not be edited: "${edited}"`);
        return pass('Fake project and its folder were preselected; Tab reached Choose…, it filled the Folder box, and the path stayed editable');
      },
      async after(p) {
        await p.eval(`(() => { const f = document.querySelector('#launch-form [name=working_dir]'); f.value = ''; f.dataset.auto = ''; const c = document.getElementById('vim-keys'); if (!c.checked) { c.checked = true; c.dispatchEvent(new Event('change')); } })()`);
        await closeAll(p);
      },
    },
  ];
}

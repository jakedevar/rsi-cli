// Local #302: the Issues workspace and the New Issue dialog at the allowed minimum window (760x480) and at 1024x640: the inspector's
// tab row (Blocked by, Blocks, Events, Handoffs) is on screen, and the dialog keeps its title and its Cancel / Create buttons in view
// with every field reachable. The every-view audit (npm run audit -- views --size 760x480) counts the same controls.
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const SIZES = [[760, 480], [1024, 640]];

export default function steps({ pass, fail, sleep, closeAll, command }) {
  return [
    {
      id: 'issues-small-window', group: 'core',
      title: 'at 760x480 and 1024x640 the Issues inspector tab row and the New Issue dialog (title, every field, Cancel and Create) are on screen; the dialog body scrolls, not the buttons',
      async run(p) {
        await closeAll(p);
        await p.eval(`(() => { const c = document.getElementById('vim-keys'); if (!c.checked) { c.checked = true; c.dispatchEvent(new Event('change')); } })()`);
        await command(p, 'issues');
        if (!(await p.waitFor(`!document.getElementById('issues-view').hidden && document.querySelectorAll('#issues-list li').length > 0 && !!document.querySelector('#issues-inspector .iv-tabs')`, 8000))) return fail('the Issues workspace did not show an inspector with its tab row');
        const notes = [];
        for (const [w, h] of SIZES) {
          await p.send('Emulation.setDeviceMetricsOverride', { width: w, height: h, deviceScaleFactor: 1, mobile: false });
          await sleep(400);
          const tabs = await p.eval(`(() => [...document.querySelectorAll('#issues-inspector .iv-tabs [role=tab]')].map((b) => { const r = b.getBoundingClientRect(); return { t: b.textContent, top: r.top, bottom: r.bottom, left: r.left, right: r.right }; }))()`);
          if (tabs.length !== 4) return fail(`${w}x${h}: ${tabs.length} inspector tabs, expected 4`);
          const off = tabs.filter((t) => t.bottom > h + 1 || t.top < 0 || t.right > w + 1 || t.left < -1);
          if (off.length) return fail(`${w}x${h}: inspector tabs off screen: ${off.map((t) => `${t.t} y ${Math.round(t.top)}..${Math.round(t.bottom)}`).join(', ')}`);
          await p.element('[data-action="issues-new"]', '(e) => e.click()', { step: 'press New Issue' });
          if (!(await p.waitFor(`document.getElementById('issue-dialog').open && !!document.querySelector('#issue-form .actions')`, 5000))) return fail(`${w}x${h}: the New Issue dialog did not open`);
          await sleep(300);
          const d = await p.eval(`(() => {
            const dlg = document.getElementById('issue-dialog').getBoundingClientRect();
            const box = (sel) => { const e = document.querySelector(sel); const r = e && e.getBoundingClientRect(); return r ? { top: r.top, bottom: r.bottom, left: r.left, right: r.right } : null; };
            const fields = [...document.querySelectorAll('#issue-form .form-fields input, #issue-form .form-fields textarea, #issue-form .form-fields select')].map((e) => { const r = e.getBoundingClientRect(); return { n: e.name, top: r.top, bottom: r.bottom }; });
            const body = document.querySelector('#issue-form .form-fields');
            const btns = [...document.querySelectorAll('#issue-form .actions button')].map((b) => { const r = b.getBoundingClientRect(); return { t: b.textContent, top: r.top, bottom: r.bottom, left: r.left, right: r.right }; });
            return { dlg: { top: dlg.top, bottom: dlg.bottom }, h2: box('#issue-form h2'), fields, btns, bodyScrolls: body.scrollHeight > body.clientHeight + 1, vh: innerHeight, vw: innerWidth };
          })()`);
          await p.shot(join(tmpdir(), `rsi-302-new-issue-${w}x${h}.png`));
          if (d.dlg.top < 0 || d.dlg.bottom > h) return fail(`${w}x${h}: the dialog runs off the window (${Math.round(d.dlg.top)}..${Math.round(d.dlg.bottom)})`);
          if (!d.h2 || d.h2.top < 0) return fail(`${w}x${h}: the dialog title is not on screen`);
          const names = d.btns.map((b) => b.t);
          if (!names.includes('Cancel') || !names.includes('Create')) return fail(`${w}x${h}: the dialog buttons are ${names.join(', ')}`);
          const hidden = d.btns.filter((b) => b.bottom > h + 1 || b.top < 0 || b.right > w + 1 || b.left < -1);
          if (hidden.length) return fail(`${w}x${h}: dialog buttons off screen: ${hidden.map((b) => b.t).join(', ')}`);
          if (d.fields.length < 5) return fail(`${w}x${h}: the form shows ${d.fields.length} fields`);
          const cut = d.fields.filter((f) => f.bottom > h + 1);
          if (cut.length && !d.bodyScrolls) return fail(`${w}x${h}: fields below the window but the body does not scroll: ${cut.map((f) => f.n).join(', ')}`);
          await p.element('#issue-form .actions .btn:not(.primary)', '(e) => e.click()', { step: 'press Cancel' });
          if (!(await p.waitFor(`!document.getElementById('issue-dialog').open`, 3000))) return fail(`${w}x${h}: Cancel did not close the dialog`);
          notes.push(`${w}x${h}: tabs y ${Math.round(Math.max(...tabs.map((t) => t.bottom)))} of ${h}, ${d.fields.length} fields${d.bodyScrolls ? ' (body scrolls)' : ''}, buttons y ${Math.round(Math.max(...d.btns.map((b) => b.bottom)))}`);
        }
        return pass(notes.join('; '));
      },
      async after(p) { await closeAll(p); await p.send('Emulation.clearDeviceMetricsOverride'); },
    },
  ];
}

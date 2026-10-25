// Local #310: a view that reads from rsid opens its frame at once ("Loading…", busy, in a dialog that fits 760x480), the frame
// keeps its size when the answer is a refusal (the message and Retry take the place of Loading…), Retry reads again, and an
// answer that comes after the user closed the frame draws nothing. Real browser, real Scheduled jobs view: the step holds
// the view's two reads in the page (the page's own invoke is wrapped), so the answer comes when the step says.
import { tmpdir } from 'node:os';
import { join } from 'node:path';

export default function steps({ pass, fail, sleep, closeAll }) {
  return [
    {
      id: 'view-load', group: 'core',
      title: 'Scheduled jobs shows Loading… in a frame at once while its reads are held; a refusal shows the message and Retry in the same frame; Retry then opens the view; closing early drops the late answer',
      async run(p) {
        await closeAll(p);
        await p.send('Emulation.setDeviceMetricsOverride', { width: 760, height: 480, deviceScaleFactor: 1, mobile: false });
        await sleep(300);
        // Hold list_scheduled_jobs / list_scheduled_job_holds until the step releases them with an answer or a refusal.
        await p.eval(`(() => {
          const core = window.__TAURI__.core;
          if (!core.__orig) core.__orig = core.invoke;
          window.__hold = { pending: [], calls: [] };
          core.invoke = (cmd, args) => {
            if (cmd !== 'list_scheduled_jobs' && cmd !== 'list_scheduled_job_holds') return core.__orig(cmd, args);
            window.__hold.calls.push(cmd);
            return new Promise((resolve, reject) => window.__hold.pending.push({ cmd, resolve, reject }));
          };
          return true;
        })()`);
        const release = (how) => p.eval(`(() => {
          const held = window.__hold.pending.splice(0);
          for (const h of held) {
            if (${JSON.stringify(how)} === 'refuse') h.reject(new Error('daemon error -32000: rsid is draining [draining]'));
            else if (h.cmd === 'list_scheduled_jobs') h.resolve({ jobs: [], next_cursor: null });
            else h.resolve([]);
          }
          return held.length;
        })()`);
        const look = () => p.eval(`(() => {
          const d = document.getElementById('view-load-dialog');
          const s = d?.querySelector('.view-load-status');
          const r = d?.querySelector('.view-load-box')?.getBoundingClientRect();
          const retry = [...(d?.querySelectorAll('button') ?? [])].find((b) => b.textContent.startsWith('Retry'));
          return { open: !!d?.open, text: s?.textContent ?? '', busy: s?.getAttribute('aria-busy'), role: s?.getAttribute('role'),
            retryShown: !!retry && !retry.hidden, active: document.activeElement?.textContent?.slice(0, 12) ?? '', w: r ? Math.round(r.width) : 0, h: r ? Math.round(r.height) : 0,
            bottom: r ? Math.round(r.bottom) : 0, right: r ? Math.round(r.right) : 0, vw: innerWidth, vh: innerHeight, schedules: !!document.getElementById('admin-schedules-dialog')?.open };
        })()`);
        // Choose the view from the Actions palette and stop at the Enter: the step times what follows.
        const choose = async () => {
          await p.element('#open-palette', '(e) => e.click()', { step: 'open Actions palette' });
          await p.element('#palette-input', undefined, { step: 'Actions palette accepts input' });
          await sleep(500);
          await p.keys('Scheduled jobs');
          await sleep(300);
          await p.key('Enter');
        };
        try {
          await choose();
          if (!(await p.waitFor(`!!document.getElementById('view-load-dialog')?.open`, 300))) return fail('choosing Scheduled jobs with its reads held drew no frame within 300 ms');
          const loading = await look();
          await p.shot(join(tmpdir(), 'rsi-310-loading-760x480.png'));
          if (loading.text !== 'Loading…' || loading.busy !== 'true' || loading.role !== 'status') return fail(`the frame is not a busy Loading… status: ${JSON.stringify(loading)}`);
          if (loading.bottom > loading.vh || loading.right > loading.vw) return fail(`the frame runs off a 760x480 window: ${JSON.stringify(loading)}`);
          if (loading.retryShown) return fail('Retry is offered while the view is still loading');
          if (loading.schedules) return fail('the view itself opened before its reads returned');
          const calls = await p.eval(`window.__hold.calls.slice()`);
          if (!(calls.includes('list_scheduled_jobs') && calls.includes('list_scheduled_job_holds'))) return fail(`the two reads did not start together: ${JSON.stringify(calls)}`);

          await release('refuse');
          if (!(await p.waitFor(`document.querySelector('#view-load-dialog .view-load-status')?.getAttribute('aria-busy') === 'false'`, 3000))) return fail('a refused read left Loading… on screen');
          const refused = await look();
          await p.shot(join(tmpdir(), 'rsi-310-refused-760x480.png'));
          if (!/Failed to load scheduled jobs/.test(refused.text) || !/draining|refused|could not/i.test(refused.text)) return fail(`the refusal is not shown with rsid's words: ${JSON.stringify(refused.text)}`);
          if (!refused.retryShown || !refused.active.startsWith('Retry')) return fail(`Retry is not offered and focused: ${JSON.stringify(refused)}`);
          if (refused.role !== 'alert') return fail(`the error is not announced as an alert (role ${refused.role})`);
          if (refused.w !== loading.w || refused.h !== loading.h) return fail(`the frame changed size from ${loading.w}x${loading.h} to ${refused.w}x${refused.h}`);
          if (refused.bottom > refused.vh) return fail(`the error frame runs off a 760x480 window (bottom ${refused.bottom})`);

          await p.key('Enter'); // Retry has focus
          if (!(await p.waitFor(`document.querySelector('#view-load-dialog .view-load-status')?.textContent === 'Loading…'`, 2000))) return fail('Retry did not bring Loading… back');
          await sleep(100);
          await release('ok');
          if (!(await p.waitFor(`!!document.getElementById('admin-schedules-dialog')?.open && !document.getElementById('view-load-dialog')?.open`, 4000))) return fail('after Retry and an answer the Scheduled jobs dialog did not replace the frame');

          await closeAll(p);
          // Early close: choose again, close the frame, then the answer comes late.
          await choose();
          if (!(await p.waitFor(`!!document.getElementById('view-load-dialog')?.open`, 300))) return fail('the second choice drew no frame');
          await p.key('Escape');
          await sleep(150);
          await release('ok');
          await sleep(500);
          const late = await look();
          if (late.open || late.schedules) return fail(`an answer after the user closed the frame still drew: ${JSON.stringify(late)}`);
          return pass(`760x480: Loading… (busy) at once with both reads started together; a refusal shows "${refused.text.slice(0, 60)}…" with Retry focused in a ${refused.w}x${refused.h} frame (was ${loading.w}x${loading.h}); Retry then opened the view; a late answer after Escape drew nothing`);
        } finally {
          await p.eval(`(() => { const c = window.__TAURI__.core; if (c.__orig) { c.invoke = c.__orig; } for (const h of window.__hold?.pending ?? []) h.resolve(null); return true; })()`);
          await closeAll(p);
          await p.send('Emulation.clearDeviceMetricsOverride');
        }
      },
      after: closeAll,
    },
  ];
}

// Local #273: the session view at the allowed minimum window (760x480) and on a 2560 px display.
import { tmpdir } from 'node:os';
import { join } from 'node:path';

export default function steps({ pass, fail, sleep }) {
  return [
    {
      id: 'window-sizes', group: 'core',
      title: 'at 760x480 the header is at most a quarter of the pane and a pending question shows its options and Submit answer without an inner scroll; at 2560 px transcript lines and the answer box are capped',
      async run(p) {
        const resize = (w, h) => p.send('Emulation.setDeviceMetricsOverride', { width: w, height: h, deviceScaleFactor: 1, mobile: false });
        // The terminal overlay a previous step leaves open would cover the list.
        if (await p.eval(`document.getElementById('terminal-overlay')?.hidden === false`)) { await p.click(`document.querySelector('#terminal-overlay button')`); await sleep(300); }
        await resize(760, 480); await sleep(300);
        await p.click(`[...document.querySelectorAll('#session-list .session-item')].find((l) => l.innerText.includes('waiting'))`);
        if (!(await p.waitFor(`!document.getElementById('approval-banner').hidden`, 5000))) return fail('the waiting session shows no question banner');
        await sleep(300);
        const m = await p.eval(`(() => {
          const h = (s) => document.querySelector(s).getBoundingClientRect();
          const ban = document.getElementById('approval-banner');
          const submit = [...ban.querySelectorAll('button')].find((b) => /Submit answer/.test(b.textContent)).getBoundingClientRect();
          const opts = [...ban.querySelectorAll('.approval-opt')].map((o) => o.getBoundingClientRect());
          const meta = [...document.querySelectorAll('.session-head .meta > *')].filter((e) => e.checkVisibility()).map((e) => e.getBoundingClientRect().height);
          return { pane: h('#session-view').height, head: h('.session-head').height, banner: ban.clientHeight, bannerScroll: ban.scrollHeight, events: h('#events').height,
            submitBottom: submit.bottom, submitTop: submit.top, optBottom: Math.max(...opts.map((r) => r.bottom)), vh: innerHeight, docW: document.documentElement.scrollWidth,
            tallestMetaItem: Math.max(...meta) };
        })()`);
        await p.shot(join(tmpdir(), 'rsi-273-760x480.png'));
        if (m.head > m.pane * 0.26) return fail(`the header is ${m.head} of ${m.pane} px at 760x480 (more than about a quarter)`);
        if (m.tallestMetaItem > 24) return fail(`a header meta item is ${m.tallestMetaItem} px tall: it wrapped instead of shortening`);
        if (m.bannerScroll > m.banner + 1) return fail(`the question banner scrolls inside (${m.bannerScroll} of ${m.banner} px)`);
        if (m.submitBottom > m.vh || m.submitTop < 0 || m.optBottom > m.vh) return fail(`Submit answer or an option is outside the window (${JSON.stringify(m)})`);
        if (m.events < 40) return fail(`the conversation is ${m.events} px tall`);
        if (m.docW > 760) return fail(`the page scrolls sideways (${m.docW} px)`);
        await resize(2560, 1440); await sleep(300);
        const w = await p.eval(`(() => {
          const ev = [...document.querySelectorAll('#events .ev')];
          const px = (e) => e.getBoundingClientRect().width;
          const ch = parseFloat(getComputedStyle(document.querySelector('#events')).fontSize) * 0.55;
          const q = document.querySelector('#approval-banner textarea');
          return { ev: Math.max(0, ...ev.map(px)), evCount: ev.length, answer: q ? px(q) : 0, events: px(document.getElementById('events')), message: px(document.getElementById('message')), ch };
        })()`);
        const cap = 1100;
        if (w.evCount === 0 || w.ev > cap) return fail(`a transcript line is ${Math.round(w.ev)} px wide at 2560 (cap ${cap}) in ${w.evCount} events`);
        if (w.answer === 0 || w.answer > cap) return fail(`the answer box is ${Math.round(w.answer)} px wide at 2560`);
        if (w.message > cap) return fail(`the message box is ${Math.round(w.message)} px wide at 2560`);
        return pass(`760x480: header ${Math.round(m.head)}/${Math.round(m.pane)} px, banner ${Math.round(m.banner)} px unscrolled, conversation ${Math.round(m.events)} px; 2560: line ${Math.round(w.ev)} px, answer box ${Math.round(w.answer)} px`);
      },
      async after(p) { await p.send('Emulation.clearDeviceMetricsOverride'); },
    },
  ];
}

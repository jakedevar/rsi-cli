// Local #288: Settings at the allowed minimum window (760x480): every group of the sidebar rail is on screen without
// scrolling, and a row's value is not squeezed into the label.
import { tmpdir } from 'node:os';
import { join } from 'node:path';

export default function steps({ pass, fail, sleep, closeAll, command }) {
  return [
    {
      id: 'settings-small-window', group: 'settings',
      title: 'at 760x480 every Settings group is visible in the rail without scrolling and a row label is not squeezed against its value',
      async run(p) {
        await p.send('Emulation.setDeviceMetricsOverride', { width: 760, height: 480, deviceScaleFactor: 1, mobile: false });
        await sleep(300);
        await command(p, 'set');
        if (!(await p.waitFor(`!document.getElementById('settings-view').hidden`, 5000))) return fail('Settings did not open');
        await sleep(400);
        const m = await p.eval(`(() => {
          const rail = document.querySelector('#settings-view .settings-rail');
          const groups = [...rail.querySelectorAll('.settings-group')];
          const rows = [...document.querySelectorAll('#settings-view .settings-row')];
          const row = rows[0];
          const label = row?.querySelector('.sv-label').getBoundingClientRect();
          const value = row?.querySelector('.sv-value').getBoundingClientRect();
          return { groups: groups.length, lastBottom: Math.max(0, ...groups.map((g) => g.getBoundingClientRect().bottom)), vh: innerHeight,
            railScroll: rail.scrollHeight, railClient: rail.clientHeight, docW: document.documentElement.scrollWidth,
            labelHeight: label?.height ?? 0, labelWidth: label?.width ?? 0, overlap: !!(label && value && label.bottom > value.top + 1 && label.right > value.left + 1 && label.left < value.right) };
        })()`);
        await p.shot(join(tmpdir(), 'rsi-288-settings-760x480.png'));
        if (m.groups < 6) return fail(`the rail shows ${m.groups} groups`);
        if (m.railScroll > m.railClient + 1 || m.lastBottom > m.vh) return fail(`the rail scrolls or runs off the window (${m.railScroll} of ${m.railClient} px, last group bottom ${Math.round(m.lastBottom)} of ${m.vh})`);
        if (m.overlap || m.labelHeight > 40) return fail(`a row label is squeezed (${Math.round(m.labelWidth)} px wide, ${Math.round(m.labelHeight)} px tall, overlap ${m.overlap})`);
        if (m.docW > 760) return fail(`the page scrolls sideways (${m.docW} px)`);
        return pass(`760x480: ${m.groups} groups fit the rail (${m.railScroll}/${m.railClient} px, no scroll); the first row label is ${Math.round(m.labelWidth)} px wide on one line`);
      },
      async after(p) { await closeAll(p); await p.send('Emulation.clearDeviceMetricsOverride'); },
    },
  ];
}

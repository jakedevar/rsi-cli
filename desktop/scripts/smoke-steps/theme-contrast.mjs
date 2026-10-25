// Local #283 (leftovers of #274): the contrast walker (scripts/audit-contrast.mjs) finds nothing under WCAG AA in Light, High
// Contrast, Transparent and Truly Transparent, on the session list and the New session dialog; the native window theme follows the
// app theme; and the New session summary's glyphs are drawn by a font, not as boxes.
import { contrastAudit } from '../audit-contrast.mjs';

// A glyph is "drawn" when its raster is not blank and not the box a missing glyph gets: compared with the raster of code points no
// font has (U+FFFF, U+10FFFE), at the element's own font. The old ▯ and ⊡ draw as boxes only on hosts whose fonts lack them
// (the audit's Linux screenshot, not every Mac), so they are reported, not required.
const GLYPHS_EXPRESSION = (font) => `(() => {
  const c = document.createElement('canvas'); c.width = 48; c.height = 48;
  const g = c.getContext('2d', { willReadFrequently: true });
  g.font = ${JSON.stringify(font)}; g.textBaseline = 'middle';
  const raster = (ch) => { g.clearRect(0, 0, 48, 48); g.fillStyle = '#000'; g.fillText(ch, 4, 24); return Array.from(g.getImageData(0, 0, 48, 48).data).filter((_, i) => i % 4 === 3).join(','); };
  const boxes = new Set(['\\u{FFFF}', '\\u{10FFFE}', '\\u{E0FFF}'].map(raster));
  const blank = raster(' ');
  const text = document.getElementById('launch-summary')?.textContent ?? '';
  const missing = [...new Set([...text].filter((ch) => ch.charCodeAt(0) > 127 && ch !== '·'))].filter((ch) => { const r = raster(ch); return r === blank || boxes.has(r); });
  const old = ['▯', '⊡'].filter((ch) => { const r = raster(ch); return r === blank || boxes.has(r); });
  return { text, missing, oldMissing: old };
})()`;

export default function steps({ pass, fail, sleep, closeAll, bridgeLog }) {
  return [
    {
      id: 'theme-contrast', group: 'settings',
      title: 'no visible text is under AA in Light, High Contrast, Transparent and Truly Transparent; the native window follows the theme; the New session summary glyphs draw',
      async run(p) {
        const apply = (name) => p.eval(`import('./themes.js').then((m) => { m.applyThemeByName(${JSON.stringify(name)}); return true; })`);
        const windowCalls = () => bridgeLog.filter((e) => e.cmd === 'set_window_theme').length;
        const notes = [];
        for (const theme of ['light', 'high-contrast', 'transparent', 'truly-transparent']) {
          await apply(theme);
          await sleep(200);
          for (const [where, open] of [['session list', async () => {}], ['New session dialog', async () => { await p.element('#new-session', '(e) => e.click()', { step: 'open the launch dialog' }); await sleep(300); }]]) {
            await open();
            const r = await contrastAudit(p);
            if (r.checked < 10) return fail(`${theme} ${where}: the walker only found ${r.checked} texts`);
            if (r.failed) return fail(`${theme} ${where}: ${r.failed} of ${r.checked} texts under AA: ${r.failures.slice(0, 5).map((f) => `${f.selector} "${f.text}" ${f.fg} on ${f.bg} ${f.ratio}:1 (needs ${f.need})`).join('; ')}`);
            notes.push(`${theme} ${where} ${r.checked}`);
          }
          await closeAll(p);
        }
        // The window theme: an explicit theme pins it, Match system hands it back to the OS (a stubbed command here).
        const before = windowCalls();
        await apply('light'); await sleep(100);
        await apply('goth'); await sleep(100);
        await apply('system'); await sleep(100);
        const sent = windowCalls() - before;
        if (sent !== 3) return fail(`the page sent ${sent} set_window_theme calls for three theme changes (light, dark, system)`);
        // The glyphs, in the dialog's own font.
        await apply('system');
        await p.element('#new-session', '(e) => e.click()', { step: 'open the launch dialog' });
        await sleep(300);
        const font = await p.eval(`(() => { const e = document.getElementById('launch-summary'); const cs = e && getComputedStyle(e); return cs ? cs.fontSize + ' ' + cs.fontFamily : '11px monospace'; })()`);
        const g = await p.eval(GLYPHS_EXPRESSION(font));
        if (!g.text) return fail('the New session summary is empty');
        if (g.missing.length) return fail(`the New session summary "${g.text}" has glyphs that draw as boxes or nothing: ${g.missing.join(' ')}`);
        return pass(`no text under AA: ${notes.join(', ')}; ${sent} window-theme calls; the summary "${g.text}" draws every glyph (on this host's fonts the old ▯ ⊡ ${g.oldMissing.length ? `drew as boxes: ${g.oldMissing.join(' ')}` : 'happened to draw'})`);
      },
      after: closeAll,
    },
  ];
}

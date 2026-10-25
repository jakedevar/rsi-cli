// Local #293: the Issues inspector draws the body as Markdown (the transcript's renderer) with a View raw button, not as a raw <pre>.
// The smoke bridge's render_markdown shim only escapes text, so the contrast walk puts a sample of the real renderer's output (a heading,
// a link, inline code, a long fenced block, a table, a quote) inside the rendered body and judges the inspector in the default, Light and
// High Contrast themes; the long code block must scroll sideways inside it, not widen the page.
import { contrastAudit } from '../audit-contrast.mjs';

const SAMPLE = `<h2>Plan</h2><p>See <a href="https://example.com/x">the link</a> and <code>desktop/src/issues.ts</code>, <strong>bold</strong>.</p>
<ul><li>first item</li><li>second item</li></ul><blockquote><p>a quote</p></blockquote>
<pre><code class="language-rust">let very_long_line = "${'x'.repeat(240)}";\n</code></pre>
<table><thead><tr><th>col</th><th>val</th></tr></thead><tbody><tr><td>a</td><td>b</td></tr></tbody></table>`;

export default function steps({ pass, fail, sleep, closeAll, command }) {
  return [
    {
      id: 'issue-markdown', group: 'a11y',
      title: 'the Issue inspector body is rendered Markdown with a View raw toggle (raw <pre> and back, aria-pressed, focus kept); a long code block scrolls inside it; no inspector text is under AA in the default, Light and High Contrast themes',
      async run(p) {
        await p.eval(`(() => { const c = document.getElementById('vim-keys'); if (!c.checked) { c.checked = true; c.dispatchEvent(new Event('change')); } })()`);
        await closeAll(p);
        await command(p, 'issues');
        if (!(await p.waitFor(`!document.getElementById('issues-view').hidden && document.querySelectorAll('#issues-list li').length > 0 && !!document.querySelector('#issues-inspector .iv-bodybox')`, 8000))) return fail('the Issues workspace did not show an inspector body');
        const shape = () => p.eval(`(() => {
          const box = document.querySelector('#issues-inspector .iv-bodybox');
          const t = box && box.querySelector('.iv-raw-toggle');
          return { md: !!(box && box.querySelector('.iv-md .md')), raw: !!(box && box.querySelector('pre.iv-raw')), text: box ? box.innerText : '',
            toggle: t ? { text: t.textContent, pressed: t.getAttribute('aria-pressed'), tab: t.tabIndex, tag: t.tagName } : null };
        })()`);
        const rendered = await shape();
        if (!rendered.md || rendered.raw) return fail(`the body is not drawn by the Markdown renderer: ${JSON.stringify(rendered)}`);
        if (!rendered.toggle || rendered.toggle.tag !== 'BUTTON' || rendered.toggle.text !== 'View raw' || rendered.toggle.pressed !== 'false' || rendered.toggle.tab < 0) return fail(`View raw is not a closed Tab-stop button: ${JSON.stringify(rendered.toggle)}`);
        // Mouse: the toggle swaps in the raw text, keeps its label and focus, and swaps back.
        await p.eval(`document.querySelector('#issues-inspector .iv-raw-toggle').focus()`);
        await p.element('#issues-inspector .iv-raw-toggle', '(e) => e.click()', { step: 'press View raw' });
        await sleep(200);
        const raw = await shape();
        if (!raw.raw || raw.md || raw.toggle?.pressed !== 'true' || raw.toggle?.text !== 'View raw') return fail(`View raw did not show the raw text: ${JSON.stringify(raw)}`);
        if (!(await p.eval(`document.activeElement && document.activeElement.classList.contains('iv-raw-toggle')`))) return fail('focus left the View raw button after the redraw');
        await p.element('#issues-inspector .iv-raw-toggle', '(e) => e.click()', { step: 'press View raw again' });
        await sleep(200);
        const back = await shape();
        if (!back.md || back.raw || back.toggle?.pressed !== 'false') return fail(`a second press did not return to the rendered body: ${JSON.stringify(back)}`);
        // The long code block scrolls inside the body; the page does not grow sideways.
        await p.eval(`(() => { const md = document.querySelector('#issues-inspector .iv-md .md'); md.innerHTML = ${JSON.stringify(SAMPLE)}; md.classList.remove('md-pending'); })()`);
        await sleep(100);
        const fit = await p.eval(`(() => {
          const pre = document.querySelector('#issues-inspector .iv-md .md pre'), body = document.querySelector('#issues-inspector .iv-md');
          const b = body.getBoundingClientRect(), r = pre.getBoundingClientRect();
          return { scrolls: pre.scrollWidth > pre.clientWidth, overflowX: getComputedStyle(pre).overflowX, inside: r.right <= b.right + 1, page: document.documentElement.scrollWidth <= window.innerWidth + 1 };
        })()`);
        if (!fit.scrolls || fit.overflowX !== 'auto' || !fit.inside || !fit.page) return fail(`the long code block does not scroll inside the body: ${JSON.stringify(fit)}`);
        const notes = [];
        for (const theme of ['default', 'light', 'high-contrast']) {
          await p.eval(`import('./themes.js').then((m) => { m.applyThemeByName(${JSON.stringify(theme)}); return true; })`);
          await sleep(200);
          const r = await contrastAudit(p, { root: '#issues-inspector' });
          if (r.checked < 8) return fail(`${theme}: the walker only found ${r.checked} texts in the inspector`);
          if (r.failed) return fail(`${theme}: ${r.failed} of ${r.checked} inspector texts under AA: ${r.failures.slice(0, 5).map((f) => `${f.selector} "${f.text}" ${f.fg} on ${f.bg} ${f.ratio}:1 (needs ${f.need})`).join('; ')}`);
          notes.push(`${theme} ${r.checked}`);
        }
        return pass(`the body is rendered Markdown with a View raw button (raw <pre> and back, focus kept), a long code block scrolls inside it, and no inspector text is under AA (${notes.join(', ')})`);
      },
      async after(p) {
        await p.eval(`import('./themes.js').then((m) => { m.applyThemeByName('system'); return true; })`);
        await closeAll(p);
      },
    },
  ];
}

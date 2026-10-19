// Syntax colours for fenced code blocks in rendered Markdown (gap sweep G4,
// hub #3000): the transcript's message bodies and the file viewer's preview.
// The TUI colours them with syntect (crates/rsi/src/ui/highlight.rs); the
// desktop reuses the viewer's highlighter (viewer-model.ts). The code text is
// read with textContent and written back as text nodes only, so the sanitized
// HTML is never re-parsed.
import { el } from './dom.js';
import { fenceLanguage, highlightBlock, segments } from './viewer-model.js';

/** Colour every `<pre><code class="language-x">` under `root` once. */
export function highlightCodeBlocks(root: HTMLElement): void {
  for (const code of root.querySelectorAll<HTMLElement>('pre > code[class*="language-"]')) {
    if (code.dataset.hl === '1') continue;
    code.dataset.hl = '1';
    const name = [...code.classList].find((c) => c.startsWith('language-'))?.slice('language-'.length) ?? '';
    const lang = fenceLanguage(name);
    if (!lang) continue;
    const text = code.textContent ?? '';
    const parts: Node[] = [];
    highlightBlock(lang, text.replace(/\n$/, '')).forEach((row, i) => {
      if (i > 0) parts.push(document.createTextNode('\n'));
      for (const seg of segments(row.line, row.spans, [])) {
        parts.push(seg.tok ? el('span', `hl-${seg.tok}`, seg.text) : document.createTextNode(seg.text));
      }
    });
    if (text.endsWith('\n')) parts.push(document.createTextNode('\n'));
    code.replaceChildren(...parts);
  }
}

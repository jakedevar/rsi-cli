// Markdown message bodies. The backend (`render_markdown`, markdown.rs) parses
// and sanitizes with a strict allowlist; the HTML it returns is the ONLY daemon-
// derived text in this app that is ever assigned to innerHTML. Everything else
// uses textContent.
import { api } from './api.js';
import { $, el, toast } from './dom.js';
import { BATCH_TEXTS, MarkdownCache, cacheKey, takeBatch } from './mdmodel.js';

interface Job { key: string; src: string; targets: Set<HTMLElement> }

const cache = new MarkdownCache();
const queue: Job[] = [];
const queued = new Map<string, Job>();
let scheduled = false;
let unavailable = false;

/** Sanitized HTML from the Rust backend (see markdown.rs); never pass anything else here. */
function setSanitizedHtml(target: HTMLElement, html: string): void {
  target.innerHTML = html; // sanitized by ammonia allowlist in the backend
  target.classList.remove('md-pending');
  decorateCode(target);
}

function decorateCode(root: HTMLElement): void {
  for (const pre of root.querySelectorAll('pre')) {
    const btn = el('button', 'copy-code', 'Copy');
    btn.type = 'button';
    btn.setAttribute('aria-label', 'Copy code');
    pre.append(btn);
  }
}

/**
 * A body element for Markdown `text`. Shows the cached HTML at once, otherwise
 * plain text until the batched render lands.
 */
export function markdownBody(sessionId: string, e: { id: number; sequence: number }, part: number, text: string): HTMLElement {
  const box = el('div', 'md');
  const key = cacheKey(sessionId, e, part);
  const hit = cache.get(key, text);
  if (hit !== undefined) {
    setSanitizedHtml(box, hit);
    return box;
  }
  box.textContent = text;
  box.classList.add('md-pending');
  if (unavailable) return box;
  let job = queued.get(key);
  if (!job || job.src !== text) {
    job = { key, src: text, targets: new Set() };
    queued.set(key, job);
    queue.push(job);
  }
  job.targets.add(box);
  schedule();
  return box;
}

function schedule(): void {
  if (scheduled || queue.length === 0) return;
  scheduled = true;
  window.setTimeout(() => { void flush(); }, 0);
}

async function flush(): Promise<void> {
  const pane = $('events');
  try {
    while (queue.length) {
      const batch = takeBatch(queue, BATCH_TEXTS);
      for (const j of batch) if (queued.get(j.key) === j) queued.delete(j.key);
      const stick = pane.scrollHeight - pane.scrollTop - pane.clientHeight < 80;
      let htmls: string[];
      try {
        htmls = await api.renderMarkdown(batch.map((j) => j.src));
      } catch (err) {
        // Leave the plain-text fallback in place; do not retry in a loop.
        unavailable = true;
        queue.length = 0;
        queued.clear();
        toast(`Markdown rendering unavailable: ${String(err)}`);
        return;
      }
      batch.forEach((j, i) => {
        const html = htmls[i] ?? '';
        cache.set(j.key, j.src, html);
        for (const t of j.targets) if (t.isConnected) setSanitizedHtml(t, html);
      });
      if (stick) pane.scrollTop = pane.scrollHeight;
      // Yield so a 2000-event conversation stays responsive.
      await new Promise<void>((r) => window.setTimeout(r, 0));
    }
  } finally {
    scheduled = false;
    schedule();
  }
}

/** Delegated clicks inside rendered Markdown: links are inert, code gets a copy button. */
export function wireMarkdownEvents(pane: HTMLElement): void {
  pane.addEventListener('click', (ev) => {
    const target = ev.target as HTMLElement | null;
    const copy = target?.closest<HTMLElement>('.copy-code');
    if (copy) {
      ev.preventDefault();
      const code = copy.parentElement?.querySelector('code')?.textContent ?? '';
      void navigator.clipboard?.writeText(code).then(
        () => toast('Code copied', true),
        () => toast('Could not copy'),
      );
      return;
    }
    const link = target?.closest<HTMLAnchorElement>('.md a');
    if (link) {
      // Links never navigate the webview; copy the URL instead.
      ev.preventDefault();
      const href = link.getAttribute('href') ?? '';
      void navigator.clipboard?.writeText(href).then(
        () => toast(`Link copied: ${href}`, true),
        () => toast(href, true),
      );
    }
  });
}

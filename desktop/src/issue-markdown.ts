// The Issue inspector's body and handoffs as Markdown (local #293). The TUI draws both as wrapped Markdown
// (crates/rsi/src/ui/issue_detail.rs `markdown_lines`); here the transcript's renderer does it: the backend parses and
// sanitizes (markdown.rs), the text is shown as plain text until the render lands or when rendering is unavailable, and
// the raw text is one button away.
import { el } from './dom.js';
import { markdownText } from './markdown.js';

/** Shown for an Issue with no body (the TUI's wording). */
export const NO_DESCRIPTION = '(no description)';

/** The text block: rendered Markdown, or the raw monospace text when `raw`. An empty text shows `placeholder`. */
export function issueTextBlock(key: string, text: string, raw: boolean, placeholder = NO_DESCRIPTION): HTMLElement {
  if (text === '') return el('div', 'iv-body iv-empty', placeholder);
  if (raw) return el('pre', 'iv-body iv-raw', text);
  const wrap = el('div', 'iv-body iv-md');
  wrap.append(markdownText(key, text));
  return wrap;
}

/** Cache keys: one per Issue body and one per handoff; a key only hits while its text is unchanged. */
export const bodyKey = (issueId: string): string => `issue:${issueId}:body`;
export const handoffKey = (issueId: string, sequence: number): string => `issue:${issueId}:handoff:${sequence}`;

/**
 * The "View raw" toggle. Its label and class never change (the redraw refocuses a button by class and label,
 * focus-return.ts); `aria-pressed` says which way it is.
 */
export function rawToggle(raw: boolean): HTMLButtonElement {
  const b = el('button', 'btn iv-raw-toggle', 'View raw');
  b.type = 'button';
  b.dataset.mdRaw = '1';
  b.setAttribute('aria-pressed', String(raw));
  b.title = raw ? 'Showing the raw text; press to render the Markdown' : 'Show the raw text instead of the rendered Markdown';
  return b;
}

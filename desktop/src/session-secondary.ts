// The session header's second line: the short id with Copy ID, and the folder, branch and full id one click away.
// It is rebuilt on every header render (each refresh), so a keyboard user's focus on Copy ID or a Copy button in the
// details must be put back on the rebuilt button (local #297); the Details fold keeps its open state too.
import { type Session } from './api.js';
import { el } from './dom.js';
import { keepFocus } from './focus-return.js';
import { headDetails, shortSessionId } from './session-head-model.js';

export function renderSecondary(host: HTMLElement, s: Session, copy: (text: string, what: string) => void): void {
  const wasOpen = host.querySelector<HTMLDetailsElement>('details.head-details')?.open ?? false;
  const id = el('span', 'mono', `ID ${shortSessionId(s.id)}`);
  id.title = s.id;
  const copyId = el('button', 'btn link', 'Copy ID');
  copyId.type = 'button';
  copyId.dataset.action = 'copy-session-id';
  copyId.dataset.focusKey = 'copy-session-id';
  copyId.title = 'Copy the full session ID';
  const details = el('details', 'head-details');
  details.open = wasOpen;
  details.append(el('summary', undefined, 'Details'));
  for (const d of headDetails(s)) {
    const row = el('div', 'detail-row');
    const btn = el('button', 'btn link', 'Copy');
    btn.type = 'button';
    btn.dataset.focusKey = `copy:${d.label}`;
    btn.addEventListener('click', () => { copy(d.value, d.what); });
    row.append(el('span', 'detail-label', d.label), el('span', 'mono', d.value), btn);
    details.append(row);
  }
  keepFocus(host, () => host.replaceChildren(id, copyId, details));
}

// The visible action buttons of the Issue detail pane (local #265): plain-word Edit and Change status buttons, so a first-time
// user with a mouse, or Tab with Vim keys off, can do what the e and S keys do. Each button carries the existing action id, so
// the click goes through the same dispatch, forms, confirmations and RPCs as the key, the palette and the menu.
import { el } from './dom.js';
import { issueButtons, issueRef, type Issue } from './issues-model.js';

/** The row of buttons for this Issue, or null when no action can run on it. */
export function issueActionRow(issue: Issue): HTMLElement | null {
  const buttons = issueButtons(issue);
  if (buttons.length === 0) return null;
  const row = el('div', 'iv-actions');
  row.setAttribute('role', 'group');
  row.setAttribute('aria-label', `Actions for ${issueRef(issue)}`);
  for (const b of buttons) {
    const btn = el('button', 'btn', b.label);
    btn.type = 'button';
    btn.dataset.action = b.action;
    btn.title = b.title;
    row.append(btn);
  }
  return row;
}

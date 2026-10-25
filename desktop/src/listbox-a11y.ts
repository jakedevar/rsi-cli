// One screen-reader wiring for every keyboard list (local #318).
//
// The lists in this app draw a visual cursor (`.sel` plus aria-selected on an <li role="option">) and rebuild their rows on
// every key, while focus stays on a filter box, on the dialog or on the list itself. A screen reader hears nothing then: the
// focused element does not point at the current row. The session list already did it right (role=tree on the list, tabindex 0,
// aria-activedescendant on the cursor row, sessionTree.ts). This is the same thing for all the others, in one place:
//
//   syncListbox(list, owner)   call it after the rows are drawn (and after a highlight moves without a redraw).
//
// `owner` is the element that holds keyboard focus: the filter <input> (it becomes the ARIA 1.2 combobox that controls the
// list), the list itself, or the dialog / panel the keys are handled on. The helper names the list, gives the current row a
// stable id, and points the owner's aria-activedescendant at it. Ids derive from the row's key (data-key, else data-index, else
// its position), so a re-render that rebuilds the nodes keeps the same id. It sets attributes only: no key, mouse or focus
// behaviour changes. It uses nothing but children / getAttribute / setAttribute so the small fake documents in the DOM
// tests can run it.

let nextListId = 1;

/** Roles of a row that can be the current one. */
const ROW_ROLES = new Set(['option', 'treeitem', 'row']);

export interface ListboxOpts {
  /** The accessible name, set only when the list has none. */
  label?: string;
  /** The list's role when it has none (default listbox; a file tree is a tree; a table of rows is a grid whose rows are role=row). */
  role?: 'listbox' | 'tree' | 'grid';
}

function isRow(n: Element): boolean {
  return typeof n.getAttribute === 'function' && ROW_ROLES.has(n.getAttribute('role') ?? '');
}

/** The rows of a list in order (a note or heading row without a row role is skipped). */
export function listRows(list: Element): Element[] {
  const out: Element[] = [];
  const walk = (n: Element): void => {
    for (const c of Array.from(n.children ?? [])) {
      if (isRow(c)) out.push(c); // a row's own children are not rows
      else walk(c);
    }
  };
  walk(list);
  return out;
}

/** The id for row `i`: stable while the row keeps its key. */
function rowId(listId: string, row: Element, i: number): string {
  const ds = (row as HTMLElement).dataset;
  return `${listId}-o${String(ds?.key ?? ds?.index ?? i).replace(/[^\w-]/g, '_')}`;
}

/** The row marked current (aria-selected="true"), or null. */
export function currentRow(list: Element): Element | null {
  return listRows(list).find((r) => r.getAttribute('aria-selected') === 'true') ?? null;
}

/**
 * Make `owner` announce the current row of `list`. Idempotent: call it on every render. Returns the id it points at
 * (null when the list has no current row, then aria-activedescendant is removed).
 */
export function syncListbox(list: HTMLElement, owner: HTMLElement | null, opts: ListboxOpts = {}): string | null {
  const own = owner ?? list;
  if (!list.getAttribute('role')) list.setAttribute('role', opts.role ?? 'listbox');
  if (opts.label && !list.getAttribute('aria-label') && !list.getAttribute('aria-labelledby')) list.setAttribute('aria-label', opts.label);
  if (!list.id) list.id = `listbox-${nextListId++}`;
  if (own !== list && own.tagName?.toLowerCase() === 'input') {
    // A filter box over a list is the ARIA 1.2 combobox that controls it.
    own.setAttribute('role', 'combobox');
    own.setAttribute('aria-controls', list.id);
    own.setAttribute('aria-expanded', 'true');
    own.setAttribute('aria-autocomplete', 'list');
    own.setAttribute('aria-haspopup', 'listbox');
  }
  const rows = listRows(list);
  let active: string | null = null;
  rows.forEach((row, i) => {
    const id = rowId(list.id, row, i);
    if ((row as HTMLElement).id !== id) (row as HTMLElement).id = id;
    if (row.getAttribute('aria-selected') === 'true') active = id;
  });
  if (active) own.setAttribute('aria-activedescendant', active);
  else own.removeAttribute('aria-activedescendant');
  return active;
}

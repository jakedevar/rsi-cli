// The attribute that names a dialog's key table, so the ? help lists a dialog's keys by an explicit id and not by guessing
// from its title (local #335: "Sort sessions" is the Sort Picker). keyhelp.ts reads it; every dialog that has its own table
// and a title that differs from the table's name sets it.
export const KEY_TABLE_ATTR = 'data-key-table';

/** Mark a dialog with the name of its key table (overlay-help.generated.ts), which the ? help lists. */
export function setKeyTable(dlg: Element, table: string | undefined): void {
  if (table) dlg.setAttribute(KEY_TABLE_ATTR, table);
}

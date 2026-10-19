// Keys shared by the desktop's overlays (gap sweep G4, hub #3000), pure (no
// DOM): the pickers' Space leader (docs/keybindings.md: "Space N  Close and
// open a blank session prompt", "Space ;  Open the command palette" in the
// Sort, Label, Parent, Project and Telescope pickers and the read-only
// overlays). Tested in tests/overlays.test.mjs.

/** Space N / Space ; inside a picker: `arm` on Space, then the action, else nothing (the key acts normally). */
export function pickerLeader(armed: boolean, k: { key: string; ctrlKey?: boolean; metaKey?: boolean; altKey?: boolean }, canArm: boolean): { armed: boolean; run: 'arm' | 'launch' | 'palette' | null } {
  const plain = !k.ctrlKey && !k.metaKey && !k.altKey;
  if (armed) {
    if (plain && (k.key === 'N' || k.key === 'n')) return { armed: false, run: 'launch' };
    if (plain && k.key === ';') return { armed: false, run: 'palette' };
    return { armed: false, run: null };
  }
  if (plain && k.key === ' ' && canArm) return { armed: true, run: 'arm' };
  return { armed: false, run: null };
}


// ---------- the text-area background hex editor (overlay/text_area_bg_editor.rs) ----------

/** `#RRGGBB` or `RRGGBB` (any case) as `#RRGGBB`, else null (`parse_hex_color`). */
export function normalizeHex(s: string): string | null {
  const t = s.trim();
  const hex = t.startsWith('#') ? t.slice(1) : t;
  return /^[0-9a-fA-F]{6}$/.test(hex) ? `#${hex.toUpperCase()}` : null;
}

export type HexEffect =
  | { kind: 'none' }
  /** Save `value` ("" clears the override); `error` when the input is not a colour. */
  | { kind: 'commit'; value: string | null }
  | { kind: 'clear' }
  | { kind: 'close' }
  | { kind: 'leader'; run: 'arm' | 'launch' | 'palette' };

/**
 * One key of the hex editor: `#` and hex digits type (uppercased, at most 7
 * characters; the seventh commits, as in the TUI), Backspace deletes, Enter
 * commits, Delete clears the override, Esc or q close. Space N / Space ; are
 * the overlay leader (Space is never a hex character).
 */
export function hexEditKey(input: string, leaderArmed: boolean, k: { key: string; ctrlKey?: boolean; metaKey?: boolean; altKey?: boolean }): { input: string; armed: boolean; effect: HexEffect } {
  const l = pickerLeader(leaderArmed, k, true);
  if (l.run) return { input, armed: l.armed, effect: { kind: 'leader', run: l.run } };
  if (k.ctrlKey || k.metaKey || k.altKey) return { input, armed: false, effect: { kind: 'none' } };
  const commit = (v: string): HexEffect => ({ kind: 'commit', value: v.trim() === '' ? '' : normalizeHex(v) });
  switch (k.key) {
    case 'Escape': case 'q': return { input, armed: false, effect: { kind: 'close' } };
    case 'Enter': return { input, armed: false, effect: commit(input) };
    case 'Backspace': return { input: input.slice(0, -1), armed: false, effect: { kind: 'none' } };
    case 'Delete': return { input: '', armed: false, effect: { kind: 'clear' } };
    default: break;
  }
  if (/^[#0-9a-fA-F]$/.test(k.key) && input.length < 7) {
    const next = input + k.key.toUpperCase();
    return { input: next, armed: false, effect: next.length === 7 ? commit(next) : { kind: 'none' } };
  }
  return { input, armed: false, effect: { kind: 'none' } };
}

// ---------- the command palette's argument edit (overlay/command_palette.rs) ----------

/** Tab on a palette row: the `:` command line to edit its argument in, or null when it has no `:` command. */
export function paletteArgument(keys: readonly string[]): string | null {
  const c = keys.find((k) => /^:[a-z]/i.test(k));
  return c ? `${c.split(/\s+/)[0]} ` : null;
}

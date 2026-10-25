// Theme picker and theme role editor (DOM side of theme-model.ts; slice 7).
// The chosen theme and the role overrides are applied as CSS variables on
// :root and kept in localStorage. All text is set with textContent.
import { overlayKey } from './actions/detail.js';
import { api, inTauri } from './api.js';
import { el, toast } from './dom.js';
import { uiChanged } from './state.js';
import { THEME_ROLES } from './settings.generated.js';
import {
  type Assessment, DEFAULT_CHOICE, DESKTOP_DEFAULT_NAME, MATCH_SYSTEM_NAME, STORAGE_KEY, type ThemeChoice, allVarNames,
  assessRole, colorSchemeOf, cssVars, decodeChoice, effectiveChoice, encodeChoice, findTheme, isMatchSystemName,
  parseHex, pickerEntries, pickerIndexOf, pickerKey, resolveRoles, roleBaseline, setOverride, swatch, systemSwatch,
  themeByKey, unmappedRoles,
} from './theme-model.js';
import { focusOwnsKey } from './modal-gate.js';
import { syncListbox } from './listbox-a11y.js';
import { setKeyTable } from './keytable.js';

let current: ThemeChoice = DEFAULT_CHOICE;
const listeners: (() => void)[] = [];

function storage(): Storage | undefined {
  try { return window.localStorage; } catch { return undefined; }
}

/** Hear every theme change (the settings pane redraws its rows). */
export function onThemeChange(cb: () => void): void {
  listeners.push(cb);
}

export function currentChoice(): ThemeChoice {
  return current;
}

/** The system's light/dark setting right now (dark when the page cannot tell). */
export function systemIsLight(): boolean {
  try { return window.matchMedia('(prefers-color-scheme: light)').matches; } catch { return false; }
}

/** The choice with "Match system" resolved to the theme the system asks for. */
function effective(choice: ThemeChoice): ThemeChoice {
  return effectiveChoice(choice, systemIsLight());
}

export function themeName(choice: ThemeChoice = current): string {
  const shown = effective(choice);
  const name = themeByKey(shown.theme)?.name ?? DESKTOP_DEFAULT_NAME;
  return choice.system ? `${MATCH_SYSTEM_NAME}: now ${themeByKey(shown.theme) ? 'Light' : 'Dark'}` : name;
}

export function overrideCount(): number {
  return Object.keys(current.overrides).length;
}

/** Set the CSS variables of `choice` (a preview when `persist` is false). */
function paint(requested: ThemeChoice): void {
  const choice = effective(requested);
  const style = document.documentElement.style;
  for (const v of allVarNames()) style.removeProperty(v);
  for (const [v, hex] of Object.entries(cssVars(choice))) style.setProperty(v, hex);
  // Native controls (checkboxes, radios, select arrows, scrollbars) follow the colours the theme paints.
  style.setProperty('color-scheme', colorSchemeOf(choice));
  document.documentElement.dataset.theme = choice.theme ?? 'desktop-default';
  syncWindowTheme(requested.system === true, colorSchemeOf(choice));
}

let windowTheme = '';

/**
 * The native window (title bar, frame) follows the app's effective theme: "Match system" hands the window back to the
 * OS (which already shows what the page matched), an explicit theme pins the window to its light or dark side. Sent
 * only when it changed; a plain browser preview has no window to theme.
 */
function syncWindowTheme(matchSystem: boolean, scheme: 'light' | 'dark'): void {
  const want = matchSystem ? 'system' : scheme;
  if (want === windowTheme || !inTauri()) return;
  windowTheme = want;
  api.setWindowTheme(want).catch(() => { windowTheme = ''; /* an older shell without the command: the title bar keeps the OS look */ });
}

function commit(choice: ThemeChoice): void {
  current = choice;
  paint(choice);
  try { storage()?.setItem(STORAGE_KEY, encodeChoice(choice)); } catch { /* quota or disabled storage */ }
  for (const cb of listeners) cb();
  uiChanged();
}

/** Apply the stored theme at startup. */
export function initThemes(): void {
  current = decodeChoice(storage()?.getItem(STORAGE_KEY));
  paint(current);
  // "Match system" follows the system live; an explicit theme ignores the change.
  try {
    const mq = window.matchMedia('(prefers-color-scheme: light)');
    const onChange = () => { if (current.system) { paint(current); for (const cb of listeners) cb(); uiChanged(); } };
    if (typeof mq.addEventListener === 'function') mq.addEventListener('change', onChange);
    else (mq as MediaQueryList).addListener?.(onChange);
  } catch { /* no matchMedia: the theme stays as painted */ }
}

/** Clear every role override; the theme stays (the TUI's "Reset active theme"). */
export function resetRoles(): void {
  const n = overrideCount();
  commit({ ...current, overrides: {} });
  toast(n > 0 ? `Cleared ${n} role override${n === 1 ? '' : 's'}` : 'No role is overridden', n > 0);
}

/** `:theme <name>` applies a theme directly. */
export function applyThemeByName(name: string): void {
  if (isMatchSystemName(name)) {
    commit({ ...current, theme: null, system: true });
    toast(`Theme: ${themeName()}`, true);
    return;
  }
  const t = findTheme(name);
  if (!t && !/^desktop|default$/i.test(name.trim())) {
    toast(`No built-in theme “${name}”`);
    return;
  }
  commit({ overrides: current.overrides, theme: t?.key ?? null });
  toast(`Theme: ${themeName()}`, true);
}

// ---------- picker ----------

function pickDialog(id: string, title: string, keyTable?: string): { dlg: HTMLDialogElement; box: HTMLElement } {
  let dlg = document.getElementById(id) as HTMLDialogElement | null;
  if (!dlg) {
    dlg = document.createElement('dialog');
    dlg.id = id;
    dlg.className = 'pick theme-dialog';
    document.body.append(dlg);
  }
  dlg.onkeydown = null;
  dlg.onclose = null;
  setKeyTable(dlg, keyTable);
  const box = el('div');
  box.append(el('h2', '', title));
  dlg.replaceChildren(box);
  return { dlg, box };
}

/** The built-in theme picker: j/k preview live, Enter applies, Esc/q rolls back, 1-9 pick directly. */
export function openThemePicker(): void {
  const entries = pickerEntries();
  const opening = current;
  let index = pickerIndexOf(opening);
  let done = false;
  const { dlg, box } = pickDialog('theme-picker', 'Choose theme', 'Theme Picker');
  const list = el('ul', 'pick-list theme-list');
  const hint = el('div', 'pick-empty', 'j/k preview · Enter applies · 1-9 pick · Esc rolls back · Match system follows your light/dark setting');
  box.append(list, hint);
  const draw = () => {
    list.replaceChildren(...entries.map((e, i) => {
      const li = el('li', `pick-item${i === index ? ' sel' : ''}`);
      li.setAttribute('role', 'option');
      li.setAttribute('aria-selected', String(i === index));
      li.append(el('span', 'pick-mark', i === pickerIndexOf(opening) ? '●' : ''));
      li.append(el('span', '', i > 1 && i <= 10 ? `${i - 1}  ${e.name}` : e.name));
      const sw = el('span', 'theme-swatch');
      for (const hex of e.system ? systemSwatch() : swatch(e.key)) {
        const chip = el('span', 'chip');
        chip.style.background = hex;
        sw.append(chip);
      }
      li.append(sw);
      li.addEventListener('click', () => { index = i; preview(); finish(true); });
      return li;
    }));
    list.children[index]?.scrollIntoView({ block: 'nearest' });
    syncListbox(list, dlg, { label: 'Themes' });
  };
  const picked = (): ThemeChoice => {
    const e = entries[index]!;
    return e.system ? { overrides: opening.overrides, theme: null, system: true } : { overrides: opening.overrides, theme: e.key };
  };
  const preview = () => paint(picked());
  const finish = (apply: boolean) => {
    if (done) return;
    done = true;
    if (apply) {
      commit(picked());
      toast(`Theme: ${themeName()}`, true);
    } else {
      paint(opening);
    }
    if (dlg.open) dlg.close();
  };
  dlg.onkeydown = (ev) => {
    if (focusOwnsKey(ev.key, ev.target as Element | null)) return;
    if (ev.isComposing) return;
    // `Space N` / `Space ;` leave the picker (rolling back) for a new session or the palette.
    if (overlayKey(ev, dlg)) return;
    if (ev.ctrlKey || ev.metaKey || ev.altKey) return;
    const fx = pickerKey(index, entries.length, ev.key);
    if (fx.kind === 'none') return;
    ev.preventDefault();
    ev.stopPropagation();
    if (fx.kind === 'move') { index = fx.index; preview(); draw(); }
    else if (fx.kind === 'apply') { index = fx.index; preview(); finish(true); }
    else finish(false);
  };
  dlg.onclose = () => finish(false);
  draw();
  dlg.showModal();
  dlg.tabIndex = -1;
  dlg.focus();
}

// ---------- role editor ----------

function describe(a: Assessment): string {
  switch (a.kind) {
    case 'invalid': return a.reason;
    case 'none': return 'No contrast check for this role';
    case 'readable': return `Contrast ${a.ratio.toFixed(2)}:1 against ${roleLabel(a.against)}`;
    case 'low': return `Low contrast ${a.ratio.toFixed(2)}:1 against ${roleLabel(a.against)} (minimum ${a.minimum}:1): Enter again to keep it`;
  }
}

function roleLabel(key: string): string {
  return THEME_ROLES.find((r) => r.key === key)?.label ?? key;
}

/**
 * The role editor: pick a role (j/k), type #RRGGBB, Enter previews then
 * commits (a low-contrast colour needs a second unchanged Enter), Delete
 * resets the role, Esc restores the overrides it opened with.
 */
export function openRoleEditor(startRole?: string): void {
  const opening = current;
  let working = current;
  let index = Math.max(0, THEME_ROLES.findIndex((r) => r.key === startRole));
  let acked: string | null = null;
  let previewed: string | null = null;
  const unmapped = new Set(unmappedRoles());
  const { dlg, box } = pickDialog('theme-roles', `Theme roles · ${themeName(opening)}`, 'Theme Role Editor');
  const list = el('ul', 'pick-list theme-list');
  const input = el('input');
  input.type = 'text';
  input.placeholder = '#RRGGBB';
  input.setAttribute('aria-label', 'Role colour');
  input.spellcheck = false;
  const status = el('div', 'pick-empty');
  status.setAttribute('aria-live', 'polite');
  box.append(list, input, status, el('div', 'pick-empty', 'j/k role · type a colour · Enter preview, Enter commit · Delete resets · Esc restores'));
  const role = () => THEME_ROLES[index]!;
  const draw = () => {
    const roles = resolveRoles(effective(working));
    list.replaceChildren(...THEME_ROLES.map((r, i) => {
      const li = el('li', `pick-item${i === index ? ' sel' : ''}`);
      li.setAttribute('role', 'option');
      li.setAttribute('aria-selected', String(i === index));
      li.dataset.key = r.key;
      const chip = el('span', 'chip');
      chip.style.background = roles[r.key]!;
      li.append(chip, el('span', '', r.label));
      const note = working.overrides[r.key] ? `${roles[r.key]} (override)` : roles[r.key]!;
      li.append(el('span', 'pick-detail', unmapped.has(r.key) ? `${note} · no desktop surface yet` : note));
      li.addEventListener('click', () => { index = i; load(); });
      return li;
    }));
    list.children[index]?.scrollIntoView({ block: 'nearest' });
    syncListbox(list, input, { label: 'Theme roles' });
  };
  const load = () => {
    acked = null;
    previewed = null;
    const r = role();
    input.value = working.overrides[r.key] ?? roleBaseline(themeByKey(effective(working).theme), r.key);
    status.textContent = describe(assessRole(effective(working), r.key, input.value));
    draw();
  };
  const enter = () => {
    const r = role();
    const a = assessRole(effective(working), r.key, input.value);
    status.textContent = describe(a);
    if (a.kind === 'invalid') return;
    const hex = parseHex(input.value)!;
    if (previewed !== hex) {
      previewed = hex;
      acked = null;
      paint(setOverride(working, r.key, hex));
      status.textContent = `${describe(a)} · previewing; Enter commits`;
      return;
    }
    if (a.kind === 'low' && acked !== hex) {
      acked = hex;
      status.textContent = describe(a);
      return;
    }
    working = setOverride(working, r.key, hex);
    commit(working);
    toast(`${r.label} → ${hex}`, true);
    load();
  };
  const resetRole = () => {
    const r = role();
    working = setOverride(working, r.key, null);
    commit(working);
    toast(`${r.label} reset to the theme's colour`, true);
    load();
  };
  let closing = false;
  const restore = () => {
    if (closing) return;
    closing = true;
    // Esc restores the complete opening snapshot (the TUI's role editor).
    if (encodeChoice(current) !== encodeChoice(opening)) commit(opening);
    else paint(current);
    if (dlg.open) dlg.close();
  };
  input.addEventListener('input', () => {
    acked = null;
    previewed = null;
    paint(working);
    status.textContent = describe(assessRole(working, role().key, input.value));
  });
  dlg.onkeydown = (ev) => {
    if (focusOwnsKey(ev.key, ev.target as Element | null)) return;
    if (ev.isComposing || ev.ctrlKey || ev.metaKey || ev.altKey) return;
    const inInput = ev.target === input;
    let handled = true;
    if (ev.key === 'Escape') restore();
    else if (ev.key === 'Enter') enter();
    else if (ev.key === 'Delete' && !(inInput && input.value !== '' && input.selectionStart !== input.value.length)) resetRole();
    else if (ev.key === 'ArrowDown' || (!inInput && ev.key === 'j')) { index = Math.min(THEME_ROLES.length - 1, index + 1); load(); }
    else if (ev.key === 'ArrowUp' || (!inInput && ev.key === 'k')) { index = Math.max(0, index - 1); load(); }
    else if (!inInput && ev.key === 'q') restore();
    // `?` is never part of a #RRGGBB value: contextual help, as in the TUI (gap sweep G4).
    else if (ev.key === '?') void import('./keyhelp.js').then((h) => import('./commands.js').then((c) => h.openKeyHelp(c.snapshot())));
    else handled = false;
    if (handled) { ev.preventDefault(); ev.stopPropagation(); }
  };
  dlg.onclose = () => { if (!closing) restore(); };
  load();
  dlg.showModal();
  input.focus();
  input.select();
}

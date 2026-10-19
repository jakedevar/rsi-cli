// The text-area background (gap sweep G4, hub #3000): the TUI's Settings ›
// Appearance › Screen rows "Text area background" and "Background color" and
// its hex editor overlay (crates/rsi/src/overlay/text_area_bg_editor.rs). In
// the TUI it is a state.json preference; here it is this desktop's (stored in
// localStorage) and paints behind every text field. Keys are overlay-keys.ts
// hexEditKey (tested).
import { el } from './dom.js';
import { hexEditKey, normalizeHex } from './overlay-keys.js';
import { runLeader } from './pickers.js';
import { focusOwnsKey } from './modal-gate.js';

const KEY = 'rsi.desktop.textAreaBackground';

export interface TextBg {
  on: boolean;
  /** `#RRGGBB`, or "" for the theme's own field colour. */
  hex: string;
}

export function parseTextBg(raw: string | null): TextBg {
  try {
    const v: unknown = raw ? JSON.parse(raw) : null;
    if (v && typeof v === 'object') {
      const o = v as { on?: unknown; hex?: unknown };
      return { on: o.on === true, hex: typeof o.hex === 'string' ? normalizeHex(o.hex) ?? '' : '' };
    }
  } catch { /* fall through */ }
  return { on: false, hex: '' };
}

function storage(): Storage | undefined {
  try { return window.localStorage; } catch { return undefined; }
}

let current: TextBg | null = null;

export function textBg(): TextBg {
  current ??= parseTextBg(storage()?.getItem(KEY) ?? null);
  return current;
}

export function setTextBg(v: TextBg): void {
  current = v;
  try { storage()?.setItem(KEY, JSON.stringify(v)); } catch { /* this run only */ }
  applyTextBg();
}

/** The CSS for the setting: off draws nothing; on with no colour uses the theme's mantle. */
export function textBgCss(v: TextBg): string {
  if (!v.on) return '';
  const fill = v.hex || 'var(--mantle)';
  return `textarea, input[type="text"], input[type="search"], input:not([type]) { background-color: ${fill} !important; }`;
}

export function applyTextBg(): void {
  let style = document.getElementById('text-bg-style');
  if (!style) {
    style = document.createElement('style');
    style.id = 'text-bg-style';
    document.head.append(style);
  }
  style.textContent = textBgCss(textBg());
}

/** The hex editor: type #RRGGBB (the seventh character applies it), Enter applies, Delete clears, Esc or q closes. */
export function openTextBgEditor(onChange: () => void): void {
  const dlg = el('dialog', 'pick text-bg-dialog');
  dlg.setAttribute('aria-label', 'Text area background');
  let input = textBg().hex;
  let armed = false;
  const field = el('div', 'text-bg-field');
  const sw = el('span', 'swatch');
  const msg = el('div', 'pick-hint');
  const render = (error = false): void => {
    field.textContent = input || '#';
    field.classList.toggle('empty', input === '');
    field.classList.toggle('error', error);
    const hex = normalizeHex(input);
    sw.style.background = hex ?? 'transparent';
    msg.textContent = error ? 'Not a colour: use #RRGGBB' : textBg().hex ? `Applied: ${textBg().hex}${textBg().on ? '' : ' (the Text area background switch is off)'}` : 'No override: the theme colour';
  };
  dlg.append(el('h2', undefined, 'Text area background'), el('div', 'text-bg-row'), msg,
    el('div', 'pick-hint', 'Type #RRGGBB (applies at 7 characters) · Enter apply · Delete clear · Esc or q close · Space N new session · Space ; palette'));
  dlg.querySelector('.text-bg-row')!.append(sw, field);
  dlg.addEventListener('keydown', (ev) => {
    if (focusOwnsKey(ev.key, ev.target as Element | null)) return;
    if (ev.isComposing) return;
    const r = hexEditKey(input, armed, ev);
    input = r.input;
    armed = r.armed;
    const e = r.effect;
    if (e.kind === 'none' && (ev.ctrlKey || ev.metaKey || ev.altKey)) return;
    ev.preventDefault();
    ev.stopPropagation();
    switch (e.kind) {
      case 'none': render(); return;
      case 'close': dlg.close(); return;
      case 'clear': setTextBg({ ...textBg(), hex: '' }); onChange(); render(); return;
      case 'commit':
        if (e.value === null) { render(true); return; }
        setTextBg({ ...textBg(), hex: e.value });
        onChange();
        render();
        return;
      case 'leader': if (e.run !== 'arm') { dlg.close(); runLeader(e.run); } return;
    }
  });
  dlg.addEventListener('close', () => dlg.remove());
  dlg.addEventListener('click', (ev) => { if (ev.target === dlg) dlg.close(); });
  document.body.append(dlg);
  render();
  dlg.showModal();
  dlg.focus();
}

export const TEXT_BG_CSS = `
.text-bg-dialog .text-bg-row { display: flex; align-items: center; gap: 8px; margin: 6px 0; }
.text-bg-dialog .text-bg-field { font-family: var(--mono); font-size: 16px; padding: 4px 8px; min-width: 9ch; border: 1px solid var(--surface1); border-radius: 4px; }
.text-bg-dialog .text-bg-field.empty { color: var(--subtext); }
.text-bg-dialog .text-bg-field.error { border-color: var(--red); }
.text-bg-dialog .swatch { width: 22px; height: 22px; border-radius: 4px; border: 1px solid var(--surface1); }
`;

// All text is rendered with textContent; no daemon string is ever interpreted as HTML.
// DOM helpers shared by every view module.

export function $<T extends HTMLElement>(id: string): T {
  const el = document.getElementById(id);
  if (!el) throw new Error(`missing #${id}`);
  return el as T;
}

export function el<K extends keyof HTMLElementTagNameMap>(
  tag: K, cls?: string, text?: string,
): HTMLElementTagNameMap[K] {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text !== undefined) e.textContent = text;
  return e;
}

let toastTimer: number | undefined;
export function toast(msg: string, ok = false): void {
  const t = $('toast');
  t.textContent = msg;
  t.className = ok ? 'toast ok' : 'toast';
  t.hidden = false;
  window.clearTimeout(toastTimer);
  toastTimer = window.setTimeout(() => { t.hidden = true; }, ok ? 2500 : 6000);
}

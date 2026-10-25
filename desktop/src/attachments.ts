// Attaching images to a message (local #267): the Attach image button, dropping image files onto the
// conversation or the message box, and the removable thumbnails. All three, and the paste, send the
// image through the one paste path (UploadPasteImage, which stores it under ~/.rsi/paste/rsid/ on the
// daemon host) and put `@path` in the message. The rules are in attachments-model.ts.

import { invoke } from './api.js';
import {
  IMAGE_ACCEPT, attachSummary, chipLabel, dropVerdict, hasRef, judgeImageFile, removeRef,
} from './attachments-model.js';
import { $, el, toast } from './dom.js';
import { dataUrlBase64, imageReference } from './input-model.js';
import { isReadOnly, selected, state } from './state.js';

interface Chip { readonly path: string; readonly node: HTMLElement; readonly url: string | null }

/** The images attached to the message being written, by saved path. */
const chips = new Map<string, Chip>();

const box = (): HTMLTextAreaElement => $<HTMLTextAreaElement>('message');
const tray = (): HTMLElement => $('attach-tray');

/** Read a file as base64 (a `data:` URL, its payload kept). */
function fileBase64(file: Blob): Promise<string> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onerror = () => reject(reader.error ?? new Error('cannot read the image'));
    reader.onload = () => {
      const data = typeof reader.result === 'string' ? dataUrlBase64(reader.result) : null;
      if (data) resolve(data); else reject(new Error('the clipboard image is empty'));
    };
    reader.readAsDataURL(file);
  });
}

function previewUrl(file: Blob): string | null {
  try { return typeof URL.createObjectURL === 'function' ? URL.createObjectURL(file) : null; } catch { return null; }
}

function dropChip(path: string): void {
  const c = chips.get(path);
  if (!c) return;
  chips.delete(path);
  c.node.remove();
  if (c.url) { try { URL.revokeObjectURL(c.url); } catch { /* nothing to free */ } }
  tray().hidden = chips.size === 0;
}

/** Remove a thumbnail and the `@path` it stands for from the message. */
function removeAttachment(path: string): void {
  const m = box();
  const next = removeRef(m.value, path);
  if (next !== m.value) { m.value = next; m.dispatchEvent(new Event('input', { bubbles: true })); }
  dropChip(path);
  if (!m.disabled) m.focus();
}

/** A thumbnail with the file name and a remove button, shown above the message box. */
function addChip(path: string, name: string, file: Blob): void {
  if (chips.has(path)) return;
  const label = chipLabel(name, path);
  const node = el('li', 'attach-chip');
  node.dataset.path = path;
  const url = previewUrl(file);
  const img = el('img', 'attach-thumb');
  img.alt = '';
  img.title = path;
  if (url) {
    img.src = url;
    img.addEventListener('error', () => { img.hidden = true; });
    node.append(img);
  }
  const text = el('span', 'attach-name', label);
  text.title = path;
  const remove = el('button', 'attach-remove', '×');
  remove.type = 'button';
  remove.title = `Remove ${label}`;
  remove.setAttribute('aria-label', `Remove image ${label}`);
  remove.addEventListener('click', () => removeAttachment(path));
  node.append(text, remove);
  tray().append(node);
  chips.set(path, { path, node, url });
  tray().hidden = false;
}

/** Drop the thumbnails whose `@path` is no longer in the message (it was sent, cleared or edited away). */
function syncChips(): void {
  const text = box().value;
  for (const path of [...chips.keys()]) if (!hasRef(text, path)) dropChip(path);
}

/**
 * Save one image with rsid and put `@path` in `field` at `at` (the end when null). Shared by the paste, the
 * Attach image button and a drop. Resolves to the saved path; a thumbnail is added when the field is the message box.
 */
export async function insertImage(field: HTMLTextAreaElement | HTMLInputElement, file: Blob, mime: string, name: string, at: number | null): Promise<string> {
  const data = await fileBase64(file);
  const r = await invoke<{ path: string }>('upload_paste_image', { request: { mime, data_base64: data } });
  const ref = imageReference(r.path);
  const from = at ?? field.value.length;
  field.setRangeText(ref, from, at === null ? from : (field.selectionEnd ?? from), 'end');
  field.dispatchEvent(new Event('input', { bubbles: true }));
  if (field === box()) addChip(r.path, name, file);
  return r.path;
}

/** Attach image files to the message box: refused files get a plain message, the rest are saved and shown. */
export async function attachFiles(files: readonly File[]): Promise<void> {
  const refusals: string[] = [];
  const failures: string[] = [];
  let attached = 0;
  for (const f of files) {
    const v = judgeImageFile(f);
    if (!v.ok) { refusals.push(v.message); continue; }
    try {
      await insertImage(box(), f, v.mime, f.name, null);
      attached += 1;
    } catch (e) {
      failures.push(`"${f.name}" was not attached: ${String(e)}`);
    }
  }
  if (files.length === 0) return;
  const s = attachSummary(attached, refusals, failures);
  toast(s.text, s.ok);
}

// ---------- the button ----------

function canAttach(): boolean {
  const s = selected();
  return !!state.selectedId && !!s && !isReadOnly(s) && !box().disabled;
}

function onPick(): void {
  const input = $<HTMLInputElement>('attach-file');
  const picked = [...(input.files ?? [])];
  input.value = ''; // the same file can be chosen again
  if (picked.length === 0) return;
  if (!canAttach()) { toast('Open a session that can take a message, then attach the image.'); return; }
  void attachFiles(picked);
}

// ---------- drop ----------

const hasFiles = (ev: DragEvent): boolean => [...(ev.dataTransfer?.types ?? [])].includes('Files');
const overSession = (ev: DragEvent): boolean => {
  const t = ev.target;
  return t instanceof Node && !$('session-view').hidden && $('session-view').contains(t);
};
const modalOpen = (): boolean => document.querySelector('dialog[open]') !== null;

function dragEnv(ev: DragEvent) {
  return { files: hasFiles(ev), overSession: overSession(ev), modalOpen: modalOpen(), readOnly: isReadOnly(selected()) };
}

function markDrop(on: boolean): void {
  $('session-view').classList.toggle('drop-target', on);
}

function onDragOver(ev: DragEvent): void {
  const env = dragEnv(ev);
  if (!env.files) return;
  ev.preventDefault(); // else the webview opens the dropped file in place of the app
  const v = dropVerdict(env);
  if (ev.dataTransfer) ev.dataTransfer.dropEffect = v.kind === 'attach' ? 'copy' : 'none';
  markDrop(v.kind === 'attach');
}

function onDrop(ev: DragEvent): void {
  const env = dragEnv(ev);
  if (!env.files) return;
  ev.preventDefault();
  markDrop(false);
  const v = dropVerdict(env);
  if (v.kind === 'refuse') { toast(v.message); return; }
  if (v.kind === 'attach') void attachFiles([...(ev.dataTransfer?.files ?? [])]);
}

/** Wire the Attach image button, the file drop and the thumbnails (once, at boot). */
export function initAttachments(): void {
  const input = $<HTMLInputElement>('attach-file');
  input.accept = IMAGE_ACCEPT;
  input.addEventListener('change', onPick);
  $('attach-image').addEventListener('click', () => {
    if (!canAttach()) { toast('Open a session that can take a message, then attach the image.'); return; }
    input.click();
  });
  box().addEventListener('input', syncChips);
  document.addEventListener('dragover', onDragOver, true);
  document.addEventListener('drop', onDrop, true);
  document.addEventListener('dragleave', (ev) => {
    if (!hasFiles(ev)) return;
    if (ev.relatedTarget === null || !(ev.relatedTarget instanceof Node) || !$('session-view').contains(ev.relatedTarget)) markDrop(false);
  }, true);
  document.addEventListener('dragend', () => markDrop(false), true);
}

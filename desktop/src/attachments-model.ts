// Attaching images to a message (local #267): the pure rules. The button, the drop and the paste all
// send the image through the one paste path (UploadPasteImage, hub #3044) with the same type and size
// limits (src-tauri/src/input.rs: PNG, JPEG, GIF, WebP, up to 32 MiB), then put `@path` in the message.
// This file decides which files are accepted, what a refusal says, and where a drop may land.

import { imageExtension, pasteImageMime } from './input-model.js';

/** Largest image rsid stores (rsi_common::input_assets::MAX_PASTE_IMAGE_BYTES; src-tauri/src/input.rs). */
export const MAX_IMAGE_BYTES = 32 * 1024 * 1024;

/** The `accept` list of the file picker: only the types the paste path takes. */
export const IMAGE_ACCEPT = 'image/png,image/jpeg,image/gif,image/webp';

/** The part of a File the rules need (a test passes a plain object). */
export interface PickedFile {
  readonly name: string;
  readonly type: string;
  readonly size: number;
}

export type FileVerdict =
  | { readonly ok: true; readonly mime: string }
  | { readonly ok: false; readonly message: string };

/** The MIME type to upload for a file: its own type, else (an OS drop may carry none) its extension. */
export function imageMimeOf(f: PickedFile): string | null {
  const fromType = pasteImageMime(f.type);
  if (fromType) return fromType;
  if (f.type.trim() !== '') return null; // a known non-image type (a PDF, a HEIC) is refused, whatever its name says
  const ext = /\.([A-Za-z0-9]+)$/u.exec(f.name)?.[1]?.toLowerCase();
  if (!ext) return null;
  return pasteImageMime(ext === 'jpg' || ext === 'jpeg' ? 'image/jpeg' : `image/${ext}`);
}

const mb = (bytes: number): string => `${Math.max(1, Math.round(bytes / (1024 * 1024)))} MB`;

/** Accept or refuse one file, with a plain sentence for a refusal. */
export function judgeImageFile(f: PickedFile): FileVerdict {
  const name = f.name.trim() === '' ? 'This file' : `"${f.name}"`;
  const mime = imageMimeOf(f);
  if (!mime || imageExtension(mime) === null) {
    return { ok: false, message: `${name} was not attached: only PNG, JPEG, GIF and WebP images can be attached.` };
  }
  if (f.size <= 0) return { ok: false, message: `${name} was not attached: the file is empty.` };
  if (f.size > MAX_IMAGE_BYTES) {
    return { ok: false, message: `${name} was not attached: it is ${mb(f.size)} and images can be up to ${mb(MAX_IMAGE_BYTES)}.` };
  }
  return { ok: true, mime };
}

/** One toast for a batch: the refusals (each on its own line), or how many were attached. */
export function attachSummary(attached: number, refusals: readonly string[], failures: readonly string[]): { text: string; ok: boolean } {
  const problems = [...refusals, ...failures];
  if (problems.length > 0) return { text: problems.join('\n'), ok: false };
  return { text: attached === 1 ? 'Image attached' : `${attached} images attached`, ok: true };
}

/** The label under a thumbnail: the file name, else the saved file's name. */
export function chipLabel(name: string, path: string): string {
  const n = name.trim();
  if (n !== '') return n;
  return path.split(/[\\/]/u).at(-1) ?? path;
}

const isSpace = (c: string | undefined): boolean => c === undefined || /\s/u.test(c);

/** Where `@path` stands in `text` as a whole word (followed by space or the end), or -1. */
export function refIndex(text: string, path: string): number {
  const ref = `@${path}`;
  let from = 0;
  for (;;) {
    const at = text.indexOf(ref, from);
    if (at < 0) return -1;
    if (isSpace(text[at + ref.length]) && (at === 0 || isSpace(text[at - 1]))) return at;
    from = at + 1;
  }
}

/** Whether the message still carries the reference to this image. */
export const hasRef = (text: string, path: string): boolean => refIndex(text, path) >= 0;

/** The message without the first `@path` reference and the one space after it (the paste inserted `@path `). */
export function removeRef(text: string, path: string): string {
  const at = refIndex(text, path);
  if (at < 0) return text;
  const end = at + 1 + path.length;
  return text.slice(0, at) + text.slice(text[end] === ' ' ? end + 1 : end);
}

export interface DropEnv {
  /** The drag carries files (not text or a link). */
  readonly files: boolean;
  /** A session is open and the pointer is over its conversation or message box. */
  readonly overSession: boolean;
  /** A dialog or overlay is open. */
  readonly modalOpen: boolean;
  /** The open session cannot take a message (archived). */
  readonly readOnly: boolean;
}

export type DropVerdict =
  | { readonly kind: 'ignore' }
  | { readonly kind: 'attach' }
  | { readonly kind: 'refuse'; readonly message: string };

/** What a file drop does where it lands. A drop of text or a link is left alone. */
export function dropVerdict(env: DropEnv): DropVerdict {
  if (!env.files) return { kind: 'ignore' };
  if (env.modalOpen) return { kind: 'refuse', message: 'Close the open window, then drop the image onto the conversation or the message box.' };
  if (!env.overSession) return { kind: 'refuse', message: 'To attach an image, open a session and drop it onto the conversation or the message box.' };
  if (env.readOnly) return { kind: 'refuse', message: 'This session is archived (read-only), so images cannot be attached.' };
  return { kind: 'attach' };
}

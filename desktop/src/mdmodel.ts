// Pure helpers for Markdown rendering (no DOM), unit-tested under tests/.

/** Texts per `render_markdown` call; the backend bound is 200 texts / 2 MiB. */
export const BATCH_TEXTS = 40;
export const BATCH_BYTES = 512 * 1024;
const CACHE_MAX = 6000;

/** Split a long message for folding, preferring a line boundary near `limit`. */
export function splitFold(text: string, limit: number): [string, string] {
  if (text.length <= limit) return [text, ''];
  const nl = text.lastIndexOf('\n', limit);
  const cut = nl > limit / 2 ? nl + 1 : limit;
  return [text.slice(0, cut), text.slice(cut)];
}

/** Cache key for one rendered chunk of an event. */
export function cacheKey(sessionId: string, e: { id: number; sequence: number }, part = 0): string {
  return `${sessionId}:${e.id}:${e.sequence}:${part}`;
}

/** Rendered-HTML cache. An entry only hits while its source text is unchanged. */
export class MarkdownCache {
  private readonly map = new Map<string, { src: string; html: string }>();

  get(key: string, src: string): string | undefined {
    const hit = this.map.get(key);
    return hit && hit.src === src ? hit.html : undefined;
  }

  set(key: string, src: string, html: string): void {
    if (this.map.size >= CACHE_MAX) {
      // Drop the oldest quarter (Map keeps insertion order).
      let drop = Math.ceil(CACHE_MAX / 4);
      for (const k of this.map.keys()) {
        if (drop-- <= 0) break;
        this.map.delete(k);
      }
    }
    this.map.set(key, { src, html });
  }

  get size(): number { return this.map.size; }
}

/**
 * Take the next batch from the end of the queue (newest first, since the
 * newest messages are the ones on screen), bounded by count and bytes. Always
 * takes at least one item.
 */
export function takeBatch<T extends { src: string }>(
  queue: T[], maxTexts = BATCH_TEXTS, maxBytes = BATCH_BYTES,
): T[] {
  const out: T[] = [];
  let bytes = 0;
  while (queue.length && out.length < maxTexts) {
    const next = queue[queue.length - 1]!;
    if (out.length && bytes + next.src.length > maxBytes) break;
    bytes += next.src.length;
    out.push(queue.pop()!);
  }
  return out;
}

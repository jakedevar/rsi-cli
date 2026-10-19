// The create-entity form's keys (gap sweep G4, hub #3000), pure (no DOM):
// docs/keybindings.md "Create Entity / Normal", "/ Insert", "/ Body" and
// "/ Topology" (crates/rsi/src/overlay/create_entity_form.rs). The desktop
// form is a dialog of native fields: "insert" is focus in a text box, "normal"
// is focus anywhere else in the dialog (a select, a check box, a button, the
// dialog itself). actions/hierarchy-run.ts runs the commands.

export type EntityField = 'kind' | 'name' | 'tags' | 'parent' | 'topology' | 'provider' | 'model' | 'effort' | 'sandbox' | 'prompt' | 'other';

export type FormCmd =
  | { kind: 'focus'; field: EntityField }
  | { kind: 'kind'; by: 1 | -1 }
  | { kind: 'provider'; by: 1 | -1 }
  | { kind: 'effort'; by: 1 | -1 }
  | { kind: 'sandbox' }
  | { kind: 'model-picker' }
  | { kind: 'parent-picker' }
  | { kind: 'create' }
  | { kind: 'close' }
  | { kind: 'discard' }
  /** Esc in a text box: back to the form's normal mode. */
  | { kind: 'leave-edit' }
  | { kind: 'topology-move'; by: number }
  | { kind: 'topology-preview' }
  | { kind: 'topology-choose' }
  | { kind: 'topology-clear' }
  | { kind: 'swallow' }
  | { kind: 'pass' };

export interface FormKeyCtx {
  /** The focused field. */
  field: EntityField;
  /** Focus is in a text box taking typed text (name, tags, model, or the body in insert mode). */
  editing: boolean;
  /** `g` waiting for `p`. */
  pending: '' | 'g';
}

export interface KeyLike { key: string; ctrlKey?: boolean; shiftKey?: boolean; altKey?: boolean; metaKey?: boolean }

const FOCUS_KEYS: Readonly<Record<string, EntityField>> = { n: 'name', b: 'prompt', k: 'kind', T: 'tags', t: 'topology' };

/** One key in the form; `mac` makes Cmd stand in for Ctrl on the Enter and D chords. */
export function formKey(ctx: FormKeyCtx, k: KeyLike, mac = false): { cmd: FormCmd; pending: '' | 'g' } {
  const done = (cmd: FormCmd) => ({ cmd, pending: '' as const });
  const ctrl = !!k.ctrlKey || (mac && !!k.metaKey);
  // Any mode.
  if (k.key === 'Enter' && ctrl) return done({ kind: 'create' });
  if (ctrl && !k.altKey && k.key.toLowerCase() === 'd' && !k.shiftKey) return done({ kind: 'discard' });
  if (k.altKey || (k.metaKey && !mac)) return done({ kind: 'pass' });
  if (ctx.editing) {
    if (k.key === 'Escape') return done(ctx.field === 'prompt' ? { kind: 'pass' } : { kind: 'leave-edit' });
    return done({ kind: 'pass' });
  }
  if (ctx.field === 'topology') {
    switch (k.key) {
      case 'j': case 'ArrowDown': return done({ kind: 'topology-move', by: 1 });
      case 'k': case 'ArrowUp': return done({ kind: 'topology-move', by: -1 });
      case 'G': return done({ kind: 'topology-move', by: Infinity });
      case ' ': return done({ kind: 'topology-preview' });
      case 'Enter': return done({ kind: 'topology-choose' });
      case 'Escape': return done({ kind: 'topology-clear' });
      default: break;
    }
  }
  if (ctrl && k.key.toLowerCase() === 'p') return done({ kind: 'provider', by: k.shiftKey ? -1 : 1 });
  if (ctrl) return done({ kind: 'pass' });
  if (ctx.pending === 'g') {
    if (k.key === 'p') return done({ kind: 'parent-picker' });
    // Any other key cancels the chord and then acts normally.
  }
  if (ctx.field === 'kind' && (k.key === 'h' || k.key === 'ArrowLeft')) return done({ kind: 'kind', by: -1 });
  if (ctx.field === 'kind' && (k.key === 'l' || k.key === 'ArrowRight')) return done({ kind: 'kind', by: 1 });
  const focus = FOCUS_KEYS[k.key];
  if (focus) return done({ kind: 'focus', field: focus });
  switch (k.key) {
    case 'i': return done({ kind: 'focus', field: ['name', 'tags', 'model', 'prompt'].includes(ctx.field) ? ctx.field : 'name' });
    case 'e': return done({ kind: 'effort', by: 1 });
    case 'E': return done({ kind: 'effort', by: -1 });
    case 's': return done({ kind: 'sandbox' });
    case 'm': return done({ kind: 'model-picker' });
    case 'g': return { cmd: { kind: 'swallow' }, pending: 'g' };
    case 'Enter': return done(ctx.field === 'other' || ctx.field === 'kind' || ctx.field === 'sandbox' || ctx.field === 'effort' || ctx.field === 'provider' || ctx.field === 'parent' ? { kind: 'create' } : { kind: 'pass' });
    case 'Escape': return done({ kind: 'close' });
    default: return done({ kind: 'pass' });
  }
}

/** Step through a list of choices, wrapping (kind, provider, effort). */
export function stepChoice<T>(list: readonly T[], current: T, by: 1 | -1): T {
  if (list.length === 0) return current;
  const i = list.indexOf(current);
  if (i < 0) return list[0]!;
  return list[(i + by + list.length) % list.length]!;
}

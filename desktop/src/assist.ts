// Assist overlays, DOM side (hub #3015 slice 13B): memory search, the AI
// command and AI chat, the dialectic and the entity-card editor. Every daemon
// call is one Tauri command in src-tauri/src/assist.rs; the model runs inside
// rsid (GenerateText / QueryMemory), never from the desktop. The rules are in
// assist-model.ts (pure, tested); the registry entries in actions/assist.ts.
// Daemon text is set with textContent only.
import { invoke } from './api.js';
import type { Invocation } from './commands.js';
import { el, toast } from './dom.js';
import {
  type CardState, type CardWho, type DialecticSource, type MemoryResult, type Turn,
  activeProject, addFact, ask, assistRefusal, cancelEdit, cardNavKey, cardTitle, commitEdit, escapeAction,
  factsToSave, generatedText, memoryDetail, memoryHeader, memoryQueryToSend, moveSelection, openCard,
  parseCardCommand, parseMemoryResults, parseQueryResponse, previewKey, qCloses, questionToSend, reply, sourceLine,
  askPrefill,
} from './assist-model.js';
import { isReadOnly, projectName, selected } from './state.js';
import { button, dialogEl, focusDialog, overlayKey } from './actions/detail.js';
import type { AssistActionId, AssistSnapshot } from './actions/assist.js';
import { focusOwnsKey } from './modal-gate.js';

const CSS = `
.assist-dialog { width: min(760px, 92vw); }
.assist-dialog h2 { margin: 0 0 8px; font-size: 15px; }
.assist-input { width: 100%; box-sizing: border-box; padding: 6px 8px; background: var(--mantle); border: 1px solid var(--surface1); border-radius: 4px; color: var(--text); font: inherit; }
.assist-status { min-height: 16px; margin: 6px 0; font-size: 11px; color: var(--subtext); }
.assist-results { list-style: none; margin: 0; padding: 0; max-height: 52vh; overflow: auto; }
.assist-result { padding: 5px 8px; border-radius: 6px; cursor: pointer; }
.assist-result.sel { background: var(--surface0); }
.assist-result-head { font-weight: 600; font-family: var(--mono); font-size: 12px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.assist-result-snippet { color: var(--subtext); font-size: 12px; white-space: pre-wrap; max-height: 5.4em; overflow: hidden; }
.assist-log { max-height: 48vh; min-height: 120px; overflow: auto; padding: 6px; margin-bottom: 8px; background: var(--mantle); border: 1px solid var(--surface0); border-radius: 6px; }
.assist-msg { margin: 0 0 8px; white-space: pre-wrap; word-break: break-word; }
.assist-msg .who { display: block; font-size: 11px; color: var(--overlay0); }
.assist-msg.user .who { color: var(--blue); }
.assist-msg.assistant .who { color: var(--green); }
.assist-source-text { max-height: 7em; overflow: auto; margin: 0 0 8px; padding: 6px; font-family: var(--mono); font-size: 12px; white-space: pre-wrap; background: var(--crust); border-radius: 6px; color: var(--subtext); }
.assist-preview { max-height: 40vh; overflow: auto; margin: 0 0 8px; padding: 8px; font-family: var(--mono); font-size: 12px; white-space: pre-wrap; background: var(--crust); border: 1px solid var(--green); border-radius: 6px; }
.assist-sources { list-style: none; margin: 0 0 8px; padding: 0; font-size: 12px; color: var(--subtext); }
.card-facts { list-style: none; margin: 0 0 8px; padding: 0; max-height: 56vh; overflow: auto; counter-reset: fact; }
.card-fact { display: flex; gap: 8px; padding: 4px 8px; border-radius: 6px; cursor: pointer; }
.card-fact::before { counter-increment: fact; content: counter(fact) "."; color: var(--overlay0); min-width: 2em; text-align: right; }
.card-fact.sel { background: var(--surface0); }
.card-fact .assist-input { flex: 1; }
.card-fact.pending { outline: 1px dashed var(--red); }
`;

// ---------- shared ----------

function hint(text: string): HTMLElement {
  return el('div', 'dialog-note', text);
}

function isTextField(t: EventTarget | null): boolean {
  return t instanceof HTMLElement && !!t.closest('input, textarea, select, [contenteditable=""], [contenteditable="true"]');
}

/** The project the overlays act on: the open session's, else the list's project filter. */
function currentProject(): string | null {
  const filter = document.getElementById('project-filter') as HTMLSelectElement | null;
  return activeProject(selected()?.project_id ?? null, filter?.value || null);
}

function messageBox(): HTMLTextAreaElement | null {
  return document.getElementById('message') as HTMLTextAreaElement | null;
}

export function assistSnapshot(): AssistSnapshot {
  const s = selected();
  const box = messageBox();
  return { draft: !!s && !isReadOnly(s) && !!box && box.value.trim() !== '' };
}

/** A conversation log, one block per message. */
function drawLog(log: HTMLElement, turns: readonly Turn[], pending: boolean): void {
  log.replaceChildren(...turns.map((t) => {
    const p = el('p', `assist-msg ${t.role}`);
    p.append(el('span', 'who', t.role === 'user' ? 'You' : 'Answer'), document.createTextNode(t.content));
    return p;
  }));
  if (pending) log.append(el('p', 'assist-msg pending', 'Thinking…'));
  log.scrollTop = log.scrollHeight;
}

// ---------- memory search ----------

function openMemorySearch(): void {
  const existing = document.getElementById('memory-dialog') as HTMLDialogElement | null;
  if (existing?.open) { existing.close(); return; }
  const dlg = dialogEl('memory-dialog');
  dlg.classList.add('assist-dialog');
  const input = el('input', 'assist-input');
  input.type = 'search';
  input.placeholder = 'Search memory…';
  input.setAttribute('aria-label', 'Search memory');
  const status = el('div', 'assist-status');
  const list = el('ul', 'assist-results');
  list.setAttribute('role', 'listbox');
  list.setAttribute('aria-label', 'Memory results');
  let results: MemoryResult[] = [];
  let sel = 0;
  let seq = 0;

  const draw = (): void => {
    list.replaceChildren(...results.map((r, i) => {
      const li = el('li', `assist-result${i === sel ? ' sel' : ''}`);
      li.setAttribute('role', 'option');
      li.setAttribute('aria-selected', String(i === sel));
      li.append(el('div', 'assist-result-head', memoryHeader(r)), el('div', 'assist-result-snippet', r.snippet));
      li.title = memoryDetail(r);
      li.addEventListener('click', () => { sel = i; draw(); input.focus(); });
      return li;
    }));
    list.querySelector('.sel')?.scrollIntoView({ block: 'nearest' });
  };

  const search = (): void => {
    const q = memoryQueryToSend(input.value);
    const mine = ++seq;
    sel = 0;
    if (q === null) { results = []; status.textContent = ''; draw(); return; }
    status.textContent = 'Searching…';
    invoke<unknown>('memory_search', { query: q }).then((v) => {
      if (mine !== seq) return;
      results = parseMemoryResults(v);
      status.textContent = results.length === 0 ? 'No results' : `${results.length} result${results.length === 1 ? '' : 's'}`;
      draw();
    }, (e: unknown) => {
      if (mine !== seq) return;
      results = [];
      status.textContent = `Memory search failed: ${String(e)}`;
      draw();
    });
  };

  input.addEventListener('input', search);
  dlg.onkeydown = (ev) => {
    if (focusOwnsKey(ev.key, ev.target as Element | null)) return;
    if (ev.isComposing) return;
    const ctrl = ev.ctrlKey || ev.metaKey;
    if (ev.key === 'ArrowDown' || (ctrl && ev.key === 'n')) { ev.preventDefault(); sel = moveSelection(results.length, sel, 1); draw(); }
    else if (ev.key === 'ArrowUp' || (ctrl && ev.key === 'p')) { ev.preventDefault(); sel = moveSelection(results.length, sel, -1); draw(); }
    else if (ev.key === 'Escape') { ev.preventDefault(); dlg.close(); }
  };
  const close = button('Close');
  close.addEventListener('click', () => dlg.close());
  const actions = el('div', 'actions');
  actions.append(close);
  const box = el('div', 'launch-form');
  box.append(el('h2', undefined, 'Memory search'), input, status, list, actions,
    hint('Type to search the daemon’s memory index (ten best). Up/Down or Ctrl-N/Ctrl-P move, Esc closes.'));
  dlg.replaceChildren(box);
  draw();
  dlg.showModal();
  input.focus();
}

// ---------- AI command ----------

function sourceOf(target: HTMLTextAreaElement | null): HTMLTextAreaElement | null {
  return target ?? messageBox();
}

function openAiCommand(source: HTMLTextAreaElement): void {
  const sourceText = source.value;
  const refusal = assistRefusal('command', sourceText);
  if (refusal) { toast(refusal); return; }
  const dlg = dialogEl('ai-command-dialog');
  dlg.classList.add('assist-dialog');
  const input = el('input', 'assist-input');
  input.type = 'text';
  input.placeholder = 'Instruction, e.g. “make it shorter”';
  input.setAttribute('aria-label', 'Instruction');
  const status = el('div', 'assist-status');
  const preview = el('pre', 'assist-preview');
  preview.hidden = true;
  const run = button('Run', 'btn primary');
  const accept = button('Accept (a)', 'btn primary');
  const discard = button('Discard (d)');
  const cancel = button('Cancel');
  accept.hidden = true;
  discard.hidden = true;
  let phase: 'input' | 'running' | 'preview' = 'input';
  let result = '';
  let live = true;

  const setPhase = (p: typeof phase): void => {
    phase = p;
    input.disabled = p !== 'input';
    run.hidden = p !== 'input';
    accept.hidden = p !== 'preview';
    discard.hidden = p !== 'preview';
    preview.hidden = p !== 'preview';
    status.textContent = p === 'running' ? 'Running through rsid’s prompt processor… (Esc cancels)' : p === 'preview' ? 'a accepts, d or Esc discards' : '';
  };
  const go = (): void => {
    if (phase !== 'input') return;
    const instruction = input.value.trim();
    if (!instruction) return;
    setPhase('running');
    invoke<unknown>('ai_command', { instruction, sourceText }).then((v) => {
      if (!live) return;
      const out = generatedText(v);
      if (!out.ok) { dlg.close(); toast(`AI command failed: ${out.error}`); return; }
      result = out.text;
      preview.textContent = result;
      setPhase('preview');
      focusDialog(dlg);
    }, (e: unknown) => {
      if (!live) return;
      dlg.close();
      toast(`AI command failed: ${String(e)}`);
    });
  };
  const doAccept = (): void => {
    source.value = result;
    source.dispatchEvent(new Event('input', { bubbles: true }));
    dlg.close();
    source.focus();
  };
  run.addEventListener('click', go);
  accept.addEventListener('click', doAccept);
  discard.addEventListener('click', () => dlg.close());
  cancel.addEventListener('click', () => dlg.close());
  dlg.onclose = () => { live = false; };
  dlg.onkeydown = (ev) => {
    if (focusOwnsKey(ev.key, ev.target as Element | null)) return;
    if (ev.isComposing) return;
    if (phase === 'preview') {
      const k = previewKey(ev.key);
      if (k === 'accept' && !ev.ctrlKey && !ev.metaKey) { ev.preventDefault(); doAccept(); }
      else if (k === 'discard') { ev.preventDefault(); dlg.close(); }
      return;
    }
    if (ev.key === 'Escape') { ev.preventDefault(); dlg.close(); }
    else if (ev.key === 'Enter' && phase === 'input') { ev.preventDefault(); go(); }
  };
  const shown = el('pre', 'assist-source-text', sourceText);
  const actions = el('div', 'actions');
  actions.append(cancel, discard, run, accept);
  const box = el('div', 'launch-form');
  box.append(el('h2', undefined, 'AI command'), shown, input, status, preview, actions,
    hint('Enter runs the instruction on the text above; the result is a preview until you accept it.'));
  dlg.replaceChildren(box);
  setPhase('input');
  dlg.showModal();
  input.focus();
}

// ---------- AI chat and the dialectic ----------

interface ConversationOptions {
  id: string;
  title: string;
  placeholder: string;
  /** The text the chat is about (AI chat), shown above the log. */
  sourceText?: string;
  prefill?: string;
  /** Scope line under the title. */
  scope?: string;
  send(turns: Turn[]): Promise<{ turns: Turn[]; sources?: DialecticSource[] }>;
  withSources: boolean;
  hintText: string;
}

function openConversation(o: ConversationOptions): void {
  const dlg = dialogEl(o.id);
  dlg.classList.add('assist-dialog');
  const log = el('div', 'assist-log');
  log.setAttribute('aria-live', 'polite');
  log.tabIndex = 0;
  const input = el('input', 'assist-input');
  input.type = 'text';
  input.placeholder = o.placeholder;
  input.setAttribute('aria-label', o.placeholder);
  input.value = o.prefill ?? '';
  const sourcesBtn = button('Sources (s)');
  const sourcesList = el('ul', 'assist-sources');
  sourcesList.hidden = true;
  sourcesBtn.hidden = !o.withSources;
  let turns: Turn[] = [];
  let sources: DialecticSource[] = [];
  let sourcesOpen = false;
  let inFlight = false;
  let live = true;

  const drawSources = (): void => {
    sourcesBtn.textContent = `Sources (s) · ${sources.length}`;
    sourcesList.hidden = !sourcesOpen || sources.length === 0;
    sourcesList.replaceChildren(...sources.map((s) => el('li', undefined, sourceLine(s))));
  };
  const draw = (): void => {
    drawLog(log, turns, inFlight);
    input.disabled = inFlight;
    if (o.withSources) drawSources();
  };
  const sendNow = (): void => {
    const q = questionToSend(input.value, inFlight);
    if (q === null) return;
    turns = ask(turns, q);
    input.value = '';
    inFlight = true;
    draw();
    o.send(turns).then((r) => {
      if (!live) return;
      inFlight = false;
      turns = r.turns;
      if (r.sources) sources = r.sources;
      draw();
      input.focus();
    }, (e: unknown) => {
      if (!live) return;
      inFlight = false;
      turns = reply(turns, { ok: false, error: String(e) });
      draw();
      input.focus();
    });
  };
  sourcesBtn.addEventListener('click', () => { sourcesOpen = !sourcesOpen; drawSources(); });
  dlg.onclose = () => { live = false; };
  dlg.onkeydown = (ev) => {
    if (focusOwnsKey(ev.key, ev.target as Element | null)) return;
    if (ev.isComposing) return;
    if (ev.key === 'Escape') {
      ev.preventDefault();
      if (escapeAction(input.value, inFlight) === 'clear') input.value = ''; else dlg.close();
      return;
    }
    if (inFlight) return;
    if (ev.key === 'Enter') { ev.preventDefault(); sendNow(); return; }
    if (ev.key === 'q' && !ev.ctrlKey && !ev.metaKey && qCloses(input.value, inFlight)) { ev.preventDefault(); dlg.close(); return; }
    if (o.withSources && ev.key === 's' && !ev.ctrlKey && !ev.metaKey && input.value === '') {
      ev.preventDefault(); sourcesOpen = !sourcesOpen; drawSources(); return;
    }
    if (ev.key === 'ArrowUp') { ev.preventDefault(); log.scrollTop -= 40; }
    else if (ev.key === 'ArrowDown') { ev.preventDefault(); log.scrollTop += 40; }
  };
  const send = button('Send', 'btn primary');
  send.addEventListener('click', sendNow);
  const close = button('Close');
  close.addEventListener('click', () => dlg.close());
  const actions = el('div', 'actions');
  actions.append(sourcesBtn, close, send);
  const box = el('div', 'launch-form');
  box.append(el('h2', undefined, o.title));
  if (o.scope) box.append(hint(o.scope));
  if (o.sourceText !== undefined) box.append(el('pre', 'assist-source-text', o.sourceText));
  box.append(log, sourcesList, input, actions, hint(o.hintText));
  dlg.replaceChildren(box);
  draw();
  dlg.showModal();
  input.focus();
}

function openAiChat(source: HTMLTextAreaElement): void {
  const sourceText = source.value;
  const refusal = assistRefusal('chat', sourceText);
  if (refusal) { toast(refusal); return; }
  openConversation({
    id: 'ai-chat-dialog',
    title: 'AI chat',
    placeholder: 'Ask about this text…',
    sourceText,
    withSources: false,
    send: async (turns) => {
      const v = await invoke<unknown>('ai_chat', { sourceText, turns });
      return { turns: reply(turns, generatedText(v)) };
    },
    hintText: 'Enter asks rsid’s prompt processor; the text is never changed. Up/Down scroll, Esc clears then closes, q closes an empty input.',
  });
}

function openDialectic(prefill: string): void {
  const existing = document.getElementById('dialectic-dialog') as HTMLDialogElement | null;
  if (existing?.open && prefill === '') { existing.close(); return; }
  const projectId = currentProject();
  const name = projectName(projectId);
  openConversation({
    id: 'dialectic-dialog',
    title: 'Ask (dialectic)',
    placeholder: 'Ask about what rsi has learned…',
    prefill,
    scope: projectId ? `Project: ${name ?? projectId.slice(0, 8)}` : 'All projects',
    withSources: true,
    send: async (turns) => {
      const query = turns[turns.length - 1]!.content;
      const v = await invoke<unknown>('query_memory', { query, projectId, history: turns });
      const r = parseQueryResponse(v);
      if (!r) return { turns: reply(turns, { ok: false, error: 'the daemon sent no answer' }) };
      return { turns: reply(turns, { ok: true, text: r.answer }), sources: r.sources };
    },
    hintText: 'Enter sends, s (empty input) shows the sources, Up/Down scroll, Esc clears then closes, q closes an empty input.',
  });
}

// ---------- entity cards ----------

interface CardRef { who: CardWho; entityType: 'project' | 'user'; entityId: string; title: string }

function cardRef(who: CardWho): CardRef | null {
  if (who === 'user') return { who, entityType: 'user', entityId: 'self', title: cardTitle('user', undefined) };
  const project = currentProject();
  if (!project) return null;
  return { who, entityType: 'project', entityId: project, title: cardTitle('project', projectName(project)) };
}

async function loadFacts(ref: CardRef): Promise<string[]> {
  const card = await invoke<unknown>('get_entity_card', { entityType: ref.entityType, entityId: ref.entityId });
  const facts = card && typeof card === 'object' ? (card as { facts?: unknown }).facts : undefined;
  return Array.isArray(facts) ? facts.filter((f): f is string => typeof f === 'string') : [];
}

function saveFacts(ref: CardRef, facts: string[]): Promise<unknown> {
  return invoke('set_entity_card', { entityType: ref.entityType, entityId: ref.entityId, facts });
}

async function addCardFact(who: CardWho, fact: string): Promise<void> {
  const ref = cardRef(who);
  if (!ref) { toast('No active project'); return; }
  try {
    const facts = addFact(await loadFacts(ref), fact);
    if (!facts) { toast(`${who === 'user' ? 'User' : 'Project'} card is full (40 facts max)`); return; }
    await saveFacts(ref, facts);
    toast(`Added fact to ${who} card`, true);
  } catch (e) {
    toast(`Failed: ${String(e)}`);
  }
}

async function openCardEditor(who: CardWho): Promise<void> {
  const ref = cardRef(who);
  if (!ref) { toast('No active project — select a project first'); return; }
  let facts: string[];
  try {
    facts = await loadFacts(ref);
  } catch (e) {
    toast(`Failed to load ${who === 'user' ? 'user ' : ''}card: ${String(e)}`);
    return;
  }
  let st: CardState = openCard(facts);
  const dlg = dialogEl('card-dialog');
  dlg.classList.add('assist-dialog');
  const list = el('ul', 'card-facts');
  list.setAttribute('role', 'listbox');
  list.setAttribute('aria-label', 'Card facts');
  const status = el('div', 'assist-status');
  let editor: HTMLInputElement | null = null;

  const save = (notice = false): void => {
    saveFacts(ref, factsToSave(st)).then(
      () => { if (notice) toast('Card saved', true); },
      (e: unknown) => toast(`Failed to save card: ${String(e)}`),
    );
  };
  const draw = (): void => {
    editor = null;
    list.replaceChildren(...st.facts.map((f, i) => {
      const li = el('li', `card-fact${i === st.selected ? ' sel' : ''}${i === st.selected && st.pendingDelete ? ' pending' : ''}`);
      li.setAttribute('role', 'option');
      li.setAttribute('aria-selected', String(i === st.selected));
      if (i === st.selected && st.editing !== null) {
        const inp = el('input', 'assist-input');
        inp.type = 'text';
        inp.value = st.editing;
        inp.setAttribute('aria-label', `Fact ${i + 1}`);
        editor = inp;
        li.append(inp);
      } else {
        li.append(el('span', undefined, f));
        li.addEventListener('click', () => { if (st.editing === null) { st = { ...st, selected: i, pendingDelete: false }; draw(); } });
        li.addEventListener('dblclick', () => { if (st.editing === null) { st = { ...st, selected: i, editing: st.facts[i] ?? '' }; draw(); } });
      }
      return li;
    }));
    if (st.facts.length === 0) list.append(el('li', 'pick-empty', 'No facts yet: a adds one.'));
    status.textContent = st.editing !== null ? 'Enter commits (empty deletes), Esc cancels'
      : st.pendingDelete ? 'd again deletes the fact' : `${st.facts.length}/40 facts`;
    list.querySelector('.sel')?.scrollIntoView({ block: 'nearest' });
    if (editor) { const ed: HTMLInputElement = editor; ed.focus(); ed.setSelectionRange(ed.value.length, ed.value.length); }
    else focusDialog(dlg);
  };
  dlg.onclose = () => { save(); };
  dlg.onkeydown = (ev) => {
    if (focusOwnsKey(ev.key, ev.target as Element | null)) return;
    if (ev.isComposing) return;
    if (st.editing !== null) {
      if (ev.key === 'Enter') { ev.preventDefault(); const r = commitEdit(st, editor?.value ?? ''); st = r.st; draw(); save(); }
      else if (ev.key === 'Escape') { ev.preventDefault(); st = cancelEdit(st); draw(); }
      return;
    }
    if (overlayKey(ev, dlg)) return;
    const r = cardNavKey(st, { key: ev.key, ctrl: ev.ctrlKey || ev.metaKey, alt: ev.altKey });
    if (r.effect === 'none' && r.st === st) return;
    ev.preventDefault();
    st = r.st;
    if (r.effect === 'close') { dlg.close(); return; }
    if (r.effect === 'full') toast('Card is full (40 facts max)');
    draw();
    if (r.effect === 'save') save();
    else if (r.effect === 'saved-notice') save(true);
  };
  const add = button('Add (a)');
  add.addEventListener('click', () => { const r = cardNavKey(st, { key: 'a' }); st = r.st; if (r.effect === 'full') toast('Card is full (40 facts max)'); draw(); });
  const close = button('Save and close', 'btn primary');
  close.addEventListener('click', () => dlg.close());
  const actions = el('div', 'actions');
  actions.append(add, close);
  const box = el('div', 'launch-form');
  box.append(el('h2', undefined, `${ref.title} card`),
    hint('Facts are injected into session context at launch.'), list, status, actions,
    hint('j/k move, g/G first/last, a add, e/i/Enter edit, dd delete, J/K reorder, Ctrl-S save, Esc/q save and close.'));
  dlg.replaceChildren(box);
  dlg.showModal();
  draw();
}

// ---------- dispatch and wiring ----------

export function runAssist(id: AssistActionId, inv: Invocation): void {
  switch (id) {
    case 'memory-search': openMemorySearch(); return;
    case 'dialectic': openDialectic(askPrefill(inv.arg)); return;
    case 'card': {
      const c = parseCardCommand(inv.arg);
      if (c.kind === 'invalid') toast(`Unknown command: ${c.text}`);
      else if (c.kind === 'add') void addCardFact(c.who, c.fact);
      else void openCardEditor(c.who);
      return;
    }
    case 'card-user': void openCardEditor('user'); return;
    case 'ai-command': { const s = sourceOf(null); if (s) openAiCommand(s); return; }
    case 'ai-chat': { const s = sourceOf(null); if (s) openAiChat(s); return; }
  }
}

/**
 * The text surfaces the AI chords work in: the message box, the launch prompt,
 * and (gap sweep G4) the continue prompt and the Ctrl-G input modal.
 */
function chordSource(t: EventTarget | null): HTMLTextAreaElement | null {
  if (!(t instanceof HTMLTextAreaElement) || !isTextField(t)) return null;
  if (t.id === 'message') return t;
  if (t.name === 'query' && t.closest('#launch-form')) return t;
  if (t.closest('#continue-dialog, #g1-modal-dialog')) return t;
  return null;
}

export function initAssist(): void {
  const style = document.createElement('style');
  style.id = 'assist-style';
  style.textContent = CSS;
  document.head.append(style);
  // Ctrl-Alt-A (AI command; plain Ctrl-A stays select-all, like the TUI's
  // standard editing) and Ctrl-Shift-A (AI chat) in a draft. Cmd works as Ctrl
  // on macOS. ev.code, because Alt changes ev.key on macOS.
  document.addEventListener('keydown', (ev) => {
    if (focusOwnsKey(ev.key, ev.target as Element | null)) return;
    if (ev.code !== 'KeyA' || !(ev.ctrlKey || ev.metaKey) || ev.isComposing) return;
    const src = chordSource(ev.target);
    if (!src) return;
    if (ev.shiftKey && !ev.altKey) { ev.preventDefault(); ev.stopPropagation(); openAiChat(src); }
    else if (ev.altKey && !ev.shiftKey) { ev.preventDefault(); ev.stopPropagation(); openAiCommand(src); }
  }, true);
}

// The Claude skills list (Settings > Claude Skills, `:skills`) and its Skill
// Preview (hub #3062), through rsid's ListSkills / GetSkill via the Tauri
// commands list_skills and get_skill; e and d enable, disable or delete the
// selected skill through SetSkillEnabled / DeleteSkill (#97: set_skill_enabled,
// delete_skill). Pure rules in skills-model.ts.
import { invoke } from './api.js';
import { el, toast } from './dom.js';
import { deleteChord, moveCursor } from './admin-model.js';
import { actions, button, inField, listBox, note, rowEl, show } from './admin-dom.js';
import { dialogEl, overlayKey } from './actions/detail.js';
import {
  type SkillContent, type SkillDeleted, type SkillToggled, type SkillsView, cursorAfterDelete, skillPreviewScroll,
  skillDeletedNotice, skillRowValue, skillToggledNotice,
} from './skills-model.js';
import { focusOwnsKey } from './modal-gate.js';
import { syncListbox } from './listbox-a11y.js';

/** Which skills overlay is open (the slice snapshot). */
let openName: string | null = null;
export function skillsOpen(): string | null {
  return openName;
}

function skillDialog(id: string, name: string): HTMLDialogElement {
  const dlg = dialogEl(id);
  dlg.classList.add('admin-dialog');
  openName = name;
  dlg.onclose = () => { if (openName === name) openName = null; };
  return dlg;
}


/**
 * The TUI's Settings › Skills keys on the Settings row: the desktop has one
 * Claude skills row, so both open the skills list, where they act on the
 * selected skill.
 */
export const SKILL_ROW_KEYS: Readonly<Record<string, string>> = {
  e: 'Enable or disable skill (opens the skills list; e there renames the selected skill through rsid)',
  d: 'Delete skill (opens the skills list; d twice there deletes the selected skill through rsid)',
};

let skillsAt = 0;

async function loadSkills(): Promise<SkillsView | null> {
  try {
    return await invoke<SkillsView>('list_skills');
  } catch (e) {
    toast(`Failed to list ~/.claude/skills: ${String(e)}`);
    return null;
  }
}

/**
 * The Claude skills list (Settings › Claude Skills, `:skills`): Enter previews
 * SKILL.md, e enables or disables the selected skill (SetSkillEnabled), d asks
 * for a second d and then deletes it (DeleteSkill, a move into rsid's skill trash, #91; the TUI asks for a second d too, #131).
 */
export async function openSkills(): Promise<void> {
  const view = await loadSkills();
  if (!view) return;
  const dlg = skillDialog('skills-dialog', 'claude-skills');
  const box = el('div', 'launch-form');
  const ul = listBox('Claude skills');
  const empty = note('No skills.');
  const previewBtn = button('Preview (Enter)', 'btn primary');
  const toggleBtn = button('Enable/disable (e)');
  const deleteBtn = button('Delete (d)');
  const reloadBtn = button('Reload (R)');
  const confirm = note('');
  const closeBtn = button('Close');
  const skipped = view.skipped_outside_root > 0
    ? ` ${view.skipped_outside_root} linked outside the directory not shown.` : '';
  box.append(el('h2', undefined, 'Claude skills'),
    note(`${view.skills_dir} on the daemon host.${view.truncated ? ' The list is cut.' : ''}${skipped}`),
    ul, empty, confirm, actions(closeBtn, reloadBtn, deleteBtn, toggleBtn, previewBtn),
    note('j/k move, Enter preview, e enable/disable, d d delete, R reload, q/Esc close.'));
  const rows = view.skills;
  skillsAt = Math.max(0, Math.min(skillsAt, rows.length - 1));
  let armed: string | null = null;
  let busy = false;
  const draw = (): void => {
    empty.hidden = rows.length > 0;
    ul.hidden = rows.length === 0;
    ul.replaceChildren(...rows.map((r, i) => {
      const li = rowEl(r.name, skillRowValue(r), i === skillsAt, r.enabled ? '' : 'admin-off');
      li.addEventListener('click', () => { skillsAt = i; armed = null; draw(); });
      li.addEventListener('dblclick', () => preview());
      return li;
    }));
    ul.querySelector('.sel')?.scrollIntoView({ block: 'nearest' });
    syncListbox(ul, dlg);
    previewBtn.disabled = rows.length === 0;
    toggleBtn.disabled = rows.length === 0 || busy;
    deleteBtn.disabled = rows.length === 0 || busy;
    confirm.textContent = armed !== null
      ? `Press d again to delete skill "${armed}" (moves ${view.skills_dir}/${armed} to the daemon's skill trash).` : '';
  };
  const preview = (): void => {
    const r = rows[skillsAt];
    if (!r) return;
    dlg.close();
    void openSkillPreview(r.name, () => void openSkills());
  };
  const reload = (): void => { dlg.close(); void openSkills(); };
  const toggle = async (): Promise<void> => {
    const r = rows[skillsAt];
    if (!r || busy) return;
    busy = true;
    try {
      toast(skillToggledNotice(await invoke<SkillToggled>('set_skill_enabled', { name: r.name, enabled: !r.enabled })));
      reload();
    } catch (e) {
      toast(`Toggle failed: ${String(e)}`);
    } finally { busy = false; draw(); }
  };
  const remove = async (): Promise<void> => {
    const step = deleteChord(armed, rows[skillsAt]?.name);
    armed = step.armed;
    draw();
    if (step.run === null || busy) return;
    busy = true;
    try {
      const gone = await invoke<SkillDeleted>('delete_skill', { name: step.run });
      toast(skillDeletedNotice(gone));
      skillsAt = cursorAfterDelete(skillsAt, rows.length - 1);
      reload();
    } catch (e) {
      toast(`Delete failed: ${String(e)}`);
    } finally { busy = false; draw(); }
  };
  previewBtn.addEventListener('click', preview);
  toggleBtn.addEventListener('click', () => { armed = null; void toggle(); });
  deleteBtn.addEventListener('click', () => void remove());
  reloadBtn.addEventListener('click', reload);
  closeBtn.addEventListener('click', () => dlg.close());
  dlg.onkeydown = (ev) => {
    if (focusOwnsKey(ev.key, ev.target as Element | null)) return;
    if (inField(ev) || ev.ctrlKey || ev.metaKey || ev.altKey) return;
    if (overlayKey(ev, dlg)) return;
    if (ev.key === 'd') { ev.preventDefault(); void remove(); return; }
    armed = null;
    const moved = moveCursor(skillsAt, rows.length, ev.key);
    if (moved !== null) { ev.preventDefault(); skillsAt = moved; draw(); return; }
    draw();
    switch (ev.key) {
      case 'e': ev.preventDefault(); void toggle(); break;
      case 'Enter': ev.preventDefault(); preview(); break;
      case 'R': ev.preventDefault(); reload(); break;
      case 'q': case 'Escape': ev.preventDefault(); dlg.close(); break;
    }
  };
  draw();
  show(dlg, box);
}

/**
 * The Skill Preview overlay: a skill's SKILL.md read through rsid (`GetSkill`),
 * read-only. j/k scroll a line, Ctrl-D/U ten, g/G the ends, q/Esc close
 * (`after` reopens the list it came from).
 */
export async function openSkillPreview(name: string, after?: () => void): Promise<void> {
  let skill: SkillContent;
  try {
    skill = await invoke<SkillContent>('get_skill', { name });
  } catch (e) {
    toast(`Failed to read SKILL.md: ${String(e)}`);
    after?.();
    return;
  }
  const dlg = skillDialog('skill-preview-dialog', 'skill-preview');
  const box = el('div', 'launch-form');
  const body = el('pre', 'preview-body');
  body.textContent = skill.content;
  const cut = skill.truncated ? ` (first ${skill.content.length} characters of ${skill.bytes} bytes)` : '';
  box.append(el('h2', undefined, `Skill: ${skill.name}${skill.enabled ? '' : ' (disabled)'}`),
    note(`${skill.path} on the daemon host${cut}`), body,
    note('j/k scroll, Ctrl-D/U page, g/G top or bottom, q/Esc close.'));
  const total = skill.content.split('\n').length;
  let offset = 0;
  const close = (): void => { dlg.close(); after?.(); };
  dlg.onkeydown = (ev) => {
    if (focusOwnsKey(ev.key, ev.target as Element | null)) return;
    if (ev.metaKey || ev.altKey) return;
    if (!ev.ctrlKey && overlayKey(ev, dlg)) return;
    const next = skillPreviewScroll(offset, total, ev.key, ev.ctrlKey);
    if (next !== null) {
      ev.preventDefault();
      offset = next;
      const lh = Number.parseFloat(getComputedStyle(body).lineHeight) || 18;
      body.scrollTop = offset * lh;
      return;
    }
    if (!ev.ctrlKey && (ev.key === 'q' || ev.key === 'Escape')) { ev.preventDefault(); close(); }
  };
  show(dlg, box);
}

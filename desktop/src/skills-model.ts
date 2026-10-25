// Pure rules of the Claude skills list and Skill Preview (hub #3062:
// ListSkills / GetSkill; #97: SetSkillEnabled / DeleteSkill). No DOM; tested in tests/promptcreator.test.mjs. The
// DOM side is skills.ts.

/** One row of rsid's `ListSkills` (`user: true`: the daemon user's ~/.claude/skills). */
export interface SkillSummary {
  name: string;
  enabled: boolean;
  description: string | null;
  has_skill_md: boolean;
}

/** rsid's `ListSkills` answer. */
export interface SkillsView {
  skills_dir: string;
  skills: SkillSummary[];
  truncated: boolean;
  skipped_outside_root: number;
}

/** rsid's `GetSkill` answer. */
export interface SkillContent {
  name: string;
  enabled: boolean;
  path: string;
  content: string;
  bytes: number;
  truncated: boolean;
}

/** rsid's `SetSkillEnabled` answer. */
export interface SkillToggled {
  name: string;
  enabled: boolean;
  changed: boolean;
  path: string;
}

/** rsid's `DeleteSkill` answer: the skill moved into rsid's skill trash (#91). */
export interface SkillDeleted {
  name: string;
  enabled: boolean;
  path: string;
  trash_path: string;
}

/** The TUI's notice after `e` (settings_keys.rs toggle_skill_at_selected_index). */
export function skillToggledNotice(s: SkillToggled): string {
  return `${s.enabled ? 'Enabled' : 'Disabled'} skill: ${s.name}`;
}

/** The TUI's notice after the confirmed `d d` (operator_config.rs delete_skill). */
export function skillDeletedNotice(s: SkillDeleted): string {
  return `Deleted skill: ${s.name} (moved to ${s.trash_path})`;
}

/** The skills list cursor after a delete: clamped to the shorter list (the TUI's clamp). */
export function cursorAfterDelete(at: number, remaining: number): number {
  return Math.max(0, Math.min(at, remaining - 1));
}

/** The skill row's value column: state and description (the TUI's Settings › Skills row). */
export function skillRowValue(s: SkillSummary): string {
  const state = s.enabled ? 'enabled' : 'disabled';
  if (!s.has_skill_md) return `${state} · no SKILL.md`;
  return s.description ? `${state} · ${s.description}` : state;
}

/**
 * The Skill Preview overlay's scroll keys (crates/rsi/src/overlay/skill_preview.rs):
 * j/k one line, Ctrl-D/U ten, g/G the ends; `null` when the key is not a scroll
 * key. `total` is the line count; the offset never passes the last line.
 */
export function skillPreviewScroll(offset: number, total: number, key: string, ctrl: boolean): number | null {
  const last = Math.max(0, total - 1);
  if (ctrl) {
    if (key === 'd') return Math.min(offset + 10, last);
    if (key === 'u') return Math.max(0, offset - 10);
    return null;
  }
  switch (key) {
    case 'j': case 'ArrowDown': return offset + 1 < total ? offset + 1 : offset;
    case 'k': case 'ArrowUp': return Math.max(0, offset - 1);
    case 'G': case 'End': return last;
    case 'g': case 'Home': return 0;
    default: return null;
  }
}

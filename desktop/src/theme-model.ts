// The TUI's built-in themes and semantic roles mapped onto the desktop's CSS
// variables (pure, no DOM; tested in tests/theme.test.mjs). The palettes,
// themes, role baselines and contrast rules are generated from
// crates/rsi/src/ui/theme.rs and theme_roles.rs (settings.generated.ts).
//
// - A theme gives every semantic role a colour (the TUI's `semantic_baseline`);
//   a role override replaces it (the TUI's role editor), and the desktop CSS
//   variables are read from the roles (`DESKTOP_VARS`).
// - "Desktop default" keeps the shipped stylesheet (Catppuccin Mocha) and sets
//   only the variables of the roles you override.
// - The choice and the overrides persist in localStorage; overrides are global
//   to the theme, as in the TUI's state.json, and "Reset active theme" clears them.
import { PALETTES, ROLE_BASELINE, THEMES, THEME_ROLES, type RoleRow, type ThemeRow } from './settings.generated.js';

export type Hex = string;
export type Overrides = Readonly<Record<string, Hex>>;

export interface ThemeChoice {
  /** A TUI theme key, or null for the desktop's own stylesheet. */
  theme: string | null;
  overrides: Overrides;
  /**
   * "Match system": the theme follows the system light/dark setting (Light when the system is light, the
   * desktop default otherwise), live. An explicit theme pick clears it; `theme` is ignored while it is set.
   */
  system?: boolean;
}

export const DEFAULT_CHOICE: ThemeChoice = { theme: null, overrides: {} };
/** A profile that never stored a choice (a new install) matches the system. A stored choice never does unless it says so. */
export const FIRST_RUN_CHOICE: ThemeChoice = { theme: null, overrides: {}, system: true };
/** The built-in theme "Match system" shows when the system is light (the dark side is the desktop default). */
export const SYSTEM_LIGHT_THEME = 'light';
export const DESKTOP_DEFAULT_NAME = 'Desktop default (Catppuccin Mocha)';
export const STORAGE_KEY = 'rsi.desktop.theme.v1';

/**
 * Each desktop CSS variable (static/styles.css) and where its colour comes
 * from: a semantic role (so a role override shows), or a palette slot the
 * roles do not cover.
 */
export const DESKTOP_VARS: Readonly<Record<string, { role: string } | { slot: string }>> = {
  '--base': { role: 'canvas' },
  '--mantle': { role: 'panel' },
  '--crust': { role: 'elevated_surface' },
  '--surface0': { slot: 'surface0' },
  '--surface1': { role: 'selected_row' },
  '--overlay0': { role: 'disabled' },
  '--text': { role: 'primary_text' },
  '--subtext': { role: 'muted_text' },
  '--muted': { role: 'muted_text' },
  '--blue': { role: 'accent' },
  '--green': { role: 'running' },
  '--yellow': { slot: 'yellow' },
  '--red': { role: 'error' },
  '--mauve': { role: 'pin' },
  '--peach': { role: 'warning' },
  '--teal': { slot: 'teal' },
};

/** The shipped stylesheet's values (static/styles.css `:root`; a test pins them). */
export const DESKTOP_DEFAULT_VARS: Readonly<Record<string, Hex>> = {
  '--base': '#1e1e2e', '--mantle': '#181825', '--crust': '#11111b',
  '--surface0': '#313244', '--surface1': '#45475a', '--overlay0': '#6c7086',
  '--text': '#cdd6f4', '--subtext': '#a6adc8', '--muted': '#9ca0b8',
  '--blue': '#89b4fa', '--green': '#a6e3a1', '--yellow': '#f9e2af', '--red': '#f38ba8',
  '--mauve': '#cba6f7', '--peach': '#fab387', '--teal': '#94e2d5',
};

/**
 * Text drawn on a filled accent (badges, primary and danger buttons): the accent variable it sits on and the
 * `--on-*` variable that carries its text colour. The stylesheet defaults each to `var(--crust)`; a theme or an
 * override sets the one that reaches 4.5:1 (`onColor`).
 */
export const ON_ACCENT_VARS: Readonly<Record<string, string>> = {
  '--on-blue': '--blue', '--on-green': '--green', '--on-yellow': '--yellow', '--on-red': '--red',
  '--on-mauve': '--mauve', '--on-peach': '--peach', '--on-teal': '--teal',
};
const AA_TEXT = 4.5;
/** The surfaces text is drawn on (panes, bars, cards and the selected row) and the text variables a theme keeps readable on them. */
export const TEXT_SURFACES: readonly string[] = ['--base', '--mantle', '--crust', '--surface0', '--surface1'];
export const ADJUSTED_TEXT_VARS: readonly string[] = ['--subtext', '--muted', '--blue', '--green', '--yellow', '--red', '--mauve', '--peach', '--teal'];

/** Desktop-default colours of the roles no desktop variable shows. */
const DESKTOP_DEFAULT_UNMAPPED: Readonly<Record<string, Hex>> = {
  border: '#313244', focused_border: '#89b4fa', success: '#89b4fa', waiting: '#f9e2af', toast: '#cdd6f4',
};

/** Roles whose colour no desktop surface shows yet (the editor still keeps them). */
export function unmappedRoles(): string[] {
  const mapped = new Set(Object.values(DESKTOP_VARS).flatMap((s) => ('role' in s ? [s.role] : [])));
  return THEME_ROLES.map((r) => r.key).filter((k) => !mapped.has(k));
}

export function themeByKey(key: string | null): ThemeRow | undefined {
  return key === null ? undefined : THEMES.find((t) => t.key === key);
}

/** `:theme <name>`: a theme by key or display name, case- and punctuation-insensitive. */
export function findTheme(name: string): ThemeRow | undefined {
  const norm = (s: string) => s.toLowerCase().replace(/[^a-z0-9]/g, '');
  const n = norm(name);
  if (n === '') return undefined;
  return THEMES.find((t) => norm(t.key) === n || norm(t.name) === n);
}

// ---------- colours ----------

export function parseHex(input: string): Hex | null {
  const t = input.trim().replace(/^#/, '');
  return /^[0-9a-fA-F]{6}$/.test(t) ? `#${t.toLowerCase()}` : null;
}

function rgb(hex: Hex): [number, number, number] {
  const h = hex.replace('#', '');
  return [0, 2, 4].map((i) => parseInt(h.slice(i, i + 2), 16)) as [number, number, number];
}

function linear(c: number): number {
  const v = c / 255;
  return v <= 0.04045 ? v / 12.92 : ((v + 0.055) / 1.055) ** 2.4;
}

export function relativeLuminance(hex: Hex): number {
  const [r, g, b] = rgb(hex);
  return 0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b);
}

/** WCAG contrast ratio (the TUI's `contrast_ratio`). */
export function contrastRatio(a: Hex, b: Hex): number {
  const la = relativeLuminance(a);
  const lb = relativeLuminance(b);
  return (Math.max(la, lb) + 0.05) / (Math.min(la, lb) + 0.05);
}

/**
 * The text colour for a fill: `preferred` (the theme's own dark surface) when it reaches 4.5:1, else the theme's
 * text colour, else black or white, whichever reads better. The best of black and white is always >= 4.58:1.
 */
export function onColor(fill: Hex, preferred: Hex, text: Hex): Hex {
  const candidates = [preferred, text, '#000000', '#ffffff'];
  for (const c of candidates) if (contrastRatio(c, fill) >= AA_TEXT) return c;
  return contrastRatio('#000000', fill) >= contrastRatio('#ffffff', fill) ? '#000000' : '#ffffff';
}

function toHex(c: [number, number, number]): Hex {
  return `#${c.map((v) => Math.round(Math.min(255, Math.max(0, v))).toString(16).padStart(2, '0')).join('')}`;
}

/** `color` moved `t` (0..1) of the way to black or white. */
function shifted(color: Hex, toward: 'black' | 'white', t: number): Hex {
  const goal = toward === 'white' ? 255 : 0;
  return toHex(rgb(color).map((v) => v + (goal - v) * t) as [number, number, number]);
}

/** The largest move toward black or white the text tokens accept before the theme keeps its own colour. */
const MAX_TEXT_SHIFT = 0.65;

/**
 * Text `color` adjusted to read at `min` on every one of `surfaces`: the same hue, moved toward white on dark surfaces
 * or toward black on light ones by the smallest step (of 1%) that clears it. A colour that already reads is returned
 * as is; one that would need more than MAX_TEXT_SHIFT, or surfaces that need opposite directions, keep their colour.
 */
export function readableOn(color: Hex, surfaces: readonly Hex[], min = AA_TEXT): Hex {
  const worst = (c: Hex) => Math.min(...surfaces.map((s) => contrastRatio(c, s)));
  if (worst(color) >= min) return color;
  const light = surfaces.filter((s) => contrastRatio(s, '#000000') > contrastRatio(s, '#ffffff')).length;
  if (light !== 0 && light !== surfaces.length) return color;
  const toward = light === 0 ? 'white' : 'black';
  for (let t = 0.01; t <= MAX_TEXT_SHIFT + 1e-9; t += 0.01) {
    const c = shifted(color, toward, t);
    if (worst(c) >= min) return c;
  }
  return color;
}

/** The choice the page is painted with: "Match system" resolves to Light or the desktop default. */
export function effectiveChoice(choice: ThemeChoice, systemLight: boolean): ThemeChoice {
  if (!choice.system) return choice;
  return { ...choice, theme: systemLight ? SYSTEM_LIGHT_THEME : null };
}

/** `color-scheme` for a choice (native checkboxes, selects, scrollbars): by the canvas it paints. */
export function colorSchemeOf(choice: ThemeChoice): 'light' | 'dark' {
  const canvas = resolveRoles(choice)['canvas']!;
  return contrastRatio(canvas, '#000000') > contrastRatio(canvas, '#ffffff') ? 'light' : 'dark';
}

// ---------- roles ----------

/** A role's colour in a theme before any override (`semantic_baseline`). */
export function roleBaseline(theme: ThemeRow | undefined, role: string): Hex {
  if (!theme) {
    for (const [v, src] of Object.entries(DESKTOP_VARS)) if ('role' in src && src.role === role) return DESKTOP_DEFAULT_VARS[v]!;
    return DESKTOP_DEFAULT_UNMAPPED[role] ?? '#ffffff';
  }
  const palette = PALETTES[theme.palette] ?? {};
  const src = ROLE_BASELINE[role];
  if (!src) throw new Error(`role ${role} has no baseline`);
  if ('primary' in src) return theme.primary ?? palette.blue ?? '#ffffff';
  if (src.transparent && theme.transparency !== 'opaque') return src.transparent;
  return palette[src.slot] ?? '#000000';
}

/** Every role's effective colour: the override, else the theme's baseline. */
export function resolveRoles(choice: ThemeChoice): Record<string, Hex> {
  const theme = themeByKey(choice.theme);
  const out: Record<string, Hex> = {};
  for (const r of THEME_ROLES) out[r.key] = choice.overrides[r.key] ?? roleBaseline(theme, r.key);
  return out;
}

/**
 * The CSS variables to set on `:root` for a choice. The desktop default sets
 * only the variables of overridden roles; a TUI theme sets every one.
 */
export function cssVars(choice: ThemeChoice): Record<string, Hex> {
  const theme = themeByKey(choice.theme);
  const roles = resolveRoles(choice);
  const out: Record<string, Hex> = {};
  for (const [v, src] of Object.entries(DESKTOP_VARS)) {
    if ('role' in src) {
      if (theme || choice.overrides[src.role]) out[v] = roles[src.role]!;
    } else if (theme) {
      out[v] = PALETTES[theme.palette]?.[src.slot] ?? DESKTOP_DEFAULT_VARS[v]!;
    }
  }
  // A built-in theme's accent and secondary text colours read at 4.5:1 on every surface text sits on (the pane
  // surfaces and the selected row); a role override the operator typed is theirs and is left alone.
  if (theme) {
    const surfaces = TEXT_SURFACES.map((v) => out[v]!);
    for (const [v, src] of Object.entries(DESKTOP_VARS)) {
      if (!ADJUSTED_TEXT_VARS.includes(v) || ('role' in src && choice.overrides[src.role])) continue;
      out[v] = readableOn(out[v]!, surfaces);
    }
  }
  for (const r of THEME_ROLES) if (theme || choice.overrides[r.key]) out[`--role-${r.key.replace(/_/g, '-')}`] = roles[r.key]!;
  // Text on a filled accent follows the fill; only when the fill, the dark surface or the text colour changed.
  const eff = { ...DESKTOP_DEFAULT_VARS, ...out };
  const moved = (v: string) => v in out;
  for (const [on, fill] of Object.entries(ON_ACCENT_VARS)) {
    if (moved(fill) || moved('--crust') || moved('--text')) out[on] = onColor(eff[fill]!, eff['--crust']!, eff['--text']!);
  }
  return out;
}

/** Every variable cssVars can set (cleared before a new choice is applied). */
export function allVarNames(): string[] {
  return [...Object.keys(DESKTOP_VARS), ...Object.keys(ON_ACCENT_VARS), ...THEME_ROLES.map((r) => `--role-${r.key.replace(/_/g, '-')}`)];
}

export type Assessment =
  | { kind: 'invalid'; reason: string }
  | { kind: 'none' }
  | { kind: 'readable'; ratio: number; against: string }
  | { kind: 'low'; ratio: number; minimum: number; against: string };

/** The contrast check of a candidate colour for `role` (theme_roles.rs `assess_contrast`, truecolor). */
export function assessRole(choice: ThemeChoice, role: string, input: string): Assessment {
  const hex = parseHex(input);
  if (!hex) return { kind: 'invalid', reason: 'expected #RRGGBB' };
  const desc: RoleRow | undefined = THEME_ROLES.find((r) => r.key === role);
  if (!desc) return { kind: 'invalid', reason: `unknown role ${role}` };
  if (!desc.against) return { kind: 'none' };
  const bg = resolveRoles(choice)[desc.against]!;
  const ratio = contrastRatio(hex, bg);
  return ratio >= desc.minimum
    ? { kind: 'readable', ratio, against: desc.against }
    : { kind: 'low', ratio, minimum: desc.minimum, against: desc.against };
}

export function setOverride(choice: ThemeChoice, role: string, hex: Hex | null): ThemeChoice {
  const overrides: Record<string, Hex> = { ...choice.overrides };
  if (hex === null) delete overrides[role];
  else overrides[role] = hex;
  return { ...choice, overrides };
}

// ---------- persistence ----------

export function encodeChoice(choice: ThemeChoice): string {
  return JSON.stringify(choice.system
    ? { theme: choice.theme, overrides: choice.overrides, system: true }
    : { theme: choice.theme, overrides: choice.overrides });
}

/**
 * A stored choice (unknown roles and bad colours dropped). Nothing stored is a new install and matches the
 * system; a stored choice without `system` is an explicit pick and keeps its theme; unreadable text is treated
 * as nothing stored.
 */
export function decodeChoice(text: string | null | undefined): ThemeChoice {
  if (!text) return FIRST_RUN_CHOICE;
  let v: unknown;
  try { v = JSON.parse(text); } catch { return FIRST_RUN_CHOICE; }
  if (!v || typeof v !== 'object') return FIRST_RUN_CHOICE;
  const o = v as { theme?: unknown; overrides?: unknown; system?: unknown };
  const theme = typeof o.theme === 'string' && themeByKey(o.theme) ? o.theme : null;
  const overrides: Record<string, Hex> = {};
  if (o.overrides && typeof o.overrides === 'object') {
    for (const [k, raw] of Object.entries(o.overrides as Record<string, unknown>)) {
      const hex = typeof raw === 'string' ? parseHex(raw) : null;
      if (hex && THEME_ROLES.some((r) => r.key === k)) overrides[k] = hex;
    }
  }
  return o.system === true ? { theme: null, overrides, system: true } : { theme, overrides };
}

// ---------- picker ----------

export const MATCH_SYSTEM_NAME = 'Match system (light/dark)';

export interface PickerEntry { key: string | null; name: string; system?: boolean }

/** The picker's entries: "Match system", the desktop default, then the TUI's themes in their order. */
export function pickerEntries(): PickerEntry[] {
  return [
    { key: null, name: MATCH_SYSTEM_NAME, system: true },
    { key: null, name: DESKTOP_DEFAULT_NAME },
    ...THEMES.map((t) => ({ key: t.key, name: t.name })),
  ];
}

/** The picker row a choice sits on. */
export function pickerIndexOf(choice: ThemeChoice): number {
  if (choice.system) return 0;
  return Math.max(1, pickerEntries().findIndex((e) => !e.system && e.key === choice.theme));
}

/** `:theme system`: does this name ask to follow the system? */
export function isMatchSystemName(name: string): boolean {
  return /^(match[\s-]*)?system$/i.test(name.trim());
}

/** Eight swatch colours for a picker row (the TUI's `theme_swatch` idea). */
export function swatch(key: string | null): Hex[] {
  const choice: ThemeChoice = { theme: key, overrides: {} };
  const roles = resolveRoles(choice);
  return ['canvas', 'panel', 'primary_text', 'accent', 'running', 'waiting', 'warning', 'error'].map((r) => roles[r]!);
}

/** The "Match system" row's swatch: the dark side's four first colours, then the light side's. */
export function systemSwatch(): Hex[] {
  return [...swatch(null).slice(0, 4), ...swatch(SYSTEM_LIGHT_THEME).slice(0, 4)];
}

export type PickerEffect =
  | { kind: 'none' }
  | { kind: 'move'; index: number }
  | { kind: 'apply'; index: number }
  | { kind: 'cancel' };

/** Theme picker keys: j/k (arrows), 1-9 select, Enter applies, Esc/q cancels. */
export function pickerKey(index: number, count: number, key: string): PickerEffect {
  switch (key) {
    case 'j': case 'ArrowDown': return { kind: 'move', index: Math.min(count - 1, index + 1) };
    case 'k': case 'ArrowUp': return { kind: 'move', index: Math.max(0, index - 1) };
    case 'g': case 'Home': return { kind: 'move', index: 0 };
    case 'G': case 'End': return { kind: 'move', index: count - 1 };
    case 'Enter': return { kind: 'apply', index };
    case 'Escape': case 'q': return { kind: 'cancel' };
    default:
      if (/^[1-9]$/.test(key)) {
        const i = Number(key) - 1;
        // The TUI's 1-9 pick its first nine themes; the desktop list starts with "Match system" and its own default.
        return i + 2 < count ? { kind: 'apply', index: i + 2 } : { kind: 'none' };
      }
      return { kind: 'none' };
  }
}

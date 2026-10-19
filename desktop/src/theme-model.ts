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
}

export const DEFAULT_CHOICE: ThemeChoice = { theme: null, overrides: {} };
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
  '--text': '#cdd6f4', '--subtext': '#a6adc8',
  '--blue': '#89b4fa', '--green': '#a6e3a1', '--yellow': '#f9e2af', '--red': '#f38ba8',
  '--mauve': '#cba6f7', '--peach': '#fab387', '--teal': '#94e2d5',
};

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
  for (const r of THEME_ROLES) if (theme || choice.overrides[r.key]) out[`--role-${r.key.replace(/_/g, '-')}`] = roles[r.key]!;
  return out;
}

/** Every variable cssVars can set (cleared before a new choice is applied). */
export function allVarNames(): string[] {
  return [...Object.keys(DESKTOP_VARS), ...THEME_ROLES.map((r) => `--role-${r.key.replace(/_/g, '-')}`)];
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
  return JSON.stringify({ theme: choice.theme, overrides: choice.overrides });
}

/** A stored choice; anything malformed falls back to the default (unknown roles and bad colours dropped). */
export function decodeChoice(text: string | null | undefined): ThemeChoice {
  if (!text) return DEFAULT_CHOICE;
  let v: unknown;
  try { v = JSON.parse(text); } catch { return DEFAULT_CHOICE; }
  if (!v || typeof v !== 'object') return DEFAULT_CHOICE;
  const o = v as { theme?: unknown; overrides?: unknown };
  const theme = typeof o.theme === 'string' && themeByKey(o.theme) ? o.theme : null;
  const overrides: Record<string, Hex> = {};
  if (o.overrides && typeof o.overrides === 'object') {
    for (const [k, raw] of Object.entries(o.overrides as Record<string, unknown>)) {
      const hex = typeof raw === 'string' ? parseHex(raw) : null;
      if (hex && THEME_ROLES.some((r) => r.key === k)) overrides[k] = hex;
    }
  }
  return { theme, overrides };
}

// ---------- picker ----------

/** The picker's entries: the desktop default first, then the TUI's themes in their order. */
export function pickerEntries(): { key: string | null; name: string }[] {
  return [{ key: null, name: DESKTOP_DEFAULT_NAME }, ...THEMES.map((t) => ({ key: t.key, name: t.name }))];
}

/** Eight swatch colours for a picker row (the TUI's `theme_swatch` idea). */
export function swatch(key: string | null): Hex[] {
  const choice: ThemeChoice = { theme: key, overrides: {} };
  const roles = resolveRoles(choice);
  return ['canvas', 'panel', 'primary_text', 'accent', 'running', 'waiting', 'warning', 'error'].map((r) => roles[r]!);
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
        // The TUI's 1-9 pick its first nine themes; the desktop list starts with its own default.
        return i + 1 < count ? { kind: 'apply', index: i + 1 } : { kind: 'none' };
      }
      return { kind: 'none' };
  }
}

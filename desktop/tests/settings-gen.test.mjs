import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
  braceBody, parseDaemonDefaults, parseRegistry, parseRoles, parseThemes, rustString, settingsIsFresh,
} from '../scripts/settings.mjs';

test('src/settings.generated.ts matches the TUI sources (run `node scripts/settings.mjs` if not)', () => {
  assert.equal(settingsIsFresh(), true);
});

test('Rust helpers: balanced bodies skip strings and comments; string escapes decode', () => {
  assert.equal(braceBody('fn x() { a("}"); // }\n { b } }', 0), ' a("}"); // }\n { b } ');
  assert.equal(rustString('a \\"b\\" \\\n    c'), 'a "b" c');
});

const REGISTRY = `
pub enum SettingsGroup {
    Appearance,
    Workspace,
}
impl SettingsGroup {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Appearance => "APPEARANCE",
            Self::Workspace => "WORKSPACE",
        }
    }
    pub const fn summary(self) -> &'static str {
        match self {
            Self::Appearance => "How does it look?",
            Self::Workspace => {
                "What do I see?"
            }
        }
    }
}
pub enum SettingsSection {
    ThemeColors,
    EditingMode,
}
impl SettingsSection {
    pub const fn group(self) -> SettingsGroup {
        match self {
            Self::ThemeColors => SettingsGroup::Appearance,
            Self::EditingMode => {
                SettingsGroup::Workspace
            }
        }
    }
    pub const fn label(self) -> &'static str {
        match self {
            Self::ThemeColors => "Theme & Colors",
            Self::EditingMode => "Editing Mode",
        }
    }
    pub const fn summary(self) -> &'static str {
        match self {
            Self::ThemeColors => "Themes.",
            Self::EditingMode => "Keys.",
        }
    }
}
pub static SETTINGS: &[SettingSpec] = &[
    SettingSpec {
        id: SettingId::EditingMode,
        section: SettingsSection::EditingMode,
        label: "Editing mode",
        summary: "Standard or \\"Vim\\".",
        detail: Some(
            "More.",
        ),
        keywords: &["vim", "keys"],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("editing_mode"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
];
`;

test('parses groups, sections and rows', () => {
  const r = parseRegistry(REGISTRY);
  assert.deepEqual(r.groups, [
    { id: 'appearance', label: 'APPEARANCE', summary: 'How does it look?' },
    { id: 'workspace', label: 'WORKSPACE', summary: 'What do I see?' },
  ]);
  assert.deepEqual(r.sections.map((s) => [s.id, s.group]), [['theme_colors', 'appearance'], ['editing_mode', 'workspace']]);
  assert.deepEqual(r.rows, [{
    id: 'editing_mode', section: 'editing_mode', label: 'Editing mode', summary: 'Standard or "Vim".', detail: 'More.',
    keywords: ['vim', 'keys'], kind: 'cycle', owner: { kind: 'daemon', args: ['editing_mode'] }, apply: 'live', destructive: false,
  }]);
});

test('a row the parser cannot read fails loudly', () => {
  assert.throws(() => parseRegistry(REGISTRY.replace('section: SettingsSection::EditingMode,', '')), /has no section/);
});

test('parses daemon widgets, resolving rsi_common consts', () => {
  const src = `
    pub fn defaults() -> Vec<Self> {
        vec![
            Self {
                field: "retry_enabled".to_string(),
                label: "Retry".to_string(),
                value: DaemonFeatureValue::Bool(false),
            },
            Self {
                field: rsi_common::x::FIELD.to_string(),
                label: "Hours".to_string(),
                value: DaemonFeatureValue::Cycle {
                    options: ["1", "6"].into_iter().map(str::to_string).collect(),
                    current: 0,
                },
            },
            Self {
                field: "mode".to_string(),
                label: "Mode".to_string(),
                value: DaemonFeatureValue::Cycle {
                    options: rsi_common::y::CHOICES.iter().map(|c| (*c).to_string()).collect(),
                    current: 0,
                },
            },
        ]
    }`;
  const resolve = (path, list) => (list ? (path === 'rsi_common::y::CHOICES' ? ['a', 'b'] : null) : 'hours_field');
  assert.deepEqual(parseDaemonDefaults(src, resolve), {
    retry_enabled: { label: 'Retry', widget: 'switch' },
    hours_field: { label: 'Hours', widget: 'choice', options: ['1', '6'] },
    mode: { label: 'Mode', widget: 'choice', options: ['a', 'b'] },
  });
});

test('parses palettes, theme definitions and role baselines', () => {
  const src = `
const A_PALETTE: CustomPalette = CustomPalette {
    // a comment, with a comma
    base: rgb(1, 2, 3),
    text: Color::White,
    dark_purple: rgb(10, 0, 255),
};
const THEME_DEFINITIONS: [ThemeDefinition; 1] = [
    ThemeDefinition::new(
        "a",
        "Theme A",
        &A_PALETTE,
        0,
        rgb(255, 0, 0),
        BorderPolicy::AccentFocusOnly,
        TransparencyPolicy::Scrimmed,
    )
    .with_loader(&X),
];
fn semantic_baseline(role: ThemeRole) -> Color {
    let palette = active_palette();
    match role {
        ThemeRole::Canvas => palette.base,
        ThemeRole::Border => {
            if is_transparent_theme() {
                Color::White
            } else {
                palette.dark_purple
            }
        }
        ThemeRole::FocusedBorder | ThemeRole::Accent => active_primary_color(),
        ThemeRole::PrimaryText | ThemeRole::Toast => palette.text,
    }
}`;
  const t = parseThemes(src);
  assert.deepEqual(t.palettes, { A_PALETTE: { base: '#010203', text: '#ffffff', dark_purple: '#0a00ff' } });
  assert.deepEqual(t.themes, [{ key: 'a', name: 'Theme A', palette: 'A_PALETTE', primary: '#ff0000', transparency: 'scrimmed' }]);
  assert.deepEqual(t.baseline, {
    canvas: { slot: 'base' }, border: { slot: 'dark_purple', transparent: '#ffffff' },
    focused_border: { primary: true }, accent: { primary: true }, primary_text: { slot: 'text' }, toast: { slot: 'text' },
  });
});

test('parses role descriptors in ThemeRole::ALL order', () => {
  const src = `
    pub const ALL: [Self; 2] = [Self::Canvas, Self::Toast];
    pub const fn descriptor(self) -> ThemeRoleDescriptor {
        match self {
            Self::Canvas => descriptor(self, "canvas", "Canvas", Surface, None, 3.0),
            Self::Toast => descriptor(
                self,
                "toast",
                "Toast text",
                Text,
                Some(Self::ElevatedSurface),
                4.5,
            ),
        }
    }`;
  assert.deepEqual(parseRoles(src), [
    { key: 'canvas', label: 'Canvas', kind: 'surface', against: null, minimum: 3 },
    { key: 'toast', label: 'Toast text', kind: 'text', against: 'elevated_surface', minimum: 4.5 },
  ]);
});

//! Theme system for the flywheel TUI.
//!
//! Custom themes with semantic color helpers for the UI.

use std::sync::atomic::{AtomicU8, AtomicU32, Ordering};

use ratatui::style::Color;
use rsi_common::types::SessionStatus;

use super::theme_roles::ThemeRole;

const DEFAULT_THEME_INDEX: usize = 0; // Goth
static ACTIVE_THEME_INDEX: AtomicU8 = AtomicU8::new(DEFAULT_THEME_INDEX as u8);

const NO_THEME_ROLE_OVERRIDE: u32 = u32::MAX;
static THEME_ROLE_OVERRIDES: [AtomicU32; 17] = [
    AtomicU32::new(NO_THEME_ROLE_OVERRIDE),
    AtomicU32::new(NO_THEME_ROLE_OVERRIDE),
    AtomicU32::new(NO_THEME_ROLE_OVERRIDE),
    AtomicU32::new(NO_THEME_ROLE_OVERRIDE),
    AtomicU32::new(NO_THEME_ROLE_OVERRIDE),
    AtomicU32::new(NO_THEME_ROLE_OVERRIDE),
    AtomicU32::new(NO_THEME_ROLE_OVERRIDE),
    AtomicU32::new(NO_THEME_ROLE_OVERRIDE),
    AtomicU32::new(NO_THEME_ROLE_OVERRIDE),
    AtomicU32::new(NO_THEME_ROLE_OVERRIDE),
    AtomicU32::new(NO_THEME_ROLE_OVERRIDE),
    AtomicU32::new(NO_THEME_ROLE_OVERRIDE),
    AtomicU32::new(NO_THEME_ROLE_OVERRIDE),
    AtomicU32::new(NO_THEME_ROLE_OVERRIDE),
    AtomicU32::new(NO_THEME_ROLE_OVERRIDE),
    AtomicU32::new(NO_THEME_ROLE_OVERRIDE),
    AtomicU32::new(NO_THEME_ROLE_OVERRIDE),
];
static COMMITTED_THEME_ROLE_OVERRIDES: [AtomicU32; 17] = [
    AtomicU32::new(NO_THEME_ROLE_OVERRIDE),
    AtomicU32::new(NO_THEME_ROLE_OVERRIDE),
    AtomicU32::new(NO_THEME_ROLE_OVERRIDE),
    AtomicU32::new(NO_THEME_ROLE_OVERRIDE),
    AtomicU32::new(NO_THEME_ROLE_OVERRIDE),
    AtomicU32::new(NO_THEME_ROLE_OVERRIDE),
    AtomicU32::new(NO_THEME_ROLE_OVERRIDE),
    AtomicU32::new(NO_THEME_ROLE_OVERRIDE),
    AtomicU32::new(NO_THEME_ROLE_OVERRIDE),
    AtomicU32::new(NO_THEME_ROLE_OVERRIDE),
    AtomicU32::new(NO_THEME_ROLE_OVERRIDE),
    AtomicU32::new(NO_THEME_ROLE_OVERRIDE),
    AtomicU32::new(NO_THEME_ROLE_OVERRIDE),
    AtomicU32::new(NO_THEME_ROLE_OVERRIDE),
    AtomicU32::new(NO_THEME_ROLE_OVERRIDE),
    AtomicU32::new(NO_THEME_ROLE_OVERRIDE),
    AtomicU32::new(NO_THEME_ROLE_OVERRIDE),
];

#[derive(Clone, Copy)]
struct CustomPalette {
    rosewater: Color,
    flamingo: Color,
    pink: Color,
    mauve: Color,
    red: Color,
    maroon: Color,
    peach: Color,
    yellow: Color,
    green: Color,
    teal: Color,
    sky: Color,
    sapphire: Color,
    blue: Color,
    lavender: Color,
    text: Color,
    subtext1: Color,
    subtext0: Color,
    overlay2: Color,
    overlay1: Color,
    overlay0: Color,
    surface2: Color,
    surface1: Color,
    surface0: Color,
    base: Color,
    mantle: Color,
    crust: Color,
    /// Surface tier: page/pane background — lowest elevation.
    tier_base: Color,
    /// Surface tier: a contained sub-surface (cards, overlays, bubbles,
    /// bottom strip, input bar) — one step up from base.
    tier_panel: Color,
    /// Surface tier: a further-elevated surface (hover/dropdown/decorative
    /// raised elements).
    tier_raised: Color,
    /// Surface tier: the actively selected/cursor-target row or item.
    tier_selected: Color,
    /// Deep purple used for user message borders.
    dark_purple: Color,
    /// Hierarchy kind colors for session cards.
    group_powder_blue: Color,
    epic_purple: Color,
    story_yellow_orange: Color,
    task_gray: Color,
    raw_session_magenta: Color,
}

const fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color::Rgb(r, g, b)
}

const GOTH_PALETTE: CustomPalette = CustomPalette {
    rosewater: rgb(243, 197, 198),
    flamingo: rgb(225, 134, 134),
    pink: rgb(223, 106, 170),
    mauve: rgb(181, 138, 215),
    red: rgb(229, 83, 105),
    maroon: rgb(170, 76, 98),
    peach: rgb(223, 143, 118),
    yellow: rgb(210, 180, 90),
    green: rgb(111, 191, 141),
    teal: rgb(79, 166, 163),
    sky: rgb(123, 182, 217),
    sapphire: rgb(92, 150, 192),
    blue: rgb(92, 140, 234),
    lavender: rgb(203, 183, 255),
    text: rgb(244, 240, 255),
    subtext1: rgb(203, 196, 221),
    subtext0: rgb(158, 153, 183),
    overlay2: rgb(138, 132, 162),
    overlay1: rgb(118, 112, 144),
    overlay0: rgb(96, 91, 118),
    surface2: rgb(43, 48, 68),
    surface1: rgb(32, 36, 52),
    surface0: rgb(26, 29, 39),
    base: rgb(11, 13, 17),
    mantle: rgb(8, 9, 13),
    crust: rgb(17, 19, 26),
    // Value-preserving tier migration: tier_base=base, tier_panel=surface0,
    // tier_raised=surface1, tier_selected=surface1 (same as raised —
    // preserves today's card_selected_bg()/selected_row_bg() overlap).
    tier_base: rgb(11, 13, 17),
    tier_panel: rgb(26, 29, 39),
    tier_raised: rgb(32, 36, 52),
    tier_selected: rgb(32, 36, 52),
    dark_purple: rgb(100, 55, 165),
    group_powder_blue: rgb(165, 201, 224),
    epic_purple: rgb(181, 126, 220),
    story_yellow_orange: rgb(230, 166, 92),
    task_gray: rgb(138, 138, 138),
    raw_session_magenta: rgb(208, 117, 168),
};

const TRANSPARENT_PALETTE: CustomPalette = CustomPalette {
    // Muted dark accents keep contrast on any backdrop, with amber highlights for pop
    rosewater: rgb(214, 182, 176),
    flamingo: rgb(204, 134, 126),
    pink: rgb(201, 129, 173),
    mauve: rgb(173, 132, 211),
    red: rgb(210, 101, 118),
    maroon: rgb(147, 73, 91),
    peach: rgb(223, 149, 105),
    yellow: rgb(255, 208, 96),
    green: rgb(138, 189, 136),
    teal: rgb(94, 182, 173),
    sky: rgb(125, 200, 223),
    sapphire: rgb(69, 130, 164),
    blue: rgb(115, 160, 230),
    lavender: rgb(195, 191, 255),
    text: rgb(232, 230, 220),
    subtext1: rgb(196, 191, 184),
    subtext0: rgb(150, 146, 144),
    overlay2: rgb(122, 118, 136),
    overlay1: rgb(105, 101, 117),
    overlay0: rgb(83, 80, 94),
    // Deliberate dark scrims. The terminal window may still be globally
    // transparent, so these need to survive wallpaper blending.
    surface2: rgb(31, 34, 49),
    surface1: rgb(18, 21, 31),
    surface0: rgb(8, 10, 16),
    base: rgb(5, 6, 10),
    mantle: rgb(4, 5, 8),
    crust: rgb(3, 4, 7),
    // Value-preserving tier migration — PINNED, part of the transparent
    // regression baseline. tier_panel/tier_selected are NOT equal to
    // surface0/surface1 (a pre-existing 1-unit discrepancy in tier_panel;
    // tier_selected has no equal surface field at all) — see
    // glass_panel_bg()/selected_row_bg() and the plan's precision note.
    tier_base: rgb(5, 6, 10),
    tier_panel: rgb(8, 10, 15),
    tier_raised: rgb(18, 21, 31),
    tier_selected: rgb(34, 34, 55),
    dark_purple: rgb(90, 48, 150),
    group_powder_blue: rgb(181, 214, 232),
    epic_purple: rgb(195, 145, 230),
    story_yellow_orange: rgb(244, 185, 110),
    task_gray: rgb(158, 158, 158),
    raw_session_magenta: rgb(220, 135, 185),
};

const JUNK_YARD_PALETTE: CustomPalette = CustomPalette {
    rosewater: rgb(249, 224, 227),
    flamingo: rgb(237, 115, 133),
    pink: rgb(233, 116, 209),
    mauve: rgb(179, 136, 235),
    red: rgb(240, 87, 93),
    maroon: rgb(173, 78, 103),
    peach: rgb(255, 177, 122),
    yellow: rgb(243, 212, 95),
    green: rgb(102, 188, 126),
    teal: rgb(30, 185, 128),
    sky: rgb(126, 232, 250),
    sapphire: rgb(0, 154, 196),
    blue: rgb(93, 133, 255),
    lavender: rgb(201, 197, 255),
    text: rgb(249, 247, 239),
    subtext1: rgb(225, 222, 210),
    subtext0: rgb(176, 171, 161),
    overlay2: rgb(155, 163, 157),
    overlay1: rgb(139, 145, 140),
    overlay0: rgb(111, 114, 111),
    surface2: rgb(44, 53, 59),
    surface1: rgb(36, 44, 49),
    surface0: rgb(30, 37, 41),
    base: rgb(19, 20, 23),
    mantle: rgb(25, 29, 32),
    // Dark, like every other theme: crust is the foreground on bright accent
    // badges (mode, active tab), where a light crust washed out.
    crust: rgb(16, 18, 20),
    // Value-preserving tier migration (same pattern as Goth: tier_selected
    // == tier_raised, preserving the existing raised/selected overlap).
    tier_base: rgb(19, 20, 23),
    tier_panel: rgb(30, 37, 41),
    tier_raised: rgb(36, 44, 49),
    tier_selected: rgb(36, 44, 49),
    dark_purple: rgb(105, 50, 175),
    group_powder_blue: rgb(138, 172, 192),
    epic_purple: rgb(168, 121, 202),
    story_yellow_orange: rgb(196, 145, 74),
    task_gray: rgb(140, 140, 140),
    raw_session_magenta: rgb(197, 113, 157),
};

// Anchored to the well-known gruvbox-dark hex palette (bg0_h/bg0/bg1/bg2 for
// the tier ladder, fg for text, bright red/green/yellow/blue for accents).
// surface0/surface1 intentionally equal tier_panel/tier_raised (no legacy
// discrepancy to replicate — that quirk is Transparent-only, pinned history).
const GRUVBOX_WARM_PALETTE: CustomPalette = CustomPalette {
    rosewater: rgb(251, 241, 199),
    flamingo: rgb(231, 108, 39),
    pink: rgb(211, 134, 155),
    mauve: rgb(200, 119, 155),
    red: rgb(255, 83, 61),
    maroon: rgb(213, 48, 38),
    peach: rgb(254, 128, 25),
    yellow: rgb(250, 189, 47),
    green: rgb(184, 187, 38),
    teal: rgb(104, 157, 106),
    sky: rgb(142, 192, 124),
    sapphire: rgb(91, 155, 158),
    blue: rgb(131, 165, 152),
    lavender: rgb(184, 150, 214),
    text: rgb(235, 219, 178),
    subtext1: rgb(213, 196, 161),
    subtext0: rgb(189, 174, 147),
    overlay2: rgb(168, 153, 132),
    overlay1: rgb(146, 131, 116),
    overlay0: rgb(124, 111, 100),
    surface2: rgb(102, 92, 84),
    surface1: rgb(60, 56, 54),
    surface0: rgb(40, 40, 40),
    base: rgb(29, 32, 33),
    mantle: rgb(20, 22, 23),
    crust: rgb(18, 18, 18),
    tier_base: rgb(29, 32, 33),
    tier_panel: rgb(40, 40, 40),
    tier_raised: rgb(60, 56, 54),
    tier_selected: rgb(80, 73, 69),
    dark_purple: rgb(120, 68, 148),
    group_powder_blue: rgb(168, 187, 180),
    epic_purple: rgb(191, 138, 182),
    story_yellow_orange: rgb(216, 158, 74),
    task_gray: rgb(153, 143, 131),
    raw_session_magenta: rgb(202, 122, 120),
};

// Maximized-separation dark theme: near-black tier ladder, saturated
// high-luminance accents. surface0/surface1 = tier_panel/tier_raised (same
// requirement as Gruvbox Warm).
const HIGH_CONTRAST_PALETTE: CustomPalette = CustomPalette {
    rosewater: rgb(255, 210, 210),
    flamingo: rgb(255, 120, 120),
    pink: rgb(255, 60, 200),
    mauve: rgb(190, 90, 255),
    red: rgb(255, 60, 60),
    maroon: rgb(200, 20, 20),
    peach: rgb(255, 150, 40),
    yellow: rgb(255, 230, 0),
    green: rgb(60, 255, 120),
    teal: rgb(0, 255, 210),
    sky: rgb(80, 210, 255),
    sapphire: rgb(0, 160, 255),
    blue: rgb(60, 120, 255),
    lavender: rgb(180, 170, 255),
    text: rgb(255, 255, 255),
    subtext1: rgb(225, 225, 225),
    subtext0: rgb(200, 200, 200),
    overlay2: rgb(170, 170, 170),
    overlay1: rgb(130, 130, 130),
    overlay0: rgb(90, 90, 90),
    surface2: rgb(72, 72, 72),
    surface1: rgb(36, 36, 36),
    surface0: rgb(18, 18, 18),
    base: rgb(0, 0, 0),
    mantle: rgb(4, 4, 4),
    crust: rgb(0, 0, 0),
    tier_base: rgb(0, 0, 0),
    tier_panel: rgb(18, 18, 18),
    tier_raised: rgb(36, 36, 36),
    tier_selected: rgb(58, 58, 58),
    dark_purple: rgb(140, 60, 220),
    group_powder_blue: rgb(140, 210, 255),
    epic_purple: rgb(200, 100, 255),
    story_yellow_orange: rgb(255, 180, 40),
    task_gray: rgb(150, 150, 150),
    raw_session_magenta: rgb(255, 60, 180),
};

// Light bg, dark text — the largest departure from the other 5 themes.
// Elevation ladder is progressively-grayer-not-lighter (the light-theme
// mirror of the dark themes' progressively-lighter one). `crust` stays dark
// (never inherited from the light base tone): it feeds
// active_tab_fg/mode-badge foregrounds drawn on bright accent backgrounds,
// and dark-on-bright is needed there regardless of overall theme lightness.
// surface0/surface1 = tier_panel/tier_raised (same requirement as the other
// two new themes). Accents are deep inks: they colour text on the white
// canvas, so each keeps at least 4.5:1 against tier_base and tier_panel.
const LIGHT_PALETTE: CustomPalette = CustomPalette {
    rosewater: rgb(147, 95, 86),
    flamingo: rgb(170, 83, 69),
    pink: rgb(182, 63, 133),
    mauve: rgb(120, 80, 170),
    red: rgb(200, 40, 40),
    maroon: rgb(150, 30, 40),
    peach: rgb(171, 84, 0),
    yellow: rgb(148, 99, 0),
    green: rgb(17, 125, 45),
    teal: rgb(0, 122, 112),
    sky: rgb(29, 113, 172),
    sapphire: rgb(20, 100, 150),
    blue: rgb(28, 102, 181),
    lavender: rgb(116, 96, 174),
    text: rgb(30, 30, 30),
    subtext1: rgb(55, 53, 50),
    subtext0: rgb(80, 78, 74),
    overlay2: rgb(95, 91, 86),
    overlay1: rgb(120, 116, 110),
    overlay0: rgb(150, 146, 140),
    surface2: rgb(200, 196, 184),
    surface1: rgb(230, 227, 219),
    surface0: rgb(243, 241, 236),
    base: rgb(255, 255, 255),
    mantle: rgb(250, 249, 246),
    crust: rgb(20, 20, 20),
    tier_base: rgb(255, 255, 255),
    tier_panel: rgb(243, 241, 236),
    tier_raised: rgb(230, 227, 219),
    tier_selected: rgb(214, 210, 198),
    dark_purple: rgb(90, 60, 140),
    group_powder_blue: rgb(64, 113, 153),
    epic_purple: rgb(110, 70, 160),
    story_yellow_orange: rgb(160, 92, 0),
    task_gray: rgb(112, 108, 103),
    raw_session_magenta: rgb(170, 60, 110),
};

const RAINBOW_PALETTE: CustomPalette = CustomPalette {
    rosewater: rgb(255, 214, 231),
    flamingo: rgb(255, 138, 128),
    pink: rgb(255, 79, 216),
    mauve: rgb(199, 125, 255),
    red: rgb(255, 77, 90),
    maroon: rgb(229, 73, 125),
    peach: rgb(255, 159, 67),
    yellow: rgb(255, 225, 77),
    green: rgb(92, 255, 136),
    teal: rgb(46, 242, 208),
    sky: rgb(86, 217, 255),
    sapphire: rgb(47, 167, 255),
    blue: rgb(95, 124, 255),
    lavender: rgb(184, 167, 255),
    text: rgb(255, 248, 255),
    subtext1: rgb(232, 223, 244),
    subtext0: rgb(200, 189, 217),
    overlay2: rgb(160, 148, 178),
    overlay1: rgb(143, 126, 160),
    overlay0: rgb(119, 103, 137),
    surface2: rgb(48, 34, 77),
    surface1: rgb(33, 24, 55),
    surface0: rgb(21, 16, 36),
    base: rgb(8, 6, 15),
    mantle: rgb(5, 4, 10),
    crust: rgb(5, 4, 10),
    tier_base: rgb(8, 6, 15),
    tier_panel: rgb(21, 16, 36),
    tier_raised: rgb(33, 24, 55),
    tier_selected: rgb(38, 20, 63),
    dark_purple: rgb(167, 107, 255),
    group_powder_blue: rgb(142, 219, 255),
    epic_purple: rgb(209, 124, 255),
    story_yellow_orange: rgb(255, 177, 74),
    task_gray: rgb(167, 160, 181),
    raw_session_magenta: rgb(255, 95, 200),
};

// Terminal-default backgrounds are a semantic policy, not raw palette data.
// Keep readable RGB fallbacks here for active selection, search interpolation,
// and any role that intentionally paints rather than yielding to the terminal.
const TRULY_TRANSPARENT_PALETTE: CustomPalette = CustomPalette {
    rosewater: Color::LightMagenta,
    flamingo: Color::LightRed,
    pink: Color::LightMagenta,
    mauve: Color::Magenta,
    red: Color::LightRed,
    maroon: Color::Red,
    peach: Color::LightYellow,
    yellow: Color::Yellow,
    green: Color::LightGreen,
    teal: Color::LightCyan,
    sky: Color::LightCyan,
    sapphire: Color::Cyan,
    blue: Color::LightBlue,
    lavender: Color::LightMagenta,
    text: Color::White,
    subtext1: Color::White,
    subtext0: Color::Gray,
    overlay2: Color::Gray,
    overlay1: Color::Gray,
    overlay0: Color::DarkGray,
    surface2: rgb(64, 68, 82),
    surface1: rgb(45, 49, 61),
    surface0: rgb(28, 31, 40),
    base: rgb(14, 16, 22),
    mantle: rgb(10, 12, 17),
    crust: Color::Black,
    tier_base: rgb(14, 16, 22),
    tier_panel: rgb(28, 31, 40),
    tier_raised: rgb(45, 49, 61),
    tier_selected: rgb(64, 68, 82),
    dark_purple: Color::Magenta,
    group_powder_blue: Color::LightCyan,
    epic_purple: Color::LightMagenta,
    story_yellow_orange: Color::LightYellow,
    task_gray: Color::Gray,
    raw_session_magenta: Color::LightMagenta,
};

const CUP_A_JOE_PALETTE: CustomPalette = CustomPalette {
    rosewater: rgb(242, 210, 189),
    flamingo: rgb(232, 154, 130),
    pink: rgb(229, 138, 174),
    mauve: rgb(195, 154, 217),
    red: rgb(240, 106, 99),
    maroon: rgb(208, 97, 105),
    peach: rgb(242, 166, 90),
    yellow: rgb(232, 196, 92),
    green: rgb(143, 203, 120),
    teal: rgb(102, 194, 165),
    sky: rgb(142, 201, 214),
    sapphire: rgb(95, 168, 198),
    blue: rgb(127, 166, 216),
    lavender: rgb(184, 167, 217),
    text: rgb(255, 243, 218),
    subtext1: rgb(230, 211, 180),
    subtext0: rgb(200, 177, 143),
    overlay2: rgb(180, 152, 118),
    overlay1: rgb(160, 132, 94),
    overlay0: rgb(136, 108, 72),
    surface2: rgb(70, 48, 37),
    surface1: rgb(49, 33, 25),
    surface0: rgb(33, 22, 17),
    base: rgb(18, 12, 9),
    mantle: rgb(12, 8, 6),
    crust: rgb(10, 7, 5),
    tier_base: rgb(18, 12, 9),
    tier_panel: rgb(33, 22, 17),
    tier_raised: rgb(49, 33, 25),
    tier_selected: rgb(56, 35, 24),
    dark_purple: rgb(167, 122, 194),
    group_powder_blue: rgb(159, 197, 211),
    epic_purple: rgb(190, 140, 208),
    story_yellow_orange: rgb(231, 174, 98),
    task_gray: rgb(178, 162, 143),
    raw_session_magenta: rgb(217, 130, 167),
};

const EMERALD_PALETTE: CustomPalette = CustomPalette {
    rosewater: rgb(255, 224, 213),
    flamingo: rgb(255, 146, 127),
    pink: rgb(255, 119, 183),
    mauve: rgb(201, 139, 255),
    red: rgb(255, 107, 115),
    maroon: rgb(224, 95, 131),
    peach: rgb(255, 170, 92),
    yellow: rgb(245, 215, 110),
    green: rgb(69, 224, 155),
    teal: rgb(53, 214, 193),
    sky: rgb(101, 215, 255),
    sapphire: rgb(58, 167, 222),
    blue: rgb(122, 156, 255),
    lavender: rgb(176, 165, 255),
    text: rgb(237, 255, 247),
    subtext1: rgb(208, 242, 226),
    subtext0: rgb(172, 211, 191),
    overlay2: rgb(130, 170, 150),
    overlay1: rgb(107, 149, 128),
    overlay0: rgb(83, 125, 103),
    surface2: rgb(26, 73, 56),
    surface1: rgb(18, 51, 40),
    surface0: rgb(12, 33, 25),
    base: rgb(6, 17, 13),
    mantle: rgb(3, 10, 7),
    crust: rgb(3, 10, 7),
    tier_base: rgb(6, 17, 13),
    tier_panel: rgb(12, 33, 25),
    tier_raised: rgb(18, 51, 40),
    tier_selected: rgb(23, 57, 44),
    dark_purple: rgb(168, 117, 214),
    group_powder_blue: rgb(142, 216, 199),
    epic_purple: rgb(197, 139, 238),
    story_yellow_orange: rgb(240, 184, 102),
    task_gray: rgb(152, 179, 165),
    raw_session_magenta: rgb(233, 125, 181),
};

const DIAMOND_PALETTE: CustomPalette = CustomPalette {
    rosewater: rgb(255, 227, 229),
    flamingo: rgb(255, 155, 152),
    pink: rgb(242, 139, 197),
    mauve: rgb(199, 160, 255),
    red: rgb(255, 112, 124),
    maroon: rgb(220, 93, 130),
    peach: rgb(255, 179, 107),
    yellow: rgb(244, 219, 122),
    green: rgb(103, 223, 160),
    teal: rgb(82, 216, 208),
    sky: rgb(121, 220, 255),
    sapphire: rgb(88, 185, 232),
    blue: rgb(123, 159, 255),
    lavender: rgb(185, 176, 255),
    text: rgb(247, 253, 255),
    subtext1: rgb(221, 239, 244),
    subtext0: rgb(187, 216, 224),
    overlay2: rgb(132, 167, 176),
    overlay1: rgb(110, 147, 156),
    overlay0: rgb(86, 122, 133),
    surface2: rgb(37, 70, 81),
    surface1: rgb(25, 49, 59),
    surface0: rgb(15, 32, 40),
    base: rgb(7, 16, 20),
    mantle: rgb(3, 8, 10),
    crust: rgb(2, 6, 8),
    tier_base: rgb(7, 16, 20),
    tier_panel: rgb(15, 32, 40),
    tier_raised: rgb(25, 49, 59),
    tier_selected: rgb(29, 55, 64),
    dark_purple: rgb(169, 156, 255),
    group_powder_blue: rgb(169, 228, 242),
    epic_purple: rgb(192, 173, 255),
    story_yellow_orange: rgb(241, 201, 121),
    task_gray: rgb(167, 187, 194),
    raw_session_magenta: rgb(233, 154, 200),
};

const RUBY_PALETTE: CustomPalette = CustomPalette {
    rosewater: rgb(255, 216, 223),
    flamingo: rgb(255, 143, 154),
    pink: rgb(255, 111, 174),
    mauve: rgb(209, 139, 255),
    red: rgb(255, 90, 111),
    maroon: rgb(233, 71, 105),
    peach: rgb(255, 155, 98),
    yellow: rgb(245, 211, 106),
    green: rgb(102, 213, 138),
    teal: rgb(80, 207, 193),
    sky: rgb(114, 211, 242),
    sapphire: rgb(85, 173, 224),
    blue: rgb(120, 152, 255),
    lavender: rgb(185, 164, 255),
    text: rgb(255, 245, 247),
    subtext1: rgb(242, 217, 223),
    subtext0: rgb(215, 181, 190),
    overlay2: rgb(185, 145, 155),
    overlay1: rgb(168, 123, 135),
    overlay0: rgb(149, 97, 111),
    surface2: rgb(84, 33, 44),
    surface1: rgb(58, 23, 31),
    surface0: rgb(39, 16, 21),
    base: rgb(20, 7, 10),
    mantle: rgb(11, 3, 5),
    crust: rgb(11, 3, 5),
    tier_base: rgb(20, 7, 10),
    tier_panel: rgb(39, 16, 21),
    tier_raised: rgb(58, 23, 31),
    tier_selected: rgb(68, 23, 32),
    dark_purple: rgb(196, 119, 230),
    group_powder_blue: rgb(168, 207, 232),
    epic_purple: rgb(206, 142, 250),
    story_yellow_orange: rgb(242, 176, 108),
    task_gray: rgb(189, 160, 167),
    raw_session_magenta: rgb(255, 116, 174),
};

const SAPHIRE_PALETTE: CustomPalette = CustomPalette {
    rosewater: rgb(255, 224, 230),
    flamingo: rgb(255, 146, 158),
    pink: rgb(240, 125, 184),
    mauve: rgb(192, 140, 255),
    red: rgb(255, 105, 120),
    maroon: rgb(224, 90, 124),
    peach: rgb(255, 167, 93),
    yellow: rgb(242, 213, 103),
    green: rgb(89, 217, 144),
    teal: rgb(67, 208, 196),
    sky: rgb(110, 215, 255),
    sapphire: rgb(77, 180, 255),
    blue: rgb(111, 158, 255),
    lavender: rgb(177, 167, 255),
    text: rgb(242, 247, 255),
    subtext1: rgb(215, 227, 248),
    subtext0: rgb(178, 198, 230),
    overlay2: rgb(134, 158, 194),
    overlay1: rgb(113, 137, 178),
    overlay0: rgb(85, 114, 158),
    surface2: rgb(23, 54, 99),
    surface1: rgb(16, 38, 74),
    surface0: rgb(10, 24, 48),
    base: rgb(5, 11, 23),
    mantle: rgb(2, 6, 13),
    crust: rgb(2, 6, 13),
    tier_base: rgb(5, 11, 23),
    tier_panel: rgb(10, 24, 48),
    tier_raised: rgb(16, 38, 74),
    tier_selected: rgb(21, 49, 89),
    dark_purple: rgb(154, 121, 232),
    group_powder_blue: rgb(156, 206, 255),
    epic_purple: rgb(182, 155, 255),
    story_yellow_orange: rgb(240, 181, 104),
    task_gray: rgb(151, 169, 195),
    raw_session_magenta: rgb(231, 126, 184),
};

const DAMON_PALETTE: CustomPalette = CustomPalette {
    rosewater: rgb(245, 231, 233),
    flamingo: rgb(255, 103, 116),
    pink: rgb(255, 53, 100),
    mauve: rgb(177, 105, 230),
    red: rgb(255, 30, 48),
    maroon: rgb(205, 12, 37),
    peach: rgb(244, 104, 91),
    yellow: rgb(232, 176, 95),
    green: rgb(103, 199, 133),
    teal: rgb(78, 198, 187),
    sky: rgb(104, 192, 235),
    sapphire: rgb(72, 165, 225),
    blue: rgb(113, 138, 235),
    lavender: rgb(203, 186, 255),
    text: rgb(242, 237, 238),
    subtext1: rgb(213, 204, 206),
    subtext0: rgb(179, 168, 171),
    overlay2: rgb(157, 146, 148),
    overlay1: rgb(126, 117, 119),
    overlay0: rgb(91, 84, 86),
    surface2: rgb(51, 48, 49),
    surface1: rgb(31, 29, 30),
    surface0: rgb(17, 16, 16),
    base: rgb(0, 0, 0),
    mantle: rgb(5, 5, 5),
    crust: rgb(0, 0, 0),
    tier_base: rgb(0, 0, 0),
    tier_panel: rgb(16, 15, 15),
    tier_raised: rgb(29, 27, 28),
    tier_selected: rgb(59, 9, 17),
    dark_purple: rgb(156, 92, 220),
    group_powder_blue: rgb(174, 200, 230),
    epic_purple: rgb(182, 134, 250),
    story_yellow_orange: rgb(230, 164, 110),
    task_gray: rgb(192, 192, 192),
    raw_session_magenta: rgb(255, 95, 154),
};

// Loader spectra: hand-tuned, hue-ordered gradient stops that each theme's
// rainbow activity indicators interpolate (in OKLab) around a closed loop.
// They need not reuse the theme's accents, but they are tuned to its canvas
// and mood: adjacent stops stay perceptually close so the loop never jumps,
// and every stop keeps a clear lightness gap from the theme's canvas. Arc
// spectra (gems, Goth, Daemon) walk out and back so the loop stays seamless.

// Fallback for a theme that declares no spectrum: a balanced full wheel.
const DEFAULT_LOADER: [Color; 8] = [
    rgb(232, 98, 109),
    rgb(240, 165, 92),
    rgb(240, 214, 92),
    rgb(112, 214, 132),
    rgb(76, 205, 214),
    rgb(98, 150, 240),
    rgb(166, 124, 232),
    rgb(226, 108, 190),
];

// Goth: a nightshade arc (blood crimson, rose, orchid, amethyst, indigo,
// midnight blue) and back, so the loader stays in the theme's gothic register.
const GOTH_LOADER: [Color; 10] = [
    rgb(203, 68, 83),
    rgb(217, 99, 156),
    rgb(199, 127, 210),
    rgb(180, 154, 237),
    rgb(101, 134, 231),
    rgb(60, 126, 190),
    rgb(101, 134, 231),
    rgb(180, 154, 237),
    rgb(199, 127, 210),
    rgb(217, 99, 156),
];

// Junk Yard: an oil-slick full wheel (rust, amber, hazard yellow, toxic green,
// teal, cyan, electric blue, violet, hot pink).
const JUNK_YARD_LOADER: [Color; 9] = [
    rgb(229, 86, 74),
    rgb(250, 164, 83),
    rgb(238, 208, 76),
    rgb(94, 212, 118),
    rgb(13, 195, 158),
    rgb(82, 202, 224),
    rgb(71, 116, 246),
    rgb(173, 123, 238),
    rgb(225, 97, 188),
];

// Transparent: a soft full wheel that survives wallpaper blending.
const TRANSPARENT_LOADER: [Color; 9] = [
    rgb(222, 126, 136),
    rgb(238, 164, 113),
    rgb(244, 204, 100),
    rgb(143, 201, 144),
    rgb(108, 202, 196),
    rgb(115, 187, 225),
    rgb(135, 162, 234),
    rgb(184, 154, 229),
    rgb(212, 136, 184),
];

// Gruvbox Warm: the canonical gruvbox brights in hue order, bridged back to
// red through its faded blue and purple.
const GRUVBOX_WARM_LOADER: [Color; 9] = [
    rgb(244, 83, 64),
    rgb(248, 132, 45),
    rgb(243, 186, 60),
    rgb(185, 188, 55),
    rgb(139, 191, 121),
    rgb(108, 165, 159),
    rgb(111, 144, 175),
    rgb(160, 120, 173),
    rgb(210, 132, 153),
];

// High Contrast: a saturated, high-luminance full wheel for the black canvas.
const HIGH_CONTRAST_LOADER: [Color; 9] = [
    rgb(255, 65, 60),
    rgb(255, 156, 63),
    rgb(255, 231, 54),
    rgb(71, 255, 120),
    rgb(18, 252, 223),
    rgb(63, 207, 255),
    rgb(64, 125, 255),
    rgb(173, 102, 255),
    rgb(254, 65, 208),
];

// Light: deep ink tones that keep contrast on a white canvas.
const LIGHT_LOADER: [Color; 9] = [
    rgb(204, 40, 39),
    rgb(205, 107, 23),
    rgb(188, 136, 0),
    rgb(39, 142, 61),
    rgb(0, 134, 129),
    rgb(0, 119, 188),
    rgb(77, 87, 183),
    rgb(125, 77, 173),
    rgb(187, 59, 135),
];

// RAAAAINNNNNNBOOOZZZZZZZZ: the theme's own neon accents in true hue order.
const RAINBOW_LOADER: [Color; 10] = [
    rgb(255, 77, 90),
    rgb(255, 159, 67),
    rgb(255, 225, 77),
    rgb(92, 255, 136),
    rgb(46, 242, 208),
    rgb(86, 217, 255),
    rgb(47, 167, 255),
    rgb(95, 124, 255),
    rgb(199, 125, 255),
    rgb(255, 79, 216),
];

// Truly Transparent: a bright, soft wheel that reads over unknown terminal
// backgrounds (the theme's ANSI accents cannot be blended or faded).
const TRULY_TRANSPARENT_LOADER: [Color; 8] = [
    rgb(248, 118, 122),
    rgb(255, 166, 89),
    rgb(246, 214, 83),
    rgb(124, 223, 129),
    rgb(77, 220, 220),
    rgb(89, 170, 248),
    rgb(172, 143, 248),
    rgb(236, 127, 202),
];

// Cup`a Joe: a sun-faded vintage wheel; warm hues carry the colour, cool hues
// stay dusty.
const CUP_A_JOE_LOADER: [Color; 8] = [
    rgb(212, 101, 101),
    rgb(237, 156, 85),
    rgb(235, 198, 105),
    rgb(152, 190, 132),
    rgb(113, 179, 168),
    rgb(118, 165, 198),
    rgb(174, 145, 196),
    rgb(212, 129, 162),
];

// Emerald: a gem arc (gold, lime, emerald, jade, aqua, teal-blue) and back.
const EMERALD_LOADER: [Color; 10] = [
    rgb(238, 208, 89),
    rgb(177, 221, 84),
    rgb(65, 214, 134),
    rgb(40, 205, 167),
    rgb(68, 214, 214),
    rgb(69, 180, 214),
    rgb(68, 214, 214),
    rgb(40, 205, 167),
    rgb(65, 214, 134),
    rgb(177, 221, 84),
];

// Diamond: prismatic fire, pastel flashes interleaved with ice.
const DIAMOND_LOADER: [Color; 8] = [
    rgb(168, 235, 249),
    rgb(255, 168, 181),
    rgb(168, 235, 249),
    rgb(246, 212, 118),
    rgb(168, 235, 249),
    rgb(133, 233, 182),
    rgb(168, 235, 249),
    rgb(195, 175, 255),
];

// Ruby: a gem arc (gold, amber, coral, ruby, crimson, magenta, orchid) and back.
const RUBY_LOADER: [Color; 12] = [
    rgb(248, 202, 101),
    rgb(252, 158, 71),
    rgb(252, 119, 86),
    rgb(240, 65, 98),
    rgb(219, 66, 140),
    rgb(206, 80, 199),
    rgb(191, 124, 236),
    rgb(206, 80, 199),
    rgb(219, 66, 140),
    rgb(240, 65, 98),
    rgb(252, 119, 86),
    rgb(252, 158, 71),
];

// Saphire: a gem arc (aqua, sky, azure, sapphire, indigo, violet, orchid) and
// back.
const SAPHIRE_LOADER: [Color; 12] = [
    rgb(103, 226, 218),
    rgb(82, 207, 243),
    rgb(45, 166, 250),
    rgb(60, 121, 240),
    rgb(105, 103, 225),
    rgb(163, 118, 233),
    rgb(206, 131, 222),
    rgb(163, 118, 233),
    rgb(105, 103, 225),
    rgb(60, 121, 240),
    rgb(45, 166, 250),
    rgb(82, 207, 243),
];

// D. is for Daemon: a hellfire arc (ember gold, flame, blood red, crimson,
// dark wine) and back.
const DAMON_LOADER: [Color; 10] = [
    rgb(244, 195, 82),
    rgb(254, 134, 15),
    rgb(247, 81, 37),
    rgb(238, 11, 42),
    rgb(191, 18, 78),
    rgb(145, 32, 97),
    rgb(191, 18, 78),
    rgb(238, 11, 42),
    rgb(247, 81, 37),
    rgb(254, 134, 15),
];

/// How a theme treats passive structural backgrounds.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TransparencyPolicy {
    /// Every passive surface paints its palette fallback.
    Opaque,
    /// The legacy Transparent theme paints deliberate dark scrims.
    Scrimmed,
    /// Passive surfaces yield to the terminal's configured background.
    TerminalDefault,
}

/// Per-theme border rendering strategy.
///
/// An explicit field on `ThemeDefinition` (not a string-keyed predicate like
/// `is_transparent_theme()`): a single source of truth declared right in the
/// `THEME_DEFINITIONS` table, and a `match` over it is exhaustive at compile
/// time. Decoupled from "is transparent" (which gates an unrelated concern —
/// the backfill-bypass/scrim logic).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BorderPolicy {
    /// Every pane/panel always renders a full colored border — current
    /// universal behavior. Used by both transparent themes.
    FullBorders,
    /// Unfocused panes render a border that blends into the surrounding tier
    /// (bg-contrast, not removed — see `pane_block`); the focused pane
    /// renders a full accent-colored border.
    AccentFocusOnly,
}

/// A palette accent a theme's text-accent mapping may name. Neutral text
/// tiers (text, subtext, overlay) are deliberately absent: the mapping exists
/// so secondary text carries the theme's colour instead of uniform grey. Red
/// is absent too: it means error.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum AccentSlot {
    /// The theme's declared primary colour.
    Primary,
    Rosewater,
    Flamingo,
    Pink,
    Mauve,
    Peach,
    Yellow,
    Green,
    Teal,
    Sky,
    Sapphire,
    Blue,
    Lavender,
}

impl AccentSlot {
    fn resolve(self) -> Color {
        let palette = active_palette();
        match self {
            Self::Primary => active_primary_color(),
            Self::Rosewater => palette.rosewater,
            Self::Flamingo => palette.flamingo,
            Self::Pink => palette.pink,
            Self::Mauve => palette.mauve,
            Self::Peach => palette.peach,
            Self::Yellow => palette.yellow,
            Self::Green => palette.green,
            Self::Teal => palette.teal,
            Self::Sky => palette.sky,
            Self::Sapphire => palette.sapphire,
            Self::Blue => palette.blue,
            Self::Lavender => palette.lavender,
        }
    }
}

/// Per-theme text-accent mapping: which palette accent colours each kind of
/// session metadata and secondary label. Generalises the Truly Transparent
/// treatment (coloured metadata instead of grey) to every theme, with the
/// choices tuned to each theme's mood.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct TextAccents {
    /// Model names.
    model: AccentSlot,
    /// Timestamps, durations and uptime.
    time: AccentSlot,
    /// Reasoning-effort levels.
    effort: AccentSlot,
    /// Turn, token and process counts.
    count: AccentSlot,
    /// Spend.
    cost: AccentSlot,
    /// Working directories and sandbox paths.
    path: AccentSlot,
    /// Section labels.
    label: AccentSlot,
}

impl TextAccents {
    /// Every slot, labelled, for exhaustive checks.
    #[cfg(test)]
    fn slots(self) -> [(&'static str, AccentSlot); 7] {
        [
            ("model", self.model),
            ("time", self.time),
            ("effort", self.effort),
            ("count", self.count),
            ("cost", self.cost),
            ("path", self.path),
            ("label", self.label),
        ]
    }
}

/// Fallback for a theme that declares no mapping: the Truly Transparent set.
const DEFAULT_TEXT_ACCENTS: TextAccents = TextAccents {
    model: AccentSlot::Lavender,
    time: AccentSlot::Sky,
    effort: AccentSlot::Peach,
    count: AccentSlot::Blue,
    cost: AccentSlot::Green,
    path: AccentSlot::Teal,
    label: AccentSlot::Primary,
};

// Goth: moonlit lavender and sky over rose effort and blood-rose paths.
const GOTH_TEXT_ACCENTS: TextAccents = TextAccents {
    model: AccentSlot::Lavender,
    time: AccentSlot::Sky,
    effort: AccentSlot::Pink,
    count: AccentSlot::Blue,
    cost: AccentSlot::Green,
    path: AccentSlot::Rosewater,
    label: AccentSlot::Primary,
};

// Junk Yard: hazard yellow time, rust effort, oil-slick cyan and teal.
const JUNK_YARD_TEXT_ACCENTS: TextAccents = TextAccents {
    model: AccentSlot::Sky,
    time: AccentSlot::Yellow,
    effort: AccentSlot::Peach,
    count: AccentSlot::Lavender,
    cost: AccentSlot::Green,
    path: AccentSlot::Sapphire,
    label: AccentSlot::Primary,
};

// Transparent and Truly Transparent share the original coloured-metadata set
// (amber section labels included); it is the treatment the others generalise.
const TRANSPARENT_TEXT_ACCENTS: TextAccents = TextAccents {
    model: AccentSlot::Lavender,
    time: AccentSlot::Sky,
    effort: AccentSlot::Peach,
    count: AccentSlot::Blue,
    cost: AccentSlot::Green,
    path: AccentSlot::Teal,
    label: AccentSlot::Yellow,
};

// Gruvbox Warm: the canonical brights (purple, aqua, yellow, blue, green).
const GRUVBOX_WARM_TEXT_ACCENTS: TextAccents = TextAccents {
    model: AccentSlot::Pink,
    time: AccentSlot::Sky,
    effort: AccentSlot::Yellow,
    count: AccentSlot::Blue,
    cost: AccentSlot::Green,
    path: AccentSlot::Teal,
    label: AccentSlot::Primary,
};

// High Contrast: saturated, widely separated hues on black.
const HIGH_CONTRAST_TEXT_ACCENTS: TextAccents = TextAccents {
    model: AccentSlot::Lavender,
    time: AccentSlot::Yellow,
    effort: AccentSlot::Peach,
    count: AccentSlot::Teal,
    cost: AccentSlot::Green,
    path: AccentSlot::Sapphire,
    label: AccentSlot::Primary,
};

// Light: deep ink tones that read as text on white.
const LIGHT_TEXT_ACCENTS: TextAccents = TextAccents {
    model: AccentSlot::Mauve,
    time: AccentSlot::Sapphire,
    effort: AccentSlot::Peach,
    count: AccentSlot::Pink,
    cost: AccentSlot::Green,
    path: AccentSlot::Teal,
    label: AccentSlot::Primary,
};

// RAAAAINNNNNNBOOOZZZZZZZZ: one neon per role.
const RAINBOW_TEXT_ACCENTS: TextAccents = TextAccents {
    model: AccentSlot::Mauve,
    time: AccentSlot::Sky,
    effort: AccentSlot::Peach,
    count: AccentSlot::Yellow,
    cost: AccentSlot::Green,
    path: AccentSlot::Pink,
    label: AccentSlot::Primary,
};

// Cup`a Joe: caramel time and honey effort, dusty cool hues for the rest.
const CUP_A_JOE_TEXT_ACCENTS: TextAccents = TextAccents {
    model: AccentSlot::Mauve,
    time: AccentSlot::Peach,
    effort: AccentSlot::Yellow,
    count: AccentSlot::Sky,
    cost: AccentSlot::Green,
    path: AccentSlot::Teal,
    label: AccentSlot::Primary,
};

// Emerald: jade, gold and aqua around the emerald primary.
const EMERALD_TEXT_ACCENTS: TextAccents = TextAccents {
    model: AccentSlot::Lavender,
    time: AccentSlot::Teal,
    effort: AccentSlot::Yellow,
    count: AccentSlot::Sky,
    cost: AccentSlot::Green,
    path: AccentSlot::Sapphire,
    label: AccentSlot::Primary,
};

// Diamond: prismatic flashes (violet, rose, gold) against ice.
const DIAMOND_TEXT_ACCENTS: TextAccents = TextAccents {
    model: AccentSlot::Mauve,
    time: AccentSlot::Sapphire,
    effort: AccentSlot::Yellow,
    count: AccentSlot::Pink,
    cost: AccentSlot::Green,
    path: AccentSlot::Teal,
    label: AccentSlot::Primary,
};

// Ruby: a warm gem register (amber, gold, coral) with orchid models.
const RUBY_TEXT_ACCENTS: TextAccents = TextAccents {
    model: AccentSlot::Mauve,
    time: AccentSlot::Peach,
    effort: AccentSlot::Yellow,
    count: AccentSlot::Lavender,
    cost: AccentSlot::Green,
    path: AccentSlot::Flamingo,
    label: AccentSlot::Primary,
};

// Saphire: sky, aqua and azure around the sapphire primary.
const SAPHIRE_TEXT_ACCENTS: TextAccents = TextAccents {
    model: AccentSlot::Lavender,
    time: AccentSlot::Sky,
    effort: AccentSlot::Yellow,
    count: AccentSlot::Teal,
    cost: AccentSlot::Green,
    path: AccentSlot::Sapphire,
    label: AccentSlot::Primary,
};

// D. is for Daemon: hellfire gold and ember, brimstone pink, bone paths.
const DAMON_TEXT_ACCENTS: TextAccents = TextAccents {
    model: AccentSlot::Mauve,
    time: AccentSlot::Yellow,
    effort: AccentSlot::Peach,
    count: AccentSlot::Flamingo,
    cost: AccentSlot::Green,
    path: AccentSlot::Rosewater,
    label: AccentSlot::Primary,
};

#[derive(Clone, Copy)]
struct ThemeDefinition {
    key: &'static str,
    display_name: &'static str,
    highlight_index: usize,
    palette: &'static CustomPalette,
    primary: Color,
    border_policy: BorderPolicy,
    transparency_policy: TransparencyPolicy,
    /// Closed-loop gradient stops for the rainbow activity indicators.
    loader: &'static [Color],
    /// Accent colours for metadata and secondary text; `None` falls back to
    /// [`DEFAULT_TEXT_ACCENTS`].
    text_accents: Option<&'static TextAccents>,
}

impl ThemeDefinition {
    const fn new(
        key: &'static str,
        display_name: &'static str,
        palette: &'static CustomPalette,
        highlight_index: usize,
        primary: Color,
        border_policy: BorderPolicy,
        transparency_policy: TransparencyPolicy,
    ) -> Self {
        Self {
            key,
            display_name,
            highlight_index,
            palette,
            primary,
            border_policy,
            transparency_policy,
            loader: &DEFAULT_LOADER,
            text_accents: None,
        }
    }

    /// Declare the theme's loader spectrum (see the `*_LOADER` stops).
    const fn with_loader(self, loader: &'static [Color]) -> Self {
        Self { loader, ..self }
    }

    /// Declare the theme's text-accent mapping (see the `*_TEXT_ACCENTS`).
    const fn with_text_accents(self, text_accents: &'static TextAccents) -> Self {
        Self {
            text_accents: Some(text_accents),
            ..self
        }
    }
}

const THEME_DEFINITIONS: [ThemeDefinition; 14] = [
    ThemeDefinition::new(
        "goth",
        "Goth",
        &GOTH_PALETTE,
        0,
        rgb(181, 138, 215),
        BorderPolicy::AccentFocusOnly,
        TransparencyPolicy::Opaque,
    )
    .with_loader(&GOTH_LOADER)
    .with_text_accents(&GOTH_TEXT_ACCENTS),
    ThemeDefinition::new(
        "junkyard",
        "Junk Yard",
        &JUNK_YARD_PALETTE,
        0,
        rgb(30, 185, 128),
        BorderPolicy::AccentFocusOnly,
        TransparencyPolicy::Opaque,
    )
    .with_loader(&JUNK_YARD_LOADER)
    .with_text_accents(&JUNK_YARD_TEXT_ACCENTS),
    ThemeDefinition::new(
        "transparent",
        "Transparent",
        &TRANSPARENT_PALETTE,
        0,
        rgb(137, 180, 250),
        BorderPolicy::FullBorders,
        TransparencyPolicy::Scrimmed,
    )
    .with_loader(&TRANSPARENT_LOADER)
    .with_text_accents(&TRANSPARENT_TEXT_ACCENTS),
    ThemeDefinition::new(
        "gruvbox-warm",
        "Gruvbox Warm",
        &GRUVBOX_WARM_PALETTE,
        1,
        rgb(254, 128, 25),
        BorderPolicy::AccentFocusOnly,
        TransparencyPolicy::Opaque,
    )
    .with_loader(&GRUVBOX_WARM_LOADER)
    .with_text_accents(&GRUVBOX_WARM_TEXT_ACCENTS),
    ThemeDefinition::new(
        "high-contrast",
        "High Contrast",
        &HIGH_CONTRAST_PALETTE,
        3,
        rgb(0, 215, 255),
        BorderPolicy::AccentFocusOnly,
        TransparencyPolicy::Opaque,
    )
    .with_loader(&HIGH_CONTRAST_LOADER)
    .with_text_accents(&HIGH_CONTRAST_TEXT_ACCENTS),
    ThemeDefinition::new(
        "light",
        "Light",
        &LIGHT_PALETTE,
        0,
        rgb(28, 102, 181),
        BorderPolicy::AccentFocusOnly,
        TransparencyPolicy::Opaque,
    )
    .with_loader(&LIGHT_LOADER)
    .with_text_accents(&LIGHT_TEXT_ACCENTS),
    ThemeDefinition::new(
        "raaaainnnnnnbooozzzzzzzz",
        "RAAAAINNNNNNBOOOZZZZZZZZ",
        &RAINBOW_PALETTE,
        3,
        rgb(46, 242, 208),
        BorderPolicy::AccentFocusOnly,
        TransparencyPolicy::Opaque,
    )
    .with_loader(&RAINBOW_LOADER)
    .with_text_accents(&RAINBOW_TEXT_ACCENTS),
    ThemeDefinition::new(
        "truly-transparent",
        "Truly Transparent",
        &TRULY_TRANSPARENT_PALETTE,
        4,
        Color::LightCyan,
        BorderPolicy::FullBorders,
        TransparencyPolicy::TerminalDefault,
    )
    .with_loader(&TRULY_TRANSPARENT_LOADER)
    .with_text_accents(&TRANSPARENT_TEXT_ACCENTS),
    ThemeDefinition::new(
        "cup-a-joe",
        "Cup`a Joe",
        &CUP_A_JOE_PALETTE,
        1,
        rgb(214, 154, 85),
        BorderPolicy::AccentFocusOnly,
        TransparencyPolicy::Opaque,
    )
    .with_loader(&CUP_A_JOE_LOADER)
    .with_text_accents(&CUP_A_JOE_TEXT_ACCENTS),
    ThemeDefinition::new(
        "emerald",
        "Emerald",
        &EMERALD_PALETTE,
        3,
        rgb(40, 209, 124),
        BorderPolicy::AccentFocusOnly,
        TransparencyPolicy::Opaque,
    )
    .with_loader(&EMERALD_LOADER)
    .with_text_accents(&EMERALD_TEXT_ACCENTS),
    ThemeDefinition::new(
        "diamond",
        "Diamond",
        &DIAMOND_PALETTE,
        3,
        rgb(141, 235, 255),
        BorderPolicy::AccentFocusOnly,
        TransparencyPolicy::Opaque,
    )
    .with_loader(&DIAMOND_LOADER)
    .with_text_accents(&DIAMOND_TEXT_ACCENTS),
    ThemeDefinition::new(
        "ruby",
        "Ruby",
        &RUBY_PALETTE,
        3,
        rgb(255, 71, 112),
        BorderPolicy::AccentFocusOnly,
        TransparencyPolicy::Opaque,
    )
    .with_loader(&RUBY_LOADER)
    .with_text_accents(&RUBY_TEXT_ACCENTS),
    ThemeDefinition::new(
        "saphire",
        "Saphire",
        &SAPHIRE_PALETTE,
        2,
        rgb(97, 160, 255),
        BorderPolicy::AccentFocusOnly,
        TransparencyPolicy::Opaque,
    )
    .with_loader(&SAPHIRE_LOADER)
    .with_text_accents(&SAPHIRE_TEXT_ACCENTS),
    ThemeDefinition::new(
        "d-is-for-devil",
        "D. is for Daemon",
        &DAMON_PALETTE,
        3,
        rgb(255, 30, 48),
        BorderPolicy::AccentFocusOnly,
        TransparencyPolicy::Opaque,
    )
    .with_loader(&DAMON_LOADER)
    .with_text_accents(&DAMON_TEXT_ACCENTS),
];

pub const THEME_COUNT: usize = THEME_DEFINITIONS.len();

fn active_palette() -> &'static CustomPalette {
    THEME_DEFINITIONS[active_theme_index()].palette
}

/// Set the active theme by index.
pub fn set_theme_by_index(index: usize) {
    if THEME_DEFINITIONS.is_empty() {
        return;
    }
    let clamped = index.min(THEME_DEFINITIONS.len() - 1) as u8;
    ACTIVE_THEME_INDEX.store(clamped, Ordering::Relaxed);
}

/// Attempt to set the active theme by name (identifier string).
/// Returns true if the theme was found.
pub fn set_theme_by_name(name: &str) -> bool {
    let normalized = normalize_theme_key(name);
    if let Some(idx) = THEME_DEFINITIONS.iter().position(|def| {
        def.key.eq_ignore_ascii_case(&normalized)
            || normalize_theme_key(def.display_name) == normalized
    }) {
        set_theme_by_index(idx);
        true
    } else {
        false
    }
}

fn normalize_theme_key(name: &str) -> String {
    name.trim().to_lowercase().replace('é', "e")
}

/// Number of available themes.
pub fn theme_count() -> usize {
    THEME_DEFINITIONS.len()
}

/// Display name for a theme index.
pub fn theme_display_name(index: usize) -> &'static str {
    THEME_DEFINITIONS[index.min(THEME_DEFINITIONS.len() - 1)].display_name
}

/// Stable key (goth, junkyard, transparent) for a theme index.
pub fn theme_key(index: usize) -> &'static str {
    THEME_DEFINITIONS[index.min(THEME_DEFINITIONS.len() - 1)].key
}

/// Current active theme index.
pub fn active_theme_index() -> usize {
    ACTIVE_THEME_INDEX.load(Ordering::Relaxed) as usize
}

/// Current active theme key.
pub fn active_theme_key() -> &'static str {
    theme_key(active_theme_index())
}

/// True when the legacy dark-scrim Transparent theme is active.
pub fn is_scrimmed_theme() -> bool {
    active_theme_definition().transparency_policy == TransparencyPolicy::Scrimmed
}

/// True when passive backgrounds should use the terminal default.
pub fn uses_terminal_default_backgrounds() -> bool {
    active_theme_definition().transparency_policy == TransparencyPolicy::TerminalDefault
}

/// True when either transparent rendering policy is active.
pub fn is_transparent_theme() -> bool {
    active_theme_definition().transparency_policy != TransparencyPolicy::Opaque
}

/// Index into the syntax highlighting theme table.
pub fn highlight_theme_index() -> usize {
    THEME_DEFINITIONS[active_theme_index()].highlight_index
}

fn active_theme_definition() -> &'static ThemeDefinition {
    &THEME_DEFINITIONS[active_theme_index()]
}

fn active_primary_color() -> Color {
    active_theme_definition().primary
}

/// Active theme's border rendering policy (`FullBorders` vs `AccentFocusOnly`).
pub fn active_border_policy() -> BorderPolicy {
    active_theme_definition().border_policy
}

/// Get swatch colors for the theme at `index`.
pub fn theme_swatch(index: usize) -> [Color; 8] {
    let palette = THEME_DEFINITIONS[index.min(THEME_DEFINITIONS.len() - 1)].palette;
    [
        palette.red,
        palette.peach,
        palette.yellow,
        palette.green,
        palette.blue,
        palette.mauve,
        palette.pink,
        palette.lavender,
    ]
}

fn encode_rgb(rgb: [u8; 3]) -> u32 {
    ((rgb[0] as u32) << 16) | ((rgb[1] as u32) << 8) | rgb[2] as u32
}

fn decode_rgb(raw: u32) -> [u8; 3] {
    [
        ((raw >> 16) & 0xff) as u8,
        ((raw >> 8) & 0xff) as u8,
        (raw & 0xff) as u8,
    ]
}

pub fn set_theme_role_override(role: ThemeRole, rgb: Option<[u8; 3]>) {
    let raw = rgb.map(encode_rgb).unwrap_or(NO_THEME_ROLE_OVERRIDE);
    THEME_ROLE_OVERRIDES[role as usize].store(raw, Ordering::Relaxed);
    COMMITTED_THEME_ROLE_OVERRIDES[role as usize].store(raw, Ordering::Relaxed);
}

/// Apply an uncommitted live-preview value. Persistence never reads this slot.
pub fn set_theme_role_preview(role: ThemeRole, rgb: Option<[u8; 3]>) {
    THEME_ROLE_OVERRIDES[role as usize].store(
        rgb.map(encode_rgb).unwrap_or(NO_THEME_ROLE_OVERRIDE),
        Ordering::Relaxed,
    );
}

pub fn get_theme_role_override(role: ThemeRole) -> Option<[u8; 3]> {
    let raw = THEME_ROLE_OVERRIDES[role as usize].load(Ordering::Relaxed);
    (raw != NO_THEME_ROLE_OVERRIDE).then(|| decode_rgb(raw))
}

pub fn clear_theme_role_overrides() {
    for role in ThemeRole::ALL {
        set_theme_role_override(role, None);
    }
}

pub fn snapshot_theme_role_overrides() -> Vec<(ThemeRole, [u8; 3])> {
    ThemeRole::ALL
        .into_iter()
        .filter_map(|role| {
            let raw = COMMITTED_THEME_ROLE_OVERRIDES[role as usize].load(Ordering::Relaxed);
            (raw != NO_THEME_ROLE_OVERRIDE).then(|| (role, decode_rgb(raw)))
        })
        .collect()
}

pub fn apply_theme_role_overrides(overrides: &[(ThemeRole, [u8; 3])]) {
    clear_theme_role_overrides();
    for &(role, rgb) in overrides {
        set_theme_role_override(role, Some(rgb));
    }
}

/// Apply the startup theme state as one unit.  Production remains lock-free;
/// tests serialize ordinary App construction with theme-mutating render tests.
pub fn apply_startup_theme_state(
    theme_name: Option<&str>,
    role_overrides: &[(ThemeRole, [u8; 3])],
) {
    #[cfg(test)]
    let _guard = test_app_initialization_guard();

    if let Some(name) = theme_name {
        let _ = set_theme_by_name(name);
    }
    apply_theme_role_overrides(role_overrides);
}

fn semantic_baseline(role: ThemeRole) -> Color {
    let palette = active_palette();
    match role {
        ThemeRole::Canvas => palette.base,
        ThemeRole::Panel => palette.tier_panel,
        ThemeRole::ElevatedSurface => palette.tier_raised,
        ThemeRole::Border => {
            if is_transparent_theme() {
                Color::White
            } else {
                palette.dark_purple
            }
        }
        ThemeRole::FocusedBorder | ThemeRole::Accent => active_primary_color(),
        ThemeRole::SelectedRow => palette.tier_selected,
        ThemeRole::PrimaryText | ThemeRole::Toast => palette.text,
        ThemeRole::MutedText => palette.subtext1,
        ThemeRole::Pin => palette.mauve,
        ThemeRole::Running => palette.green,
        ThemeRole::Waiting => palette.pink,
        ThemeRole::Success => palette.blue,
        ThemeRole::Warning => palette.peach,
        ThemeRole::Error => palette.red,
        ThemeRole::Disabled => palette.overlay0,
    }
}

pub fn semantic_color(role: ThemeRole) -> Color {
    get_theme_role_override(role)
        .map(|[r, g, b]| Color::Rgb(r, g, b))
        .unwrap_or_else(|| semantic_baseline(role))
}

macro_rules! themed_color_fn {
    ($fn_name:ident, $field:ident) => {
        pub fn $fn_name() -> Color {
            active_palette().$field
        }
    };
}

// --- Surface colors ---
pub fn base() -> Color {
    semantic_color(ThemeRole::Canvas)
}
themed_color_fn!(mantle, mantle);
themed_color_fn!(crust, crust);
themed_color_fn!(surface0, surface0);
themed_color_fn!(surface1, surface1);
themed_color_fn!(surface2, surface2);
themed_color_fn!(tier_base, tier_base);
pub fn tier_panel() -> Color {
    semantic_color(ThemeRole::Panel)
}
pub fn tier_raised() -> Color {
    semantic_color(ThemeRole::ElevatedSurface)
}
themed_color_fn!(tier_selected, tier_selected);

/// Apply the active theme's passive-background policy to a palette fallback.
/// Active selections, cursors, search matches, and badges intentionally use
/// raw palette colors instead of this helper.
pub fn structural_bg(fallback: Color) -> Color {
    if uses_terminal_default_backgrounds() {
        Color::Reset
    } else {
        fallback
    }
}

/// Root/canvas background for top-level renderers.
pub fn root_bg() -> Color {
    structural_bg(base())
}

// --- Text colors ---
pub fn text() -> Color {
    semantic_color(ThemeRole::PrimaryText)
}
pub fn subtext1() -> Color {
    semantic_color(ThemeRole::MutedText)
}
themed_color_fn!(subtext0, subtext0);
themed_color_fn!(overlay0, overlay0);
themed_color_fn!(overlay1, overlay1);
themed_color_fn!(overlay2, overlay2);

// --- Accent colors ---
themed_color_fn!(blue, blue);
themed_color_fn!(green, green);
themed_color_fn!(yellow, yellow);
themed_color_fn!(red, red);
themed_color_fn!(mauve, mauve);
themed_color_fn!(dark_purple, dark_purple);
themed_color_fn!(lavender, lavender);
themed_color_fn!(peach, peach);
themed_color_fn!(sapphire, sapphire);
themed_color_fn!(teal, teal);
themed_color_fn!(pink, pink);
themed_color_fn!(sky, sky);
themed_color_fn!(flamingo, flamingo);
themed_color_fn!(rosewater, rosewater);
themed_color_fn!(maroon, maroon);

// --- Hierarchy kind colors ---
themed_color_fn!(group_powder_blue, group_powder_blue);
themed_color_fn!(epic_purple, epic_purple);
themed_color_fn!(story_yellow_orange, story_yellow_orange);
themed_color_fn!(task_gray, task_gray);
themed_color_fn!(raw_session_magenta, raw_session_magenta);

// --- Semantic role mappings ---

// Session detail headers
pub fn assistant_role() -> Color {
    blue()
}
pub fn user_role() -> Color {
    green()
}
/// Border color for assistant messages (when not selected and not tool).
pub fn assistant_message_border() -> Color {
    assistant_role()
}
/// Border color for user messages when not selected.
pub fn user_message_border() -> Color {
    dark_purple()
}
/// Border color for tool call messages (ToolUse / ToolResult) when not selected.
pub fn tool_call_border() -> Color {
    overlay1()
}
/// Border color for tool call messages when selected (cursor is on them).
pub fn tool_call_selected_border() -> Color {
    active_primary_color()
}
/// Background color for the normal-mode block cursor.
pub fn cursor_normal_bg() -> Color {
    text()
}
/// Background color for the insert-mode cursor highlight (rendered under the terminal beam).
pub fn cursor_insert_bg() -> Color {
    green()
}
pub fn tool_name() -> Color {
    peach()
}
pub fn tool_input() -> Color {
    overlay0()
}
pub fn tool_result() -> Color {
    sapphire()
}
pub fn system_event() -> Color {
    overlay0()
}
pub fn seq_and_time() -> Color {
    text()
}
pub fn empty_state() -> Color {
    overlay0()
}

// Session list
pub fn status_starting() -> Color {
    yellow()
}
pub fn status_running() -> Color {
    semantic_color(ThemeRole::Running)
}
pub fn status_waiting() -> Color {
    semantic_color(ThemeRole::Waiting)
}
pub fn status_completed() -> Color {
    semantic_color(ThemeRole::Success)
}
pub fn status_failed() -> Color {
    semantic_color(ThemeRole::Error)
}
pub fn status_interrupted() -> Color {
    overlay0()
}
pub fn status_archived() -> Color {
    overlay0()
}
pub fn status_stalled() -> Color {
    peach()
}

/// Single source of truth: map a `SessionStatus` to its theme color.
/// Every surface (session list, bottom strip, trash/archive browsers) renders
/// status color through this one helper, so a status never renders two ways.
pub fn status_color(status: SessionStatus) -> Color {
    match status {
        SessionStatus::Starting => status_starting(),
        SessionStatus::Running => status_running(),
        SessionStatus::WaitingApproval => status_waiting(),
        SessionStatus::Completed => status_completed(),
        SessionStatus::Failed => status_failed(),
        SessionStatus::Interrupted => status_interrupted(),
        SessionStatus::Archived => status_archived(),
        SessionStatus::Deleted => subtext0(),
        _ => subtext0(),
    }
}

// UI chrome
/// Note: NOT merged with `card_bg()` (see the precision note on
/// `card_bg`/`card_selected_bg`) — transparent's `tier_panel` is pinned 1
/// RGB unit off from `TRANSPARENT_PALETTE.surface0`, an existing, real
/// discrepancy predating this migration.
pub fn glass_panel_bg() -> Color {
    structural_bg(tier_panel())
}

/// Note: NOT merged with `card_selected_bg()` — see `glass_panel_bg`'s note.
pub fn selected_row_bg() -> Color {
    semantic_color(ThemeRole::SelectedRow)
}

/// A subdued selection surface for a session that remains open in the detail
/// pane while the list cursor moves elsewhere.
pub fn viewed_session_row_bg() -> Color {
    lerp_color(tier_panel(), tier_selected(), 0.45)
}

pub fn active_row_rail() -> Color {
    semantic_color(ThemeRole::Accent)
}

/// The rail for the session currently displayed in the detail pane. It stays
/// quieter than the list cursor's primary-color rail.
pub fn viewed_session_row_rail() -> Color {
    overlay1()
}

pub fn table_header_text() -> Color {
    if is_transparent_theme() {
        subtext1()
    } else {
        header_fg()
    }
}

pub fn dim_metadata() -> Color {
    if is_transparent_theme() {
        subtext0()
    } else {
        overlay1()
    }
}

/// The active theme's text-accent mapping.
fn active_text_accents() -> &'static TextAccents {
    active_theme_definition()
        .text_accents
        .unwrap_or(&DEFAULT_TEXT_ACCENTS)
}

// Metadata and secondary text take theme-varied accent colours from the
// active theme's text-accent mapping instead of uniform grey.
pub fn model_text() -> Color {
    active_text_accents().model.resolve()
}

pub fn time_text() -> Color {
    active_text_accents().time.resolve()
}

pub fn effort_text() -> Color {
    active_text_accents().effort.resolve()
}

pub fn count_text() -> Color {
    active_text_accents().count.resolve()
}

pub fn cost_text() -> Color {
    active_text_accents().cost.resolve()
}

pub fn path_text() -> Color {
    active_text_accents().path.resolve()
}

pub fn section_label_text() -> Color {
    active_text_accents().label.resolve()
}

/// Neutral secondary text for labels inside the bounded session browser.
///
/// Unlike the generic section-label role, browser labels must not compete
/// with selected status, required actions, or operational state counts.
pub fn browser_section_label() -> Color {
    subtext1()
}

/// Faint non-interactive chrome inside the bounded session browser.
///
/// `neutral_border()` intentionally preserves a white transparent-theme
/// border, which is too strong for passive rules and dividers.
pub fn browser_decorative_separator() -> Color {
    overlay0()
}

pub fn operations_deck_cell_bg() -> Color {
    let fallback = if is_transparent_theme() {
        surface1()
    } else {
        bottom_strip_bg()
    };
    structural_bg(fallback)
}

pub fn operations_deck_cell_border() -> Color {
    if is_transparent_theme() {
        surface2()
    } else {
        surface1()
    }
}

pub fn warning_status() -> Color {
    semantic_color(ThemeRole::Warning)
}

pub fn error_status() -> Color {
    semantic_color(ThemeRole::Error)
}

pub fn card_bg() -> Color {
    structural_bg(surface0())
}
pub fn card_selected_bg() -> Color {
    surface1()
}
pub fn card_border() -> Color {
    dark_purple()
}
pub fn focused_border() -> Color {
    semantic_color(ThemeRole::FocusedBorder)
}
pub fn unfocused_border() -> Color {
    semantic_color(ThemeRole::Border)
}
/// A visible-but-muted border color for chrome that is NOT focus-aware (the
/// caller has no `focused` bool to switch on, so it can't express
/// `AccentFocusOnly`'s "hidden unless focused" behavior). Transparent keeps
/// its historical White — pinned, part of the regression baseline. Every
/// opaque theme uses `unfocused_border()` (dark_purple) so the border stays
/// visible against both dark and light tier backgrounds. Distinct from the
/// two real focus-aware helpers `pane_block`/`session_detail_block`.
pub fn neutral_border() -> Color {
    semantic_color(ThemeRole::Border)
}
pub fn session_detail_border() -> Color {
    neutral_border()
}
pub fn header_fg() -> Color {
    text()
}
pub fn header_bg() -> Color {
    structural_bg(mantle())
}
pub fn status_line_bg() -> Color {
    if uses_terminal_default_backgrounds() {
        return Color::Reset;
    }
    let bg = mantle();
    if matches!(bg, Color::Reset) {
        surface0()
    } else {
        bg
    }
}

// Tab bar
pub fn active_tab_fg() -> Color {
    crust()
}
pub fn active_tab_bg() -> Color {
    blue()
}
pub fn inactive_tab_fg() -> Color {
    subtext0()
}
pub fn inactive_tab_bg() -> Color {
    structural_bg(mantle())
}
pub fn tab_bar_bg() -> Color {
    structural_bg(mantle())
}
// Inactive tab foreground when showing status background color.
// All accent backgrounds are bright enough to need dark (crust) text.
pub fn inactive_tab_status_fg(status: SessionStatus) -> Color {
    match status {
        SessionStatus::Interrupted | SessionStatus::Archived => subtext0(), // overlay0 bg is dark
        _ => crust(),
    }
}

// Status line mode badges
pub fn mode_normal_fg() -> Color {
    crust()
}
pub fn mode_normal_bg() -> Color {
    blue()
}
pub fn mode_input_fg() -> Color {
    crust()
}
pub fn mode_input_bg() -> Color {
    green()
}
pub fn mode_command_fg() -> Color {
    crust()
}
pub fn mode_command_bg() -> Color {
    yellow()
}

// Status line sections
pub fn connected() -> Color {
    blue()
}
pub fn disconnected() -> Color {
    red()
}
pub fn session_count() -> Color {
    text()
}
pub fn active_count() -> Color {
    green()
}
pub fn waiting_count() -> Color {
    pink()
}
pub fn tab_indicator() -> Color {
    blue()
}
pub fn metadata_text() -> Color {
    text()
}

// Command bar
pub fn command_bar_active() -> Color {
    text()
}
pub fn command_bar_idle() -> Color {
    overlay0()
}

// Fold/truncation indicators (Phase 4-5)
pub fn fold_indicator() -> Color {
    overlay2()
}
pub fn code_fence_lang() -> Color {
    overlay1()
}

// Docregblock button (action button in content)
pub fn docregblock_fg() -> Color {
    crust()
}
pub fn docregblock_bg() -> Color {
    active_primary_color()
}

// Question indicator (non-executable, yellow pill/button)
pub fn question_indicator_fg() -> Color {
    crust()
}
pub fn question_indicator_bg() -> Color {
    active_primary_color()
}

// Markdown element styles
pub fn md_header() -> Color {
    active_primary_color()
}
/// H4-H6 headers — dimmer than primary to create visual hierarchy.
pub fn md_header_minor() -> Color {
    subtext1()
}
pub fn md_blockquote() -> Color {
    overlay1()
}
pub fn md_link_text() -> Color {
    blue()
}
pub fn md_link_url() -> Color {
    overlay0()
}
pub fn md_list_bullet() -> Color {
    overlay1()
}
/// Completed task checkbox and text — green for "done".
pub fn md_task_done() -> Color {
    green()
}
pub fn md_hr() -> Color {
    surface1()
}
pub fn md_table_border() -> Color {
    neutral_border()
}
pub fn md_inline_code_fg() -> Color {
    subtext1()
}
pub fn md_inline_code_bg() -> Color {
    structural_bg(surface0())
}
pub fn md_strikethrough() -> Color {
    overlay1()
}
/// Foreground color for detected file path spans.
/// Sapphire distinguishes paths from links (blue) and inline code (subtext1).
pub fn file_path_fg() -> Color {
    sapphire()
}
pub fn code_block_bg() -> Color {
    structural_bg(surface0())
}

/// Background tint for the cursor line in the file viewer.
/// Surface1 is one step above the base -- subtle but visible.
pub fn file_viewer_cursor_line_bg() -> Color {
    surface1()
}

// Message separator
pub fn message_separator() -> Color {
    surface1()
}

// Bubble backgrounds — subtle tint behind message content for visual grouping
pub fn assistant_bubble_bg() -> Color {
    // Slightly elevated from base — just enough to see the region
    structural_bg(surface0())
}
pub fn user_bubble_bg() -> Color {
    // Two steps above assistant_bubble_bg (surface0) for clear visual distinction
    structural_bg(surface2())
}
pub fn tool_bubble_bg() -> Color {
    structural_bg(surface0())
}
pub fn system_bubble_bg() -> Color {
    structural_bg(mantle())
}
pub fn bottom_strip_bg() -> Color {
    structural_bg(surface0())
}
// Dim model name in header
pub fn model_name_fg() -> Color {
    overlay0()
}

pub fn accent() -> Color {
    semantic_color(ThemeRole::Accent)
}

pub fn pin() -> Color {
    semantic_color(ThemeRole::Pin)
}

pub fn disabled() -> Color {
    semantic_color(ThemeRole::Disabled)
}

pub fn toast_text() -> Color {
    semantic_color(ThemeRole::Toast)
}

#[cfg(test)]
static THEME_TEST_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
thread_local! {
    /// Nesting depth of the pinned-theme scope on this thread.  A depth is
    /// used rather than a flag so a scope may nest (a pinned test body that
    /// renders, or constructs an `App`) without deadlocking on the
    /// non-reentrant `THEME_TEST_GUARD`.
    static THEME_TEST_SCOPE_DEPTH: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn theme_test_scope_active() -> bool {
    THEME_TEST_SCOPE_DEPTH.with(|depth| depth.get() > 0)
}

#[cfg(test)]
fn test_app_initialization_guard() -> Option<std::sync::MutexGuard<'static, ()>> {
    if theme_test_scope_active() {
        None
    } else {
        Some(
            THEME_TEST_GUARD
                .lock()
                .unwrap_or_else(|error| error.into_inner()),
        )
    }
}

/// Serialize TestBackend rendering with a scoped theme mutation.  This is
/// compiled only for tests, preserving the lock-free production resolver.
#[cfg(test)]
pub(crate) fn test_render_guard() -> Option<std::sync::MutexGuard<'static, ()>> {
    test_app_initialization_guard()
}

#[cfg(test)]
struct ThemeSnapshot {
    theme: usize,
    role_overrides: Vec<(ThemeRole, [u8; 3])>,
}

/// RAII scope pinning every global theme value for as long as it is held.
///
/// The render-time guard ([`test_render_guard`]) only covers the render call
/// itself.  A test that renders a color and then compares it against a
/// `theme::*()` call reads the globals *twice*, and a concurrent theme-mutating
/// test can land between those two reads — the render sees theme A, the
/// expected value sees theme B, and the assertion fails on a correct
/// implementation.  Holding this scope for the whole test body makes both reads
/// observe the same theme.
#[cfg(test)]
#[must_use = "the theme stays pinned only while the scope is held"]
pub(crate) struct PinnedThemeState {
    lock: Option<std::sync::MutexGuard<'static, ()>>,
    restore: Option<ThemeSnapshot>,
}

#[cfg(test)]
impl Drop for PinnedThemeState {
    fn drop(&mut self) {
        if let Some(snapshot) = self.restore.take() {
            set_theme_by_index(snapshot.theme);
            apply_theme_role_overrides(&snapshot.role_overrides);
        }
        THEME_TEST_SCOPE_DEPTH.with(|depth| depth.set(depth.get().saturating_sub(1)));
        drop(self.lock.take());
    }
}

/// Pin global theme state for the rest of the current scope, restoring it on
/// drop.  Re-entrant per thread: a nested acquisition observes the active scope
/// and takes no second lock.
#[cfg(test)]
pub(crate) fn pin_theme_state() -> PinnedThemeState {
    if theme_test_scope_active() {
        THEME_TEST_SCOPE_DEPTH.with(|depth| depth.set(depth.get() + 1));
        return PinnedThemeState {
            lock: None,
            restore: None,
        };
    }

    let lock = THEME_TEST_GUARD
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let restore = ThemeSnapshot {
        theme: active_theme_index(),
        role_overrides: snapshot_theme_role_overrides(),
    };
    THEME_TEST_SCOPE_DEPTH.with(|depth| depth.set(depth.get() + 1));
    PinnedThemeState {
        lock: Some(lock),
        restore: Some(restore),
    }
}

/// Run a theme-mutating test with all global theme state restored afterward.
#[cfg(test)]
pub(crate) fn with_theme_state<T>(test: impl FnOnce() -> T) -> T {
    let _scope = pin_theme_state();
    test()
}

#[cfg(test)]
pub(crate) fn test_rgb_channels(color: Color) -> Option<(u8, u8, u8)> {
    match color {
        Color::Rgb(r, g, b) => Some((r, g, b)),
        _ => None,
    }
}

// Overlay / popup
/// Composer border policy, shared with modal frames so every popup stays visible.
pub fn input_bar_border(focused: bool) -> Color {
    match active_border_policy() {
        BorderPolicy::FullBorders => session_detail_border(),
        BorderPolicy::AccentFocusOnly if focused => focused_border(),
        BorderPolicy::AccentFocusOnly => tier_panel(),
    }
}

/// Modal frames match the focused composer, including operator color overrides.
pub fn overlay_border() -> Color {
    input_bar_border(true)
}
pub fn overlay_title() -> Color {
    text()
}
pub fn overlay_bg() -> Color {
    structural_bg(surface0())
}
/// T1 D3 audit note: zero call sites app-wide as of this audit (dead code).
/// Left as a fixed black literal rather than absorbed into a theme fn —
/// wiring it up blind would be pure waste (Tech Wizard lens); a future
/// caller should pick a theme-aware value at that point, not assume this one
/// is already correct.
pub fn overlay_dim_bg() -> Color {
    Color::Rgb(0, 0, 0)
}
pub fn overlay_hint() -> Color {
    overlay0()
}
pub fn overlay_mode_insert_fg() -> Color {
    crust()
}
pub fn overlay_mode_insert_bg() -> Color {
    green()
}
/// Primary text, not `crust`: the badge sits on `surface2`, which is dark in
/// every dark theme, so a crust foreground would vanish there.
pub fn overlay_mode_normal_fg() -> Color {
    text()
}
pub fn overlay_mode_normal_bg() -> Color {
    surface2()
}
pub fn overlay_cwd() -> Color {
    subtext0()
}

// Input bar (persistent input in session detail)
pub fn input_bar_bg() -> Color {
    structural_bg(surface0())
}
pub fn input_bar_mode_normal_bg() -> Color {
    surface1()
}
pub fn input_bar_mode_insert_bg() -> Color {
    if is_transparent_theme() {
        mauve()
    } else {
        green()
    }
}
pub fn input_bar_hint() -> Color {
    overlay1()
}

// Suggestion dropdown
pub fn suggestion_bg() -> Color {
    structural_bg(surface0())
}
pub fn suggestion_fg() -> Color {
    text()
}
pub fn suggestion_selected_bg() -> Color {
    surface2()
}
pub fn suggestion_selected_fg() -> Color {
    text()
}
pub fn suggestion_desc() -> Color {
    overlay0()
}
pub fn suggestion_border() -> Color {
    overlay_border()
}
pub fn visual_selection_bg() -> Color {
    surface2()
}

// File viewer search highlights — sourced from the active palette so they
// track the theme. T1 D3 audit: already theme-relative (lerp/direct from
// the active palette, not a literal) — no structural fix needed here.
pub fn search_match_bg() -> Color {
    // Dim amber: the base background tinted toward the palette's yellow.
    lerp_color(base(), yellow(), 0.33)
}
pub fn search_current_bg() -> Color {
    yellow()
}
pub fn bracket_match_bg() -> Color {
    overlay0()
}

/// Colours `rainbow_palette` samples from the loader spectrum: one per column
/// of the classic strip's repeat, so its period and pace stay unchanged.
pub const RAINBOW_PALETTE_LEN: usize = 14;

/// Rainbow palette for the classic and compact loaders: the active theme's
/// loader spectrum sampled at evenly spaced points around its loop, so the
/// colours flow in hue order and stay tuned to the theme.
pub fn rainbow_palette() -> Vec<Color> {
    let spectrum = loader_spectrum();
    (0..RAINBOW_PALETTE_LEN)
        .map(|index| spectrum.sample(index as f32 / RAINBOW_PALETTE_LEN as f32))
        .collect()
}

/// The active theme's loader spectrum.
pub fn loader_spectrum() -> LoaderSpectrum {
    LoaderSpectrum::new(active_theme_definition().loader)
}

/// A colour in the OKLab perceptual space (`[L, a, b]`).
type Oklab = [f64; 3];

/// A closed loop of gradient stops sampled with perceptual (OKLab)
/// interpolation, so blends between stops keep their brightness instead of
/// muddying the way a channel-wise RGB blend does.
#[derive(Clone, Debug, PartialEq)]
pub struct LoaderSpectrum {
    stops: Vec<Oklab>,
}

impl LoaderSpectrum {
    /// Build a spectrum from `stops`; colours without a resolvable RGB value
    /// are skipped, and an empty result falls back to the default wheel.
    pub fn new(stops: &[Color]) -> Self {
        let resolved: Vec<Oklab> = stops
            .iter()
            .filter_map(|&color| to_rgb(color))
            .map(rgb_to_oklab)
            .collect();
        if resolved.is_empty() {
            return Self::new(&DEFAULT_LOADER);
        }
        Self { stops: resolved }
    }

    /// Colour at `position` around the loop; whole turns wrap, so `0.0`,
    /// `1.0` and `-1.0` all land on the first stop.
    pub fn sample(&self, position: f32) -> Color {
        let (r, g, b) = self.sample_rgb(position);
        Color::Rgb(r, g, b)
    }

    /// RGB channels of [`Self::sample`].
    pub fn sample_rgb(&self, position: f32) -> (u8, u8, u8) {
        let count = self.stops.len();
        let scaled = f64::from(position).rem_euclid(1.0) * count as f64;
        let index = (scaled.floor() as usize).min(count - 1);
        let fraction = scaled - index as f64;
        let from = self.stops[index];
        let to = self.stops[(index + 1) % count];
        oklab_to_rgb([
            from[0] + (to[0] - from[0]) * fraction,
            from[1] + (to[1] - from[1]) * fraction,
            from[2] + (to[2] - from[2]) * fraction,
        ])
    }
}

/// Resolve a colour to sRGB channels when it has a known value (RGB or a
/// basic named colour); `None` for `Reset`, indexed and other terminal-owned
/// colours.
pub fn resolve_rgb(color: Color) -> Option<(u8, u8, u8)> {
    to_rgb(color)
}

/// Perceptual (OKLab) distance between two colours, or `None` when either
/// has no resolvable RGB value.
pub fn perceptual_distance(a: Color, b: Color) -> Option<f64> {
    let a = rgb_to_oklab(to_rgb(a)?);
    let b = rgb_to_oklab(to_rgb(b)?);
    Some(((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt())
}

/// OKLab lightness (`0.0` black to `1.0` white), or `None` when the colour
/// has no resolvable RGB value.
pub fn perceptual_lightness(color: Color) -> Option<f64> {
    to_rgb(color).map(|rgb| rgb_to_oklab(rgb)[0])
}

fn srgb_channel_to_linear(channel: u8) -> f64 {
    let c = f64::from(channel) / 255.0;
    if c <= 0.040_45 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

fn linear_channel_to_srgb(channel: f64) -> u8 {
    let c = channel.clamp(0.0, 1.0);
    let encoded = if c <= 0.003_130_8 {
        c * 12.92
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    };
    (encoded * 255.0).round().clamp(0.0, 255.0) as u8
}

fn rgb_to_oklab((r, g, b): (u8, u8, u8)) -> Oklab {
    let (r, g, b) = (
        srgb_channel_to_linear(r),
        srgb_channel_to_linear(g),
        srgb_channel_to_linear(b),
    );
    let l = (0.412_221_470_8 * r + 0.536_332_536_3 * g + 0.051_445_992_9 * b).cbrt();
    let m = (0.211_903_498_2 * r + 0.680_699_545_1 * g + 0.107_396_956_6 * b).cbrt();
    let s = (0.088_302_461_9 * r + 0.281_718_837_6 * g + 0.629_978_700_5 * b).cbrt();
    [
        0.210_454_255_3 * l + 0.793_617_785_0 * m - 0.004_072_046_8 * s,
        1.977_998_495_1 * l - 2.428_592_205_0 * m + 0.450_593_709_9 * s,
        0.025_904_037_1 * l + 0.782_771_766_2 * m - 0.808_675_766_0 * s,
    ]
}

fn oklab_to_rgb([lightness, a, b]: Oklab) -> (u8, u8, u8) {
    let l = lightness + 0.396_337_777_4 * a + 0.215_803_757_3 * b;
    let m = lightness - 0.105_561_345_8 * a - 0.063_854_172_8 * b;
    let s = lightness - 0.089_484_177_5 * a - 1.291_485_548_0 * b;
    let (l, m, s) = (l * l * l, m * m * m, s * s * s);
    (
        linear_channel_to_srgb(4.076_741_662_1 * l - 3.307_711_591_3 * m + 0.230_969_929_2 * s),
        linear_channel_to_srgb(-1.268_438_004_6 * l + 2.609_757_401_1 * m - 0.341_319_396_5 * s),
        linear_channel_to_srgb(-0.004_196_086_3 * l - 0.703_418_614_7 * m + 1.707_614_701_0 * s),
    )
}

/// Resolve a colour to sRGB when it has a known value (RGB or basic named).
fn to_rgb(c: Color) -> Option<(u8, u8, u8)> {
    match c {
        Color::Rgb(r, g, b) => Some((r, g, b)),
        // Named colors resolved to approximate sRGB values.
        Color::Red => Some((255, 0, 0)),
        Color::Green => Some((0, 128, 0)),
        Color::Blue => Some((0, 0, 255)),
        Color::Yellow => Some((255, 255, 0)),
        Color::Cyan => Some((0, 255, 255)),
        Color::Magenta => Some((255, 0, 255)),
        Color::White => Some((255, 255, 255)),
        Color::Black => Some((0, 0, 0)),
        _ => None,
    }
}

/// Channel-wise blend `a` → `b` at `t` (clamped to `0.0..=1.0`), or `None`
/// when either colour has no resolvable RGB value (`Reset`, indexed, …).
pub fn blend_rgb(a: Color, b: Color, t: f32) -> Option<Color> {
    let (ar, ag, ab) = to_rgb(a)?;
    let (br, bg, bb) = to_rgb(b)?;
    let t = t.clamp(0.0, 1.0);
    let mix = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Some(Color::Rgb(mix(ar, br), mix(ag, bg), mix(ab, bb)))
}

/// Linearly interpolate between two ratatui colors.
///
/// - `t <= 0.0` returns `a`.
/// - `t >= 1.0` returns `b`.
/// - Between 0.0 and 1.0: channel-wise RGB blend.
///
/// Non-RGB colors (indexed, named) are resolved via `Color::Rgb` where possible;
/// unresolvable colors fall back to `b` at `t >= 0.5`, else `a`.
pub fn lerp_color(a: Color, b: Color, t: f32) -> Color {
    if t <= 0.0 {
        return a;
    }
    if t >= 1.0 {
        return b;
    }

    match (to_rgb(a), to_rgb(b)) {
        (Some((ar, ag, ab_)), Some((br, bg, bb_))) => {
            let r = (ar as f32 + (br as f32 - ar as f32) * t).round() as u8;
            let g = (ag as f32 + (bg as f32 - ag as f32) * t).round() as u8;
            let b_out = (ab_ as f32 + (bb_ as f32 - ab_ as f32) * t).round() as u8;
            Color::Rgb(r, g, b_out)
        }
        _ => {
            if t >= 0.5 {
                b
            } else {
                a
            }
        }
    }
}

// --- Block construction helpers ---

/// Overlay/popup block with plain (square-corner) borders.
pub fn overlay_block() -> ratatui::widgets::Block<'static> {
    use ratatui::style::Style;
    use ratatui::widgets::{Block, BorderType, Borders};

    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Plain)
        .border_style(Style::default().fg(overlay_border()))
        .style(Style::default().bg(overlay_bg()))
}

/// Overlay block for unfocused input overlays in the stacked view.
pub fn unfocused_overlay_block() -> ratatui::widgets::Block<'static> {
    use ratatui::style::Style;
    use ratatui::widgets::{Block, BorderType, Borders};

    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Plain)
        .border_style(Style::default().fg(overlay_border()))
        .style(Style::default().bg(overlay_bg()))
}

/// Overlay block with plain (square-corner) borders for Blank popups (paper-like).
pub fn blank_overlay_block() -> ratatui::widgets::Block<'static> {
    use ratatui::style::Style;
    use ratatui::widgets::{Block, BorderType, Borders};

    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Plain)
        .border_style(Style::default().fg(overlay_border()))
        .style(Style::default().bg(overlay_bg()))
}

/// Light gray accent color for Blank overlays (paper-like).
pub fn blank_border() -> Color {
    overlay2()
}

/// Slightly brighter gray-white title color for Blank overlays.
pub fn blank_title() -> Color {
    subtext1()
}

/// Border color based on context window usage percentage.
/// - Under 49%: green
/// - 50-79%: peach (orange)
/// - 80-89%: red
/// - 90-100%: dark red
pub fn context_border_color(context_pct: f64) -> Color {
    if context_pct >= 90.0 {
        // Deliberate cross-theme alarm literal (T1 D3): NOT a `rgb(139,0,0)`
        // palette field for any theme — kept as a fixed numeric constant on
        // purpose so the ">=90% context" alarm reads identically regardless
        // of active theme, the same rationale class as esp_square's
        // theme-independent feedback colors. Contrast reasoning: dark red
        // separates from near-black dark-theme bases primarily via
        // lightness and from the near-white Light-theme base via
        // saturation/hue (same class as black-on-white); backed by
        // `context_alarm_has_luminance_separation_from_every_theme_tier`.
        rgb(139, 0, 0) // dark red
    } else if context_pct >= 80.0 {
        red()
    } else if context_pct >= 50.0 {
        peach()
    } else {
        green()
    }
}

/// Pane block with plain (square-corner) borders and focus-aware, border-policy-aware styling.
pub fn pane_block(focused: bool) -> ratatui::widgets::Block<'static> {
    use ratatui::style::Style;
    use ratatui::widgets::{Block, BorderType, Borders, Padding};

    let border_color = match (active_border_policy(), focused) {
        (BorderPolicy::FullBorders, true) => focused_border(),
        (BorderPolicy::FullBorders, false) => unfocused_border(),
        (BorderPolicy::AccentFocusOnly, true) => focused_border(),
        // Unfocused + opaque theme: keep Borders::ALL (preserves the existing
        // SESSION_DETAIL_HORIZ_INSET-style inset math other callers rely on —
        // do NOT switch to Borders::NONE, which would shift inner-rect
        // dimensions) but color the border to match the surrounding panel
        // tier, so it reads as "no border" without moving anything.
        (BorderPolicy::AccentFocusOnly, false) => tier_panel(),
    };

    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Plain)
        .border_style(Style::default().fg(border_color))
        .padding(Padding::horizontal(10))
}

/// Total horizontal inset (border + padding, both sides) for `session_detail_block`.
///
/// Used by both `mod.rs` (height pre-computation) and `session.rs` (render) to
/// ensure `content_width` passed to `Paragraph::line_count` matches the actual
/// rendering rect — preventing height underestimation and bottom-clipping bugs.
///
/// Formula: 1 (left border) + SESSION_DETAIL_PAD (left pad)
///        + SESSION_DETAIL_PAD (right pad) + 1 (right border)
pub const SESSION_DETAIL_HORIZ_INSET: u16 = 4; // 1 border + 1 padding per side

/// Pane block for session detail with context-aware border colors.
/// When focused and context data is available, the border color reflects usage.
pub fn session_detail_block(
    focused: bool,
    context_pct: Option<f64>,
) -> ratatui::widgets::Block<'static> {
    use ratatui::style::Style;
    use ratatui::widgets::{Block, BorderType, Borders, Padding};

    let border_color = match active_border_policy() {
        BorderPolicy::FullBorders => session_detail_border(), // White, pinned
        BorderPolicy::AccentFocusOnly => {
            if focused {
                context_pct
                    .map(context_border_color)
                    .unwrap_or_else(focused_border)
            } else {
                tier_panel()
            }
        }
    };

    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Plain)
        .border_style(Style::default().fg(border_color))
        .style(Style::default().bg(if is_transparent_theme() {
            glass_panel_bg()
        } else {
            base()
        })) // bg logic unchanged — byte-identical to today either way
        .padding(Padding::horizontal(1)) // 1 col each side; matches SESSION_DETAIL_HORIZ_INSET = 4
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;

    const EXPECTED_THEMES: [(&str, &str); 14] = [
        ("goth", "Goth"),
        ("junkyard", "Junk Yard"),
        ("transparent", "Transparent"),
        ("gruvbox-warm", "Gruvbox Warm"),
        ("high-contrast", "High Contrast"),
        ("light", "Light"),
        ("raaaainnnnnnbooozzzzzzzz", "RAAAAINNNNNNBOOOZZZZZZZZ"),
        ("truly-transparent", "Truly Transparent"),
        ("cup-a-joe", "Cup`a Joe"),
        ("emerald", "Emerald"),
        ("diamond", "Diamond"),
        ("ruby", "Ruby"),
        ("saphire", "Saphire"),
        ("d-is-for-devil", "D. is for Daemon"),
    ];

    /// Pin theme state for a whole test body.  These tests mutate the globals,
    /// so the scope both excludes concurrent readers and restores the previous
    /// values on drop instead of leaking a theme into the rest of the process.
    fn theme_test_guard() -> PinnedThemeState {
        pin_theme_state()
    }

    #[test]
    fn popup_frames_match_input_bar_in_every_theme_and_follow_overrides() {
        use ratatui::{buffer::Buffer, layout::Rect, widgets::Widget};
        let _guard = theme_test_guard();
        clear_theme_role_overrides();
        for index in 0..theme_count() {
            set_theme_by_index(index);
            for block in [
                overlay_block(),
                blank_overlay_block(),
                unfocused_overlay_block(),
            ] {
                let area = Rect::new(0, 0, 12, 5);
                let mut buffer = Buffer::empty(area);
                block.render(area, &mut buffer);
                for (x, y, symbol) in [
                    (0, 0, "┌"),
                    (11, 0, "┐"),
                    (0, 4, "└"),
                    (11, 4, "┘"),
                    (0, 2, "│"),
                    (5, 4, "─"),
                ] {
                    assert_eq!(buffer[(x, y)].symbol(), symbol, "{}", active_theme_key());
                    assert_eq!(
                        buffer[(x, y)].fg,
                        input_bar_border(true),
                        "{}",
                        active_theme_key()
                    );
                    assert_ne!(buffer[(x, y)].fg, overlay_bg());
                }
            }
            let role = if active_border_policy() == BorderPolicy::FullBorders {
                ThemeRole::Border
            } else {
                ThemeRole::FocusedBorder
            };
            set_theme_role_override(role, Some([222, 133, 77]));
            assert_eq!(overlay_border(), Color::Rgb(222, 133, 77));
            set_theme_role_override(role, None);
        }
    }

    fn wcag_relative_luminance(color: Color) -> f64 {
        let Color::Rgb(r, g, b) = color else {
            panic!("opaque contrast matrix requires RGB colors, got {color:?}");
        };
        fn linear(channel: u8) -> f64 {
            let channel = f64::from(channel) / 255.0;
            if channel <= 0.04045 {
                channel / 12.92
            } else {
                ((channel + 0.055) / 1.055).powf(2.4)
            }
        }
        0.2126_f64.mul_add(linear(r), 0.7152_f64.mul_add(linear(g), 0.0722 * linear(b)))
    }

    fn wcag_contrast(a: Color, b: Color) -> f64 {
        let a = wcag_relative_luminance(a);
        let b = wcag_relative_luminance(b);
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }

    #[test]
    fn every_theme_declares_a_coloured_text_accent_mapping() {
        let _guard = theme_test_guard();
        clear_theme_role_overrides();
        for (index, definition) in THEME_DEFINITIONS.iter().enumerate() {
            let key = definition.key;
            let accents = definition
                .text_accents
                .unwrap_or_else(|| panic!("{key}: declares no text-accent mapping"));
            set_theme_by_index(index);
            assert_eq!(active_text_accents(), accents, "{key}: active mapping");

            let resolved = [
                ("model", model_text(), accents.model),
                ("time", time_text(), accents.time),
                ("effort", effort_text(), accents.effort),
                ("count", count_text(), accents.count),
                ("cost", cost_text(), accents.cost),
                ("path", path_text(), accents.path),
                ("label", section_label_text(), accents.label),
            ];
            let grey = [
                text(),
                subtext1(),
                subtext0(),
                overlay2(),
                overlay1(),
                overlay0(),
            ];
            for (role, color, slot) in resolved {
                assert_eq!(color, slot.resolve(), "{key}: {role} follows its slot");
                assert!(
                    !grey.contains(&color),
                    "{key}: {role} resolves to a neutral text tier ({color:?})"
                );
            }

            let mut distinct: Vec<Color> = Vec::new();
            for (_, color, _) in resolved {
                if !distinct.contains(&color) {
                    distinct.push(color);
                }
            }
            assert!(
                distinct.len() >= 5,
                "{key}: text accents must vary, got {} distinct colours",
                distinct.len()
            );
        }
    }

    #[test]
    fn truly_transparent_keeps_its_original_metadata_colours() {
        let _guard = theme_test_guard();
        assert!(set_theme_by_name("truly-transparent"));
        assert_eq!(model_text(), lavender());
        assert_eq!(time_text(), sky());
        assert_eq!(effort_text(), peach());
        assert_eq!(count_text(), blue());
        assert_eq!(cost_text(), green());
        assert_eq!(path_text(), teal());
        assert_eq!(section_label_text(), yellow());
    }

    /// Every text role stays readable on the surfaces it is drawn over:
    /// body text, secondary text, the text-accent mapping and the accents used
    /// as text (roles, syntax, links) at 4.5:1; dim metadata (`overlay1`) at
    /// 3:1; the faint comment/hint tier (`overlay0`) at 2.5:1. Truly
    /// Transparent is exempt: its colours are terminal ANSI slots drawn over
    /// the terminal's own background.
    #[test]
    fn text_roles_meet_contrast_targets_in_every_theme() {
        let _guard = theme_test_guard();
        clear_theme_role_overrides();
        let mut failures = Vec::new();
        for (index, definition) in THEME_DEFINITIONS.iter().enumerate() {
            if definition.transparency_policy == TransparencyPolicy::TerminalDefault {
                continue;
            }
            set_theme_by_index(index);
            let key = definition.key;
            let mut roles = vec![
                ("text", text(), 4.5),
                ("subtext1", subtext1(), 4.5),
                ("subtext0", subtext0(), 4.5),
                ("dim metadata", dim_metadata(), 3.0),
                ("overlay1", overlay1(), 3.0),
                ("overlay0", overlay0(), 2.5),
                ("maroon", maroon(), 3.0),
            ];
            for (name, color) in [
                ("rosewater", rosewater()),
                ("flamingo", flamingo()),
                ("pink", pink()),
                ("mauve", mauve()),
                ("red", red()),
                ("peach", peach()),
                ("yellow", yellow()),
                ("green", green()),
                ("teal", teal()),
                ("sky", sky()),
                ("sapphire", sapphire()),
                ("blue", blue()),
                ("lavender", lavender()),
                ("group kind", group_powder_blue()),
                ("epic kind", epic_purple()),
                ("story kind", story_yellow_orange()),
                ("task kind", task_gray()),
                ("session kind", raw_session_magenta()),
            ] {
                roles.push((name, color, 4.5));
            }
            for (name, slot) in active_text_accents().slots() {
                roles.push((name, slot.resolve(), 4.5));
            }
            for (surface, background) in [("tier_base", tier_base()), ("tier_panel", tier_panel())]
            {
                for &(role, foreground, minimum) in &roles {
                    let ratio = wcag_contrast(foreground, background);
                    if ratio < minimum {
                        failures.push(format!(
                            "{key}: {role} on {surface} is {ratio:.2}:1 (< {minimum})"
                        ));
                    }
                }
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    /// Accent badges (mode, active tab, docregblock, alarms) draw `crust` on
    /// the accent; the overlay normal-mode badge draws on `surface2`.
    #[test]
    fn badge_foregrounds_stay_readable_on_their_backgrounds() {
        let _guard = theme_test_guard();
        clear_theme_role_overrides();
        for (index, definition) in THEME_DEFINITIONS.iter().enumerate() {
            if definition.transparency_policy == TransparencyPolicy::TerminalDefault {
                continue;
            }
            set_theme_by_index(index);
            let key = definition.key;
            let ratio = wcag_contrast(overlay_mode_normal_fg(), overlay_mode_normal_bg());
            assert!(ratio >= 4.5, "{key}: overlay normal badge {ratio:.2}:1");
            // Light's deep-ink accents trade badge contrast for text
            // contrast; its bold badges keep the 3:1 large-text bar.
            let minimum = if key == "light" { 3.0 } else { 4.5 };
            for (pair, background) in [
                ("primary", active_primary_color()),
                ("green", green()),
                ("yellow", yellow()),
                ("blue", blue()),
            ] {
                let ratio = wcag_contrast(crust(), background);
                assert!(ratio >= minimum, "{key}: crust/{pair} {ratio:.2}:1");
            }
        }
    }

    #[test]
    fn palette_colors_change_with_theme() {
        let _guard = theme_test_guard();

        set_theme_by_name("goth");
        let goth_base = base();

        set_theme_by_name("junkyard");
        let junkyard_base = base();

        assert_ne!(
            junkyard_base, goth_base,
            "junkyard should be distinct from goth"
        );

        set_theme_by_index(DEFAULT_THEME_INDEX);
    }

    #[test]
    fn modal_borders_follow_focused_input_bar_color() {
        let _guard = theme_test_guard();

        // Modal frames share the focused input bar's color.
        assert!(set_theme_by_name("goth"));
        assert_eq!(focused_border(), Color::Rgb(181, 138, 215));
        assert_eq!(overlay_border(), input_bar_border(true));

        assert!(set_theme_by_name("junkyard"));
        assert_eq!(focused_border(), Color::Rgb(30, 185, 128));
        assert_eq!(overlay_border(), input_bar_border(true));

        // Every theme keeps modal borders aligned with the focused input bar.
        for i in 0..theme_count() {
            set_theme_by_index(i);
            let key = theme_key(i);
            assert_eq!(
                focused_border(),
                THEME_DEFINITIONS[i].primary,
                "{key}: focused_border should equal its declared primary"
            );
            assert_eq!(
                overlay_border(),
                input_bar_border(true),
                "{key}: modal border should match focused input bar"
            );
        }

        set_theme_by_index(DEFAULT_THEME_INDEX);
    }

    #[test]
    fn transparent_semantic_surfaces_are_readable_tiers() {
        let _guard = theme_test_guard();

        assert!(set_theme_by_name("transparent"));
        assert!(is_transparent_theme());
        assert_ne!(glass_panel_bg(), Color::Reset);
        assert_ne!(selected_row_bg(), Color::Reset);
        assert_ne!(operations_deck_cell_bg(), Color::Reset);
        assert_ne!(glass_panel_bg(), selected_row_bg());
        assert_ne!(operations_deck_cell_bg(), operations_deck_cell_border());
        assert_eq!(active_row_rail(), focused_border());
        assert_eq!(error_status(), red());

        set_theme_by_index(DEFAULT_THEME_INDEX);
    }

    /// T1 regression pin (written FIRST, before the surface-tier refactor):
    /// pins the 8 directly-callable `is_transparent_theme()` branch-leg
    /// outputs to their pre-tier-migration values. Transparent must remain
    /// byte-identical across the whole T1 change — this test must stay
    /// green throughout.
    #[test]
    fn transparent_theme_baseline_is_byte_identical_to_pre_tier_migration() {
        let _guard = theme_test_guard();

        assert!(set_theme_by_name("transparent"));
        assert_eq!(glass_panel_bg(), Color::Rgb(8, 10, 15));
        assert_eq!(selected_row_bg(), Color::Rgb(34, 34, 55));
        assert_eq!(table_header_text(), subtext1());
        assert_eq!(dim_metadata(), subtext0());
        assert_eq!(section_label_text(), yellow());
        assert_eq!(operations_deck_cell_bg(), surface1());
        assert_eq!(operations_deck_cell_border(), surface2());
        assert_eq!(input_bar_mode_insert_bg(), mauve());

        set_theme_by_index(DEFAULT_THEME_INDEX);
    }

    #[test]
    fn original_theme_policies_remain_pinned() {
        let _guard = theme_test_guard();
        let expected = [
            (TransparencyPolicy::Opaque, BorderPolicy::AccentFocusOnly),
            (TransparencyPolicy::Opaque, BorderPolicy::AccentFocusOnly),
            (TransparencyPolicy::Scrimmed, BorderPolicy::FullBorders),
            (TransparencyPolicy::Opaque, BorderPolicy::AccentFocusOnly),
            (TransparencyPolicy::Opaque, BorderPolicy::AccentFocusOnly),
            (TransparencyPolicy::Opaque, BorderPolicy::AccentFocusOnly),
        ];

        for (definition, (transparency, border)) in THEME_DEFINITIONS.iter().take(6).zip(expected) {
            assert_eq!(definition.transparency_policy, transparency);
            assert_eq!(definition.border_policy, border);
        }
    }

    #[test]
    fn truly_transparent_resets_only_passive_structural_backgrounds() {
        let _guard = theme_test_guard();
        assert!(set_theme_by_name("Truly Transparent"));
        assert!(is_transparent_theme());
        assert!(!is_scrimmed_theme());
        assert!(uses_terminal_default_backgrounds());
        assert_eq!(active_border_policy(), BorderPolicy::FullBorders);

        for (role, color) in [
            ("root", root_bg()),
            ("glass panel", glass_panel_bg()),
            ("card", card_bg()),
            ("header", header_bg()),
            ("status line", status_line_bg()),
            ("inactive tab", inactive_tab_bg()),
            ("tab bar", tab_bar_bg()),
            ("inline code", md_inline_code_bg()),
            ("code block", code_block_bg()),
            ("assistant bubble", assistant_bubble_bg()),
            ("user bubble", user_bubble_bg()),
            ("tool bubble", tool_bubble_bg()),
            ("system bubble", system_bubble_bg()),
            ("bottom strip", bottom_strip_bg()),
            ("overlay", overlay_bg()),
            ("input bar", input_bar_bg()),
            ("suggestion", suggestion_bg()),
            ("operations deck", operations_deck_cell_bg()),
        ] {
            assert_eq!(color, Color::Reset, "{role} should yield to terminal bg");
        }

        for (role, color) in [
            ("raw base", base()),
            ("raw panel", tier_panel()),
            ("selected row", selected_row_bg()),
            ("selected card", card_selected_bg()),
            ("cursor line", file_viewer_cursor_line_bg()),
            ("visual selection", visual_selection_bg()),
            ("search match", search_match_bg()),
            ("active tab", active_tab_bg()),
        ] {
            assert_ne!(
                color,
                Color::Reset,
                "{role} should retain an active fallback"
            );
        }
        assert_eq!(neutral_border(), unfocused_border());

        set_theme_by_index(DEFAULT_THEME_INDEX);
    }

    #[test]
    fn new_opaque_palettes_meet_critical_wcag_contrast() {
        let _guard = theme_test_guard();

        for key in [
            "raaaainnnnnnbooozzzzzzzz",
            "cup-a-joe",
            "emerald",
            "diamond",
            "ruby",
            "saphire",
            "d-is-for-devil",
        ] {
            assert!(set_theme_by_name(key));
            for (pair, foreground, background) in [
                ("text/panel", text(), tier_panel()),
                ("text/selected", text(), tier_selected()),
                ("subtext/panel", subtext1(), tier_panel()),
                ("crust/primary", crust(), active_primary_color()),
                ("crust/green", crust(), green()),
                ("crust/yellow", crust(), yellow()),
                ("crust/blue", crust(), blue()),
            ] {
                let ratio = wcag_contrast(foreground, background);
                assert!(ratio >= 4.5, "{key}: {pair} contrast {ratio:.2}:1");
            }
        }

        set_theme_by_index(DEFAULT_THEME_INDEX);
    }

    #[test]
    fn non_transparent_semantic_surfaces_preserve_existing_chrome() {
        let _guard = theme_test_guard();

        // Generalized (F-021, PI-14 item 3): loop all opaque themes for the
        // invariants that hold universally by construction
        // (glass panels follow tier_panel and cards follow surface0;
        // table_header_text/operations_deck_cell_bg are unconditionally
        // aliased to header_fg/bottom_strip_bg on the non-transparent branch).
        for key in [
            "goth",
            "junkyard",
            "gruvbox-warm",
            "high-contrast",
            "light",
            "raaaainnnnnnbooozzzzzzzz",
            "cup-a-joe",
            "emerald",
            "diamond",
            "ruby",
            "saphire",
            "d-is-for-devil",
        ] {
            assert!(set_theme_by_name(key), "{key}: should be a valid theme key");
            assert!(!is_transparent_theme(), "{key}: must not be transparent");
            assert_eq!(glass_panel_bg(), tier_panel(), "{key}: glass panel tier");
            assert_eq!(card_bg(), surface0(), "{key}: card surface");
            if key == "d-is-for-devil" {
                // Its authored palette keeps the glass panel one RGB step
                // darker than the card, preserving the intended depth.
                assert_eq!(glass_panel_bg(), rgb(16, 15, 15));
                assert_eq!(card_bg(), rgb(17, 16, 16));
            } else {
                assert_eq!(glass_panel_bg(), card_bg(), "{key}: shared chrome");
            }
            assert_eq!(
                table_header_text(),
                header_fg(),
                "{key}: table_header_text should equal header_fg"
            );
            assert_eq!(
                operations_deck_cell_bg(),
                bottom_strip_bg(),
                "{key}: operations_deck_cell_bg should equal bottom_strip_bg"
            );
        }

        // selected_row_bg()==card_selected_bg() is a "preserve EXISTING
        // chrome" pin, not a universal tier invariant: it only holds for
        // goth/junkyard, whose value-preserving migration deliberately kept
        // tier_selected == tier_raised == surface1 (see the plan's migration
        // table). The 3 new themes deliberately give tier_selected its OWN
        // value, distinct from tier_raised/surface1 — that is the point of
        // a real 4th "selected" tier (see
        // `new_themes_have_four_way_distinct_tiers`), not a regression here.
        for key in ["goth", "junkyard"] {
            assert!(set_theme_by_name(key));
            assert_eq!(
                selected_row_bg(),
                card_selected_bg(),
                "{key}: selected_row_bg should equal card_selected_bg (legacy-preserved overlap)"
            );
        }

        set_theme_by_index(DEFAULT_THEME_INDEX);
    }

    /// PI-14 item 4 (F-020): extend the readability/tier pattern to every
    /// theme, not just transparent.
    #[test]
    fn all_themes_have_valid_tiers_and_readable_chrome() {
        let _guard = theme_test_guard();

        for i in 0..theme_count() {
            set_theme_by_index(i);
            let key = theme_key(i);
            assert_ne!(tier_base(), Color::Reset, "{key}: tier_base");
            assert_ne!(tier_panel(), Color::Reset, "{key}: tier_panel");
            assert_ne!(tier_raised(), Color::Reset, "{key}: tier_raised");
            assert_ne!(tier_selected(), Color::Reset, "{key}: tier_selected");
            assert_ne!(
                tier_panel(),
                tier_selected(),
                "{key}: tier_panel and tier_selected must be distinct"
            );
            assert_ne!(
                viewed_session_row_bg(),
                tier_panel(),
                "{key}: viewed session background must remain visible above the panel"
            );
            assert_ne!(
                viewed_session_row_bg(),
                tier_selected(),
                "{key}: viewed session background must remain dimmer than the cursor"
            );
            assert_ne!(
                operations_deck_cell_bg(),
                operations_deck_cell_border(),
                "{key}: operations_deck_cell_bg/border must be distinct"
            );
            assert_eq!(
                active_row_rail(),
                focused_border(),
                "{key}: active_row_rail should equal focused_border"
            );
            assert_eq!(
                error_status(),
                red(),
                "{key}: error_status should equal red"
            );

            // T1.1: with default settings (backfill toggle OFF), the
            // text-area bg source must resolve to the caller's theme
            // fallback on every opaque theme (full background under a
            // translucent terminal) and stay Reset on Transparent (the
            // locked baseline — the Reset leg is transparent-only).
            let default_settings = crate::settings::UserSettings::default();
            let backfill_off_bg =
                crate::settings::text_area_bg_color(&default_settings, tier_panel());
            if is_transparent_theme() {
                assert_eq!(
                    backfill_off_bg,
                    Color::Reset,
                    "{key}: transparent keeps Reset when backfill is OFF"
                );
            } else {
                assert_eq!(
                    backfill_off_bg,
                    tier_panel(),
                    "{key}: opaque theme must use the fallback when backfill is OFF"
                );
            }
        }

        set_theme_by_index(DEFAULT_THEME_INDEX);
    }

    #[test]
    fn browser_operational_text_roles_are_readable_in_every_theme() {
        let _guard = theme_test_guard();

        for i in 0..theme_count() {
            set_theme_by_index(i);
            let key = theme_key(i);
            for (role, color) in [
                ("selected title", text()),
                ("selectable title", subtext1()),
                ("selected rail", active_row_rail()),
                ("selected surface", selected_row_bg()),
            ] {
                assert_ne!(color, Color::Reset, "{key}: {role} must not be Reset");
            }
            assert_ne!(
                text(),
                selected_row_bg(),
                "{key}: selected title must remain distinct from its row surface"
            );
            if uses_terminal_default_backgrounds() {
                assert_eq!(glass_panel_bg(), Color::Reset);
            } else {
                assert_ne!(glass_panel_bg(), Color::Reset, "{key}: browser scrim");
                assert_ne!(
                    subtext1(),
                    glass_panel_bg(),
                    "{key}: selectable title must remain distinct from the browser scrim"
                );
            }
        }

        assert!(set_theme_by_name("transparent"));
        assert_eq!(glass_panel_bg(), Color::Rgb(8, 10, 15));
        assert_ne!(glass_panel_bg(), Color::Reset);
        set_theme_by_index(DEFAULT_THEME_INDEX);
    }

    #[test]
    fn browser_secondary_chrome_roles_are_neutral_and_muted_in_every_theme() {
        let _guard = theme_test_guard();

        for i in 0..theme_count() {
            set_theme_by_index(i);
            let key = theme_key(i);
            assert_eq!(
                browser_section_label(),
                subtext1(),
                "{key}: browser section labels must use neutral secondary text"
            );
            assert_eq!(
                browser_decorative_separator(),
                overlay0(),
                "{key}: browser rules must use the faint decorative tier"
            );
            assert_ne!(
                browser_decorative_separator(),
                Color::Reset,
                "{key}: browser rules must remain theme-defined"
            );
            assert_ne!(
                browser_decorative_separator(),
                active_row_rail(),
                "{key}: passive browser chrome must not look selected"
            );
            if is_transparent_theme() {
                assert_ne!(
                    browser_decorative_separator(),
                    Color::White,
                    "transparent: passive browser chrome must not inherit the bright border role"
                );
            }
        }

        set_theme_by_index(DEFAULT_THEME_INDEX);
    }

    /// PI-14 item 5: a stronger bar than the loop above, intentionally
    /// scoped to only the 3 new themes — goth/junkyard/transparent are
    /// exempted to preserve their existing raised==selected overlap (see
    /// the migration table).
    #[test]
    fn new_themes_have_four_way_distinct_tiers() {
        let _guard = theme_test_guard();

        for key in [
            "gruvbox-warm",
            "high-contrast",
            "light",
            "raaaainnnnnnbooozzzzzzzz",
            "truly-transparent",
            "cup-a-joe",
            "emerald",
            "diamond",
            "ruby",
            "saphire",
        ] {
            assert!(set_theme_by_name(key));
            let tiers = [tier_base(), tier_panel(), tier_raised(), tier_selected()];
            for i in 0..tiers.len() {
                for j in (i + 1)..tiers.len() {
                    assert_ne!(
                        tiers[i], tiers[j],
                        "{key}: tier[{i}] and tier[{j}] must be pairwise distinct"
                    );
                }
            }
        }

        set_theme_by_index(DEFAULT_THEME_INDEX);
    }

    /// Shape invariants for the append-only 6->14 growth.
    #[test]
    fn theme_definitions_shape_is_fourteen_unique_round_tripping_themes() {
        let _guard = theme_test_guard();

        assert_eq!(THEME_COUNT, 14);
        assert_eq!(theme_count(), 14);

        let mut seen_keys: Vec<&str> = Vec::new();
        for (i, (expected_key, expected_display)) in EXPECTED_THEMES.iter().enumerate() {
            let key = theme_key(i);
            assert_eq!(key, *expected_key);
            assert_eq!(theme_display_name(i), *expected_display);
            assert!(!seen_keys.contains(&key), "duplicate theme key: {key}");
            seen_keys.push(key);

            assert!(
                set_theme_by_name(key),
                "set_theme_by_name({key}) should succeed"
            );
            assert_eq!(
                active_theme_key(),
                key,
                "active_theme_key should round-trip for {key}"
            );

            assert!(
                set_theme_by_name(expected_display),
                "display-name lookup should succeed for {expected_display}"
            );
            assert_eq!(active_theme_key(), key);
        }

        set_theme_by_index(DEFAULT_THEME_INDEX);
    }

    /// PI-14 item 7 (D3, F-010): safety-net check that the fixed
    /// `context_border_color(95.0)` alarm literal stays visually separated
    /// from every theme's `tier_base`/`tier_panel`. Simple numeric
    /// threshold (Manhattan distance in RGB space), not a full WCAG
    /// calculation — captures both lightness AND hue separation, which
    /// matters here (e.g. dark red vs. a dark blue-gray base can have
    /// similar luminance but very different per-channel values).
    #[test]
    fn context_alarm_has_luminance_separation_from_every_theme_tier() {
        let _guard = theme_test_guard();

        fn channel_distance(a: Color, b: Color) -> i32 {
            match (a, b) {
                (Color::Rgb(ar, ag, ab), Color::Rgb(br, bg, bb)) => {
                    (ar as i32 - br as i32).abs()
                        + (ag as i32 - bg as i32).abs()
                        + (ab as i32 - bb as i32).abs()
                }
                _ => i32::MAX, // non-RGB colors are trivially distinguishable
            }
        }

        // Comfortably below every theme's actual computed separation (the
        // tightest real value observed is ~139, on High Contrast's
        // near-black tier_base) while still catching a genuine collision.
        const MIN_SEPARATION: i32 = 60;

        for i in 0..theme_count() {
            set_theme_by_index(i);
            let key = theme_key(i);
            let alarm = context_border_color(95.0);
            assert!(
                channel_distance(alarm, tier_base()) >= MIN_SEPARATION,
                "{key}: context alarm too close to tier_base"
            );
            assert!(
                channel_distance(alarm, tier_panel()) >= MIN_SEPARATION,
                "{key}: context alarm too close to tier_panel"
            );
        }

        set_theme_by_index(DEFAULT_THEME_INDEX);
    }

    // --- lerp_color tests ---

    #[test]
    fn lerp_color_at_zero_returns_a() {
        let a = Color::Rgb(255, 0, 0);
        let b = Color::Rgb(0, 0, 255);
        assert_eq!(lerp_color(a, b, 0.0), a);
    }

    #[test]
    fn lerp_color_at_one_returns_b() {
        let a = Color::Rgb(255, 0, 0);
        let b = Color::Rgb(0, 0, 255);
        assert_eq!(lerp_color(a, b, 1.0), b);
    }

    #[test]
    fn lerp_color_midpoint() {
        let a = Color::Rgb(0, 0, 0);
        let b = Color::Rgb(100, 100, 100);
        let mid = lerp_color(a, b, 0.5);
        assert_eq!(mid, Color::Rgb(50, 50, 50));
    }

    #[test]
    fn lerp_color_clamps_below_zero() {
        let a = Color::Rgb(255, 0, 0);
        let b = Color::Rgb(0, 0, 255);
        assert_eq!(lerp_color(a, b, -1.0), a);
    }

    #[test]
    fn lerp_color_clamps_above_one() {
        let a = Color::Rgb(255, 0, 0);
        let b = Color::Rgb(0, 0, 255);
        assert_eq!(lerp_color(a, b, 2.0), b);
    }

    #[test]
    fn lerp_color_named_colors_blend() {
        // Red to Blue at 0.5 should give Rgb(127 or 128, 0, 127 or 128).
        let result = lerp_color(Color::Red, Color::Blue, 0.5);
        if let Color::Rgb(r, _g, b) = result {
            assert!(r > 100 && r < 160, "red channel at midpoint: {r}");
            assert!(b > 100 && b < 160, "blue channel at midpoint: {b}");
        } else {
            panic!("expected Rgb color, got {result:?}");
        }
    }

    #[test]
    fn lerp_color_unresolvable_falls_back_below_half() {
        // Color::Indexed is unresolvable — should fall back to a at t < 0.5.
        let a = Color::Indexed(1);
        let b = Color::Indexed(2);
        assert_eq!(lerp_color(a, b, 0.3), a);
    }

    #[test]
    fn lerp_color_unresolvable_falls_back_above_half() {
        // Color::Indexed is unresolvable — should fall back to b at t >= 0.5.
        let a = Color::Indexed(1);
        let b = Color::Indexed(2);
        assert_eq!(lerp_color(a, b, 0.7), b);
    }

    // Pins ST-STATUSCOLOR: `status_color` is the single source of truth, so a
    // status renders one way everywhere. Previously the bottom strip collapsed
    // WaitingApproval->yellow, Completed->green, Interrupted->peach, Archived/
    // Deleted->dim, diverging from the session list. Guard the theme so a
    // concurrent theme-mutating test can't flip the palette mid-assertion.
    #[test]
    fn status_color_maps_every_status_to_its_dedicated_token() {
        let _guard = theme_test_guard();
        assert_eq!(status_color(SessionStatus::Starting), status_starting());
        assert_eq!(status_color(SessionStatus::Running), status_running());
        assert_eq!(
            status_color(SessionStatus::WaitingApproval),
            status_waiting()
        );
        assert_eq!(status_color(SessionStatus::Completed), status_completed());
        assert_eq!(status_color(SessionStatus::Failed), status_failed());
        assert_eq!(
            status_color(SessionStatus::Interrupted),
            status_interrupted()
        );
        assert_eq!(status_color(SessionStatus::Archived), status_archived());
        assert_eq!(status_color(SessionStatus::Deleted), subtext0());
    }

    #[test]
    fn semantic_roles_resolve_for_all_built_in_themes() {
        with_theme_state(|| {
            clear_theme_role_overrides();
            for theme_index in 0..THEME_COUNT {
                set_theme_by_index(theme_index);
                for role in ThemeRole::ALL {
                    assert!(
                        !matches!(semantic_color(role), Color::Reset),
                        "{} did not resolve in {}",
                        role.key(),
                        theme_key(theme_index)
                    );
                }
            }
        });
    }

    #[test]
    fn semantic_override_changes_only_one_role_and_clears_cleanly() {
        with_theme_state(|| {
            clear_theme_role_overrides();
            let baseline_text = semantic_color(ThemeRole::PrimaryText);
            let baseline_error = semantic_color(ThemeRole::Error);
            set_theme_role_override(ThemeRole::PrimaryText, Some([1, 2, 3]));
            assert_eq!(semantic_color(ThemeRole::PrimaryText), Color::Rgb(1, 2, 3));
            assert_eq!(semantic_color(ThemeRole::Error), baseline_error);
            set_theme_role_override(ThemeRole::PrimaryText, None);
            assert_eq!(semantic_color(ThemeRole::PrimaryText), baseline_text);
        });
    }

    #[test]
    fn app_construction_cannot_clear_a_scoped_override_mid_render() {
        use std::sync::mpsc;
        use std::time::Duration;

        let (started_tx, started_rx) = mpsc::channel();
        let (finished_tx, finished_rx) = mpsc::channel();
        let handle = with_theme_state(|| {
            set_theme_role_override(ThemeRole::Pin, Some([9, 8, 7]));
            let handle = std::thread::spawn(move || {
                started_tx.send(()).expect("test receiver");
                let _app = crate::app::App::new(crate::client::DaemonClient::new(
                    std::path::PathBuf::from("/tmp/theme-scope-test.sock"),
                ));
                finished_tx.send(()).expect("test receiver");
            });
            started_rx.recv().expect("construction started");
            assert!(
                finished_rx.recv_timeout(Duration::from_millis(25)).is_err(),
                "App construction must wait for the active theme test scope"
            );
            assert_eq!(semantic_color(ThemeRole::Pin), Color::Rgb(9, 8, 7));
            handle
        });
        finished_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("App construction resumes after scope");
        handle.join().expect("App construction thread");
    }

    #[test]
    fn panel_override_reaches_tier_panel_consumer() {
        with_theme_state(|| {
            set_theme_role_override(ThemeRole::Panel, Some([12, 34, 56]));
            assert_eq!(tier_panel(), Color::Rgb(12, 34, 56));
        });
    }

    #[test]
    fn every_theme_declares_its_own_loader_spectrum() {
        for definition in &THEME_DEFINITIONS {
            assert_ne!(
                definition.loader,
                DEFAULT_LOADER.as_slice(),
                "{} uses the fallback loader spectrum",
                definition.key
            );
            assert!(
                definition.loader.len() >= 6,
                "{} spectrum needs enough stops for a rainbow",
                definition.key
            );
        }
        for (index, definition) in THEME_DEFINITIONS.iter().enumerate() {
            for other in &THEME_DEFINITIONS[index + 1..] {
                assert_ne!(
                    definition.loader, other.loader,
                    "{} and {} share a loader spectrum",
                    definition.key, other.key
                );
            }
        }
    }

    #[test]
    fn loader_spectra_flow_around_their_loop_without_jumps() {
        const SAMPLES: usize = 256;
        for definition in &THEME_DEFINITIONS {
            let spectrum = LoaderSpectrum::new(definition.loader);
            let colors: Vec<Color> = (0..SAMPLES)
                .map(|step| spectrum.sample(step as f32 / SAMPLES as f32))
                .collect();
            for step in 0..SAMPLES {
                let next = colors[(step + 1) % SAMPLES];
                let distance = perceptual_distance(colors[step], next).expect("rgb samples");
                assert!(
                    distance < 0.03,
                    "{} jumps {distance:.3} at sample {step}",
                    definition.key
                );
            }
        }
    }

    #[test]
    fn loader_spectra_stand_out_from_their_theme_canvas() {
        for definition in &THEME_DEFINITIONS {
            let canvas = perceptual_lightness(definition.palette.base).expect("rgb canvas");
            let spectrum = LoaderSpectrum::new(definition.loader);
            for step in 0..128 {
                let color = spectrum.sample(step as f32 / 128.0);
                let lightness = perceptual_lightness(color).expect("rgb sample");
                assert!(
                    (lightness - canvas).abs() >= 0.25,
                    "{} sample {step} ({color:?}) blends into its canvas",
                    definition.key
                );
            }
        }
    }

    #[test]
    fn loader_spectrum_wraps_whole_turns_and_lands_on_its_stops() {
        let spectrum = LoaderSpectrum::new(&EMERALD_LOADER);
        let start = spectrum.sample(0.0);
        for position in [1.0, -1.0, 3.0] {
            assert_eq!(spectrum.sample(position), start);
        }
        assert_eq!(spectrum.sample(0.25), spectrum.sample(-0.75));
        for (index, stop) in EMERALD_LOADER.iter().enumerate() {
            let at_stop = spectrum.sample(index as f32 / EMERALD_LOADER.len() as f32);
            let distance = perceptual_distance(at_stop, *stop).expect("rgb stop");
            assert!(distance < 0.005, "stop {index} drifted {distance}");
        }
    }

    #[test]
    fn loader_spectrum_skips_terminal_owned_stops() {
        assert_eq!(
            LoaderSpectrum::new(&[Color::Reset]),
            LoaderSpectrum::new(&DEFAULT_LOADER)
        );
        let single = LoaderSpectrum::new(&[Color::Reset, rgb(40, 200, 120)]);
        for step in 0..8 {
            assert_eq!(single.sample(step as f32 / 8.0), rgb(40, 200, 120));
        }
    }

    #[test]
    fn rainbow_palette_samples_the_active_theme_spectrum() {
        with_theme_state(|| {
            assert!(set_theme_by_name("emerald"));
            let spectrum = loader_spectrum();
            let emerald = rainbow_palette();
            assert_eq!(emerald.len(), RAINBOW_PALETTE_LEN);
            for (index, color) in emerald.iter().enumerate() {
                assert_eq!(
                    *color,
                    spectrum.sample(index as f32 / RAINBOW_PALETTE_LEN as f32)
                );
            }

            assert!(set_theme_by_name("ruby"));
            let ruby = rainbow_palette();
            assert_ne!(emerald, ruby, "each theme gets its own rainbow");
        });
    }
}

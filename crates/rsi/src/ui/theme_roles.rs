//! Enumerable semantic color roles and terminal-aware contrast assessment.

use ratatui::style::Color;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum ThemeRole {
    Canvas,
    Panel,
    ElevatedSurface,
    Border,
    FocusedBorder,
    SelectedRow,
    PrimaryText,
    MutedText,
    Accent,
    Pin,
    Running,
    Waiting,
    Success,
    Warning,
    Error,
    Disabled,
    Toast,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeRoleKind {
    Surface,
    Text,
    Indicator,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ThemeRoleDescriptor {
    pub role: ThemeRole,
    pub key: &'static str,
    pub label: &'static str,
    pub kind: ThemeRoleKind,
    pub contrast_against: Option<ThemeRole>,
    pub minimum_contrast: f32,
}

impl ThemeRole {
    pub const ALL: [Self; 17] = [
        Self::Canvas,
        Self::Panel,
        Self::ElevatedSurface,
        Self::Border,
        Self::FocusedBorder,
        Self::SelectedRow,
        Self::PrimaryText,
        Self::MutedText,
        Self::Accent,
        Self::Pin,
        Self::Running,
        Self::Waiting,
        Self::Success,
        Self::Warning,
        Self::Error,
        Self::Disabled,
        Self::Toast,
    ];

    pub const fn key(self) -> &'static str {
        self.descriptor().key
    }

    pub const fn label(self) -> &'static str {
        self.descriptor().label
    }

    pub const fn descriptor(self) -> ThemeRoleDescriptor {
        use ThemeRoleKind::{Indicator, Surface, Text};
        match self {
            Self::Canvas => descriptor(self, "canvas", "Canvas", Surface, None, 3.0),
            Self::Panel => descriptor(self, "panel", "Panel", Surface, Some(Self::Canvas), 3.0),
            Self::ElevatedSurface => descriptor(
                self,
                "elevated_surface",
                "Elevated surface",
                Surface,
                Some(Self::Canvas),
                3.0,
            ),
            Self::Border => {
                descriptor(self, "border", "Border", Indicator, Some(Self::Canvas), 3.0)
            }
            Self::FocusedBorder => descriptor(
                self,
                "focused_border",
                "Focused border",
                Indicator,
                Some(Self::Canvas),
                3.0,
            ),
            Self::SelectedRow => descriptor(
                self,
                "selected_row",
                "Selected row",
                Surface,
                Some(Self::Canvas),
                3.0,
            ),
            Self::PrimaryText => descriptor(
                self,
                "primary_text",
                "Primary text",
                Text,
                Some(Self::Canvas),
                4.5,
            ),
            Self::MutedText => descriptor(
                self,
                "muted_text",
                "Muted text",
                Text,
                Some(Self::Canvas),
                4.5,
            ),
            Self::Accent => {
                descriptor(self, "accent", "Accent", Indicator, Some(Self::Canvas), 3.0)
            }
            Self::Pin => descriptor(self, "pin", "Pin", Indicator, Some(Self::Canvas), 3.0),
            Self::Running => descriptor(
                self,
                "running",
                "Running",
                Indicator,
                Some(Self::Canvas),
                3.0,
            ),
            Self::Waiting => descriptor(
                self,
                "waiting",
                "Waiting",
                Indicator,
                Some(Self::Canvas),
                3.0,
            ),
            Self::Success => descriptor(
                self,
                "success",
                "Success",
                Indicator,
                Some(Self::Canvas),
                3.0,
            ),
            Self::Warning => descriptor(
                self,
                "warning",
                "Warning",
                Indicator,
                Some(Self::Canvas),
                3.0,
            ),
            Self::Error => descriptor(self, "error", "Error", Indicator, Some(Self::Canvas), 3.0),
            Self::Disabled => {
                descriptor(self, "disabled", "Disabled", Text, Some(Self::Canvas), 4.5)
            }
            Self::Toast => descriptor(
                self,
                "toast",
                "Toast text",
                Text,
                Some(Self::ElevatedSurface),
                4.5,
            ),
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|role| role.key() == key)
    }
}

const fn descriptor(
    role: ThemeRole,
    key: &'static str,
    label: &'static str,
    kind: ThemeRoleKind,
    contrast_against: Option<ThemeRole>,
    minimum_contrast: f32,
) -> ThemeRoleDescriptor {
    ThemeRoleDescriptor {
        role,
        key,
        label,
        kind,
        contrast_against,
        minimum_contrast,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalColorCapability {
    TrueColor,
    Ansi256,
    Ansi16,
    Unknown,
}

impl TerminalColorCapability {
    pub fn classify(colorterm: Option<&str>, term: Option<&str>) -> Self {
        let colorterm = colorterm.unwrap_or_default().to_ascii_lowercase();
        let term = term.unwrap_or_default().to_ascii_lowercase();
        if matches!(colorterm.as_str(), "truecolor" | "24bit")
            || term.contains("direct")
            || term.contains("truecolor")
            || term.contains("24bit")
        {
            Self::TrueColor
        } else if term.ends_with("-256color") || term.contains("256color") {
            Self::Ansi256
        } else if !term.is_empty() {
            Self::Ansi16
        } else {
            Self::Unknown
        }
    }

    pub fn from_launch_environment() -> Self {
        Self::classify(
            std::env::var("COLORTERM").ok().as_deref(),
            std::env::var("TERM").ok().as_deref(),
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContrastWarning {
    DefaultBackground,
    TerminalCapability,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ContrastAssessment {
    Invalid {
        reason: &'static str,
    },
    Readable {
        ratio: f32,
        quantized: bool,
    },
    LowContrast {
        ratio: f32,
        minimum: f32,
        quantized: bool,
    },
    Unverifiable {
        warning: ContrastWarning,
    },
}

impl ContrastAssessment {
    pub fn requires_acknowledgement(self) -> bool {
        matches!(self, Self::LowContrast { .. } | Self::Unverifiable { .. })
    }
}

pub fn parse_rgb(input: &str) -> Result<[u8; 3], &'static str> {
    let hex = input.trim().strip_prefix('#').unwrap_or(input.trim());
    if hex.len() != 6 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("expected #RRGGBB");
    }
    Ok([
        u8::from_str_radix(&hex[0..2], 16).map_err(|_| "invalid red channel")?,
        u8::from_str_radix(&hex[2..4], 16).map_err(|_| "invalid green channel")?,
        u8::from_str_radix(&hex[4..6], 16).map_err(|_| "invalid blue channel")?,
    ])
}

pub fn assess_contrast(
    foreground: Color,
    background: Color,
    minimum: f32,
    capability: TerminalColorCapability,
) -> ContrastAssessment {
    if matches!(foreground, Color::Reset) || matches!(background, Color::Reset) {
        return ContrastAssessment::Unverifiable {
            warning: ContrastWarning::DefaultBackground,
        };
    }
    if matches!(
        capability,
        TerminalColorCapability::Ansi16 | TerminalColorCapability::Unknown
    ) {
        return ContrastAssessment::Unverifiable {
            warning: ContrastWarning::TerminalCapability,
        };
    }
    let (Some(mut fg), Some(mut bg)) = (color_rgb(foreground), color_rgb(background)) else {
        return ContrastAssessment::Unverifiable {
            warning: ContrastWarning::TerminalCapability,
        };
    };
    let quantized = capability == TerminalColorCapability::Ansi256;
    if quantized {
        fg = xterm256_rgb(nearest_xterm256(fg));
        bg = xterm256_rgb(nearest_xterm256(bg));
    }
    let ratio = contrast_ratio(fg, bg);
    if ratio >= minimum {
        ContrastAssessment::Readable { ratio, quantized }
    } else {
        ContrastAssessment::LowContrast {
            ratio,
            minimum,
            quantized,
        }
    }
}

fn color_rgb(color: Color) -> Option<[u8; 3]> {
    match color {
        Color::Rgb(r, g, b) => Some([r, g, b]),
        Color::Indexed(index) => Some(xterm256_rgb(index)),
        _ => None,
    }
}

pub fn srgb_linear(channel: u8) -> f32 {
    let value = channel as f32 / 255.0;
    if value <= 0.04045 {
        value / 12.92
    } else {
        ((value + 0.055) / 1.055).powf(2.4)
    }
}

pub fn relative_luminance([r, g, b]: [u8; 3]) -> f32 {
    0.2126 * srgb_linear(r) + 0.7152 * srgb_linear(g) + 0.0722 * srgb_linear(b)
}

pub fn contrast_ratio(a: [u8; 3], b: [u8; 3]) -> f32 {
    let (light, dark) = {
        let a = relative_luminance(a);
        let b = relative_luminance(b);
        (a.max(b), a.min(b))
    };
    (light + 0.05) / (dark + 0.05)
}

pub fn nearest_xterm256(rgb: [u8; 3]) -> u8 {
    (16u16..=255)
        .min_by_key(|&index| {
            let candidate = xterm256_rgb(index as u8);
            candidate
                .into_iter()
                .zip(rgb)
                .map(|(a, b)| {
                    let delta = a as i32 - b as i32;
                    delta * delta
                })
                .sum::<i32>()
        })
        .unwrap_or(16) as u8
}

pub fn xterm256_rgb(index: u8) -> [u8; 3] {
    const ANSI: [[u8; 3]; 16] = [
        [0, 0, 0],
        [128, 0, 0],
        [0, 128, 0],
        [128, 128, 0],
        [0, 0, 128],
        [128, 0, 128],
        [0, 128, 128],
        [192, 192, 192],
        [128, 128, 128],
        [255, 0, 0],
        [0, 255, 0],
        [255, 255, 0],
        [0, 0, 255],
        [255, 0, 255],
        [0, 255, 255],
        [255, 255, 255],
    ];
    match index {
        0..=15 => ANSI[index as usize],
        16..=231 => {
            let value = index - 16;
            let component = |part: u8| if part == 0 { 0 } else { 55 + 40 * part };
            [
                component(value / 36),
                component((value % 36) / 6),
                component(value % 6),
            ]
        }
        232..=255 => {
            let value = 8 + 10 * (index - 232);
            [value, value, value]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn role_keys_and_labels_are_unique_and_stable() {
        assert_eq!(ThemeRole::ALL.len(), 17);
        assert_eq!(ThemeRole::Canvas.key(), "canvas");
        assert_eq!(ThemeRole::FocusedBorder.key(), "focused_border");
        assert_eq!(ThemeRole::Toast.label(), "Toast text");
        assert_eq!(
            ThemeRole::ALL
                .iter()
                .map(|r| r.key())
                .collect::<BTreeSet<_>>()
                .len(),
            17
        );
        assert_eq!(
            ThemeRole::ALL
                .iter()
                .map(|r| r.label())
                .collect::<BTreeSet<_>>()
                .len(),
            17
        );
    }

    #[test]
    fn role_keys_round_trip() {
        for role in ThemeRole::ALL {
            assert_eq!(ThemeRole::from_key(role.key()), Some(role));
        }
        assert_eq!(ThemeRole::from_key("future_role"), None);
    }

    #[test]
    fn terminal_capability_classifier_is_conservative() {
        let cases = [
            (
                Some("truecolor"),
                Some("xterm"),
                TerminalColorCapability::TrueColor,
            ),
            (Some("24bit"), None, TerminalColorCapability::TrueColor),
            (
                None,
                Some("xterm-direct"),
                TerminalColorCapability::TrueColor,
            ),
            (
                None,
                Some("xterm-256color"),
                TerminalColorCapability::Ansi256,
            ),
            (None, Some("xterm"), TerminalColorCapability::Ansi16),
            (None, None, TerminalColorCapability::Unknown),
        ];
        for (colorterm, term, expected) in cases {
            assert_eq!(TerminalColorCapability::classify(colorterm, term), expected);
        }
    }

    #[test]
    fn contrast_math_matches_wcag_reference_values() {
        assert!((srgb_linear(0) - 0.0).abs() < 0.0001);
        assert!((srgb_linear(255) - 1.0).abs() < 0.0001);
        assert!((contrast_ratio([0, 0, 0], [255, 255, 255]) - 21.0).abs() < 0.001);
        assert!(contrast_ratio([119, 119, 119], [255, 255, 255]) >= 4.47);
    }

    #[test]
    fn contrast_assessment_handles_thresholds_reset_and_capability() {
        assert!(matches!(
            assess_contrast(
                Color::Rgb(255, 255, 255),
                Color::Rgb(0, 0, 0),
                4.5,
                TerminalColorCapability::TrueColor
            ),
            ContrastAssessment::Readable {
                quantized: false,
                ..
            }
        ));
        assert!(matches!(
            assess_contrast(
                Color::Rgb(20, 20, 20),
                Color::Rgb(0, 0, 0),
                4.5,
                TerminalColorCapability::TrueColor
            ),
            ContrastAssessment::LowContrast { .. }
        ));
        assert_eq!(
            assess_contrast(
                Color::Reset,
                Color::Rgb(0, 0, 0),
                4.5,
                TerminalColorCapability::TrueColor
            ),
            ContrastAssessment::Unverifiable {
                warning: ContrastWarning::DefaultBackground
            }
        );
        assert_eq!(
            assess_contrast(
                Color::Rgb(255, 255, 255),
                Color::Rgb(0, 0, 0),
                4.5,
                TerminalColorCapability::Unknown
            ),
            ContrastAssessment::Unverifiable {
                warning: ContrastWarning::TerminalCapability
            }
        );
    }

    #[test]
    fn ansi256_assessment_uses_quantized_colors() {
        assert_eq!(xterm256_rgb(16), [0, 0, 0]);
        assert_eq!(xterm256_rgb(231), [255, 255, 255]);
        assert!(matches!(
            assess_contrast(
                Color::Rgb(254, 254, 254),
                Color::Rgb(1, 1, 1),
                4.5,
                TerminalColorCapability::Ansi256
            ),
            ContrastAssessment::Readable {
                quantized: true,
                ..
            }
        ));
    }

    #[test]
    fn rgb_parser_rejects_unsupported_shapes() {
        assert_eq!(parse_rgb("#12abEF"), Ok([0x12, 0xab, 0xef]));
        assert_eq!(parse_rgb("#fff"), Err("expected #RRGGBB"));
        assert_eq!(parse_rgb("reset"), Err("expected #RRGGBB"));
    }
}

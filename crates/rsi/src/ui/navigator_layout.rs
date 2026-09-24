//! Pure, display-cell-aware layout for the one-line session navigator.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::types::{NavigatorOptionalColumn, NavigatorPreset};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NavigatorColumn {
    Ordinal,
    Pin,
    Status,
    Attention,
    Function,
    Context,
    Turns,
    Age,
    Provider,
    Model,
    Effort,
    Retry,
    Cost,
    Work,
    Rotation,
    Project,
    Created,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Alignment {
    Left,
    Right,
    Center,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColumnLayout {
    pub column: NavigatorColumn,
    pub start: usize,
    pub width: usize,
    pub align: Alignment,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NavigatorLayout {
    pub columns: Vec<ColumnLayout>,
    pub inner_width: usize,
    pub complete: bool,
    /// The space between the selection rail and the first column. Normal
    /// layouts retain the rail gap; degraded widths reserve every cell.
    pub leading_gap: usize,
}

pub const OPTIONAL_ORDER: [NavigatorOptionalColumn; 8] = [
    NavigatorOptionalColumn::Age,
    NavigatorOptionalColumn::ModelEffort,
    NavigatorOptionalColumn::Retry,
    NavigatorOptionalColumn::Cost,
    NavigatorOptionalColumn::Work,
    NavigatorOptionalColumn::Rotation,
    NavigatorOptionalColumn::Project,
    NavigatorOptionalColumn::Created,
];

pub const fn required_floor(ordinal_width: usize) -> usize {
    36 + ordinal_width
}

const fn gap_after(column: NavigatorColumn) -> usize {
    match column {
        NavigatorColumn::Ordinal
        | NavigatorColumn::Pin
        | NavigatorColumn::Status
        | NavigatorColumn::Attention => 2,
        _ => 1,
    }
}

const PREFERRED_FUNCTION_WIDTH: usize = 24;
const REQUIRED_FUNCTION_WIDTH: usize = 12;

pub fn preset_columns(preset: NavigatorPreset) -> Vec<NavigatorOptionalColumn> {
    match preset {
        NavigatorPreset::Dense => vec![
            NavigatorOptionalColumn::Age,
            NavigatorOptionalColumn::ModelEffort,
        ],
        NavigatorPreset::Operations => vec![
            NavigatorOptionalColumn::Age,
            NavigatorOptionalColumn::ModelEffort,
            NavigatorOptionalColumn::Retry,
            NavigatorOptionalColumn::Work,
            NavigatorOptionalColumn::Project,
        ],
        NavigatorPreset::Cost => vec![
            NavigatorOptionalColumn::Age,
            NavigatorOptionalColumn::ModelEffort,
            NavigatorOptionalColumn::Cost,
            NavigatorOptionalColumn::Work,
            NavigatorOptionalColumn::Created,
        ],
    }
}

fn optional_columns(
    column: NavigatorOptionalColumn,
) -> &'static [(NavigatorColumn, usize, Alignment)] {
    use Alignment::{Center, Left, Right};
    use NavigatorColumn as Column;

    match column {
        NavigatorOptionalColumn::Age => &[(Column::Age, 4, Right)],
        // One persisted preference controls the complete execution identity.
        // Provider and effort are one-cell glyphs; the model cell shows a
        // verbatim suffix of the canonical ID (`glyphs::list_model_label`), so
        // a family shorthand can never replace the actual model version.
        NavigatorOptionalColumn::ModelEffort => &[
            (Column::Provider, 1, Center),
            (Column::Model, 20, Left),
            (Column::Effort, 1, Center),
        ],
        NavigatorOptionalColumn::Retry => &[(Column::Retry, 5, Right)],
        NavigatorOptionalColumn::Cost => &[(Column::Cost, 7, Right)],
        NavigatorOptionalColumn::Work => &[(Column::Work, 6, Right)],
        NavigatorOptionalColumn::Rotation => &[(Column::Rotation, 3, Right)],
        NavigatorOptionalColumn::Project => &[(Column::Project, 12, Left)],
        NavigatorOptionalColumn::Created => &[(Column::Created, 11, Right)],
    }
}
fn required_columns(ordinal_width: usize) -> [(NavigatorColumn, usize, Alignment); 7] {
    [
        (NavigatorColumn::Ordinal, ordinal_width, Alignment::Right),
        (NavigatorColumn::Pin, 1, Alignment::Center),
        (NavigatorColumn::Status, 1, Alignment::Center),
        (NavigatorColumn::Attention, 1, Alignment::Center),
        (NavigatorColumn::Function, 12, Alignment::Left),
        (NavigatorColumn::Context, 4, Alignment::Right),
        (NavigatorColumn::Turns, 5, Alignment::Right),
    ]
}

pub fn resolve(
    inner_width: usize,
    ordinal_width: usize,
    preset: NavigatorPreset,
    overrides: Option<&[NavigatorOptionalColumn]>,
) -> NavigatorLayout {
    let floor = required_floor(ordinal_width);
    if inner_width < floor {
        return degraded(inner_width, ordinal_width);
    }
    let enabled = overrides
        .map(|items| items.to_vec())
        .unwrap_or_else(|| preset_columns(preset));
    let mut visible = required_columns(ordinal_width).to_vec();
    let mut used = floor;
    for optional in OPTIONAL_ORDER {
        if enabled.contains(&optional) {
            let group = optional_columns(optional);
            let next = group.iter().map(|(_, width, _)| width + 1).sum::<usize>();
            let title_reserve = if optional == NavigatorOptionalColumn::Age {
                0
            } else {
                PREFERRED_FUNCTION_WIDTH - REQUIRED_FUNCTION_WIDTH
            };
            if used + next + title_reserve <= inner_width {
                visible.extend_from_slice(group);
                used += next;
            } else {
                break;
            }
        }
    }
    let slack = inner_width - used;
    let mut start = 2;
    let mut columns = Vec::with_capacity(visible.len());
    for (column, mut width, align) in visible {
        if column == NavigatorColumn::Function {
            width += slack;
        }
        columns.push(ColumnLayout {
            column,
            start,
            width,
            align,
        });
        start += width + gap_after(column);
    }
    NavigatorLayout {
        columns,
        inner_width,
        complete: true,
        leading_gap: 1,
    }
}

fn degraded(inner_width: usize, ordinal_width: usize) -> NavigatorLayout {
    if inner_width <= 1 {
        return NavigatorLayout {
            columns: Vec::new(),
            inner_width,
            complete: false,
            leading_gap: 0,
        };
    }
    let mut kept = required_columns(ordinal_width).to_vec();
    while !kept.is_empty() {
        let used = 1 + kept.iter().map(|(_, width, _)| width + 1).sum::<usize>() - 1;
        if used <= inner_width {
            break;
        }
        let drop = [
            NavigatorColumn::Turns,
            NavigatorColumn::Context,
            NavigatorColumn::Attention,
            NavigatorColumn::Status,
            NavigatorColumn::Pin,
            NavigatorColumn::Ordinal,
        ]
        .into_iter()
        .find(|candidate| kept.iter().any(|(column, _, _)| column == candidate));
        if let Some(drop) = drop {
            kept.retain(|(column, _, _)| *column != drop);
        } else {
            break;
        }
    }
    if kept.is_empty() {
        kept.push((NavigatorColumn::Function, 1, Alignment::Left));
    }
    let mut start = 1;
    let mut columns = Vec::new();
    for (column, width, align) in kept {
        if start >= inner_width {
            break;
        }
        let width = if column == NavigatorColumn::Function {
            width.min(inner_width - start).max(1)
        } else {
            width.min(inner_width - start)
        };
        columns.push(ColumnLayout {
            column,
            start,
            width,
            align,
        });
        start += width + 1;
    }
    NavigatorLayout {
        columns,
        inner_width,
        complete: false,
        leading_gap: 0,
    }
}

pub fn display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}
pub fn truncate_cells(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if display_width(text) <= width {
        return text.to_string();
    }
    if width == 1 {
        return "~".to_string();
    }
    let mut result = String::new();
    for grapheme in text.graphemes(true) {
        if display_width(&result) + display_width(grapheme) + 1 > width {
            break;
        }
        result.push_str(grapheme);
    }
    result.push('~');
    result
}

pub fn truncate_cells_with_ellipsis(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if display_width(text) <= width {
        return text.to_string();
    }
    if width == 1 {
        return "…".to_string();
    }
    let mut result = String::new();
    for grapheme in text.graphemes(true) {
        if display_width(&result) + display_width(grapheme) + 1 > width {
            break;
        }
        result.push_str(grapheme);
    }
    result.push('…');
    result
}
pub fn span_cells(text: &str, width: usize) -> String {
    align_cells(text, width, Alignment::Left)
}

pub fn align_cells(text: &str, width: usize, alignment: Alignment) -> String {
    let text = truncate_cells(text, width);
    let padding = width.saturating_sub(display_width(&text));
    let (left, right) = match alignment {
        Alignment::Left => (0, padding),
        Alignment::Right => (padding, 0),
        Alignment::Center => (padding / 2, padding - padding / 2),
    };
    format!("{}{}{}", " ".repeat(left), text, " ".repeat(right))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn columns(layout: &NavigatorLayout) -> Vec<NavigatorColumn> {
        layout.columns.iter().map(|cell| cell.column).collect()
    }

    fn all_enabled() -> &'static [NavigatorOptionalColumn] {
        &NavigatorOptionalColumn::ALL
    }

    #[test]
    fn t01_floors_and_one_line_layouts_are_stable() {
        assert_eq!(
            (required_floor(2), required_floor(3), required_floor(4)),
            (38, 39, 40)
        );
        for width in 1..=240 {
            for ordinal in 2..=4 {
                let layout = resolve(width, ordinal, NavigatorPreset::Dense, None);
                assert!(
                    layout
                        .columns
                        .windows(2)
                        .all(|pair| pair[0].start + pair[0].width < pair[1].start)
                );
                assert!(layout.columns.iter().all(|c| c.start + c.width <= width));
            }
        }
    }

    #[test]
    fn required_identity_columns_have_two_cell_gaps_before_function() {
        let layout = resolve(120, 2, NavigatorPreset::Dense, None);
        for pair in layout.columns.windows(2).take(4) {
            assert_eq!(pair[1].start - pair[0].start - pair[0].width, 2);
        }
        let Some(function) = layout
            .columns
            .iter()
            .find(|cell| cell.column == NavigatorColumn::Function)
        else {
            panic!("function column");
        };
        let Some(context) = layout
            .columns
            .iter()
            .find(|cell| cell.column == NavigatorColumn::Context)
        else {
            panic!("context column");
        };
        assert_eq!(context.start - function.start - function.width, 1);
    }

    #[test]
    fn t02_all_enabled_breakpoints_shift_with_ordinal_width() {
        let breakpoints = [
            (NavigatorColumn::Age, 43),
            (NavigatorColumn::Provider, 80),
            (NavigatorColumn::Model, 80),
            (NavigatorColumn::Effort, 80),
            (NavigatorColumn::Retry, 86),
            (NavigatorColumn::Cost, 94),
            (NavigatorColumn::Work, 101),
            (NavigatorColumn::Rotation, 105),
            (NavigatorColumn::Project, 118),
            (NavigatorColumn::Created, 130),
        ];
        for ordinal_width in 2..=4 {
            for (column, breakpoint) in breakpoints {
                let breakpoint = breakpoint + ordinal_width - 2;
                let before = resolve(
                    breakpoint - 1,
                    ordinal_width,
                    NavigatorPreset::Dense,
                    Some(all_enabled()),
                );
                let at = resolve(
                    breakpoint,
                    ordinal_width,
                    NavigatorPreset::Dense,
                    Some(all_enabled()),
                );
                let after = resolve(
                    breakpoint + 1,
                    ordinal_width,
                    NavigatorPreset::Dense,
                    Some(all_enabled()),
                );
                assert!(!columns(&before).contains(&column));
                assert!(columns(&at).contains(&column));
                assert!(columns(&after).contains(&column));
                let new_columns: Vec<_> = columns(&at)
                    .into_iter()
                    .filter(|candidate| !columns(&before).contains(candidate))
                    .collect();
                let expected = if column == NavigatorColumn::Provider {
                    vec![
                        NavigatorColumn::Provider,
                        NavigatorColumn::Model,
                        NavigatorColumn::Effort,
                    ]
                } else if matches!(column, NavigatorColumn::Model | NavigatorColumn::Effort) {
                    continue;
                } else {
                    vec![column]
                };
                assert_eq!(new_columns, expected);
            }
        }
    }

    #[test]
    fn t03_preset_breakpoints_follow_enabled_subsequence() {
        let cases = [
            (NavigatorPreset::Dense, vec![(43, 1), (80, 3)]),
            (
                NavigatorPreset::Operations,
                vec![(43, 1), (80, 3), (86, 1), (93, 1), (106, 1)],
            ),
            (
                NavigatorPreset::Cost,
                vec![(43, 1), (80, 3), (88, 1), (95, 1), (107, 1)],
            ),
        ];
        for ordinal_width in 2..=4 {
            for (preset, breakpoints) in &cases {
                for (breakpoint, admitted) in breakpoints {
                    let breakpoint = breakpoint + ordinal_width - 2;
                    let before = resolve(breakpoint - 1, ordinal_width, *preset, None);
                    let at = resolve(breakpoint, ordinal_width, *preset, None);
                    assert_eq!(columns(&at).len(), columns(&before).len() + admitted);
                }
            }
        }
    }

    #[test]
    fn t04_cumulative_admission_never_skips_an_enabled_candidate() {
        for ordinal_width in 2..=4 {
            for width in required_floor(ordinal_width)..=240 {
                let visible = columns(&resolve(
                    width,
                    ordinal_width,
                    NavigatorPreset::Dense,
                    Some(all_enabled()),
                ));
                let optional: Vec<_> = OPTIONAL_ORDER
                    .iter()
                    .copied()
                    .flat_map(|column| optional_columns(column).iter().map(|spec| spec.0))
                    .collect();
                let visible_optional: Vec<_> = optional
                    .iter()
                    .copied()
                    .filter(|column| visible.contains(column))
                    .collect();
                assert_eq!(visible_optional, optional[..visible_optional.len()]);
            }
        }
    }

    #[test]
    fn t05_t06_offsets_bounds_and_shared_geometry_hold_at_every_width() {
        for ordinal_width in 2..=4 {
            for width in 1..=240 {
                let header = resolve(width, ordinal_width, NavigatorPreset::Cost, None);
                let row = resolve(width, ordinal_width, NavigatorPreset::Cost, None);
                assert!(
                    header
                        .columns
                        .windows(2)
                        .all(|pair| pair[0].start + pair[0].width < pair[1].start)
                );
                assert!(
                    header
                        .columns
                        .iter()
                        .all(|cell| cell.start + cell.width <= width)
                );
                assert_eq!(header.columns, row.columns);
            }
        }
    }

    #[test]
    fn t07_sub_floor_widths_keep_the_resolved_rail_and_function_destination() {
        for ordinal_width in 2..=4 {
            let floor = required_floor(ordinal_width);
            let width_one = resolve(1, ordinal_width, NavigatorPreset::Dense, None);
            assert_eq!(width_one.leading_gap, 0);
            assert!(width_one.columns.is_empty());
            for width in 2..floor {
                let layout = resolve(width, ordinal_width, NavigatorPreset::Dense, None);
                let function = layout
                    .columns
                    .iter()
                    .find(|cell| cell.column == NavigatorColumn::Function)
                    .unwrap();
                assert_eq!(layout.leading_gap, 0);
                assert!(function.width >= 1);
                assert!(function.start + function.width <= width);
            }
        }
    }

    #[test]
    fn t19_t20_display_cell_alignment_preserves_wide_graphemes_and_fixed_glyphs() {
        assert_eq!(display_width(&span_cells("界e\u{301}", 4)), 4);
        for text in ["ascii", "界", "🦀", "e\u{301}"] {
            assert_eq!(display_width(&align_cells(text, 8, Alignment::Left)), 8);
            assert_eq!(display_width(&align_cells(text, 8, Alignment::Right)), 8);
            assert_eq!(display_width(&align_cells(text, 8, Alignment::Center)), 8);
        }
        for glyph in [
            "∞", "◆", "◐", "●", "?", "✓", "×", "■", "·", "•", "↺", "⧗", "✻", "◎", "⋈", "█",
        ] {
            assert_eq!(display_width(&align_cells(glyph, 1, Alignment::Center)), 1);
        }
        assert_eq!(truncate_cells_with_ellipsis("very-long-model", 5), "very…");
    }
}

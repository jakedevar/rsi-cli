//! Drawing the wrapped, scrolling text fields of a form (#3421).
//!
//! A field is a label and a value. The value wraps inside its own rectangle
//! (continuation lines line up under the first character of the value, not
//! under the label) and scrolls inside it to keep the cursor in view. The
//! focused field draws the live [`InputSurface`] behind it, with its vim mode;
//! every other field draws its plain text the same way, so a field is the same
//! height whether or not it has focus.

use crate::input_surface::InputSurface;
use crate::types::PopupMode;
use crate::ui::{overlay::count_visual_lines_in, session, theme};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use ratatui::widgets::Paragraph;

/// One labelled, wrapped text field.
pub(crate) struct Field<'a> {
    /// The label drawn before the value, such as `> Title: `.
    pub label: String,
    pub label_style: Style,
    /// The field's text when it has no live surface.
    pub value: &'a str,
    /// The live surface of the focused field.
    pub surface: Option<&'a InputSurface>,
    /// The fewest and most rows the value takes.
    pub min_rows: u16,
    pub max_rows: u16,
}

fn label_width(label: &str) -> u16 {
    label.chars().count().min(u16::MAX as usize) as u16
}

fn field_lines(field: &Field<'_>) -> Vec<String> {
    match field.surface {
        Some(surface) => surface.textarea.lines().to_vec(),
        None => field.value.split('\n').map(str::to_string).collect(),
    }
}

/// The rows `field` takes at `width` columns (label included).
pub(crate) fn field_rows(field: &Field<'_>, width: u16) -> u16 {
    let value_width = width.saturating_sub(label_width(&field.label)).max(1) as usize;
    let wanted = count_visual_lines_in(&field_lines(field), value_width);
    (wanted.min(u16::MAX as usize) as u16).clamp(field.min_rows.max(1), field.max_rows.max(1))
}

/// Draw `field` in `area`. `background` is the colour behind the text.
pub(crate) fn render_field(frame: &mut Frame, area: Rect, field: &Field<'_>, background: Color) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let label_cols = label_width(&field.label).min(area.width.saturating_sub(1));
    let label_area = Rect::new(area.x, area.y, label_cols, 1);
    frame.render_widget(
        Paragraph::new(Span::styled(field.label.clone(), field.label_style)),
        label_area,
    );
    let value_area = Rect::new(
        area.x + label_cols,
        area.y,
        area.width - label_cols,
        area.height,
    );
    match field.surface {
        Some(surface) => {
            surface.wrap_width.set(value_area.width as usize);
            let cursor = if surface.standard_editing || surface.mode == PopupMode::Insert {
                session::CursorStyle::Beam
            } else {
                session::CursorStyle::Block
            };
            session::render_wrapped_textarea(
                frame,
                value_area,
                &surface.textarea,
                cursor,
                session::surface_selection_for_render(surface),
                background,
            );
        }
        None => {
            let textarea = tui_textarea::TextArea::new(field_lines(field));
            session::render_wrapped_textarea(
                frame,
                value_area,
                &textarea,
                session::CursorStyle::Hidden,
                None,
                background,
            );
        }
    }
}

/// The mode pill a form shows in its border for the focused field.
pub(crate) fn mode_pill(label: &str) -> Span<'static> {
    let (fg, bg) = if label == "INSERT" || label == "EDIT" {
        (
            theme::overlay_mode_insert_fg(),
            theme::overlay_mode_insert_bg(),
        )
    } else {
        (
            theme::overlay_mode_normal_fg(),
            theme::overlay_mode_normal_bg(),
        )
    };
    Span::styled(
        format!(" {label} "),
        Style::default().fg(fg).bg(bg).add_modifier(Modifier::BOLD),
    )
}

//! Tests for overlay rendering utilities.

use ratatui::layout::Rect;

use super::centered_rect;

#[test]
fn test_centered_rect_normal_terminal() {
    let area = Rect::new(0, 0, 120, 40);
    let popup = centered_rect(area);
    assert_eq!(popup.width, 96); // 120 * 80% = 96
    assert_eq!(popup.height, 28); // 40 * 70% = 28
    assert_eq!(popup.x, 12); // (120 - 96) / 2
    assert_eq!(popup.y, 6); // (40 - 28) / 2
}

#[test]
fn test_centered_rect_small_terminal() {
    let area = Rect::new(0, 0, 40, 12);
    let popup = centered_rect(area);
    assert_eq!(popup.width, 40); // 40 * 80% = 32, clamped to min 40
    assert_eq!(popup.height, 10); // 12 * 70% = 8, clamped to min 10
}

#[test]
fn test_centered_rect_huge_terminal() {
    let area = Rect::new(0, 0, 300, 80);
    let popup = centered_rect(area);
    assert_eq!(popup.width, 120); // 300 * 80% = 240, clamped to max 120
    assert_eq!(popup.height, 30); // 80 * 70% = 56, clamped to max 30
}

// === apply_geometry_deltas tests ===

use super::apply_geometry_deltas;
use crate::types::ModalGeometry;

#[test]
fn test_geometry_zero_deltas_passthrough() {
    let base = Rect::new(20, 5, 60, 20);
    let viewport = Rect::new(0, 0, 120, 40);
    let result = apply_geometry_deltas(base, &ModalGeometry::default(), viewport);
    assert_eq!(result, base);
}

#[test]
fn test_geometry_move_right_down() {
    let base = Rect::new(20, 5, 60, 20);
    let viewport = Rect::new(0, 0, 120, 40);
    let geom = ModalGeometry {
        dx: 10,
        dy: 5,
        dw: 0,
        dh: 0,
    };
    let result = apply_geometry_deltas(base, &geom, viewport);
    assert_eq!(result.x, 30);
    assert_eq!(result.y, 10);
    assert_eq!(result.width, 60);
    assert_eq!(result.height, 20);
}

#[test]
fn test_geometry_resize_wider_taller() {
    let base = Rect::new(30, 5, 60, 20);
    let viewport = Rect::new(0, 0, 120, 40);
    let geom = ModalGeometry {
        dx: 0,
        dy: 0,
        dw: 20,
        dh: 10,
    };
    let result = apply_geometry_deltas(base, &geom, viewport);
    assert_eq!(result.width, 80);
    assert_eq!(result.height, 30);
}

#[test]
fn test_geometry_clamp_min_size() {
    let base = Rect::new(20, 5, 60, 20);
    let viewport = Rect::new(0, 0, 120, 40);
    // Shrink far beyond minimum
    let geom = ModalGeometry {
        dx: 0,
        dy: 0,
        dw: -100,
        dh: -100,
    };
    let result = apply_geometry_deltas(base, &geom, viewport);
    assert_eq!(result.width, 20); // min width
    assert_eq!(result.height, 5); // min height
}

#[test]
fn test_geometry_clamp_to_viewport() {
    let base = Rect::new(50, 20, 60, 20);
    let viewport = Rect::new(0, 0, 100, 30);
    // Move right and down beyond viewport
    let geom = ModalGeometry {
        dx: 100,
        dy: 100,
        dw: 0,
        dh: 0,
    };
    let result = apply_geometry_deltas(base, &geom, viewport);
    // Should clamp: x + w <= 100, y + h <= 30
    assert_eq!(result.x + result.width, 100);
    assert_eq!(result.y + result.height, 30);
}

#[test]
fn test_geometry_clamp_negative_position() {
    let base = Rect::new(10, 5, 30, 15);
    let viewport = Rect::new(0, 0, 100, 40);
    let geom = ModalGeometry {
        dx: -50,
        dy: -50,
        dw: 0,
        dh: 0,
    };
    let result = apply_geometry_deltas(base, &geom, viewport);
    assert_eq!(result.x, 0); // clamped to viewport.x
    assert_eq!(result.y, 0); // clamped to viewport.y
}

#[test]
fn test_geometry_huge_deltas_bounded() {
    let base = Rect::new(10, 5, 30, 15);
    let viewport = Rect::new(0, 0, 80, 24);
    // Enormous resize
    let geom = ModalGeometry {
        dx: 0,
        dy: 0,
        dw: 1000,
        dh: 1000,
    };
    let result = apply_geometry_deltas(base, &geom, viewport);
    // Width and height clamped to viewport
    assert!(result.width <= viewport.width);
    assert!(result.height <= viewport.height);
    assert!(result.x + result.width <= viewport.width);
    assert!(result.y + result.height <= viewport.height);
}

// === compute_dynamic_popup_rect — paste-overflow regression ===
//
// Bug: pasting a long prompt caused the new-session modal to expand "all the way
// down" and beyond the visible viewport. compute_dynamic_popup_rect now reserves
// MODAL_BOTTOM_MARGIN rows below the popup so it never reaches the screen edge.

use super::{compute_dynamic_popup_rect, compute_dynamic_popup_rect_with_manual_height};
use tui_textarea::TextArea;

/// Build a textarea pre-filled with `n` lines of `lorem ipsum`-ish content.
/// Used to simulate a paste of arbitrary size.
fn make_textarea_with_lines(n: usize) -> TextArea<'static> {
    let lines: Vec<String> = (0..n)
        .map(|i| format!("line {i} — the quick brown fox jumps over the lazy dog"))
        .collect();
    TextArea::new(lines)
}

#[test]
fn test_dynamic_popup_does_not_reach_screen_bottom_with_long_paste() {
    // Simulate a 50-line paste in a 30-row terminal.
    let area = Rect::new(0, 0, 100, 30);
    let textarea = make_textarea_with_lines(50);
    let max_h = (area.height * 50 / 100).max(9);
    let rect = compute_dynamic_popup_rect(area, &textarea, 6, 3, area.y + 1, max_h, None, None);

    // The popup must NOT touch the absolute bottom row of the screen — the bottom
    // dashboard / footer needs room to render below it.
    let popup_bottom = rect.y + rect.height;
    assert!(
        popup_bottom < area.y + area.height,
        "popup bottom {popup_bottom} should be strictly less than area bottom {} (paste should not push popup off-screen)",
        area.y + area.height
    );
}

#[test]
fn test_dynamic_popup_respects_max_h_cap() {
    // 50% cap on a 40-row terminal = 20 rows. With huge content, h must be ≤ 20.
    let area = Rect::new(0, 0, 100, 40);
    let textarea = make_textarea_with_lines(200);
    let max_h = (area.height * 50 / 100).max(9);
    let rect = compute_dynamic_popup_rect(area, &textarea, 6, 3, area.y + 1, max_h, None, None);
    assert!(
        rect.height <= max_h,
        "popup height {} exceeded max_h cap {max_h}",
        rect.height
    );
    // And still pinned to row 1 (just below the status bar).
    assert_eq!(rect.y, 1);
}

#[test]
fn test_dynamic_popup_grows_with_short_content() {
    // Sanity: a small textarea should produce a popup well below max_h, but
    // larger than the floor (chrome + min textarea = 9). The exact value depends
    // on visual wrapping at inner_w, so just bracket it.
    let area = Rect::new(0, 0, 100, 40);
    let textarea = make_textarea_with_lines(2);
    let max_h = (area.height * 50 / 100).max(9);
    let rect = compute_dynamic_popup_rect(area, &textarea, 6, 3, area.y + 1, max_h, None, None);
    assert!(
        rect.height >= 9,
        "popup height {} below floor of 9",
        rect.height
    );
    assert!(
        rect.height < max_h,
        "popup height {} should be well below max_h {max_h} for short content",
        rect.height
    );
}

#[test]
fn test_manual_height_delta_pins_prompt_base_height() {
    let area = Rect::new(0, 0, 100, 40);
    let three_lines = TextArea::new(vec![
        "line 1".to_string(),
        "line 2".to_string(),
        "line 3".to_string(),
    ]);
    let four_lines = TextArea::new(vec![
        "line 1".to_string(),
        "line 2".to_string(),
        "line 3".to_string(),
        "line 4".to_string(),
    ]);
    let max_h = (area.height * 50 / 100).max(9);
    let geom = ModalGeometry {
        dh: 2,
        ..Default::default()
    };

    let before = compute_dynamic_popup_rect_with_manual_height(
        area,
        &three_lines,
        6,
        3,
        area.y + 1,
        max_h,
        None,
        None,
        &geom,
    );
    let after = compute_dynamic_popup_rect_with_manual_height(
        area,
        &four_lines,
        6,
        3,
        area.y + 1,
        max_h,
        None,
        None,
        &geom,
    );

    assert_eq!(before.height, after.height);
    assert_eq!(before.height, 9);
}

#[test]
fn test_dynamic_popup_tiny_terminal_degrades_gracefully() {
    // A terminal smaller than chrome+margin should still produce a non-panicking
    // (possibly zero-height) rect rather than wrap/panic.
    let area = Rect::new(0, 0, 80, 4);
    let textarea = make_textarea_with_lines(20);
    let max_h = (area.height * 50 / 100).max(9);
    let rect = compute_dynamic_popup_rect(area, &textarea, 6, 3, area.y + 1, max_h, None, None);
    // Modal bottom must not exceed terminal bottom even when MODAL_BOTTOM_MARGIN
    // can't be honored.
    assert!(rect.y + rect.height <= area.y + area.height);
}

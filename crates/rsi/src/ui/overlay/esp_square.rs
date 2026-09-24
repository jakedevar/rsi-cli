use super::fixed_centered_rect;
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
};

// T1 D4: deliberately theme-independent. SQUARE_COLORS/HIT_COLOR/MISS_COLOR/
// CURSOR_COLOR are minigame feedback colors, not theme chrome — like a chess
// app's check-indicator or a quiz app's right/wrong flash, "correct" must
// read identically every time regardless of which of the six UI themes is
// active. Routing these through semantic theme fns would either create
// single-use functions with no reuse value elsewhere, or force a real design
// choice (should hit/miss flashes vary by theme?) that nothing in the
// program spec asked for. Exempted from theme absorption on purpose; not an
// oversight.
const SQUARE_COLORS: [(u8, u8, u8); 9] = [
    (83, 143, 255),  // 0: Blue
    (180, 49, 230),  // 1: Purple
    (247, 237, 101), // 2: Yellow
    (40, 210, 171),  // 3: Green
    (252, 162, 7),   // 4: Orange
    (246, 204, 249), // 5: Pink
    (0, 255, 255),   // 6: Cyan
    (38, 129, 137),  // 7: Teal
    (45, 26, 119),   // 8: Indigo
];

// Labels match keyboard spatial layout (home row + rows above/below)
const SQUARE_LABELS: [&str; 9] = ["u", "i", "o", "j", "k", "l", "m", ",", "."];

const HIT_COLOR: Color = Color::Rgb(50, 205, 50); // Green for correct flash
const MISS_COLOR: Color = Color::Rgb(220, 50, 50); // Red for miss flash
const CURSOR_COLOR: Color = Color::Rgb(255, 255, 255); // White border for cursor

const SQUARE_W: u16 = 9;
const SQUARE_H: u16 = 5;
const GAP_H: u16 = 1;
const GAP_V: u16 = 1;

pub fn render_esp_square(
    frame: &mut Frame,
    area: Rect,
    round: u8,
    correct: u8,
    interactive: bool,
    _rounds: &[Option<bool>],
    message: &str,
    flash: Option<(usize, bool)>,
    cursor: Option<usize>,
) {
    let popup_w = 3 * SQUARE_W + 2 * GAP_H + 4; // 31 + 4 = 35
    let popup_h = 3 * SQUARE_H + 2 * GAP_V + 6; // 17 + 6 = 23
    let popup_area = fixed_centered_rect(area, popup_w, popup_h);
    frame.render_widget(Clear, popup_area);

    let outer = Block::default()
        .title(" ESP Square ")
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded);
    frame.render_widget(outer, popup_area);

    let inner = Rect {
        x: popup_area.x + 1,
        y: popup_area.y + 1,
        width: popup_area.width.saturating_sub(2),
        height: popup_area.height.saturating_sub(2),
    };

    // Header
    let mut header_spans = vec![Span::styled(
        format!("  Round {}/12", round),
        Style::default(),
    )];
    if interactive || round == 12 {
        header_spans.push(Span::raw("  "));
        header_spans.push(Span::styled(
            format!("Correct {}/12", correct),
            Style::default(),
        ));
    }
    let header = Line::from(header_spans);
    frame.render_widget(
        Paragraph::new(header),
        Rect {
            x: inner.x,
            y: inner.y,
            width: inner.width,
            height: 1,
        },
    );

    // Grid
    let grid_w = 3 * SQUARE_W + 2 * GAP_H;
    let grid_origin_x = inner.x + (inner.width.saturating_sub(grid_w)) / 2;
    let grid_origin_y = inner.y + 2; // below header + gap

    for row in 0..3u16 {
        for col in 0..3u16 {
            let idx = (row * 3 + col) as usize;
            let (r, g, b) = SQUARE_COLORS[idx];
            let color = Color::Rgb(r, g, b);
            let x = grid_origin_x + col * (SQUARE_W + GAP_H);
            let y = grid_origin_y + row * (SQUARE_H + GAP_V);
            let sq_rect = Rect {
                x,
                y,
                width: SQUARE_W,
                height: SQUARE_H,
            };

            let is_cursor = cursor == Some(idx);

            match flash {
                Some((flash_idx, true)) if flash_idx == idx => {
                    // CORRECT FLASH: solid green fill
                    let sq_block = Block::default()
                        .borders(Borders::ALL)
                        .border_type(BorderType::Rounded)
                        .border_style(Style::default().fg(HIT_COLOR))
                        .style(Style::default().bg(HIT_COLOR));
                    frame.render_widget(sq_block, sq_rect);

                    let label = Paragraph::new(Span::styled(
                        SQUARE_LABELS[idx],
                        Style::default()
                            .fg(Color::Black)
                            .bg(HIT_COLOR)
                            .add_modifier(Modifier::BOLD),
                    ));
                    frame.render_widget(
                        label,
                        Rect {
                            x: x + SQUARE_W / 2,
                            y: y + SQUARE_H / 2,
                            width: 1,
                            height: 1,
                        },
                    );
                }
                Some((flash_idx, false)) if flash_idx == idx => {
                    // MISS/PEEK FLASH: solid red fill over the correct square
                    let sq_block = Block::default()
                        .borders(Borders::ALL)
                        .border_type(BorderType::Rounded)
                        .border_style(Style::default().fg(MISS_COLOR))
                        .style(Style::default().bg(MISS_COLOR));
                    frame.render_widget(sq_block, sq_rect);

                    let label = Paragraph::new(Span::styled(
                        SQUARE_LABELS[idx],
                        Style::default()
                            .fg(Color::White)
                            .bg(MISS_COLOR)
                            .add_modifier(Modifier::BOLD),
                    ));
                    frame.render_widget(
                        label,
                        Rect {
                            x: x + SQUARE_W / 2,
                            y: y + SQUARE_H / 2,
                            width: 1,
                            height: 1,
                        },
                    );
                }
                _ => {
                    // NORMAL STATE: hollow colored border (white if cursor)
                    let border_color = if is_cursor { CURSOR_COLOR } else { color };
                    let border_modifier = if is_cursor {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    };
                    let sq_block = Block::default()
                        .borders(Borders::ALL)
                        .border_type(BorderType::Rounded)
                        .border_style(
                            Style::default()
                                .fg(border_color)
                                .add_modifier(border_modifier),
                        );
                    frame.render_widget(sq_block, sq_rect);

                    // Key label centered inside square
                    let label_style = if is_cursor {
                        Style::default()
                            .fg(CURSOR_COLOR)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(color).add_modifier(Modifier::DIM)
                    };
                    let label = Paragraph::new(Span::styled(SQUARE_LABELS[idx], label_style));
                    frame.render_widget(
                        label,
                        Rect {
                            x: x + SQUARE_W / 2,
                            y: y + SQUARE_H / 2,
                            width: 1,
                            height: 1,
                        },
                    );
                }
            }
        }
    }

    // Footer / game-over message
    let footer_y = grid_origin_y + 3 * SQUARE_H + 2 * GAP_V;
    if round == 12 {
        // Game-over message
        let msg_line = Line::from(Span::styled(
            message,
            Style::default().add_modifier(Modifier::BOLD),
        ));
        frame.render_widget(
            Paragraph::new(msg_line),
            Rect {
                x: inner.x + 1,
                y: footer_y,
                width: inner.width.saturating_sub(2),
                height: 1,
            },
        );
        let hint = Line::from(Span::styled(
            "  r: new game  Esc: close",
            Style::default().add_modifier(Modifier::DIM),
        ));
        frame.render_widget(
            Paragraph::new(hint),
            Rect {
                x: inner.x,
                y: footer_y + 1,
                width: inner.width,
                height: 1,
            },
        );
    } else {
        let hint = Line::from(Span::styled(
            "  ;: pass  r: reset  t: toggle  Esc: close",
            Style::default().add_modifier(Modifier::DIM),
        ));
        frame.render_widget(
            Paragraph::new(hint),
            Rect {
                x: inner.x,
                y: footer_y,
                width: inner.width,
                height: 1,
            },
        );
    }
}

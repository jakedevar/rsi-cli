pub fn render_app(app: &mut rsi::app::App, width: u16, height: u16) -> ratatui::buffer::Buffer {
    let backend = ratatui::backend::TestBackend::new(width, height);
    let mut terminal = ratatui::Terminal::new(backend).expect("test terminal should initialize");
    app.last_terminal_size = (width, height);
    terminal
        .draw(|frame| rsi::ui::render(frame, app))
        .expect("test render should succeed");
    terminal.backend().buffer().clone()
}

pub fn buffer_to_snapshot(buffer: &ratatui::buffer::Buffer) -> String {
    let area = buffer.area;
    let mut lines = Vec::with_capacity(area.height as usize);

    for y in area.y..area.y + area.height {
        let mut line = String::new();
        for x in area.x..area.x + area.width {
            line.push_str(buffer[(x, y)].symbol());
        }
        let mut line = line.trim_end().to_string();
        if y == area.y {
            normalize_trailing_clock(&mut line);
            normalize_status_time(&mut line);
        }
        lines.push(line);
    }

    lines.join("\n")
}

pub fn line_text(buffer: &ratatui::buffer::Buffer, y: u16) -> String {
    let area = buffer.area;
    let mut line = String::new();
    for x in area.x..area.x + area.width {
        line.push_str(buffer[(x, y)].symbol());
    }
    line
}

pub fn find_cells_with_bg(
    buffer: &ratatui::buffer::Buffer,
    color: ratatui::style::Color,
) -> Vec<(u16, u16)> {
    let area = buffer.area;
    let mut cells = Vec::new();
    for y in area.y..area.y + area.height {
        for x in area.x..area.x + area.width {
            if buffer[(x, y)].style().bg == Some(color) {
                cells.push((x, y));
            }
        }
    }
    cells
}

pub fn find_prompt_rect_by_marker(
    buffer: &ratatui::buffer::Buffer,
    marker: &str,
) -> ratatui::layout::Rect {
    let (marker_x, marker_y) =
        find_text(buffer, marker).unwrap_or_else(|| panic!("marker {marker:?} not found"));
    let top_y = marker_y
        .checked_sub(1)
        .expect("prompt marker should be below a border row");

    let area = buffer.area;
    let mut left = marker_x;
    while left > area.x && !buffer[(left - 1, top_y)].symbol().trim().is_empty() {
        left -= 1;
    }

    let mut right = marker_x;
    while right + 1 < area.x + area.width && !buffer[(right + 1, top_y)].symbol().trim().is_empty()
    {
        right += 1;
    }

    let mut bottom = None;
    for y in marker_y + 1..area.y + area.height {
        if right > left + 1
            && is_horizontal_border(buffer[(left + 1, y)].symbol())
            && !buffer[(left, y)].symbol().trim().is_empty()
            && !buffer[(right, y)].symbol().trim().is_empty()
        {
            bottom = Some(y);
            break;
        }
    }
    let bottom_y =
        bottom.unwrap_or_else(|| panic!("bottom border for marker {marker:?} not found"));

    ratatui::layout::Rect::new(left, top_y, right - left + 1, bottom_y - top_y + 1)
}

fn find_text(buffer: &ratatui::buffer::Buffer, needle: &str) -> Option<(u16, u16)> {
    let area = buffer.area;
    for y in area.y..area.y + area.height {
        let line = line_text(buffer, y);
        if let Some(idx) = line.find(needle) {
            let x = area.x + line[..idx].chars().count() as u16;
            return Some((x, y));
        }
    }
    None
}

fn is_horizontal_border(symbol: &str) -> bool {
    matches!(symbol, "─" | "═" | "━" | "╌" | "╍" | "┄" | "┅")
}

fn normalize_trailing_clock(line: &mut String) {
    let mut chars: Vec<char> = line.chars().collect();
    let len = chars.len();
    if len < 5 {
        return;
    }

    let clock = &chars[len - 5..];
    if clock[0].is_ascii_digit()
        && clock[1].is_ascii_digit()
        && clock[2] == ':'
        && clock[3].is_ascii_digit()
        && clock[4].is_ascii_digit()
    {
        chars.splice(len - 5.., "HH:MM".chars());
        *line = chars.into_iter().collect();
    }
}

fn normalize_status_time(line: &mut String) {
    let marker = "time ";
    let Some(start) = line.find(marker).map(|idx| idx + marker.len()) else {
        return;
    };
    let end = start + 5;
    if line.len() < end {
        return;
    }

    let chars: Vec<char> = line[start..end].chars().collect();
    if chars.len() == 5
        && chars[0].is_ascii_digit()
        && chars[1].is_ascii_digit()
        && chars[2] == ':'
        && chars[3].is_ascii_digit()
        && chars[4].is_ascii_digit()
    {
        line.replace_range(start..end, "HH:MM");
    }
}

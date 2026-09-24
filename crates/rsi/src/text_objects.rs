//! Text object boundary detection for vim operator composition.
//!
//! Operates on `&[String]` (logical lines from tui-textarea) and returns
//! a `TextRange` describing the selected region.

/// A range of text in (row, col) coordinates. End is inclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextRange {
    pub start_row: usize,
    pub start_col: usize,
    pub end_row: usize,
    pub end_col: usize,
}

/// Text object kind — "inner" excludes delimiters, "around" includes them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextObjectKind {
    Inner,  // i
    Around, // a
}

/// Find the range for a word text object (iw/aw).
pub fn word_object(
    lines: &[String],
    row: usize,
    col: usize,
    kind: TextObjectKind,
) -> Option<TextRange> {
    let line = lines.get(row)?;
    let chars: Vec<char> = line.chars().collect();

    if chars.is_empty() {
        return None;
    }

    let col = col.min(chars.len().saturating_sub(1));
    let cur = chars[col];

    // Determine word class of character under cursor
    let class = char_class(cur);

    // Find word start (scan backward while same class)
    let start = (0..=col)
        .rev()
        .take_while(|&i| char_class(chars[i]) == class)
        .last()
        .unwrap_or(col);

    // Find word end (scan forward while same class)
    let end = (col..chars.len())
        .take_while(|&i| char_class(chars[i]) == class)
        .last()
        .unwrap_or(col);

    match kind {
        TextObjectKind::Inner => Some(TextRange {
            start_row: row,
            start_col: start,
            end_row: row,
            end_col: end,
        }),
        TextObjectKind::Around => {
            // Include trailing whitespace, or leading if no trailing
            let trail_end = (end + 1..chars.len())
                .take_while(|&i| chars[i] == ' ' || chars[i] == '\t')
                .last();

            if let Some(te) = trail_end {
                return Some(TextRange {
                    start_row: row,
                    start_col: start,
                    end_row: row,
                    end_col: te,
                });
            }

            // No trailing whitespace — include leading whitespace
            let lead_start = (0..start)
                .rev()
                .take_while(|&i| chars[i] == ' ' || chars[i] == '\t')
                .last();

            let s = lead_start.unwrap_or(start);
            Some(TextRange {
                start_row: row,
                start_col: s,
                end_row: row,
                end_col: end,
            })
        }
    }
}

/// Find the range for a delimiter-pair text object.
pub fn delimited_object(
    lines: &[String],
    row: usize,
    col: usize,
    open: char,
    close: char,
    kind: TextObjectKind,
) -> Option<TextRange> {
    if open == close {
        return quote_object(lines, row, col, open, kind);
    }
    bracket_object(lines, row, col, open, close, kind)
}

/// Find matching quote pair (non-nested).
fn quote_object(
    lines: &[String],
    row: usize,
    col: usize,
    quote: char,
    kind: TextObjectKind,
) -> Option<TextRange> {
    let line = lines.get(row)?;
    let chars: Vec<char> = line.chars().collect();

    // Strategy: find the pair of quotes surrounding the cursor.
    // Try cursor-as-open first (if cursor is on a quote), then search outward.
    let (open_pos, close_pos) = find_quote_pair(&chars, col, quote)?;

    match kind {
        TextObjectKind::Inner => Some(TextRange {
            start_row: row,
            start_col: open_pos + 1,
            end_row: row,
            end_col: close_pos.saturating_sub(1),
        }),
        TextObjectKind::Around => Some(TextRange {
            start_row: row,
            start_col: open_pos,
            end_row: row,
            end_col: close_pos,
        }),
    }
}

/// Find a quote pair surrounding `col`. Returns (open, close) positions.
fn find_quote_pair(chars: &[char], col: usize, quote: char) -> Option<(usize, usize)> {
    // Collect all quote positions on the line
    let positions: Vec<usize> = chars
        .iter()
        .enumerate()
        .filter(|&(_, c)| *c == quote)
        .map(|(i, _)| i)
        .collect();

    // Find the pair that contains the cursor
    // Quotes are paired sequentially: 0-1, 2-3, 4-5, ...
    for pair in positions.chunks(2) {
        if pair.len() == 2 && pair[0] <= col && col <= pair[1] {
            return Some((pair[0], pair[1]));
        }
    }
    None
}

/// Find matching bracket pair with nesting support.
#[allow(clippy::needless_range_loop)]
fn bracket_object(
    lines: &[String],
    row: usize,
    col: usize,
    open: char,
    close: char,
    kind: TextObjectKind,
) -> Option<TextRange> {
    // Search backward for opening bracket
    let mut depth: i32 = 0;
    let mut open_pos = None;

    'outer_back: for r in (0..=row).rev() {
        let chars: Vec<char> = lines[r].chars().collect();
        let end = if r == row {
            col
        } else {
            chars.len().saturating_sub(1)
        };
        for c in (0..=end).rev() {
            if chars[c] == close {
                depth += 1;
            } else if chars[c] == open {
                if depth == 0 {
                    open_pos = Some((r, c));
                    break 'outer_back;
                }
                depth -= 1;
            }
        }
    }

    let (open_row, open_col) = open_pos?;

    // Search forward for closing bracket
    depth = 0;
    let mut close_pos = None;

    'outer_fwd: for r in open_row..lines.len() {
        let chars: Vec<char> = lines[r].chars().collect();
        let start = if r == open_row { open_col } else { 0 };
        for c in start..chars.len() {
            if chars[c] == open {
                depth += 1;
            } else if chars[c] == close {
                depth -= 1;
                if depth == 0 {
                    close_pos = Some((r, c));
                    break 'outer_fwd;
                }
            }
        }
    }

    let (close_row, close_col) = close_pos?;

    match kind {
        TextObjectKind::Inner => {
            // Inner: exclude the delimiters
            if open_row == close_row {
                if open_col + 1 > close_col {
                    // Empty delimiters like ()
                    return Some(TextRange {
                        start_row: open_row,
                        start_col: open_col + 1,
                        end_row: close_row,
                        end_col: open_col, // degenerate range
                    });
                }
                Some(TextRange {
                    start_row: open_row,
                    start_col: open_col + 1,
                    end_row: close_row,
                    end_col: close_col - 1,
                })
            } else {
                Some(TextRange {
                    start_row: open_row,
                    start_col: open_col + 1,
                    end_row: close_row,
                    end_col: close_col.saturating_sub(1),
                })
            }
        }
        TextObjectKind::Around => Some(TextRange {
            start_row: open_row,
            start_col: open_col,
            end_row: close_row,
            end_col: close_col,
        }),
    }
}

/// Find the range for a sentence text object (is/as).
pub fn sentence_object(
    lines: &[String],
    row: usize,
    col: usize,
    kind: TextObjectKind,
) -> Option<TextRange> {
    // Flatten to single string for sentence scanning
    let flat: String = lines.join("\n");
    let chars: Vec<char> = flat.chars().collect();

    // Convert (row, col) to flat offset
    let mut offset = 0;
    for line in lines.iter().take(row) {
        offset += line.len() + 1; // +1 for newline
    }
    offset += col;

    if offset >= chars.len() {
        return None;
    }

    // Find sentence start: scan backward for sentence-ending punctuation followed by space
    let start = find_sentence_start(&chars, offset);
    let end = find_sentence_end(&chars, offset);

    // Convert flat offsets back to (row, col)
    let (sr, sc) = flat_to_rowcol(lines, start);
    let (er, ec) = flat_to_rowcol(lines, end);

    match kind {
        TextObjectKind::Inner => Some(TextRange {
            start_row: sr,
            start_col: sc,
            end_row: er,
            end_col: ec,
        }),
        TextObjectKind::Around => {
            // Include trailing whitespace
            let trail = (end + 1..chars.len())
                .take_while(|&i| chars[i].is_whitespace())
                .last()
                .unwrap_or(end);
            let (tr, tc) = flat_to_rowcol(lines, trail);
            Some(TextRange {
                start_row: sr,
                start_col: sc,
                end_row: tr,
                end_col: tc,
            })
        }
    }
}

/// Find the range for a paragraph text object (ip/ap).
pub fn paragraph_object(
    lines: &[String],
    row: usize,
    _col: usize,
    kind: TextObjectKind,
) -> Option<TextRange> {
    if row >= lines.len() {
        return None;
    }

    let is_blank = |r: usize| lines[r].trim().is_empty();

    // If cursor is on a blank line, the paragraph is the blank block
    if is_blank(row) {
        let start = (0..=row)
            .rev()
            .take_while(|&r| is_blank(r))
            .last()
            .unwrap_or(row);
        let end = (row..lines.len())
            .take_while(|&r| is_blank(r))
            .last()
            .unwrap_or(row);

        return Some(TextRange {
            start_row: start,
            start_col: 0,
            end_row: end,
            end_col: lines[end].len().saturating_sub(1),
        });
    }

    // Find contiguous non-blank lines
    let start = (0..=row)
        .rev()
        .take_while(|&r| !is_blank(r))
        .last()
        .unwrap_or(row);
    let end = (row..lines.len())
        .take_while(|&r| !is_blank(r))
        .last()
        .unwrap_or(row);

    match kind {
        TextObjectKind::Inner => Some(TextRange {
            start_row: start,
            start_col: 0,
            end_row: end,
            end_col: lines[end].len().saturating_sub(1),
        }),
        TextObjectKind::Around => {
            // Include trailing blank lines
            let trail_end = (end + 1..lines.len())
                .take_while(|&r| is_blank(r))
                .last()
                .unwrap_or(end);
            let ec = if trail_end < lines.len() {
                lines[trail_end].len().saturating_sub(1)
            } else {
                0
            };
            Some(TextRange {
                start_row: start,
                start_col: 0,
                end_row: trail_end,
                end_col: ec,
            })
        }
    }
}

// === Helpers ===

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CharClass {
    Word,
    Whitespace,
    Punctuation,
}

fn char_class(ch: char) -> CharClass {
    if ch.is_alphanumeric() || ch == '_' {
        CharClass::Word
    } else if ch.is_whitespace() {
        CharClass::Whitespace
    } else {
        CharClass::Punctuation
    }
}

fn find_sentence_start(chars: &[char], offset: usize) -> usize {
    // Scan backward for start of sentence
    for i in (0..offset).rev() {
        if is_sentence_end_char(chars[i]) {
            // Skip whitespace after sentence end
            let after = (i + 1..=offset)
                .find(|&j| !chars[j].is_whitespace())
                .unwrap_or(offset);
            return after;
        }
    }
    // Start of buffer
    (0..=offset)
        .find(|&i| !chars[i].is_whitespace())
        .unwrap_or(0)
}

fn find_sentence_end(chars: &[char], offset: usize) -> usize {
    for (i, &ch) in chars.iter().enumerate().skip(offset) {
        if is_sentence_end_char(ch) {
            return i;
        }
    }
    chars.len().saturating_sub(1)
}

fn is_sentence_end_char(ch: char) -> bool {
    matches!(ch, '.' | '!' | '?')
}

fn flat_to_rowcol(lines: &[String], offset: usize) -> (usize, usize) {
    let mut remaining = offset;
    for (r, line) in lines.iter().enumerate() {
        let line_len = line.len() + 1; // +1 for newline
        if remaining < line_len {
            return (r, remaining);
        }
        remaining -= line_len;
    }
    // Past end — return last position
    let last = lines.len().saturating_sub(1);
    (
        last,
        lines.get(last).map_or(0, |l| l.len().saturating_sub(1)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(s: &str) -> Vec<String> {
        s.lines().map(|l| l.to_string()).collect()
    }

    // ========================
    // Word objects
    // ========================

    #[test]
    fn test_iw_middle_of_word() {
        let l = lines("hello world");
        let r = word_object(&l, 0, 1, TextObjectKind::Inner).unwrap();
        assert_eq!(
            r,
            TextRange {
                start_row: 0,
                start_col: 0,
                end_row: 0,
                end_col: 4
            }
        );
    }

    #[test]
    fn test_iw_start_of_word() {
        let l = lines("hello world");
        let r = word_object(&l, 0, 0, TextObjectKind::Inner).unwrap();
        assert_eq!(
            r,
            TextRange {
                start_row: 0,
                start_col: 0,
                end_row: 0,
                end_col: 4
            }
        );
    }

    #[test]
    fn test_iw_end_of_word() {
        let l = lines("hello world");
        let r = word_object(&l, 0, 4, TextObjectKind::Inner).unwrap();
        assert_eq!(
            r,
            TextRange {
                start_row: 0,
                start_col: 0,
                end_row: 0,
                end_col: 4
            }
        );
    }

    #[test]
    fn test_aw_includes_trailing_space() {
        let l = lines("hello world");
        let r = word_object(&l, 0, 1, TextObjectKind::Around).unwrap();
        // "hello " (includes trailing space)
        assert_eq!(
            r,
            TextRange {
                start_row: 0,
                start_col: 0,
                end_row: 0,
                end_col: 5
            }
        );
    }

    #[test]
    fn test_aw_last_word_includes_leading_space() {
        let l = lines("hello world");
        let r = word_object(&l, 0, 7, TextObjectKind::Around).unwrap();
        // " world" (includes leading space since no trailing)
        assert_eq!(
            r,
            TextRange {
                start_row: 0,
                start_col: 5,
                end_row: 0,
                end_col: 10
            }
        );
    }

    // ========================
    // Quote objects
    // ========================

    #[test]
    fn test_inner_double_quote() {
        let l = lines(r#"say "hello" now"#);
        // Cursor on 'h' at col 5
        let r = delimited_object(&l, 0, 5, '"', '"', TextObjectKind::Inner).unwrap();
        assert_eq!(
            r,
            TextRange {
                start_row: 0,
                start_col: 5,
                end_row: 0,
                end_col: 9
            }
        );
    }

    #[test]
    fn test_around_double_quote() {
        let l = lines(r#"say "hello" now"#);
        let r = delimited_object(&l, 0, 5, '"', '"', TextObjectKind::Around).unwrap();
        assert_eq!(
            r,
            TextRange {
                start_row: 0,
                start_col: 4,
                end_row: 0,
                end_col: 10
            }
        );
    }

    #[test]
    fn test_inner_single_quote() {
        let l = lines("say 'hello' now");
        let r = delimited_object(&l, 0, 5, '\'', '\'', TextObjectKind::Inner).unwrap();
        assert_eq!(
            r,
            TextRange {
                start_row: 0,
                start_col: 5,
                end_row: 0,
                end_col: 9
            }
        );
    }

    // ========================
    // Bracket objects
    // ========================

    #[test]
    fn test_inner_parens() {
        let l = lines("fn(body)");
        let r = delimited_object(&l, 0, 3, '(', ')', TextObjectKind::Inner).unwrap();
        assert_eq!(
            r,
            TextRange {
                start_row: 0,
                start_col: 3,
                end_row: 0,
                end_col: 6
            }
        );
    }

    #[test]
    fn test_around_parens() {
        let l = lines("fn(body)");
        let r = delimited_object(&l, 0, 3, '(', ')', TextObjectKind::Around).unwrap();
        assert_eq!(
            r,
            TextRange {
                start_row: 0,
                start_col: 2,
                end_row: 0,
                end_col: 7
            }
        );
    }

    #[test]
    fn test_inner_braces() {
        let l = lines("fn() { body }");
        let r = delimited_object(&l, 0, 7, '{', '}', TextObjectKind::Inner).unwrap();
        assert_eq!(
            r,
            TextRange {
                start_row: 0,
                start_col: 6,
                end_row: 0,
                end_col: 11
            }
        );
    }

    #[test]
    fn test_nested_brackets() {
        let l = lines("(a (b) c)");
        // Cursor on 'b' at col 4 — should find inner parens (b)
        let r = delimited_object(&l, 0, 4, '(', ')', TextObjectKind::Inner).unwrap();
        assert_eq!(
            r,
            TextRange {
                start_row: 0,
                start_col: 4,
                end_row: 0,
                end_col: 4
            }
        );
    }

    #[test]
    fn test_no_matching_delimiters() {
        let l = lines("hello world");
        let r = delimited_object(&l, 0, 3, '"', '"', TextObjectKind::Inner);
        assert_eq!(r, None);
    }

    #[test]
    fn test_empty_delimiters() {
        let l = lines(r#""""#);
        // Two adjacent quotes — cursor at col 1 (between them)
        let r = delimited_object(&l, 0, 1, '"', '"', TextObjectKind::Inner);
        // Inner of empty quotes — start > end is degenerate
        assert!(r.is_some());
    }

    // ========================
    // Paragraph objects
    // ========================

    #[test]
    fn test_inner_paragraph() {
        let l = lines("line1\nline2\n\nline4");
        let r = paragraph_object(&l, 0, 0, TextObjectKind::Inner).unwrap();
        assert_eq!(r.start_row, 0);
        assert_eq!(r.end_row, 1);
    }

    #[test]
    fn test_around_paragraph() {
        let l = lines("line1\nline2\n\nline4");
        let r = paragraph_object(&l, 0, 0, TextObjectKind::Around).unwrap();
        assert_eq!(r.start_row, 0);
        // Around includes trailing blank line
        assert_eq!(r.end_row, 2);
    }

    // ========================
    // Multiline bracket objects
    // ========================

    #[test]
    fn test_multiline_braces() {
        let l = lines("fn() {\n  body\n}");
        // Cursor on "body" at row 1, col 2
        let r = delimited_object(&l, 1, 2, '{', '}', TextObjectKind::Inner).unwrap();
        assert_eq!(r.start_row, 0);
        assert_eq!(r.start_col, 6); // after '{'
        assert_eq!(r.end_row, 2);
        assert_eq!(r.end_col, 0); // 0 because '}' is at col 0, inner is col before
    }
}

//! Clipboard utilities.
//!
//! Provides OSC 52-based clipboard write that works over SSH and in all
//! terminals that support the sequence (Ghostty, kitty, iTerm2, tmux, etc.)

/// Copy text to the system clipboard via OSC 52 escape sequence.
///
/// Writes `\x1b]52;c;{base64}\x07` directly to stdout. Zero dependencies —
/// hand-rolled base64 keeps this usable in any terminal context.
pub(crate) fn osc52_copy(text: &str) {
    use std::io::Write;
    let seq = encode_osc52(text);
    let _ = std::io::stdout().write_all(seq.as_bytes());
    let _ = std::io::stdout().flush();
}

/// Pure OSC52 encoder, retained separately so UUID-copy behavior is testable
/// without requiring a real terminal or host clipboard.
pub(crate) fn encode_osc52(text: &str) -> String {
    const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    let input = text.as_bytes();
    let mut encoded = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        encoded.push(B64[((n >> 18) & 0x3F) as usize] as char);
        encoded.push(B64[((n >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            encoded.push(B64[((n >> 6) & 0x3F) as usize] as char);
        } else {
            encoded.push('=');
        }
        if chunk.len() > 2 {
            encoded.push(B64[(n & 0x3F) as usize] as char);
        } else {
            encoded.push('=');
        }
    }

    format!("\x1b]52;c;{}\x07", encoded)
}

#[cfg(test)]
mod tests {
    use super::encode_osc52;

    #[test]
    fn t28_encode_osc52_frames_the_complete_session_uuid_payload() {
        let uuid = "123e4567-e89b-12d3-a456-426614174000";
        let encoded = encode_osc52(uuid);
        assert!(encoded.starts_with("\x1b]52;c;"));
        assert!(encoded.ends_with('\x07'));
        assert!(encoded.contains("MTIzZTQ1NjctZTg5Yi0xMmQzLWE0NTYtNDI2NjE0MTc0MDAw"));
    }
}

// ── Image / text clipboard reading ──────────────────────────────────────

/// Result of reading the OS clipboard.
pub enum ClipboardContent {
    /// Image was found and saved to disk. `reference` is the `@path ` string to insert.
    Image {
        #[allow(dead_code)]
        path: std::path::PathBuf,
        reference: String,
    },
    /// Plain text was found.
    Text(String),
    /// Clipboard was empty or unreadable.
    Empty,
}

thread_local! {
    static CLIPBOARD: std::cell::RefCell<Option<arboard::Clipboard>> = std::cell::RefCell::new(None);
}

/// Read the OS clipboard, trying multiple backends.
///
/// Strategy:
/// 1. Try `arboard` native image reading (works on most systems)
/// 2. If that fails on X11, fall back to `xclip -selection clipboard -t image/png -o`
///    (handles Flameshot and other apps that set image/png but not the BMP format arboard expects)
/// 3. Try `arboard` text reading
/// 4. If that fails, fall back to `xclip -selection clipboard -o` for text
/// 5. Return `Empty` if nothing found
pub fn read_clipboard(paste_dir: &std::path::Path) -> ClipboardContent {
    let has_image = CLIPBOARD.with(|cb| {
        let mut cb_borrow = cb.borrow_mut();
        if cb_borrow.is_none() {
            *cb_borrow = arboard::Clipboard::new().ok();
        }
        if let Some(ref mut clipboard) = *cb_borrow {
            if let Ok(img_data) = clipboard.get_image() {
                let id = uuid::Uuid::new_v4();
                let path = paste_dir.join(format!("{id}.png"));

                if image::save_buffer(
                    &path,
                    &img_data.bytes,
                    img_data.width as u32,
                    img_data.height as u32,
                    image::ColorType::Rgba8,
                )
                .is_ok()
                {
                    let reference = format!("@{} ", path.display());
                    return Some(ClipboardContent::Image { path, reference });
                }
            }
        }
        None
    });

    if let Some(res) = has_image {
        return res;
    }

    // arboard image failed — try xclip image (handles Flameshot on X11)
    if std::env::var("DISPLAY").is_ok() {
        let result = try_xclip_image(paste_dir);
        if !matches!(result, ClipboardContent::Empty) {
            return result;
        }
    }

    // Fall back to text via arboard
    let has_text = CLIPBOARD.with(|cb| {
        let mut cb_borrow = cb.borrow_mut();
        if cb_borrow.is_none() {
            *cb_borrow = arboard::Clipboard::new().ok();
        }
        if let Some(ref mut clipboard) = *cb_borrow {
            if let Ok(text) = clipboard.get_text() {
                if !text.is_empty() {
                    return Some(ClipboardContent::Text(text));
                }
            }
        }
        None
    });

    if let Some(res) = has_text {
        return res;
    }

    // Last resort: xclip fallback
    read_clipboard_xclip(paste_dir)
}

/// Try to read image from X11 clipboard via `xclip`, supporting PNG, JPEG, and BMP.
fn try_xclip_image(paste_dir: &std::path::Path) -> ClipboardContent {
    use std::process::Command;

    // Get list of targets to see what image formats are available
    let targets_output = Command::new("xclip")
        .args(["-selection", "clipboard", "-t", "TARGETS", "-o"])
        .output();

    let mut targets = match targets_output {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|s| s.trim().to_string())
            .collect::<Vec<_>>(),
        _ => Vec::new(),
    };

    if targets.is_empty() {
        // Fallback: if TARGETS query failed, populate with default targets to check
        targets = vec![
            "image/png".to_string(),
            "image/jpeg".to_string(),
            "image/jpg".to_string(),
            "image/bmp".to_string(),
        ];
    }

    // Try formats in order of preference
    for (mime, format) in [
        ("image/png", image::ImageFormat::Png),
        ("image/jpeg", image::ImageFormat::Jpeg),
        ("image/jpg", image::ImageFormat::Jpeg),
        ("image/bmp", image::ImageFormat::Bmp),
    ] {
        if targets.iter().any(|t| t == mime) {
            let output = Command::new("xclip")
                .args(["-selection", "clipboard", "-t", mime, "-o"])
                .output();
            if let Ok(out) = output {
                if out.status.success() && out.stdout.len() > 8 {
                    let id = uuid::Uuid::new_v4();
                    let path = paste_dir.join(format!("{id}.png"));

                    if mime == "image/png" {
                        // For PNG, validate magic bytes and write directly to avoid re-encoding
                        if out.stdout.starts_with(&[0x89, b'P', b'N', b'G']) {
                            if std::fs::write(&path, &out.stdout).is_ok() {
                                let reference = format!("@{} ", path.display());
                                return ClipboardContent::Image { path, reference };
                            }
                        }
                    } else {
                        // For other formats (JPEG/BMP), decode and save/convert to PNG
                        if let Ok(img) = image::load_from_memory_with_format(&out.stdout, format) {
                            if img.save(&path).is_ok() {
                                let reference = format!("@{} ", path.display());
                                return ClipboardContent::Image { path, reference };
                            }
                        }
                    }
                }
            }
        }
    }

    ClipboardContent::Empty
}

/// Try to read text from X11 clipboard via `xclip`.
fn try_xclip_text() -> ClipboardContent {
    use std::process::Command;

    // Get list of targets to see if there is actually text available
    let targets_output = Command::new("xclip")
        .args(["-selection", "clipboard", "-t", "TARGETS", "-o"])
        .output();

    let targets = match targets_output {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|s| s.trim().to_string())
            .collect::<Vec<_>>(),
        _ => Vec::new(),
    };

    // If targets are advertised but none are text, do not read as text
    if !targets.is_empty() {
        let has_text = targets
            .iter()
            .any(|t| t == "UTF8_STRING" || t == "TEXT" || t == "STRING");
        if !has_text {
            return ClipboardContent::Empty;
        }
    }

    // Try text targets in order of preference
    for target in ["UTF8_STRING", "TEXT", "STRING"] {
        if targets.is_empty() || targets.iter().any(|t| t == target) {
            let output = Command::new("xclip")
                .args(["-selection", "clipboard", "-t", target, "-o"])
                .output();
            if let Ok(out) = output {
                if out.status.success() {
                    let text = String::from_utf8_lossy(&out.stdout).into_owned();
                    if !text.is_empty() {
                        return ClipboardContent::Text(text);
                    }
                }
            }
        }
    }

    // Ultimate fallback if targets was empty (we couldn't check)
    if targets.is_empty() {
        let output = Command::new("xclip")
            .args(["-selection", "clipboard", "-o"])
            .output();

        if let Ok(out) = output {
            if out.status.success() {
                let text = String::from_utf8_lossy(&out.stdout).into_owned();
                if !text.is_empty() {
                    return ClipboardContent::Text(text);
                }
            }
        }
    }

    ClipboardContent::Empty
}

/// Full fallback when arboard::Clipboard::new() itself fails.
fn read_clipboard_xclip(paste_dir: &std::path::Path) -> ClipboardContent {
    let result = try_xclip_image(paste_dir);
    if !matches!(result, ClipboardContent::Empty) {
        return result;
    }
    try_xclip_text()
}

/// Remove all `.png` files from the paste directory.
/// Called on TUI shutdown to avoid stale temp files.
pub fn cleanup_paste_dir(paste_dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(paste_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "png") {
            let _ = std::fs::remove_file(path);
        }
    }
}

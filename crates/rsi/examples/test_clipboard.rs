//! Clipboard diagnostic tool.
//!
//! Run with: `cargo run -p flywheel --example test_clipboard`
//!
//! Copy a screenshot first (e.g., Flameshot), then run this to see what's
//! actually on the clipboard and whether our fallback chain works.

use std::process::Command;

fn main() {
    println!("=== Clipboard Diagnostics ===\n");
    println!("DISPLAY={:?}", std::env::var("DISPLAY"));
    println!("WAYLAND_DISPLAY={:?}", std::env::var("WAYLAND_DISPLAY"));
    println!("XDG_SESSION_TYPE={:?}", std::env::var("XDG_SESSION_TYPE"));

    // Check xclip availability
    let has_xclip = Command::new("which")
        .arg("xclip")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    println!("\nxclip installed: {has_xclip}");

    // Show clipboard TARGETS via xclip
    if has_xclip {
        println!("\n--- xclip TARGETS (clipboard selection) ---");
        match Command::new("xclip")
            .args(["-selection", "clipboard", "-t", "TARGETS", "-o"])
            .output()
        {
            Ok(out) if out.status.success() => {
                let targets = String::from_utf8_lossy(&out.stdout);
                for target in targets.lines() {
                    println!("  {target}");
                }
                let has_image_png = targets.lines().any(|t| t == "image/png");
                println!("\n  image/png available: {has_image_png}");

                if has_image_png {
                    match Command::new("xclip")
                        .args(["-selection", "clipboard", "-t", "image/png", "-o"])
                        .output()
                    {
                        Ok(img_out) if img_out.status.success() => {
                            println!("  image/png size: {} bytes", img_out.stdout.len());
                            if img_out.stdout.starts_with(&[0x89, b'P', b'N', b'G']) {
                                println!("  valid PNG header: yes");
                            }
                        }
                        Ok(img_out) => println!(
                            "  xclip image/png failed: {}",
                            String::from_utf8_lossy(&img_out.stderr)
                        ),
                        Err(e) => println!("  xclip image/png error: {e}"),
                    }
                }
            }
            Ok(out) => println!(
                "  xclip TARGETS failed: {}",
                String::from_utf8_lossy(&out.stderr)
            ),
            Err(e) => println!("  xclip error: {e}"),
        }
    }

    // Try arboard
    println!("\n--- arboard ---");
    match arboard::Clipboard::new() {
        Ok(mut cb) => {
            println!("  Clipboard::new(): ok");
            match cb.get_image() {
                Ok(img) => println!(
                    "  get_image: ok ({}x{}, {} bytes)",
                    img.width,
                    img.height,
                    img.bytes.len()
                ),
                Err(e) => println!("  get_image: FAILED ({e})"),
            }
            match cb.get_text() {
                Ok(t) if !t.is_empty() => {
                    println!(
                        "  get_text: ok ({} chars): {:?}",
                        t.len(),
                        &t[..t.len().min(80)]
                    )
                }
                Ok(_) => println!("  get_text: empty"),
                Err(e) => println!("  get_text: FAILED ({e})"),
            }
        }
        Err(e) => println!("  Clipboard::new() FAILED: {e}"),
    }

    // Test the full read_clipboard function with xclip fallback
    println!("\n--- rsi::clipboard::read_clipboard() ---");
    let paste_dir = std::env::temp_dir().join("flywheel-clipboard-test");
    let _ = std::fs::create_dir_all(&paste_dir);

    match rsi::clipboard::read_clipboard(&paste_dir) {
        rsi::clipboard::ClipboardContent::Image { path, reference } => {
            println!("  Result: IMAGE");
            println!("  Path: {}", path.display());
            println!("  Reference: {reference}");
            let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            println!("  File size: {size} bytes");
            let _ = std::fs::remove_file(&path);
        }
        rsi::clipboard::ClipboardContent::Text(text) => {
            println!(
                "  Result: TEXT ({} chars): {:?}",
                text.len(),
                &text[..text.len().min(80)]
            );
        }
        rsi::clipboard::ClipboardContent::Empty => {
            println!("  Result: EMPTY (nothing readable on clipboard)");
        }
    }
    let _ = std::fs::remove_dir(&paste_dir);

    println!("\n=== Done ===");
}

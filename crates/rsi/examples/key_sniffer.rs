//! Key event sniffer — shows exactly what crossterm receives from your terminal.
//!
//! Run with: `cargo run -p flywheel --example key_sniffer`
//!
//! Press keys to see their crossterm representation. Press Ctrl+C to quit.
//! Use this to verify Ctrl+V reaches the app from Ghostty.

use crossterm::{
    event::{
        self, Event, KeyCode, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags,
        PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use std::io::stdout;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    enable_raw_mode()?;
    execute!(
        stdout(),
        EnterAlternateScreen,
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::REPORT_EVENT_TYPES),
    )?;

    println!("Key sniffer active. Press keys to see events. Ctrl+C to quit.\r");
    println!("Looking for: KeyCode::Char('v') with CONTROL modifier\r");
    println!("---\r");

    loop {
        if event::poll(std::time::Duration::from_millis(100))? {
            match event::read()? {
                Event::Key(key) => {
                    let is_ctrl_v = key.modifiers.contains(KeyModifiers::CONTROL)
                        && matches!(key.code, KeyCode::Char('v') | KeyCode::Char('V'));

                    println!(
                        "{} code={:?} mod={:?} kind={:?}{}",
                        if is_ctrl_v { ">>> CTRL+V" } else { "   " },
                        key.code,
                        key.modifiers,
                        key.kind,
                        if is_ctrl_v { " <<<" } else { "" },
                    );
                    print!("\r");

                    // Quit on Ctrl+C
                    if key.modifiers.contains(KeyModifiers::CONTROL)
                        && key.code == KeyCode::Char('c')
                        && key.kind == KeyEventKind::Press
                    {
                        break;
                    }
                }
                Event::Paste(text) => {
                    println!(
                        "    PASTE EVENT: {:?} ({} chars)\r",
                        &text[..text.len().min(80)],
                        text.len()
                    );
                }
                other => {
                    println!("    OTHER: {:?}\r", other);
                }
            }
        }
    }

    execute!(stdout(), PopKeyboardEnhancementFlags, LeaveAlternateScreen,)?;
    disable_raw_mode()?;
    Ok(())
}

//! Embedded terminal emulator — PTY management and VT100 screen state.
//!
//! Uses `portable-pty` for cross-platform PTY allocation and `vt100` for
//! ANSI sequence parsing. The shell session persists across overlay toggles.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::io::Write;

/// Embedded terminal state — owns the PTY connection and VT100 screen.
pub struct EmbeddedTerminal {
    /// VT100 screen parser — receives raw PTY output and maintains cell grid.
    parser: vt100::Parser,
    /// Writer to send keystrokes to the shell's stdin.
    writer: Box<dyn Write + Send>,
    /// PTY master handle for resize operations.
    pty_master: Box<dyn portable_pty::MasterPty + Send>,
    /// Child process handle for lifecycle management.
    child: Box<dyn portable_pty::Child + Send + Sync>,
    /// Whether the shell process is still alive.
    pub alive: bool,
    /// Current terminal dimensions.
    rows: u16,
    cols: u16,
}

impl EmbeddedTerminal {
    /// Spawn a new shell in a PTY.
    ///
    /// Uses `$SHELL` or falls back to `/bin/sh`. Returns the terminal state
    /// and an unbounded receiver for PTY output bytes. The caller must poll
    /// the receiver and feed bytes to [`process_bytes`].
    pub fn spawn(
        rows: u16,
        cols: u16,
    ) -> Result<(Self, tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>), String> {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());

        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| format!("PTY open failed: {e}"))?;

        let cmd = CommandBuilder::new(&shell);
        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| format!("Shell spawn failed: {e}"))?;
        // Critical: drop slave so master reader sees EOF when child exits
        drop(pair.slave);

        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| format!("PTY reader clone failed: {e}"))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| format!("PTY writer failed: {e}"))?;

        // Dedicated reader thread — blocking I/O, sends bytes over channel
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        std::thread::Builder::new()
            .name("pty-reader".into())
            .spawn(move || {
                let mut reader = reader;
                let mut buf = [0u8; 4096];
                loop {
                    match std::io::Read::read(&mut reader, &mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            if tx.send(buf[..n].to_vec()).is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
            })
            .map_err(|e| format!("Reader thread spawn failed: {e}"))?;

        Ok((
            Self {
                parser: vt100::Parser::new(rows, cols, 1000),
                writer,
                pty_master: pair.master,
                child,
                alive: true,
                rows,
                cols,
            },
            rx,
        ))
    }

    /// Feed raw PTY output bytes into the VT100 parser.
    pub fn process_bytes(&mut self, data: &[u8]) {
        self.parser.process(data);
    }

    /// Write bytes to the PTY (send keystrokes to shell).
    pub fn write_input(&mut self, data: &[u8]) -> std::io::Result<()> {
        self.writer.write_all(data)?;
        self.writer.flush()
    }

    /// Forward pasted clipboard text to the PTY.
    ///
    /// If the inner program enabled bracketed paste mode (DECSET 2004),
    /// wrap the text in `\e[200~ … \e[201~` so the program treats it as a
    /// single paste (e.g. shells/editors won't auto-execute on embedded
    /// newlines). Otherwise send the raw text, translating newlines to `\r`
    /// to match what the Enter key sends.
    pub fn paste(&mut self, text: &str) -> std::io::Result<()> {
        let payload = build_paste_payload(text, self.parser.screen().bracketed_paste());
        self.writer.write_all(&payload)?;
        self.writer.flush()
    }

    /// Get the current VT100 screen state for rendering.
    pub fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }

    /// Check if the shell process is still alive (non-blocking).
    pub fn check_alive(&mut self) -> bool {
        if self.alive {
            if let Ok(Some(_status)) = self.child.try_wait() {
                self.alive = false;
            }
        }
        self.alive
    }

    /// Resize the terminal dimensions. Notifies shell via TIOCSWINSZ.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        if rows == self.rows && cols == self.cols {
            return;
        }
        self.rows = rows;
        self.cols = cols;
        self.parser.set_size(rows, cols);
        let _ = self.pty_master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
    }

    /// Kill the shell process.
    pub fn kill(&mut self) {
        if self.alive {
            let _ = self.child.kill();
            self.alive = false;
        }
    }
}

impl Drop for EmbeddedTerminal {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Convert a crossterm KeyEvent to bytes suitable for writing to a PTY.
pub fn crossterm_key_to_bytes(key: KeyEvent) -> Vec<u8> {
    // Handle Ctrl+key combinations
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        match key.code {
            KeyCode::Char(c) if c.is_ascii_lowercase() => {
                return vec![(c as u8) - b'a' + 1];
            }
            KeyCode::Char(c) if c.is_ascii_uppercase() => {
                return vec![(c.to_ascii_lowercase() as u8) - b'a' + 1];
            }
            _ => {}
        }
    }

    // Handle Alt+key (send ESC prefix)
    if key.modifiers.contains(KeyModifiers::ALT) {
        if let KeyCode::Char(c) = key.code {
            let mut buf = vec![0x1b]; // ESC
            let mut char_buf = [0u8; 4];
            let s = c.encode_utf8(&mut char_buf);
            buf.extend_from_slice(s.as_bytes());
            return buf;
        }
    }

    match key.code {
        KeyCode::Char(c) => {
            let mut buf = [0u8; 4];
            let s = c.encode_utf8(&mut buf);
            s.as_bytes().to_vec()
        }
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => b"\x1b[A".to_vec(),
        KeyCode::Down => b"\x1b[B".to_vec(),
        KeyCode::Right => b"\x1b[C".to_vec(),
        KeyCode::Left => b"\x1b[D".to_vec(),
        KeyCode::Home => b"\x1b[H".to_vec(),
        KeyCode::End => b"\x1b[F".to_vec(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        KeyCode::PageUp => b"\x1b[5~".to_vec(),
        KeyCode::PageDown => b"\x1b[6~".to_vec(),
        KeyCode::Insert => b"\x1b[2~".to_vec(),
        KeyCode::F(n) => match n {
            1 => b"\x1bOP".to_vec(),
            2 => b"\x1bOQ".to_vec(),
            3 => b"\x1bOR".to_vec(),
            4 => b"\x1bOS".to_vec(),
            5 => b"\x1b[15~".to_vec(),
            6 => b"\x1b[17~".to_vec(),
            7 => b"\x1b[18~".to_vec(),
            8 => b"\x1b[19~".to_vec(),
            9 => b"\x1b[20~".to_vec(),
            10 => b"\x1b[21~".to_vec(),
            11 => b"\x1b[23~".to_vec(),
            12 => b"\x1b[24~".to_vec(),
            _ => vec![],
        },
        _ => vec![],
    }
}

/// Build the byte payload to forward pasted clipboard text to a PTY.
///
/// When `bracketed` is true (inner program enabled DECSET 2004), the text is
/// wrapped in `\e[200~ … \e[201~` so it is treated as a single paste. When
/// false, newlines are normalized to `\r` to match the Enter key.
pub fn build_paste_payload(text: &str, bracketed: bool) -> Vec<u8> {
    if bracketed {
        let mut buf = Vec::with_capacity(text.len() + 12);
        buf.extend_from_slice(b"\x1b[200~");
        buf.extend_from_slice(text.as_bytes());
        buf.extend_from_slice(b"\x1b[201~");
        buf
    } else {
        text.replace("\r\n", "\r").replace('\n', "\r").into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::build_paste_payload;

    #[test]
    fn bracketed_paste_wraps_text_verbatim() {
        let out = build_paste_payload("echo hi\nls\n", true);
        assert_eq!(out, b"\x1b[200~echo hi\nls\n\x1b[201~");
    }

    #[test]
    fn unbracketed_paste_normalizes_newlines_to_cr() {
        assert_eq!(build_paste_payload("a\nb", false), b"a\rb");
        assert_eq!(build_paste_payload("a\r\nb", false), b"a\rb");
        assert_eq!(build_paste_payload("a\rb", false), b"a\rb");
    }

    #[test]
    fn unbracketed_paste_without_newline_is_unchanged() {
        assert_eq!(build_paste_payload("hello", false), b"hello");
    }
}

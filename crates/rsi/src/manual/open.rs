//! Render the installed binary's manual into the user data directory and open it.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::app::App;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManualMode {
    Browser,
    Pager,
    Pdf,
}

#[derive(Debug)]
pub struct ManualPaths {
    pub html: PathBuf,
    pub markdown: PathBuf,
    pub pdf: PathBuf,
}

/// Write both formats from the current binary's registries.
///
/// # Errors
///
/// Returns an I/O error if the manual directory or either rendered file cannot be written.
pub fn write_manual_to(data_dir: &Path) -> std::io::Result<ManualPaths> {
    let directory = data_dir.join("manual");
    std::fs::create_dir_all(&directory)?;
    let paths = ManualPaths {
        html: directory.join("rsi-manual.html"),
        markdown: directory.join("rsi-manual.md"),
        pdf: directory.join("rsi-manual.pdf"),
    };
    std::fs::write(&paths.html, super::render_html())?;
    std::fs::write(&paths.markdown, super::render_markdown())?;
    Ok(paths)
}

fn file_url(path: &Path) -> String {
    let mut url = String::from("file://");
    for byte in path.to_string_lossy().bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_' | b'.' | b'~') {
            url.push(char::from(byte));
        } else {
            let _ = write!(url, "%{byte:02X}");
        }
    }
    url
}

pub fn open_manual(app: &mut App, mode: ManualMode) {
    let paths = match write_manual_to(&rsi_common::identity::data_dir()) {
        Ok(paths) => paths,
        Err(error) => {
            app.notify_error(format!("Could not write manual: {error}"));
            return;
        }
    };
    match mode {
        ManualMode::Browser => open_browser(app, &paths.html),
        ManualMode::Pager => {
            app.pending_external = Some(crate::app::ExternalRequest::Pager(paths.markdown))
        }
        ManualMode::Pdf => {
            match Command::new("pandoc")
                .arg(&paths.markdown)
                .arg("-o")
                .arg(&paths.pdf)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
            {
                Ok(status) if status.success() => open_browser(app, &paths.pdf),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    open_browser(app, &paths.html);
                    app.notify("pandoc not found — use the browser's Print → Save as PDF");
                }
                Ok(status) => app.notify_error(format!("pandoc exited with {status}")),
                Err(error) => app.notify_error(format!("pandoc failed: {error}")),
            }
        }
    }
}

fn open_browser(app: &mut App, path: &Path) {
    if let Err(error) = crate::browser::open_url(&file_url(path)) {
        app.notify_error(format!("Could not open {}: {error}", path.display()));
    }
}

/// Read `$PAGER` as an executable and its arguments, appending the manual path.
pub fn pager_command(path: &Path) -> (String, Vec<std::ffi::OsString>) {
    let setting = std::env::var("PAGER").unwrap_or_else(|_| "less -R".to_string());
    let mut words = setting.split_whitespace();
    let program = words.next().unwrap_or("less").to_string();
    let mut args: Vec<std::ffi::OsString> = words.map(std::ffi::OsString::from).collect();
    args.push(path.as_os_str().to_owned());
    (program, args)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn open_manual_writes_rendered_html_to_data_dir() {
        let temp = tempfile::tempdir().expect("temp data dir");
        let paths = write_manual_to(temp.path()).expect("write manual");
        assert_eq!(
            std::fs::read_to_string(paths.html).unwrap(),
            super::super::render_html()
        );
        assert_eq!(
            std::fs::read_to_string(paths.markdown).unwrap(),
            super::super::render_markdown()
        );
    }
}

//! Streaming extraction of `git archive` output (ustar plus pax path headers)
//! into a private directory. Only regular files are written, like the legacy
//! exporter; an absolute or parent-escaping member is refused, never clamped.
use std::fs::File;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};

const BLOCK: usize = 512;
const PAX_LIMIT: usize = 1024 * 1024;

fn bad(message: impl Into<String>) -> io::Error {
    io::Error::other(message.into())
}

enum State {
    Header,
    /// Skip `left` content bytes then `pad` padding bytes.
    Skip {
        left: usize,
        pad: usize,
    },
    Pax {
        left: usize,
        pad: usize,
        data: Vec<u8>,
        global: bool,
    },
    File {
        left: usize,
        pad: usize,
        out: File,
    },
    Padding(usize),
    End,
}

pub struct Extractor {
    root: PathBuf,
    buffer: Vec<u8>,
    state: State,
    pax_path: Option<String>,
    zero_blocks: u8,
    files: usize,
}

fn octal(field: &[u8]) -> io::Result<usize> {
    if field.first().is_some_and(|b| b & 0x80 != 0) {
        // GNU base-256 size for members above 8 GiB.
        let mut value: u64 = u64::from(field[0] & 0x7f);
        for byte in &field[1..] {
            value = value
                .checked_mul(256)
                .and_then(|v| v.checked_add(u64::from(*byte)))
                .ok_or_else(|| bad("tar member size overflows"))?;
        }
        return usize::try_from(value).map_err(|_| bad("tar member size overflows"));
    }
    let text: String = field
        .iter()
        .take_while(|b| **b != 0)
        .map(|b| char::from(*b))
        .collect();
    let text = text.trim();
    if text.is_empty() {
        return Ok(0);
    }
    usize::from_str_radix(text, 8).map_err(|_| bad("malformed tar numeric field"))
}
fn padding(size: usize) -> usize {
    (BLOCK - size % BLOCK) % BLOCK
}
fn cstr(field: &[u8]) -> &[u8] {
    let end = field.iter().position(|b| *b == 0).unwrap_or(field.len());
    &field[..end]
}

impl Extractor {
    pub fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            buffer: Vec::new(),
            state: State::Header,
            pax_path: None,
            zero_blocks: 0,
            files: 0,
        }
    }
    pub fn files(&self) -> usize {
        self.files
    }
    /// True when the archive's end marker was read.
    pub fn complete(&self) -> bool {
        matches!(self.state, State::End)
    }
    pub fn feed(&mut self, bytes: &[u8]) -> io::Result<()> {
        let mut buffer = std::mem::take(&mut self.buffer);
        buffer.extend_from_slice(bytes);
        let mut offset = 0;
        let result = loop {
            let state = std::mem::replace(&mut self.state, State::End);
            match self.advance(state, &buffer[offset..]) {
                Ok((next, used)) => {
                    self.state = next;
                    if used == 0 {
                        break Ok(());
                    }
                    offset += used;
                }
                Err(error) => break Err(error),
            }
        };
        buffer.drain(..offset);
        self.buffer = buffer;
        result
    }
    /// Consume some of `data`; returns the next state and bytes used (0 = need more).
    fn advance(&mut self, state: State, data: &[u8]) -> io::Result<(State, usize)> {
        match state {
            State::End => Ok((State::End, 0)),
            State::Header => {
                if data.len() < BLOCK {
                    return Ok((State::Header, 0));
                }
                self.header(&data[..BLOCK]).map(|state| (state, BLOCK))
            }
            State::Skip { left, pad } => {
                let take = left.min(data.len());
                if take == 0 && left > 0 {
                    return Ok((State::Skip { left, pad }, 0));
                }
                let left = left - take;
                Ok(if left == 0 {
                    (Self::after(pad), take)
                } else {
                    (State::Skip { left, pad }, take)
                })
            }
            State::Padding(pad) => {
                let take = pad.min(data.len());
                if take == 0 {
                    return Ok((State::Padding(pad), 0));
                }
                Ok(if take == pad {
                    (State::Header, take)
                } else {
                    (State::Padding(pad - take), take)
                })
            }
            State::Pax {
                left,
                pad,
                data: mut record,
                global,
            } => {
                let take = left.min(data.len());
                if take == 0 && left > 0 {
                    return Ok((
                        State::Pax {
                            left,
                            pad,
                            data: record,
                            global,
                        },
                        0,
                    ));
                }
                record.extend_from_slice(&data[..take]);
                if left == take {
                    if !global {
                        self.pax_path = pax_path(&record)?;
                    }
                    return Ok((Self::after(pad), take));
                }
                Ok((
                    State::Pax {
                        left: left - take,
                        pad,
                        data: record,
                        global,
                    },
                    take,
                ))
            }
            State::File { left, pad, mut out } => {
                let take = left.min(data.len());
                if take == 0 && left > 0 {
                    return Ok((State::File { left, pad, out }, 0));
                }
                out.write_all(&data[..take])?;
                if left == take {
                    out.flush()?;
                    return Ok((Self::after(pad), take));
                }
                Ok((
                    State::File {
                        left: left - take,
                        pad,
                        out,
                    },
                    take,
                ))
            }
        }
    }
    fn after(pad: usize) -> State {
        if pad == 0 {
            State::Header
        } else {
            State::Padding(pad)
        }
    }
    fn header(&mut self, block: &[u8]) -> io::Result<State> {
        if block.iter().all(|b| *b == 0) {
            self.zero_blocks += 1;
            return Ok(if self.zero_blocks >= 2 {
                State::End
            } else {
                State::Header
            });
        }
        self.zero_blocks = 0;
        let size = octal(&block[124..136])?;
        let pad = padding(size);
        let kind = block[156];
        match kind {
            b'x' | b'g' => {
                if size > PAX_LIMIT {
                    return Err(bad("oversized pax header in archive"));
                }
                Ok(if size == 0 {
                    Self::after(pad)
                } else {
                    State::Pax {
                        left: size,
                        pad,
                        data: Vec::new(),
                        global: kind == b'g',
                    }
                })
            }
            b'0' | 0 => {
                let name = match self.pax_path.take() {
                    Some(path) => path,
                    None => {
                        let name = String::from_utf8(cstr(&block[0..100]).to_vec())
                            .map_err(|_| bad("non-UTF8 archive member name"))?;
                        let prefix = if &block[257..262] == b"ustar" {
                            String::from_utf8(cstr(&block[345..500]).to_vec())
                                .map_err(|_| bad("non-UTF8 archive member prefix"))?
                        } else {
                            String::new()
                        };
                        if prefix.is_empty() {
                            name
                        } else {
                            format!("{prefix}/{name}")
                        }
                    }
                };
                let relative = Path::new(&name);
                if relative.is_absolute()
                    || relative.components().any(|c| {
                        matches!(
                            c,
                            Component::ParentDir | Component::Prefix(_) | Component::RootDir
                        )
                    })
                {
                    return Err(bad("unsafe path in candidate archive"));
                }
                let destination = self.root.join(relative);
                if let Some(parent) = destination.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let out = File::create(&destination)?;
                self.files += 1;
                Ok(if size == 0 {
                    State::Header
                } else {
                    State::File {
                        left: size,
                        pad,
                        out,
                    }
                })
            }
            _ => {
                self.pax_path = None;
                Ok(if size == 0 {
                    Self::after(pad)
                } else {
                    State::Skip { left: size, pad }
                })
            }
        }
    }
}

/// The `path` record of a pax extended header, if any.
fn pax_path(records: &[u8]) -> io::Result<Option<String>> {
    let mut rest = records;
    let mut path = None;
    while !rest.is_empty() {
        let space = rest
            .iter()
            .position(|b| *b == b' ')
            .ok_or_else(|| bad("malformed pax record"))?;
        let length: usize = std::str::from_utf8(&rest[..space])
            .ok()
            .and_then(|n| n.parse().ok())
            .ok_or_else(|| bad("malformed pax record length"))?;
        if length <= space + 1 || length > rest.len() {
            return Err(bad("malformed pax record length"));
        }
        let record = &rest[space + 1..length];
        let record = record.strip_suffix(b"\n").unwrap_or(record);
        if let Some(value) = record.strip_prefix(b"path=") {
            path = Some(String::from_utf8(value.to_vec()).map_err(|_| bad("non-UTF8 pax path"))?);
        }
        rest = &rest[length..];
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(name: &str, kind: u8, size: usize) -> [u8; BLOCK] {
        let mut block = [0u8; BLOCK];
        block[..name.len().min(100)].copy_from_slice(&name.as_bytes()[..name.len().min(100)]);
        let octal = format!("{size:011o}");
        block[124..135].copy_from_slice(octal.as_bytes());
        block[156] = kind;
        block[257..262].copy_from_slice(b"ustar");
        block
    }
    fn member(archive: &mut Vec<u8>, name: &str, kind: u8, body: &[u8]) {
        archive.extend_from_slice(&header(name, kind, body.len()));
        archive.extend_from_slice(body);
        archive.extend(std::iter::repeat_n(0, padding(body.len())));
    }
    fn record(key: &str, value: &str) -> Vec<u8> {
        let body = format!(" {key}={value}\n");
        let mut length = body.len() + 1;
        while format!("{length}{body}").len() != length {
            length = format!("{length}{body}").len();
        }
        format!("{length}{body}").into_bytes()
    }
    fn finish(archive: &mut Vec<u8>) {
        archive.extend(std::iter::repeat_n(0, BLOCK * 2));
    }

    #[test]
    fn files_directories_pax_paths_and_chunked_input_extract_exactly() {
        let long = format!("{}/leaf.txt", "d".repeat(130));
        let mut archive = Vec::new();
        member(
            &mut archive,
            "pax_global_header",
            b'g',
            &record("comment", "c"),
        );
        member(&mut archive, "crates/", b'5', b"");
        member(&mut archive, "crates/a.rs", b'0', b"alpha\n");
        member(&mut archive, "PaxHeader", b'x', &record("path", &long));
        member(&mut archive, "short", b'0', &vec![7u8; 700]);
        member(&mut archive, "link", b'2', b"");
        member(&mut archive, "empty", b'0', b"");
        finish(&mut archive);
        for chunk in [archive.len(), 1, 97, 512] {
            let temp = tempfile::tempdir().unwrap();
            let mut extractor = Extractor::new(temp.path());
            for piece in archive.chunks(chunk) {
                extractor.feed(piece).unwrap();
            }
            assert!(extractor.complete(), "chunk {chunk}");
            assert_eq!(extractor.files(), 3, "chunk {chunk}");
            assert_eq!(
                std::fs::read(temp.path().join("crates/a.rs")).unwrap(),
                b"alpha\n"
            );
            assert_eq!(
                std::fs::read(temp.path().join(&long)).unwrap(),
                vec![7u8; 700]
            );
            assert_eq!(std::fs::read(temp.path().join("empty")).unwrap(), b"");
            assert!(!temp.path().join("link").exists());
        }
    }

    #[test]
    fn unsafe_members_are_refused_not_clamped() {
        for name in ["../escape", "/abs"] {
            let mut archive = Vec::new();
            member(&mut archive, name, b'0', b"x");
            let temp = tempfile::tempdir().unwrap();
            let error = Extractor::new(temp.path()).feed(&archive).unwrap_err();
            assert!(error.to_string().contains("unsafe path"), "{name}");
        }
    }
}

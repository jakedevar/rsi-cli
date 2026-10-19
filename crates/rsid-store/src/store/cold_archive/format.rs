//! `rsi-cold-rows-v1`: the row codec of a cold-archive chunk, and its digests.
//!
//! A chunk is the rows one table contributes for one session, in archive order.
//! Each row is its columns' values in `columns_json` order; each value is a tag
//! byte followed by its data: `0` NULL, `1` INTEGER (i64 LE), `2` REAL (f64 LE),
//! `3` TEXT (u32 LE length + UTF-8) and `4` BLOB (u32 LE length + bytes). Values
//! are read through `ValueRef`, so `SQLite` storage classes round-trip exactly.
//! `raw_sha256` covers the uncompressed stream; the stream is stored as one zstd
//! frame.

use super::{ColdError, ColdResult};
use rusqlite::types::{Value, ValueRef};
use rusqlite::{Connection, Row};
use sha2::{Digest, Sha256};

/// Name recorded with every chunk (`archive_meta.row_codec`).
pub const ROW_CODEC: &str = "rsi-cold-rows-v1";
/// zstd level of every chunk (one frame per chunk).
pub const ZSTD_LEVEL: i32 = 6;

const TAG_NULL: u8 = 0;
const TAG_INTEGER: u8 = 1;
const TAG_REAL: u8 = 2;
const TAG_TEXT: u8 = 3;
const TAG_BLOB: u8 = 4;

/// Ordered `(name, declared type)` of a table, from `PRAGMA table_info`.
pub type ColumnList = Vec<(String, String)>;

/// The columns of `table` in declaration order.
pub fn table_columns(conn: &Connection, table: &str) -> ColdResult<ColumnList> {
    let mut stmt = conn.prepare("SELECT name, type FROM pragma_table_info(?1) ORDER BY cid")?;
    let rows = stmt.query_map([table], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut columns = Vec::new();
    for row in rows {
        columns.push(row?);
    }
    Ok(columns)
}

/// `columns_json` of a chunk.
pub fn columns_json(columns: &[(String, String)]) -> ColdResult<String> {
    serde_json::to_string(columns).map_err(ColdError::from)
}

/// Parses a chunk's `columns_json`.
pub fn parse_columns_json(raw: &str) -> ColdResult<ColumnList> {
    serde_json::from_str(raw).map_err(|error| {
        ColdError::new(
            "cold_archive_bad_columns",
            format!("columns_json is not an ordered [name, type] list: {error}"),
        )
    })
}

/// The `length()` `SQLite` reports for a stored `content` value: characters up to
/// the first NUL for TEXT, bytes for a BLOB.
fn sqlite_length(value: ValueRef<'_>) -> i64 {
    match value {
        ValueRef::Text(bytes) => {
            let end = bytes
                .iter()
                .position(|byte| *byte == 0)
                .unwrap_or(bytes.len());
            let count = String::from_utf8_lossy(&bytes[..end]).chars().count();
            i64::try_from(count).unwrap_or(i64::MAX)
        }
        ValueRef::Blob(bytes) => i64::try_from(bytes.len()).unwrap_or(i64::MAX),
        _ => 0,
    }
}

/// [`sqlite_length`] of an owned decoded value.
#[must_use]
pub fn sqlite_length_of_value(value: &Value) -> i64 {
    match value {
        Value::Text(text) => {
            let end = text.find('\0').unwrap_or(text.len());
            i64::try_from(text[..end].chars().count()).unwrap_or(i64::MAX)
        }
        Value::Blob(bytes) => i64::try_from(bytes.len()).unwrap_or(i64::MAX),
        _ => 0,
    }
}

/// The summary of an encoded chunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedRows {
    /// The uncompressed `rsi-cold-rows-v1` stream.
    pub raw: Vec<u8>,
    pub row_count: i64,
    /// `SUM(LENGTH(content))` of the rows (0 for a table without `content`).
    pub content_len: i64,
    pub min_id: Option<i64>,
    pub max_id: Option<i64>,
    pub min_sequence: Option<i64>,
    pub max_sequence: Option<i64>,
}

impl EncodedRows {
    /// Lowercase hex `SHA-256` of the uncompressed stream.
    #[must_use]
    pub fn sha256(&self) -> String {
        sha256_hex(&self.raw)
    }
}

/// Lowercase hex `SHA-256`.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// Which columns of a table carry the id, the sequence and the content that the
/// chunk summary records (`None` when the table has no such column).
#[derive(Debug, Clone, Copy)]
pub struct ColumnRoles {
    pub id: Option<usize>,
    pub sequence: Option<usize>,
    pub content: Option<usize>,
}

/// Streams rows into a chunk.
pub struct RowEncoder {
    buf: Vec<u8>,
    columns: usize,
    roles: ColumnRoles,
    rows: i64,
    content_len: i64,
    min_id: Option<i64>,
    max_id: Option<i64>,
    min_sequence: Option<i64>,
    max_sequence: Option<i64>,
}

impl RowEncoder {
    #[must_use]
    pub const fn new(columns: usize, roles: ColumnRoles) -> Self {
        Self {
            buf: Vec::new(),
            columns,
            roles,
            rows: 0,
            content_len: 0,
            min_id: None,
            max_id: None,
            min_sequence: None,
            max_sequence: None,
        }
    }

    /// Appends one row (its first `columns` values).
    pub fn push_row(&mut self, row: &Row<'_>) -> ColdResult<()> {
        for index in 0..self.columns {
            let value = row.get_ref(index)?;
            if self.roles.content == Some(index) {
                self.content_len = self.content_len.saturating_add(sqlite_length(value));
            }
            if self.roles.id == Some(index)
                && let ValueRef::Integer(id) = value
            {
                self.min_id = Some(self.min_id.map_or(id, |current| current.min(id)));
                self.max_id = Some(self.max_id.map_or(id, |current| current.max(id)));
            }
            if self.roles.sequence == Some(index)
                && let ValueRef::Integer(sequence) = value
            {
                self.min_sequence = Some(
                    self.min_sequence
                        .map_or(sequence, |current| current.min(sequence)),
                );
                self.max_sequence = Some(
                    self.max_sequence
                        .map_or(sequence, |current| current.max(sequence)),
                );
            }
            self.push_value(value)?;
        }
        self.rows += 1;
        Ok(())
    }

    fn push_value(&mut self, value: ValueRef<'_>) -> ColdResult<()> {
        match value {
            ValueRef::Null => self.buf.push(TAG_NULL),
            ValueRef::Integer(number) => {
                self.buf.push(TAG_INTEGER);
                self.buf.extend_from_slice(&number.to_le_bytes());
            }
            ValueRef::Real(number) => {
                self.buf.push(TAG_REAL);
                self.buf.extend_from_slice(&number.to_le_bytes());
            }
            ValueRef::Text(bytes) => {
                if std::str::from_utf8(bytes).is_err() {
                    return Err(ColdError::new(
                        "cold_archive_invalid_utf8",
                        "a TEXT value is not valid UTF-8 and cannot round-trip",
                    ));
                }
                self.buf.push(TAG_TEXT);
                self.push_length(bytes.len())?;
                self.buf.extend_from_slice(bytes);
            }
            ValueRef::Blob(bytes) => {
                self.buf.push(TAG_BLOB);
                self.push_length(bytes.len())?;
                self.buf.extend_from_slice(bytes);
            }
        }
        Ok(())
    }

    fn push_length(&mut self, length: usize) -> ColdResult<()> {
        let length = u32::try_from(length).map_err(|_| {
            ColdError::new(
                "cold_archive_value_too_large",
                "a value is larger than 4 GiB",
            )
        })?;
        self.buf.extend_from_slice(&length.to_le_bytes());
        Ok(())
    }

    /// Ends the chunk.
    #[must_use]
    pub fn finish(self) -> EncodedRows {
        EncodedRows {
            raw: self.buf,
            row_count: self.rows,
            content_len: self.content_len,
            min_id: self.min_id,
            max_id: self.max_id,
            min_sequence: self.min_sequence,
            max_sequence: self.max_sequence,
        }
    }
}

/// Compresses a chunk stream into one zstd frame.
pub fn compress(raw: &[u8]) -> ColdResult<Vec<u8>> {
    zstd::bulk::compress(raw, ZSTD_LEVEL)
        .map_err(|error| ColdError::new("cold_archive_compress", error.to_string()))
}

/// Decompresses a chunk and checks its length and digest against the manifest.
pub fn decompress_checked(
    payload: &[u8],
    raw_bytes: i64,
    expected_sha256: &str,
) -> ColdResult<Vec<u8>> {
    let capacity = usize::try_from(raw_bytes).map_err(|_| {
        ColdError::new(
            "cold_archive_verify_raw_len",
            "the recorded raw length is negative",
        )
    })?;
    let raw = zstd::bulk::decompress(payload, capacity).map_err(|error| {
        ColdError::new(
            "cold_archive_verify_decompress",
            format!("the chunk does not decompress: {error}"),
        )
    })?;
    if raw.len() != capacity {
        return Err(ColdError::new(
            "cold_archive_verify_raw_len",
            format!(
                "decompressed {} bytes, the manifest records {capacity}",
                raw.len()
            ),
        ));
    }
    let actual = sha256_hex(&raw);
    if actual != expected_sha256 {
        return Err(ColdError::new(
            "cold_archive_verify_sha_mismatch",
            format!("sha256 {actual} differs from the manifest's {expected_sha256}"),
        ));
    }
    Ok(raw)
}

fn truncated() -> ColdError {
    ColdError::new(
        "cold_archive_verify_decode",
        "the row stream ends inside a value",
    )
}

/// Decodes a chunk stream into rows of `columns` values. Strict: a truncated
/// stream, an unknown tag, invalid UTF-8 or a column count that does not divide
/// the stream is an error.
pub fn decode_rows(raw: &[u8], columns: usize) -> ColdResult<Vec<Vec<Value>>> {
    if columns == 0 {
        return Err(ColdError::new(
            "cold_archive_verify_decode",
            "a chunk must have at least one column",
        ));
    }
    let mut rows = Vec::new();
    let mut at = 0usize;
    while at < raw.len() {
        let mut row = Vec::with_capacity(columns);
        for _ in 0..columns {
            let tag = *raw.get(at).ok_or_else(truncated)?;
            at += 1;
            let value = match tag {
                TAG_NULL => Value::Null,
                TAG_INTEGER => {
                    let bytes: [u8; 8] = raw
                        .get(at..at + 8)
                        .ok_or_else(truncated)?
                        .try_into()
                        .map_err(|_| truncated())?;
                    at += 8;
                    Value::Integer(i64::from_le_bytes(bytes))
                }
                TAG_REAL => {
                    let bytes: [u8; 8] = raw
                        .get(at..at + 8)
                        .ok_or_else(truncated)?
                        .try_into()
                        .map_err(|_| truncated())?;
                    at += 8;
                    Value::Real(f64::from_le_bytes(bytes))
                }
                TAG_TEXT | TAG_BLOB => {
                    let length_bytes: [u8; 4] = raw
                        .get(at..at + 4)
                        .ok_or_else(truncated)?
                        .try_into()
                        .map_err(|_| truncated())?;
                    at += 4;
                    let length = u32::from_le_bytes(length_bytes) as usize;
                    let end = at.checked_add(length).ok_or_else(truncated)?;
                    let bytes = raw.get(at..end).ok_or_else(truncated)?;
                    at = end;
                    if tag == TAG_TEXT {
                        let text = std::str::from_utf8(bytes).map_err(|_| {
                            ColdError::new(
                                "cold_archive_verify_decode",
                                "a TEXT value is not valid UTF-8",
                            )
                        })?;
                        Value::Text(text.to_string())
                    } else {
                        Value::Blob(bytes.to_vec())
                    }
                }
                other => {
                    return Err(ColdError::new(
                        "cold_archive_verify_decode",
                        format!("unknown value tag {other}"),
                    ));
                }
            };
            row.push(value);
        }
        rows.push(row);
    }
    Ok(rows)
}

/// Re-derives the chunk summary from decoded rows, for verification.
#[must_use]
pub fn summarize_decoded(rows: &[Vec<Value>], roles: ColumnRoles) -> DecodedSummary {
    let mut summary = DecodedSummary {
        row_count: i64::try_from(rows.len()).unwrap_or(i64::MAX),
        ..DecodedSummary::default()
    };
    for row in rows {
        if let Some(index) = roles.content
            && let Some(value) = row.get(index)
        {
            summary.content_len = summary
                .content_len
                .saturating_add(sqlite_length_of_value(value));
        }
        if let Some(index) = roles.id
            && let Some(Value::Integer(id)) = row.get(index)
        {
            summary.min_id = Some(summary.min_id.map_or(*id, |current| current.min(*id)));
            summary.max_id = Some(summary.max_id.map_or(*id, |current| current.max(*id)));
        }
        if let Some(index) = roles.sequence
            && let Some(Value::Integer(sequence)) = row.get(index)
        {
            summary.min_sequence = Some(
                summary
                    .min_sequence
                    .map_or(*sequence, |current| current.min(*sequence)),
            );
            summary.max_sequence = Some(
                summary
                    .max_sequence
                    .map_or(*sequence, |current| current.max(*sequence)),
            );
        }
    }
    summary
}

/// What decoding a chunk shows about it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DecodedSummary {
    pub row_count: i64,
    pub content_len: i64,
    pub min_id: Option<i64>,
    pub max_id: Option<i64>,
    pub min_sequence: Option<i64>,
    pub max_sequence: Option<i64>,
}

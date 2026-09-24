//! Private bounded JSON admission and streaming canonical encoding.
#![allow(
    clippy::expect_used,
    clippy::missing_const_for_fn,
    clippy::redundant_pub_crate
)]

use std::collections::BTreeSet;

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::bounds::{JsonContainerItems, JsonDepth, JsonNodes, WitnessBytes};
use crate::error::{WitnessBuildError, WitnessDecodeError};

pub(crate) fn parse(input: &[u8]) -> Result<Value, WitnessDecodeError> {
    WitnessBytes::try_new(input.len() as u64).map_err(|_| WitnessDecodeError::TooLarge)?;
    std::str::from_utf8(input).map_err(|_| WitnessDecodeError::Utf8)?;
    let facts = {
        let mut scanner = Scanner {
            input,
            offset: 0,
            nodes: 0,
            facts: ScanFacts::default(),
        };
        scanner.value(1)?;
        scanner.whitespace();
        if scanner.offset != input.len() {
            return Err(WitnessDecodeError::Syntax);
        }
        scanner.facts
    };
    if facts.duplicate_key {
        return Err(WitnessDecodeError::DuplicateKey);
    }
    if facts.non_integer {
        return Err(WitnessDecodeError::NonInteger);
    }
    if facts.integer_shape_error {
        return Err(WitnessDecodeError::Shape);
    }
    serde_json::from_slice(input).map_err(|_| WitnessDecodeError::Syntax)
}

#[derive(Clone, Copy, Default)]
struct ScanFacts {
    duplicate_key: bool,
    non_integer: bool,
    integer_shape_error: bool,
}
impl ScanFacts {
    fn record_number(&mut self, class: NumberClass) {
        match class {
            NumberClass::UnsignedInteger => {}
            NumberClass::SignedInteger | NumberClass::UnsignedIntegerOverflow => {
                self.integer_shape_error = true;
            }
            NumberClass::NonInteger => self.non_integer = true,
        }
    }
}

#[derive(Clone, Copy)]
enum NumberClass {
    UnsignedInteger,
    SignedInteger,
    UnsignedIntegerOverflow,
    NonInteger,
}

#[derive(Clone, Copy)]
enum NumberState {
    Start,
    Minus,
    Zero,
    Integer,
    Dot,
    Fraction,
    ExponentMark,
    ExponentSign,
    Exponent,
}
const U64_MAX_DECIMAL: &[u8; 20] = b"18446744073709551615";

struct Scanner<'a> {
    input: &'a [u8],
    offset: usize,
    nodes: u64,
    facts: ScanFacts,
}
impl Scanner<'_> {
    fn whitespace(&mut self) {
        while matches!(
            self.input.get(self.offset),
            Some(b' ' | b'\t' | b'\n' | b'\r')
        ) {
            self.offset += 1;
        }
    }
    fn value(&mut self, depth: u64) -> Result<(), WitnessDecodeError> {
        JsonDepth::try_new(depth).map_err(|_| WitnessDecodeError::Syntax)?;
        self.nodes = self
            .nodes
            .checked_add(1)
            .ok_or(WitnessDecodeError::Syntax)?;
        JsonNodes::try_new(self.nodes).map_err(|_| WitnessDecodeError::Syntax)?;
        self.whitespace();
        match self.input.get(self.offset) {
            Some(b'{') => self.object(depth),
            Some(b'[') => self.array(depth),
            Some(b'"') => self.string().map(|_| ()),
            Some(b't') if self.take(b"true") => Ok(()),
            Some(b'f') if self.take(b"false") => Ok(()),
            Some(b'n') if self.take(b"null") => Ok(()),
            Some(b'-' | b'0'..=b'9') => {
                let class = self.number()?;
                self.facts.record_number(class);
                Ok(())
            }
            _ => Err(WitnessDecodeError::Syntax),
        }
    }
    fn object(&mut self, depth: u64) -> Result<(), WitnessDecodeError> {
        self.offset += 1;
        self.whitespace();
        let mut keys = BTreeSet::new();
        let mut count = 0_u64;
        if self.input.get(self.offset) == Some(&b'}') {
            self.offset += 1;
            return Ok(());
        }
        loop {
            self.whitespace();
            let key = self.string()?;
            if !keys.insert(key) {
                self.facts.duplicate_key = true;
            }
            self.whitespace();
            if self.input.get(self.offset) != Some(&b':') {
                return Err(WitnessDecodeError::Syntax);
            }
            self.offset += 1;
            self.value(depth + 1)?;
            count = count.checked_add(1).ok_or(WitnessDecodeError::Syntax)?;
            JsonContainerItems::try_new(count).map_err(|_| WitnessDecodeError::Syntax)?;
            self.whitespace();
            match self.input.get(self.offset) {
                Some(b',') => self.offset += 1,
                Some(b'}') => {
                    self.offset += 1;
                    return Ok(());
                }
                _ => return Err(WitnessDecodeError::Syntax),
            }
        }
    }
    fn array(&mut self, depth: u64) -> Result<(), WitnessDecodeError> {
        self.offset += 1;
        self.whitespace();
        let mut count = 0_u64;
        if self.input.get(self.offset) == Some(&b']') {
            self.offset += 1;
            return Ok(());
        }
        loop {
            self.value(depth + 1)?;
            count = count.checked_add(1).ok_or(WitnessDecodeError::Syntax)?;
            JsonContainerItems::try_new(count).map_err(|_| WitnessDecodeError::Syntax)?;
            self.whitespace();
            match self.input.get(self.offset) {
                Some(b',') => self.offset += 1,
                Some(b']') => {
                    self.offset += 1;
                    return Ok(());
                }
                _ => return Err(WitnessDecodeError::Syntax),
            }
        }
    }
    fn string(&mut self) -> Result<String, WitnessDecodeError> {
        let start = self.offset;
        if self.input.get(self.offset) != Some(&b'"') {
            return Err(WitnessDecodeError::Syntax);
        }
        self.offset += 1;
        while let Some(&byte) = self.input.get(self.offset) {
            match byte {
                b'"' => {
                    self.offset += 1;
                    return serde_json::from_slice(&self.input[start..self.offset])
                        .map_err(|_| WitnessDecodeError::Syntax);
                }
                b'\\' => {
                    self.offset += 1;
                    match self.input.get(self.offset) {
                        Some(b'u') => {
                            let escape = self
                                .input
                                .get(self.offset..self.offset + 5)
                                .ok_or(WitnessDecodeError::Syntax)?;
                            if !escape[1..].iter().all(u8::is_ascii_hexdigit) {
                                return Err(WitnessDecodeError::Syntax);
                            }
                            self.offset += 5;
                        }
                        Some(b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't') => {
                            self.offset += 1;
                        }
                        Some(_) | None => return Err(WitnessDecodeError::Syntax),
                    }
                }
                0..=31 => return Err(WitnessDecodeError::Syntax),
                _ => self.offset += 1,
            }
        }
        Err(WitnessDecodeError::Syntax)
    }
    #[allow(clippy::too_many_lines)]
    fn number(&mut self) -> Result<NumberClass, WitnessDecodeError> {
        let start = self.offset;
        let mut state = NumberState::Start;
        let mut negative = false;
        let mut non_integer = false;
        loop {
            let byte = self.input.get(self.offset).copied();
            match state {
                NumberState::Start => match byte {
                    Some(b'-') => {
                        negative = true;
                        self.offset += 1;
                        state = NumberState::Minus;
                    }
                    Some(b'0') => {
                        self.offset += 1;
                        state = NumberState::Zero;
                    }
                    Some(b'1'..=b'9') => {
                        self.offset += 1;
                        state = NumberState::Integer;
                    }
                    _ => return Err(WitnessDecodeError::Syntax),
                },
                NumberState::Minus => match byte {
                    Some(b'0') => {
                        self.offset += 1;
                        state = NumberState::Zero;
                    }
                    Some(b'1'..=b'9') => {
                        self.offset += 1;
                        state = NumberState::Integer;
                    }
                    _ => return Err(WitnessDecodeError::Syntax),
                },
                NumberState::Zero => match byte {
                    Some(b'.') => {
                        non_integer = true;
                        self.offset += 1;
                        state = NumberState::Dot;
                    }
                    Some(b'e' | b'E') => {
                        non_integer = true;
                        self.offset += 1;
                        state = NumberState::ExponentMark;
                    }
                    terminator if number_terminator(terminator) => {
                        return self.classify_number(start, negative, non_integer);
                    }
                    _ => return Err(WitnessDecodeError::Syntax),
                },
                NumberState::Integer => match byte {
                    Some(b'0'..=b'9') => self.offset += 1,
                    Some(b'.') => {
                        non_integer = true;
                        self.offset += 1;
                        state = NumberState::Dot;
                    }
                    Some(b'e' | b'E') => {
                        non_integer = true;
                        self.offset += 1;
                        state = NumberState::ExponentMark;
                    }
                    terminator if number_terminator(terminator) => {
                        return self.classify_number(start, negative, non_integer);
                    }
                    _ => return Err(WitnessDecodeError::Syntax),
                },
                NumberState::Dot => match byte {
                    Some(b'0'..=b'9') => {
                        self.offset += 1;
                        state = NumberState::Fraction;
                    }
                    _ => return Err(WitnessDecodeError::Syntax),
                },
                NumberState::Fraction => match byte {
                    Some(b'0'..=b'9') => self.offset += 1,
                    Some(b'e' | b'E') => {
                        self.offset += 1;
                        state = NumberState::ExponentMark;
                    }
                    terminator if number_terminator(terminator) => {
                        return self.classify_number(start, negative, non_integer);
                    }
                    _ => return Err(WitnessDecodeError::Syntax),
                },
                NumberState::ExponentMark => match byte {
                    Some(b'+' | b'-') => {
                        self.offset += 1;
                        state = NumberState::ExponentSign;
                    }
                    Some(b'0'..=b'9') => {
                        self.offset += 1;
                        state = NumberState::Exponent;
                    }
                    _ => return Err(WitnessDecodeError::Syntax),
                },
                NumberState::ExponentSign => match byte {
                    Some(b'0'..=b'9') => {
                        self.offset += 1;
                        state = NumberState::Exponent;
                    }
                    _ => return Err(WitnessDecodeError::Syntax),
                },
                NumberState::Exponent => match byte {
                    Some(b'0'..=b'9') => self.offset += 1,
                    terminator if number_terminator(terminator) => {
                        return self.classify_number(start, negative, non_integer);
                    }
                    _ => return Err(WitnessDecodeError::Syntax),
                },
            }
        }
    }
    fn classify_number(
        &self,
        start: usize,
        negative: bool,
        non_integer: bool,
    ) -> Result<NumberClass, WitnessDecodeError> {
        if non_integer {
            return Ok(NumberClass::NonInteger);
        }
        if negative {
            return Ok(NumberClass::SignedInteger);
        }
        let digits = self
            .input
            .get(start..self.offset)
            .ok_or(WitnessDecodeError::Syntax)?;
        if digits.len() > U64_MAX_DECIMAL.len()
            || (digits.len() == U64_MAX_DECIMAL.len() && digits > U64_MAX_DECIMAL)
        {
            Ok(NumberClass::UnsignedIntegerOverflow)
        } else {
            Ok(NumberClass::UnsignedInteger)
        }
    }
    fn take(&mut self, expected: &[u8]) -> bool {
        if self.input.get(self.offset..self.offset + expected.len()) == Some(expected) {
            self.offset += expected.len();
            true
        } else {
            false
        }
    }
}

fn number_terminator(byte: Option<u8>) -> bool {
    matches!(
        byte,
        None | Some(b' ' | b'\t' | b'\n' | b'\r' | b',' | b']' | b'}')
    )
}

pub(crate) struct Sink {
    bytes: Option<Vec<u8>>,
    count: u64,
    maximum: Option<u64>,
    hash: Option<Sha256>,
}
impl Sink {
    pub(crate) const fn counting(maximum: Option<u64>) -> Self {
        Self {
            bytes: None,
            count: 0,
            maximum,
            hash: None,
        }
    }
    pub(crate) const fn bytes(maximum: u64) -> Self {
        Self {
            bytes: Some(Vec::new()),
            count: 0,
            maximum: Some(maximum),
            hash: None,
        }
    }
    pub(crate) fn hashing_prefixed(prefix: &[u8]) -> Self {
        let mut hash = Sha256::new();
        hash.update(prefix);
        Self {
            bytes: None,
            count: 0,
            maximum: None,
            hash: Some(hash),
        }
    }
    pub(crate) fn write(&mut self, value: &[u8]) -> Result<(), WitnessBuildError> {
        self.count = self
            .count
            .checked_add(value.len() as u64)
            .ok_or(WitnessBuildError::Arithmetic)?;
        if self.maximum.is_some_and(|maximum| self.count > maximum) {
            return Err(WitnessBuildError::TooLarge);
        }
        if let Some(bytes) = &mut self.bytes {
            bytes.extend_from_slice(value);
        }
        if let Some(hash) = &mut self.hash {
            hash.update(value);
        }
        Ok(())
    }
    #[must_use]
    pub(crate) const fn count(&self) -> u64 {
        self.count
    }
    pub(crate) fn finish_bytes(self) -> Vec<u8> {
        self.bytes.expect("byte sink")
    }
    pub(crate) fn finish_hash(self) -> [u8; 32] {
        self.hash.expect("hash sink").finalize().into()
    }
}
pub(crate) fn string(sink: &mut Sink, value: &str) -> Result<(), WitnessBuildError> {
    sink.write(b"\"")?;
    for character in value.chars() {
        match character {
            '"' => sink.write(b"\\\"")?,
            '\\' => sink.write(b"\\\\")?,
            '\u{08}' => sink.write(b"\\b")?,
            '\u{0c}' => sink.write(b"\\f")?,
            '\n' => sink.write(b"\\n")?,
            '\r' => sink.write(b"\\r")?,
            '\t' => sink.write(b"\\t")?,
            character if character <= '\u{1f}' => {
                let escaped = format!("\\u{:04x}", character as u32);
                sink.write(escaped.as_bytes())?;
            }
            character => {
                let mut bytes = [0; 4];
                sink.write(character.encode_utf8(&mut bytes).as_bytes())?;
            }
        }
    }
    sink.write(b"\"")
}
pub(crate) fn field(
    sink: &mut Sink,
    name: &str,
    first: &mut bool,
) -> Result<(), WitnessBuildError> {
    if !*first {
        sink.write(b",")?;
    }
    *first = false;
    string(sink, name)?;
    sink.write(b":")
}

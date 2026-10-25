//! Cfg parsing with distinct shard-policy, host and feature-view semantics.
use super::*;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Predicate {
    Atom(String, Option<String>),
    All(Vec<Predicate>),
    Any(Vec<Predicate>),
    Not(Box<Predicate>),
}
#[derive(Clone, Debug)]
pub struct Host {
    pub os: String,
    pub family: Option<String>,
}
impl Default for Host {
    fn default() -> Self {
        Self {
            os: std::env::consts::OS.into(),
            family: if cfg!(unix) {
                Some("unix".into())
            } else if cfg!(windows) {
                Some("windows".into())
            } else {
                None
            },
        }
    }
}
#[derive(Debug)]
enum Token {
    Ident(String),
    Literal(String),
    Punct(char),
    End,
}
fn token(text: &str, at: &mut usize) -> Token {
    let b = text.as_bytes();
    while *at < b.len() && b[*at].is_ascii_whitespace() {
        *at += 1;
    }
    if *at >= b.len() {
        return Token::End;
    }
    let start = *at;
    if b[*at] == b'"' {
        *at += 1;
        while *at < b.len() {
            if b[*at] == b'\\' {
                *at = (*at + 2).min(b.len());
            } else if b[*at] == b'"' {
                *at += 1;
                return Token::Literal(text[start + 1..*at - 1].into());
            } else {
                *at += 1;
            }
        }
        return Token::End;
    }
    if b[*at].is_ascii_alphabetic() || b[*at] == b'_' {
        *at += 1;
        while *at < b.len() && (b[*at].is_ascii_alphanumeric() || b[*at] == b'_') {
            *at += 1;
        }
        return Token::Ident(text[start..*at].into());
    }
    let c = text[*at..].chars().next().unwrap_or('\0');
    *at += c.len_utf8();
    Token::Punct(c)
}
pub fn parse_cfg(text: &str) -> Result<Predicate, String> {
    fn parse(text: &str, at: &mut usize) -> Result<Predicate, String> {
        let start = *at;
        let Token::Ident(name) = token(text, at) else {
            return Err(format!(
                "cannot parse cfg predicate near {}",
                crate::py::repr(&text[start..].chars().take(40).collect::<String>())
            ));
        };
        let end = *at;
        match token(text, at) {
            Token::Punct('(') if matches!(name.as_str(), "all" | "any" | "not") => {
                let mut values = Vec::new();
                loop {
                    let mut peek = *at;
                    if matches!(token(text, &mut peek), Token::Punct(')')) {
                        *at = peek;
                        break;
                    }
                    values.push(parse(text, at)?);
                    let mut peek = *at;
                    match token(text, &mut peek) {
                        Token::Punct(',') => *at = peek,
                        Token::Punct(')') => {}
                        _ => {
                            return Err(format!(
                                "cannot parse cfg predicate near {}",
                                crate::py::repr(&text[*at..].chars().take(40).collect::<String>())
                            ));
                        }
                    }
                }
                Ok(match name.as_str() {
                    "all" => Predicate::All(values),
                    "any" => Predicate::Any(values),
                    _ => {
                        if values.len() != 1 {
                            return Err("cfg not() takes one predicate".into());
                        }
                        Predicate::Not(Box::new(values.remove(0)))
                    }
                })
            }
            Token::Punct('=') => {
                let Token::Literal(value) = token(text, at) else {
                    return Err("cannot parse cfg predicate: expected a cfg string".into());
                };
                Ok(Predicate::Atom(name, Some(value)))
            }
            _ => {
                *at = end;
                Ok(Predicate::Atom(name, None))
            }
        }
    }
    let mut at = 0;
    let value = parse(text, &mut at)?;
    if !text[at..].trim().is_empty() {
        return Err(format!(
            "cannot parse cfg predicate near {}",
            crate::py::repr(&text[at..])
        ));
    }
    Ok(value)
}
impl Predicate {
    /// The focused unfeatured build treats every non-shard predicate as on.
    pub fn unfeatured(&self) -> bool {
        match self {
            Self::Atom(name, Some(value)) => {
                !(name == "feature" && value.starts_with("test-shard-"))
            }
            Self::Atom(_, None) => true,
            Self::All(v) => v.iter().all(Self::unfeatured),
            Self::Any(v) => v.iter().any(Self::unfeatured),
            Self::Not(v) => !v.unfeatured(),
        }
    }
    /// Possible values, with platform terms left open in both feature views.
    pub fn feature_values(&self, shard: &str, opt_in: bool) -> (bool, bool) {
        self.values(&|name, value| {
            if name == "test" && value.is_none() {
                return Some(true);
            }
            if name != "feature" {
                return None;
            }
            value.map(|v| {
                if v.starts_with("test-shard-") {
                    !opt_in && (v == "test-shard-mode" || v == format!("test-shard-{shard}"))
                } else {
                    opt_in
                }
            })
        })
    }
    fn values(&self, atom: &dyn Fn(&str, Option<&str>) -> Option<bool>) -> (bool, bool) {
        // (false possible, true possible), equivalent to the old set evaluator.
        match self {
            Self::Atom(n, v) => match atom(n, v.as_deref()) {
                Some(false) => (true, false),
                Some(true) => (false, true),
                None => (true, true),
            },
            Self::Not(p) => {
                let (f, t) = p.values(atom);
                (t, f)
            }
            Self::All(v) => (
                v.iter().any(|p| p.values(atom).0),
                v.iter().all(|p| p.values(atom).1),
            ),
            Self::Any(v) => (
                v.iter().all(|p| p.values(atom).0),
                v.iter().any(|p| p.values(atom).1),
            ),
        }
    }
    pub fn host_value(&self, host: &Host) -> Option<bool> {
        // Three-valued evaluation deliberately does not correlate unknown atoms.
        let (f, t) = self.values(&|n, v| match (n, v) {
            ("test", None) => Some(true),
            ("target_os", Some(v)) => Some(v == host.os),
            ("target_family", Some(v)) => host.family.as_ref().map(|family| family == v),
            ("unix" | "windows", None) => host.family.as_ref().map(|family| family == n),
            _ => None,
        });
        match (f, t) {
            (true, false) => Some(false),
            (false, true) => Some(true),
            _ => None,
        }
    }
}
pub fn host_excluded(line: &str, host: &Host) -> bool {
    static GATE: LazyLock<Re> = LazyLock::new(|| Re::new(r"(?s)^#\[\s*cfg\s*\((.*)\)\s*\]$"));
    GATE.search(line.trim())
        .and_then(|m| parse_cfg(re::group(&m, 1)).ok())
        .is_some_and(|p| p.host_value(host) == Some(false))
}

#[derive(Clone, Debug)]
pub struct CfgAttribute {
    pub start: usize,
    pub end: usize,
    pub predicate_start: usize,
    pub predicate_end: usize,
    pub cfg_attr: bool,
    pub inner: bool,
}
pub fn cfg_attributes(text: &str, code: &str, path: &str) -> Result<Vec<CfgAttribute>, String> {
    static CFG: LazyLock<Re> = LazyLock::new(|| Re::new(r"#!?\[\s*(cfg_attr|cfg)\s*\("));
    let mut result = Vec::new();
    for m in CFG.find_all(code) {
        let end = crate::rust_source::closing(code, re::end(&m) - 1)
            .map_err(|_| format!("{path}: unterminated cfg attribute"))?;
        let start = re::start(&m);
        result.push(CfgAttribute {
            start,
            end,
            predicate_start: re::end(&m),
            predicate_end: end - 1,
            cfg_attr: re::group(&m, 1) == "cfg_attr",
            inner: text[start..].starts_with("#!["),
        });
    }
    Ok(result)
}
pub fn check_shard_cfg_attributes(path: &str, source: &str, code: &str) -> Result<(), String> {
    for attr in cfg_attributes(source, code, path)? {
        let inner = &source[attr.predicate_start..attr.predicate_end];
        if !inner.contains("test-shard-") {
            continue;
        }
        let line = crate::rust_source::line(source, attr.start);
        if attr.cfg_attr {
            let compact: String = inner.chars().filter(|c| !c.is_whitespace()).collect();
            if attr.inner
                && compact.trim_end_matches(',')
                    == "all(test,feature=\"test-shard-mode\"),allow(dead_code,unused_imports)"
            {
                continue;
            }
            return Err(format!(
                "{path}:{line}: cfg_attr must not name a test shard feature"
            ));
        }
        if !parse_cfg(inner)?.unfeatured() {
            return Err(format!(
                "{path}:{line}: shard-only cfg hides code from the unfeatured test build; use any(not(feature = \"test-shard-mode\"), feature = \"test-shard-...\")"
            ));
        }
    }
    Ok(())
}

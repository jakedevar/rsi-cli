//! A small argparse-shaped option parser: `--name value`, `--name=value`,
//! boolean flags and `nargs="*"` options, with positionals collected in order.
//! A usage error maps to exit code 2, as argparse does.

use std::collections::HashMap;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `--name VALUE`.
    Value,
    /// `--name` with no value.
    Flag,
    /// `--name V1 V2 ...` up to the next `--option`.
    Multi,
}

#[derive(Default)]
pub struct Parsed {
    values: HashMap<&'static str, Vec<String>>,
    pub positionals: Vec<String>,
}

impl Parsed {
    #[must_use]
    pub fn value(&self, name: &str) -> Option<&str> {
        self.values
            .get(name)
            .and_then(|values| values.last())
            .map(String::as_str)
    }

    #[must_use]
    pub fn flag(&self, name: &str) -> bool {
        self.values.contains_key(name)
    }

    #[must_use]
    pub fn multi(&self, name: &str) -> Vec<String> {
        self.values.get(name).cloned().unwrap_or_default()
    }

    /// The option parsed as `T`, or a usage error naming the option.
    pub fn parsed<T: std::str::FromStr>(&self, name: &str) -> Result<Option<T>, String> {
        match self.value(name) {
            None => Ok(None),
            Some(text) => text
                .trim()
                .parse::<T>()
                .map(Some)
                .map_err(|_| format!("argument {name}: invalid value: '{text}'")),
        }
    }
}

fn looks_like_option(text: &str) -> bool {
    text.starts_with("--") && text.len() > 2
}

/// Parse `args` against `spec`. Unknown `--options` are usage errors.
pub fn parse(args: &[String], spec: &[(&'static str, Kind)]) -> Result<Parsed, String> {
    let mut parsed = Parsed::default();
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        index += 1;
        if arg == "--" {
            parsed.positionals.extend(args[index..].iter().cloned());
            break;
        }
        if !looks_like_option(arg) {
            parsed.positionals.push(arg.clone());
            continue;
        }
        let (name, inline) = match arg.split_once('=') {
            Some((name, value)) => (name, Some(value.to_string())),
            None => (arg.as_str(), None),
        };
        let Some((key, kind)) = spec.iter().find(|(key, _)| *key == name) else {
            return Err(format!("unrecognized arguments: {arg}"));
        };
        match kind {
            Kind::Flag => {
                if inline.is_some() {
                    return Err(format!("argument {name}: ignored explicit argument"));
                }
                parsed.values.entry(key).or_default();
            }
            Kind::Value => {
                let value = if let Some(value) = inline {
                    value
                } else if index < args.len() && !looks_like_option(&args[index]) {
                    index += 1;
                    args[index - 1].clone()
                } else {
                    return Err(format!("argument {name}: expected one argument"));
                };
                parsed.values.insert(key, vec![value]);
            }
            Kind::Multi => {
                let entry = parsed.values.entry(key).or_default();
                if let Some(value) = inline {
                    entry.push(value);
                }
                while index < args.len() && !looks_like_option(&args[index]) {
                    entry.push(args[index].clone());
                    index += 1;
                }
            }
        }
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_string()).collect()
    }

    #[test]
    fn parses_values_flags_multis_and_positionals() {
        let spec = [
            ("--out", Kind::Value),
            ("--dry-run", Kind::Flag),
            ("--drop", Kind::Multi),
        ];
        let parsed = parse(
            &strings(&["pos", "--out=a", "--drop", "x", "y", "--dry-run", "tail"]),
            &spec,
        )
        .unwrap();
        assert_eq!(parsed.value("--out"), Some("a"));
        assert!(parsed.flag("--dry-run"));
        assert_eq!(parsed.multi("--drop"), strings(&["x", "y"]));
        assert_eq!(parsed.positionals, strings(&["pos", "tail"]));
    }

    #[test]
    fn unknown_option_and_missing_value_are_usage_errors() {
        let spec = [("--out", Kind::Value)];
        assert!(parse(&strings(&["--nope"]), &spec).is_err());
        assert!(parse(&strings(&["--out"]), &spec).is_err());
        assert!(parse(&strings(&["--out", "--out"]), &spec).is_err());
    }

    #[test]
    fn negative_numbers_are_positionals() {
        let parsed = parse(&strings(&["2030-01-01T00:00:00Z", "-5"]), &[]).unwrap();
        assert_eq!(parsed.positionals.len(), 2);
    }
}

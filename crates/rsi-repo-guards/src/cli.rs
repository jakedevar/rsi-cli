//! The slice of Python `argparse` the guards used: long options with a value
//! (`--opt v` or `--opt=v`), flags, unique-prefix abbreviations, positionals,
//! `--`, mutually exclusive groups, and `-h`. A usage error prints argparse's
//! `usage:`/`error:` pair on stderr and exits 2.

use std::collections::BTreeMap;
use std::io::Write;

/// One option: its long name (`--root`) and whether it takes a value.
#[derive(Clone, Copy)]
pub struct Opt {
    pub name: &'static str,
    pub value: bool,
}

/// The parsed command line.
#[derive(Default, Debug)]
pub struct Parsed {
    pub values: BTreeMap<&'static str, String>,
    /// Every occurrence of an option, in command-line order (argparse `append`).
    pub repeated: BTreeMap<&'static str, Vec<String>>,
    pub positionals: Vec<String>,
}

impl Parsed {
    /// The value of a value option, if given.
    #[must_use]
    pub fn value(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str)
    }

    /// All values of a repeatable value option, in order.
    #[must_use]
    pub fn all(&self, name: &str) -> &[String] {
        self.repeated.get(name).map_or(&[], Vec::as_slice)
    }

    /// Whether a flag was given.
    #[must_use]
    pub fn flag(&self, name: &str) -> bool {
        self.values.contains_key(name)
    }
}

/// Outcome of [`parse`] that is not a parsed command line.
pub enum Exit {
    /// `-h`/`--help` printed the usage: exit 0.
    Help,
    /// A usage error was printed: exit 2.
    Usage,
}

impl Exit {
    /// The process exit code.
    #[must_use]
    pub const fn code(&self) -> i32 {
        match self {
            Self::Help => 0,
            Self::Usage => 2,
        }
    }
}

/// Parse `args` against `opts`; `exclusive` lists groups of options that may not
/// be combined, `max_positionals` bounds the positionals (`None`: any number).
pub fn parse(
    prog: &str,
    usage: &str,
    args: &[String],
    opts: &[Opt],
    exclusive: &[&[&'static str]],
    max_positionals: Option<usize>,
    err: &mut dyn Write,
) -> Result<Parsed, Exit> {
    let fail = |err: &mut dyn Write, message: &str| {
        let _ = writeln!(err, "usage: {prog} {usage}");
        let _ = writeln!(err, "{prog}: error: {message}");
        Exit::Usage
    };
    let mut parsed = Parsed::default();
    let mut index = 0;
    let mut only_positionals = false;
    while index < args.len() {
        let arg = &args[index];
        index += 1;
        if only_positionals || !arg.starts_with('-') || arg == "-" {
            parsed.positionals.push(arg.clone());
            continue;
        }
        if arg == "--" {
            only_positionals = true;
            continue;
        }
        if arg == "-h" || arg == "--help" {
            let _ = writeln!(err, "usage: {prog} {usage}");
            return Err(Exit::Help);
        }
        let (name, inline) = match arg.split_once('=') {
            Some((name, value)) if arg.starts_with("--") => (name, Some(value.to_string())),
            _ => (arg.as_str(), None),
        };
        let exact = opts.iter().find(|opt| opt.name == name);
        let matches: Vec<&Opt> = if name.starts_with("--") {
            opts.iter()
                .filter(|opt| opt.name.starts_with(name))
                .collect()
        } else {
            Vec::new()
        };
        let opt = match (exact, matches.as_slice()) {
            (Some(opt), _) => *opt,
            (None, [opt]) => **opt,
            (None, []) => return Err(fail(err, &format!("unrecognized arguments: {arg}"))),
            (None, many) => {
                let names: Vec<&str> = many.iter().map(|opt| opt.name).collect();
                return Err(fail(
                    err,
                    &format!("ambiguous option: {name} could match {}", names.join(", ")),
                ));
            }
        };
        let value = if opt.value {
            match inline {
                Some(value) => value,
                None if index < args.len() && !args[index].starts_with('-') => {
                    index += 1;
                    args[index - 1].clone()
                }
                None => {
                    return Err(fail(
                        err,
                        &format!("argument {}: expected one argument", opt.name),
                    ));
                }
            }
        } else if inline.is_some() {
            return Err(fail(
                err,
                &format!("argument {}: ignored explicit argument", opt.name),
            ));
        } else {
            String::new()
        };
        for group in exclusive {
            if !group.contains(&opt.name) {
                continue;
            }
            if let Some(other) = group
                .iter()
                .find(|other| **other != opt.name && parsed.values.contains_key(*other))
            {
                return Err(fail(
                    err,
                    &format!("argument {}: not allowed with argument {other}", opt.name),
                ));
            }
        }
        parsed
            .repeated
            .entry(opt.name)
            .or_default()
            .push(value.clone());
        parsed.values.insert(opt.name, value);
    }
    if let Some(max) = max_positionals
        && parsed.positionals.len() > max
    {
        let extra = parsed.positionals[max..].join(" ");
        return Err(fail(err, &format!("unrecognized arguments: {extra}")));
    }
    Ok(parsed)
}

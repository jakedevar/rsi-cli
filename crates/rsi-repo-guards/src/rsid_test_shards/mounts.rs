//! Static module reachability and opt-in versus shard feature-view parity.
use super::*;
use std::collections::VecDeque;
static MOD: LazyLock<Re> = LazyLock::new(|| {
    Re::new(r"^([ \t]*)(?:pub(?:\([^)]*\))?\s+)?mod\s+([A-Za-z_][A-Za-z_0-9]*)\s*(;|\{)")
});
static PATH: LazyLock<Re> = LazyLock::new(|| Re::new(r#"#\[\s*path\s*=\s*"([^"]+)"\s*\]"#));
static INCLUDE: LazyLock<Re> = LazyLock::new(|| Re::new(r#"^\s*include!\(\s*"([^"]+)"\s*\)\s*;"#));
static ATTR_LINE: LazyLock<Re> = LazyLock::new(|| Re::new(r"^\s*#\["));
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mount {
    pub name: Option<String>,
    pub path: Option<String>,
    pub inline: Vec<String>,
}
pub fn mount_declarations(text: &str) -> Vec<Mount> {
    let mut found = Vec::new();
    let mut stack: Vec<(String, String)> = Vec::new();
    let lines: Vec<&str> = text.split('\n').collect();
    for (i, line) in lines.iter().enumerate() {
        if let Some((indent, _)) = stack.last() {
            if line.starts_with(&format!("{indent}}}"))
                && (line.trim_end() == format!("{indent}}}")
                    || line.trim_end() == format!("{indent}}};"))
            {
                stack.pop();
                continue;
            }
        }
        let inline = || stack.iter().map(|(_, n)| n.clone()).collect::<Vec<_>>();
        if let Some(m) = INCLUDE.search(line) {
            found.push(Mount {
                name: None,
                path: Some(re::group(&m, 1).into()),
                inline: inline(),
            });
            continue;
        }
        if !line.contains("mod ") {
            continue;
        }
        let indent = &line[..line.len() - line.trim_start().len()];
        let mut rest = *line;
        let mut attr = None;
        while rest.trim_start().starts_with("#[") && rest.contains(']') {
            if let Some(m) = PATH.search(rest) {
                attr = Some(re::group(&m, 1).to_string());
            }
            rest = rest.split_once(']').map_or("", |(_, r)| r);
        }
        let scan = format!("{indent}{}", rest.trim_start());
        let Some(m) = MOD.search(&scan) else {
            continue;
        };
        let name = re::group(&m, 2).to_string();
        if re::group(&m, 3) == "{" {
            stack.push((indent.into(), name));
            continue;
        }
        let mut back = i;
        while attr.is_none() && back > 0 && ATTR_LINE.is_match(lines[back - 1]) {
            back -= 1;
            if let Some(m) = PATH.search(lines[back]) {
                attr = Some(re::group(&m, 1).into());
            }
        }
        found.push(Mount {
            name: Some(name),
            path: attr,
            inline: inline(),
        });
    }
    found
}
fn normalize(path: &str) -> String {
    let mut parts = Vec::new();
    let absolute = path.starts_with('/');
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." if parts.last().is_some_and(|p| *p != "..") => {
                parts.pop();
            }
            ".." if absolute => {}
            v => parts.push(v),
        }
    }
    format!("{}{}", if absolute { "/" } else { "" }, parts.join("/"))
}
pub fn mounted_sources(files: &Sources, root: &str) -> BTreeSet<String> {
    let mut mounted = BTreeSet::from([root.into()]);
    let mut queue = VecDeque::from([root.to_string()]);
    let mut path_mounted = BTreeSet::new();
    while let Some(parent) = queue.pop_front() {
        let Some(text) = files.get(&parent) else {
            continue;
        };
        let (directory, base) = parent.rsplit_once('/').unwrap_or(("", &parent));
        let owns = parent == root
            || path_mounted.contains(&parent)
            || matches!(base, "mod.rs" | "lib.rs" | "main.rs");
        let own = if owns {
            directory.to_string()
        } else {
            format!("{directory}/{}", base.strip_suffix(".rs").unwrap_or(base))
        };
        for mount in mount_declarations(text) {
            let inline = mount.inline.join("/");
            let anchor = if inline.is_empty() {
                own.clone()
            } else {
                format!("{own}/{inline}")
            };
            let candidates = match (&mount.name, &mount.path) {
                (None, Some(attr)) => vec![normalize(&format!("{directory}/{attr}"))],
                (Some(_), Some(attr)) => vec![normalize(&format!(
                    "{}/{attr}",
                    if mount.inline.is_empty() {
                        directory
                    } else {
                        &anchor
                    }
                ))],
                (Some(name), None) => vec![
                    format!("{anchor}/{name}.rs"),
                    format!("{anchor}/{name}/mod.rs"),
                ],
                _ => Vec::new(),
            };
            if let Some(target) = candidates.into_iter().find(|p| files.contains_key(p)) {
                if mounted.insert(target.clone()) {
                    if mount.name.is_some() && mount.path.is_some() {
                        path_mounted.insert(target.clone());
                    }
                    queue.push_back(target);
                }
            }
        }
    }
    mounted
}
pub fn target_roots(files: &Sources) -> Vec<String> {
    static ROOT: LazyLock<Re> = LazyLock::new(|| {
        Re::new(r"^crates/[^/]+/src/(?:lib\.rs|main\.rs|bin/[^/]+\.rs|bin/[^/]+/main\.rs)$")
    });
    files.keys().filter(|p| ROOT.is_match(p)).cloned().collect()
}
pub fn unmounted_test_sources(files: &Sources) -> Result<Vec<String>, String> {
    let mut mounted = BTreeSet::new();
    for root in target_roots(files) {
        mounted.extend(mounted_sources(files, &root));
    }
    let mut missing = Vec::new();
    for (path, text) in files {
        if !mounted.contains(path) && !extract_tests(path, text)?.is_empty() {
            missing.push(path.clone());
        }
    }
    Ok(missing)
}
pub fn host_excluded_line_ranges(
    lines: &[&str],
    host: &Host,
) -> Result<Vec<(usize, usize)>, String> {
    static BLOCK: LazyLock<Re> = LazyLock::new(|| {
        Re::new(r"^([ \t]*)(?:pub(?:\([^)]*\))?\s+)?mod\s+[A-Za-z_][A-Za-z_0-9]*\s*\{\s*$")
    });
    let mut ranges = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if i == 0 || !cfg::host_excluded(lines[i - 1], host) {
            continue;
        }
        let Some(m) = BLOCK.search(line) else {
            continue;
        };
        let closing = format!("{}}}", re::group(&m, 1));
        let end = (i + 1..lines.len())
            .find(|j| lines[*j].trim_end() == closing)
            .ok_or_else(|| {
                format!(
                    "cannot find the end of an OS-gated module at line {}",
                    i + 1
                )
            })?;
        ranges.push((i + 1, end + 1));
    }
    Ok(ranges)
}
pub fn host_excluded_module_files(
    files: &Sources,
    host: &Host,
) -> Result<BTreeSet<String>, String> {
    let mut excluded = BTreeSet::new();
    for (path, text) in files {
        let lines: Vec<&str> = text.lines().collect();
        for line in &lines {
            let s = line.trim();
            if s.starts_with("#![") && cfg::host_excluded(&s.replacen("#![", "#[", 1), host) {
                excluded.insert(path.clone());
            } else if !s.is_empty() && !s.starts_with("//") && !s.starts_with("#![") {
                break;
            }
        }
        for (i, line) in lines.iter().enumerate() {
            if i == 0 || !cfg::host_excluded(lines[i - 1], host) {
                continue;
            }
            let Some(m) = MOD.search(line).filter(|m| re::group(m, 3) == ";") else {
                continue;
            };
            let name = re::group(&m, 2);
            let (directory, base) = path.rsplit_once('/').unwrap_or(("", path));
            let module_dir = if matches!(base, "mod.rs" | "lib.rs" | "main.rs") {
                directory.to_string()
            } else {
                path.trim_end_matches(".rs").into()
            };
            let prefix = format!("{module_dir}/");
            let matches: Vec<&String> = files
                .keys()
                .filter(|p| {
                    p.starts_with(&prefix)
                        && (p.ends_with(&format!("/{name}.rs"))
                            || p.ends_with(&format!("/{name}/mod.rs")))
                })
                .collect();
            if matches.len() != 1 {
                return Err(format!(
                    "{path}:{}: cannot resolve OS-gated module {name}",
                    i + 1
                ));
            }
            excluded.insert(matches[0].clone());
        }
    }
    Ok(excluded)
}
#[derive(Clone)]
pub struct CfgRange {
    pub start: usize,
    pub end: usize,
    pub predicate: cfg::Predicate,
}
pub fn module_cfg_ranges(text: &str) -> Result<Vec<CfgRange>, String> {
    let code = crate::rust_source::mask(text, "module mount")?;
    let mut pairs = BTreeMap::new();
    let mut stack = Vec::new();
    for (i, b) in code.bytes().enumerate() {
        if b"[{(".contains(&b) {
            stack.push((b, i));
        } else if let Some(&(open, start)) = stack.last() {
            if matches!((open, b), (b'[', b']') | (b'{', b'}') | (b'(', b')')) {
                stack.pop();
                pairs.insert(start, i);
            }
        }
    }
    let mut ranges = Vec::new();
    for attr in cfg::cfg_attributes(text, &code, "module mount")? {
        if attr.cfg_attr {
            continue;
        }
        let predicate = cfg::parse_cfg(&text[attr.predicate_start..attr.predicate_end])?;
        if attr.inner {
            let end = pairs
                .iter()
                .filter(|(a, b)| {
                    code.as_bytes()[**a] == b'{' && **a < attr.start && attr.start < **b
                })
                .min_by_key(|(a, b)| **b - **a)
                .map_or(code.len(), |(_, b)| *b);
            ranges.push(CfgRange {
                start: attr.start,
                end,
                predicate,
            });
            continue;
        }
        let mut at = code[attr.end..]
            .find(']')
            .map_or(code.len(), |n| attr.end + n + 1);
        while at < code.len() {
            if code.as_bytes()[at].is_ascii_whitespace() {
                at += 1;
            } else if code[at..].starts_with("#[") && pairs.contains_key(&(at + 1)) {
                at = pairs[&(at + 1)] + 1;
            } else {
                break;
            }
        }
        let end = if let Some(m) = MOD.search(&code[at..]) {
            let terminator = at + re::end(&m) - 1;
            if code.as_bytes()[terminator] == b'{' {
                pairs
                    .get(&terminator)
                    .map(|e| *e + 1)
                    .ok_or_else(|| "unterminated module item".to_string())?
            } else {
                terminator + 1
            }
        } else if code[at..].starts_with("include!") {
            code[at..].find(';').map_or(at, |n| at + n + 1)
        } else {
            continue;
        };
        ranges.push(CfgRange {
            start: attr.start,
            end,
            predicate,
        });
    }
    Ok(ranges)
}
fn shard_module_source(text: &str, ranges: &[CfgRange], shard: &str, opt_in: bool) -> String {
    let mut masked = text.as_bytes().to_vec();
    for range in ranges {
        if !range.predicate.feature_values(shard, opt_in).1 {
            for b in &mut masked[range.start..range.end] {
                if *b != b'\n' {
                    *b = b' ';
                }
            }
        }
    }
    String::from_utf8(masked).unwrap_or_default()
}
pub fn check_shard_module_mounts(files: &Sources, inventory: &Inventory) -> Result<(), String> {
    let ranges: BTreeMap<String, Vec<CfgRange>> = files
        .iter()
        .map(|(p, t)| module_cfg_ranges(t).map(|r| (p.clone(), r)))
        .collect::<Result<_, _>>()?;
    let shards: BTreeSet<&str> = inventory
        .tests
        .iter()
        .filter(|t| t.shard != "bin")
        .map(|t| t.shard.as_str())
        .collect();
    for shard in shards {
        let view = |opt_in| -> Sources {
            files
                .iter()
                .map(|(p, t)| (p.clone(), shard_module_source(t, &ranges[p], shard, opt_in)))
                .collect()
        };
        let actual = view(false);
        let permissive = view(true);
        for package in PACKAGES {
            let root = format!("crates/{package}/src/lib.rs");
            if !files.contains_key(&root) {
                continue;
            }
            let reached = mounted_sources(&actual, &root);
            let possible = mounted_sources(&permissive, &root);
            for test in &inventory.tests {
                if test.shard != shard
                    || package_of(&test.source)? != *package
                    || !possible.contains(&test.source)
                {
                    continue;
                }
                let visible = |v: &Sources| {
                    v[&test.source]
                        .lines()
                        .nth(test.line - 1)
                        .is_some_and(|line| ATTR.is_match(line))
                };
                if visible(&permissive) && !(reached.contains(&test.source) && visible(&actual)) {
                    return Err(format!(
                        "{}:{}: test {} declares shard {shard}, but an enclosing module cfg excludes that shard without opt-in features; make the module mount include all(test, feature = \"test-shard-{shard}\") (runtime exceptions cannot waive this)",
                        test.source, test.line, test.name
                    ));
                }
            }
        }
    }
    Ok(())
}

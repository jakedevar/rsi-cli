//! Released migration inventory and append-only policy (#361).
//!
//! Digests cover exactly the historical source regions, including line endings.
//! Refresh derives pins from source and never permits editing a released region.
use crate::{
    Ctx,
    re::{Re, group},
};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Write,
};

pub type Result<T> = std::result::Result<T, String>;
pub type Files = BTreeMap<String, String>;
pub const STORE_ROOTS: [&str; 2] = ["crates/rsid-store/src/store", "crates/rsid/src/store"];
pub const MIGRATION_PATH: &str = "crates/rsid-store/src/store/mod.rs";
pub const MIGRATION_DIR: &str = "crates/rsid-store/src/store/migrations";
pub const MANIFEST: &str = "tools/released-migrations.json";
pub const DECLARATIONS: &str = "tools/provisional-migrations";
pub const REWIND_TESTS: &str = "crates/rsid-store/src/store/tests.rs";
pub const ALTERED_OBJECTS: &str = "crates/rsid-store/src/store/migrations/altered_objects.rs";
pub const TOKEN: &str = "${VERSION}";

pub fn digest(data: impl AsRef<[u8]>) -> String {
    format!("sha256:{:x}", Sha256::digest(data.as_ref()))
}
fn object(v: &Value) -> Result<&Map<String, Value>> {
    v.as_object().ok_or_else(|| "expected a JSON object".into())
}
fn number(v: &Value) -> Result<i64> {
    v.as_i64()
        .ok_or_else(|| "expected an integer version".into())
}
fn head(v: &Value) -> Result<i64> {
    number(&v["latest_schema_version"])
}
fn version_keys(v: &Value) -> Result<BTreeSet<i64>> {
    object(&v["blocks"])?
        .keys()
        .map(|k| {
            k.parse()
                .map_err(|_| format!("invalid migration version {k}"))
        })
        .collect()
}
fn sections(v: &Value) -> Map<String, Value> {
    v["protected_sections"]
        .as_object()
        .cloned()
        .unwrap_or_default()
}
fn is_mod(path: &str) -> bool {
    STORE_ROOTS.iter().any(|r| path == format!("{r}/mod.rs"))
}

pub fn migration_file_version(path: &str) -> Result<Option<i64>> {
    let Some((dir, name)) = path.rsplit_once('/') else {
        return Ok(None);
    };
    if !STORE_ROOTS
        .iter()
        .any(|root| dir == format!("{root}/migrations"))
    {
        return Ok(None);
    }
    let Some(digits) = name.strip_prefix('v').and_then(|s| s.strip_suffix(".rs")) else {
        return Ok(None);
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Ok(None);
    }
    let version: i64 = digits
        .parse()
        .map_err(|_| format!("invalid migration version in {path}"))?;
    if name != format!("v{version:03}.rs") {
        return Err(format!(
            "migration file {path} must be named v{version:03}.rs"
        ));
    }
    Ok(Some(version))
}

pub fn latest_schema_version(files: &Files) -> Result<i64> {
    let versions = files
        .keys()
        .map(|p| migration_file_version(p))
        .collect::<Result<Vec<_>>>()?;
    if let Some(latest) = versions.into_iter().flatten().max() {
        return Ok(latest);
    }
    let text = STORE_ROOTS
        .iter()
        .find_map(|r| files.get(&format!("{r}/mod.rs")))
        .map_or("", String::as_str);
    Re::new(r"pub const LATEST_SCHEMA_VERSION: i32 = (\d+);")
        .search(text)
        .and_then(|c| group(&c, 1).parse().ok())
        .ok_or_else(|| {
            format!("cannot find the schema head in {MIGRATION_DIR} or {MIGRATION_PATH}")
        })
}

pub fn migration_blocks(files: &Files) -> Result<BTreeMap<i64, String>> {
    let block = Re::new(r"^(?P<indent>\s*)if version < (?P<version>\d+) \{\s*$");
    let mut blocks = BTreeMap::new();
    for (path, text) in files {
        let file_version = migration_file_version(path)?;
        if file_version.is_none() && !is_mod(path) {
            continue;
        }
        let lines: Vec<&str> = text.split_inclusive('\n').collect();
        if let Some(start) = lines
            .iter()
            .position(|l| l.contains("// V0: Original schema"))
        {
            let end = (start + 1..lines.len())
                .find(|&i| {
                    lines[i].contains("// V1: Session metadata columns")
                        || lines[i].trim_end_matches(['\r', '\n']) == "        Ok(())"
                })
                .ok_or("cannot locate the released V0 migration region")?;
            if blocks.contains_key(&0) {
                return Err("duplicate migration block for V0".into());
            }
            if file_version.is_some_and(|v| v != 0) {
                return Err(format!("V0 region belongs in v000.rs, found in {path}"));
            }
            blocks.insert(0, lines[start..end].concat());
        }
        for (start, line) in lines.iter().enumerate() {
            let Some(c) = block.search(line.trim_end_matches('\n')) else {
                continue;
            };
            let version: i64 = group(&c, 2)
                .parse()
                .map_err(|_| "invalid migration block version")?;
            if blocks.contains_key(&version) {
                return Err(format!("duplicate migration block for V{version}"));
            }
            if file_version.is_some_and(|v| v != version) {
                return Err(format!("migration block V{version} found in {path}"));
            }
            let closing = format!("{}}}", group(&c, 1));
            let end = (start + 1..lines.len())
                .find(|&i| lines[i].trim_end_matches(['\r', '\n']) == closing)
                .ok_or_else(|| format!("unterminated migration block for V{version}"))?;
            blocks.insert(version, lines[start..=end].concat());
        }
    }
    if !blocks.contains_key(&0) {
        return Err("cannot locate the released V0 migration region".into());
    }
    Ok(blocks)
}

pub fn protected_sections(files: &Files) -> Result<Value> {
    let begin = Re::new(r"^\s*// RSI-RELEASED-MIGRATION-BEGIN: ([a-z0-9-]+)\s*$");
    let end = Re::new(r"^\s*// RSI-RELEASED-MIGRATION-END: ([a-z0-9-]+)\s*$");
    let mut sections = Map::new();
    for (path, text) in files {
        let lines: Vec<&str> = text.split_inclusive('\n').collect();
        let mut open = BTreeMap::new();
        for (index, line) in lines.iter().enumerate() {
            if let Some(c) = begin.search(line.trim_end_matches(['\r', '\n'])) {
                let name = group(&c, 1).to_string();
                if sections.contains_key(&name) || open.contains_key(&name) {
                    return Err(format!("duplicate protected migration section {name}"));
                }
                open.insert(name, index);
            } else if let Some(c) = end.search(line.trim_end_matches(['\r', '\n'])) {
                let name = group(&c, 1);
                let start = open
                    .remove(name)
                    .ok_or_else(|| format!("unmatched protected migration section end {name}"))?;
                sections.insert(
                    name.into(),
                    json!({"path":path,"sha256":digest(lines[start..=index].concat())}),
                );
            }
        }
        if !open.is_empty() {
            return Err(format!(
                "unterminated protected migration section(s): {}",
                open.keys().cloned().collect::<Vec<_>>().join(", ")
            ));
        }
    }
    Ok(Value::Object(sections))
}

pub fn inventory(files: &Files) -> Result<Value> {
    let mut layouts = BTreeSet::new();
    for p in files.keys() {
        if migration_file_version(p)?.is_some() {
            if let Some((d, _)) = p.rsplit_once('/') {
                layouts.insert(d.to_string());
            }
        }
    }
    if layouts.len() > 1 {
        return Err(format!(
            "migration files exist in more than one store layout ({}); only {MIGRATION_DIR} is built, move them there",
            layouts.iter().cloned().collect::<Vec<_>>().join(", ")
        ));
    }
    let latest = latest_schema_version(files)?;
    let blocks = migration_blocks(files)?;
    if !blocks.contains_key(&latest) {
        return Err(format!(
            "no migration block found for schema head V{latest}"
        ));
    }
    let mut result = Map::new();
    result.insert("schema_version".into(), json!(1));
    result.insert("latest_schema_version".into(), json!(latest));
    if let Some(dir) = layouts.first() {
        result.insert("migration_dir".into(), json!(dir));
    } else {
        result.insert(
            "migration_file".into(),
            json!(
                STORE_ROOTS
                    .iter()
                    .map(|r| format!("{r}/mod.rs"))
                    .find(|p| files.contains_key(p))
                    .unwrap_or_else(|| MIGRATION_PATH.into())
            ),
        );
    }
    result.insert(
        "blocks".into(),
        Value::Object(
            blocks
                .iter()
                .filter(|(v, _)| **v <= latest)
                .map(|(v, b)| (v.to_string(), json!(digest(b))))
                .collect(),
        ),
    );
    result.insert("protected_sections".into(), protected_sections(files)?);
    Ok(Value::Object(result))
}

pub fn without_layout(v: &Value) -> Value {
    Value::Object(
        object(v)
            .into_iter()
            .flatten()
            .filter(|(k, _)| !matches!(k.as_str(), "migration_dir" | "migration_file"))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
    )
}
pub fn validate_manifest(manifest: &Value, actual: &Value, label: &str) -> Result<()> {
    if without_layout(manifest) == without_layout(actual) {
        return Ok(());
    }
    let mut details = Vec::new();
    for (key, prefix) in [
        ("blocks", "migration blocks "),
        ("protected_sections", "protected sections "),
    ] {
        let keys: BTreeSet<_> = [manifest, actual]
            .into_iter()
            .flat_map(|v| {
                v[key]
                    .as_object()
                    .into_iter()
                    .flat_map(|m| m.keys().cloned())
            })
            .collect();
        let changed: Vec<_> = keys
            .into_iter()
            .filter(|k| manifest[key][k] != actual[key][k])
            .map(|k| if key == "blocks" { format!("V{k}") } else { k })
            .collect();
        if !changed.is_empty() {
            details.push(format!("{prefix}{}", changed.join(", ")))
        }
    }
    if manifest["latest_schema_version"] != actual["latest_schema_version"] {
        details.push("LATEST_SCHEMA_VERSION".into())
    }
    Err(format!(
        "{label} does not match its source: {}",
        if details.is_empty() {
            "manifest metadata".into()
        } else {
            details.join("; ")
        }
    ))
}

pub fn refreshed_manifest(existing: &Value, scanned: &Value) -> Result<Value> {
    let mut details = Vec::new();
    for (key, removed, changed) in [
        (
            "blocks",
            "removed migration blocks ",
            "changed migration blocks ",
        ),
        (
            "protected_sections",
            "removed protected sections ",
            "changed protected sections ",
        ),
    ] {
        let old = if key == "blocks" {
            object(&existing[key])?.clone()
        } else {
            sections(existing)
        };
        for (is_removed, prefix) in [(true, removed), (false, changed)] {
            let names: Vec<_> = old
                .iter()
                .filter(|(k, v)| {
                    let a = &scanned[key][*k];
                    if is_removed {
                        a.is_null()
                    } else {
                        !a.is_null()
                            && if key == "blocks" {
                                a != *v
                            } else {
                                a["sha256"] != v["sha256"]
                            }
                    }
                })
                .map(|(k, _)| {
                    if key == "blocks" {
                        format!("V{k}")
                    } else {
                        k.clone()
                    }
                })
                .collect();
            if !names.is_empty() {
                details.push(format!("{prefix}{}", names.join(", ")))
            }
        }
    }
    if !details.is_empty() {
        return Err(details.join("; "));
    }
    let mut result = Map::new();
    for k in [
        "schema_version",
        "latest_schema_version",
        "migration_file",
        "migration_dir",
    ] {
        let v = if k == "schema_version" {
            existing.get(k)
        } else {
            scanned.get(k)
        };
        if let Some(v) = v {
            result.insert(k.into(), v.clone());
        }
    }
    for (k, v) in object(existing)? {
        if !result.contains_key(k) && !matches!(k.as_str(), "migration_dir" | "migration_file") {
            result.insert(k.clone(), v.clone());
        }
    }
    let mut blocks = object(&existing["blocks"])?.clone();
    for (k, v) in object(&scanned["blocks"])? {
        blocks.entry(k.clone()).or_insert_with(|| v.clone());
    }
    result.insert("blocks".into(), Value::Object(blocks));
    let mut secs = Map::new();
    for k in sections(existing).keys() {
        secs.insert(k.clone(), scanned["protected_sections"][k].clone());
    }
    for (k, v) in sections(scanned) {
        secs.entry(k).or_insert(v);
    }
    result.insert("protected_sections".into(), Value::Object(secs));
    Ok(Value::Object(result))
}

pub fn validated_inventory(
    manifest: &Value,
    actual: &Value,
    label: &str,
    base: Option<&Value>,
) -> Result<(Value, bool)> {
    match validate_manifest(manifest, actual, label) {
        Ok(()) => Ok((manifest.clone(), false)),
        Err(error) => {
            if let Some(base) = base {
                if without_layout(base) == without_layout(manifest) {
                    if let Ok(derived) = refreshed_manifest(base, actual) {
                        return Ok((derived, true));
                    }
                }
            }
            Err(error)
        }
    }
}

pub fn validate_append_only(base: &Value, new: &Value, gaps: &[i64]) -> Result<()> {
    let (b, h) = (head(base)?, head(new)?);
    if h < b {
        return Err(format!("schema head regressed from V{b} to V{h}"));
    }
    for (v, fp) in object(&base["blocks"])? {
        if new["blocks"][v] != *fp {
            return Err(format!(
                "released migration block V{v} changed relative to base"
            ));
        }
    }
    for (name, s) in sections(base) {
        if new["protected_sections"][&name]["sha256"] != s["sha256"] {
            return Err(format!(
                "released migration section {name} changed relative to base"
            ));
        }
    }
    let versions = version_keys(new)?
        .difference(&version_keys(base)?)
        .copied()
        .collect::<BTreeSet<_>>();
    if h == b && !versions.is_empty() {
        return Err("new migration blocks require a LATEST_SCHEMA_VERSION bump".into());
    }
    if versions.iter().any(|v| *v <= b) {
        return Err("previously released migration pins may not be added retroactively".into());
    }
    if h > b {
        let gaps: BTreeSet<_> = gaps.iter().copied().collect();
        if gaps.iter().any(|v| !(b < *v && *v < h)) {
            return Err(format!(
                "declared pending predecessors {:?} must lie between V{b} and V{h}",
                gaps.iter().collect::<Vec<_>>()
            ));
        }
        // Compare cardinality first, avoiding an unbounded allocation on malformed input.
        if h - b - gaps.len() as i64 != versions.len() as i64
            || versions
                .iter()
                .any(|v| !(b < *v && *v <= h) || gaps.contains(v))
        {
            return Err("schema version bump must append one pinned migration block for every new version (a slice numbered after an in-flight sibling declares it as `pending_predecessors` in its tools/provisional-migrations declaration, #1285)".into());
        }
    }
    Ok(())
}

pub fn parse_pending_predecessors(texts: &[String], base_latest: i64) -> Result<Vec<i64>> {
    let mut gaps = BTreeSet::new();
    for text in texts {
        let d: Value = serde_json::from_str(text)
            .map_err(|e| format!("provisional declaration is not valid JSON: {e}"))?;
        if !d.is_object() {
            return Err("provisional declaration must be a JSON object".into());
        }
        let Some(pending) = d.get("pending_predecessors").filter(|v| !v.is_null()) else {
            continue;
        };
        let valid = pending
            .as_array()
            .filter(|v| !v.is_empty() && v.len() <= 8 && v.iter().all(|n| n.as_i64().is_some()));
        let (Some(p), Some(v)) = (valid, d["version"].as_i64()) else {
            return Err("provisional declaration pending_predecessors must be a short list of integer versions beside an integer version".into());
        };
        let values = p.iter().filter_map(Value::as_i64).collect::<BTreeSet<_>>();
        if values.len() != p.len() || values.iter().any(|n| *n >= v) {
            return Err(format!(
                "pending_predecessors {} must be distinct and below the declared V{v}",
                serde_json::to_string(p)
                    .unwrap_or_default()
                    .replace(',', ", ")
            ));
        }
        gaps.extend(values.into_iter().filter(|n| *n > base_latest));
    }
    Ok(gaps.into_iter().collect())
}

fn key_problem(value: &Value, required: &[&str], optional: &[&str]) -> Option<String> {
    let Some(o) = value.as_object() else {
        return Some("must be a JSON object".into());
    };
    let mut missing = required
        .iter()
        .filter(|k| !o.contains_key(**k))
        .map(|k| format!("`{k}`"))
        .collect::<Vec<_>>();
    missing.sort();
    let mut extra = o
        .keys()
        .filter(|k| !required.contains(&k.as_str()) && !optional.contains(&k.as_str()))
        .map(|k| format!("`{k}`"))
        .collect::<Vec<_>>();
    extra.sort();
    let mut parts = Vec::new();
    if !missing.is_empty() {
        parts.push(format!("missing key {}", missing.join(", ")))
    }
    if !extra.is_empty() {
        parts.push(format!("unexpected key {}", extra.join(", ")))
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("; "))
    }
}
pub fn validate_declaration_shape(d: &Value, name: &str, expected: Option<i64>) -> Result<()> {
    let label = format!("provisional declaration {name}");
    if let Some(p) = key_problem(
        d,
        &["schema_version", "version", "files"],
        &["pending_predecessors"],
    ) {
        return Err(format!("{label}: {p}"));
    }
    if d["schema_version"].as_i64() != Some(1) {
        return Err(format!("{label}: `schema_version` must be 1"));
    }
    let v = number(&d["version"])
        .ok()
        .filter(|v| *v >= 1)
        .ok_or_else(|| format!("{label}: `version` must be a positive integer"))?;
    if expected.is_some_and(|e| e != v) {
        return Err(format!(
            "{label}: `version` {v} does not match the migration V{}",
            expected.unwrap_or_default()
        ));
    }
    let files = d["files"]
        .as_array()
        .filter(|f| !f.is_empty())
        .ok_or_else(|| format!("{label}: `files` must be a non-empty list"))?;
    for (i, file) in files.iter().enumerate() {
        let filename = file["path"]
            .as_str()
            .map_or_else(|| format!("files[{i}]"), String::from);
        let label = format!("{label}: file entry {filename}");
        if let Some(p) = key_problem(
            file,
            &["path", "path_template", "source_blob", "sites"],
            &[],
        ) {
            return Err(format!("{label}: {p}"));
        }
        let sites = file["sites"]
            .as_array()
            .ok_or_else(|| format!("{label}: `sites` must be a list"))?;
        for key in ["path", "path_template", "source_blob"] {
            if file[key].as_str().is_none_or(str::is_empty) {
                return Err(format!("{label}: `{key}` must be a non-empty string"));
            }
        }
        let template = file["path_template"].as_str().unwrap_or_default();
        if !template.contains(TOKEN) && file["path_template"] != file["path"] {
            return Err(format!(
                "{label}: `path_template` must equal `path` or carry {TOKEN}"
            ));
        }
        for (i, site) in sites.iter().enumerate() {
            let label = format!("{label}: sites[{i}]");
            if let Some(p) = key_problem(site, &["anchor", "replacement", "scope"], &[]) {
                return Err(format!("{label}: {p}"));
            }
            if !matches!(site["scope"].as_str(), Some("unit" | "head")) {
                return Err(format!("{label}: `scope` must be `unit` or `head`"));
            }
            let (Some(a), Some(r)) = (
                site["anchor"].as_str().filter(|a| !a.is_empty()),
                site["replacement"].as_str(),
            ) else {
                return Err(format!(
                    "{label}: `anchor` and `replacement` must be non-empty strings"
                ));
            };
            if !r.contains(TOKEN) || r.replace(TOKEN, &v.to_string()) != a {
                return Err(format!(
                    "{label}: `replacement` must be `anchor` with the version {v} replaced by {TOKEN}"
                ));
            }
        }
    }
    Ok(())
}

pub fn validate_rewind(files: &Files, tests: Option<&str>) -> Result<()> {
    if let Some(c) = Re::new(r"const REWIND_TEARDOWN_COVERED_THROUGH: i32 = (\d+);")
        .search(tests.unwrap_or_default())
    {
        let covered: i64 = group(&c, 1).parse().map_err(|_| "invalid rewind version")?;
        let latest = latest_schema_version(files)?;
        if covered != latest {
            return Err(format!(
                "REWIND_TEARDOWN_COVERED_THROUGH ({covered}) does not match the schema head (V{latest}). Set it to {latest} in {REWIND_TESTS} and add the V{latest} rewind step (`active >= {latest}`) in rewind_post_v121_tail_to."
            ));
        }
    }
    Ok(())
}

pub fn altered_objects_table(text: &str) -> Result<BTreeMap<i64, BTreeSet<String>>> {
    let table =
        Re::new(r"(?s)MIGRATION_ALTERED_OBJECTS:\s*&\[\(i32,\s*&\[&str\]\)\]\s*=\s*&\[(.*?)\n\];");
    let c = table
        .search(text)
        .ok_or_else(|| format!("cannot find MIGRATION_ALTERED_OBJECTS in {ALTERED_OBJECTS}"))?;
    let body = group(&c, 1)
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    let entries = Re::new(r"\(\s*(\d+)\s*,\s*&\[([^\]]*)\]\s*,?\s*\)").find_all(&body);
    let starts: BTreeSet<_> = entries.iter().map(crate::re::start).collect();
    for start in Re::new(r"\(\s*\d+\s*,").find_all(&body) {
        let offset = crate::re::start(&start);
        if !starts.contains(&offset) {
            let line = body[..offset].bytes().filter(|b| *b == b'\n').count() + 1;
            let shown = body.lines().nth(line - 1).unwrap_or_default().trim();
            return Err(format!(
                "unparseable MIGRATION_ALTERED_OBJECTS entry in {ALTERED_OBJECTS} (table body line {line}): `{shown}`. Expected `(version, &[\"object\", ...]),` (rustfmt multiline and a trailing comma are fine)."
            ));
        }
    }
    let names = Re::new(r#""([^"]+)""#);
    let mut result: BTreeMap<i64, BTreeSet<String>> = BTreeMap::new();
    for entry in entries {
        result
            .entry(
                group(&entry, 1)
                    .parse()
                    .map_err(|_| "invalid altered-object version")?,
            )
            .or_default()
            .extend(names.findall1(group(&entry, 2)));
    }
    Ok(result)
}

pub fn altered_by_migration(text: &str) -> (BTreeSet<String>, bool) {
    let code = text
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    let create = Re::new(
        r#"(?i)\bCREATE\s+(?:UNIQUE\s+)?(?:TEMP(?:ORARY)?\s+)?(?:TABLE|INDEX|TRIGGER|VIEW)\s+(?:IF\s+NOT\s+EXISTS\s+)?["`\[]?(\w+)"#,
    );
    let alter = Re::new(
        r#"(?i)\bALTER\s+TABLE\s+["`\[]?(\w+)["`\]]?\s+(?:RENAME\s+TO\s+["`\[]?(\w+)["`\]]?)?"#,
    );
    let drop =
        Re::new(r#"(?i)\bDROP\s+(?:TABLE|INDEX|TRIGGER|VIEW)\s+(?:IF\s+EXISTS\s+)?["`\[]?(\w+)"#);
    let created: BTreeSet<_> = create
        .findall1(&code)
        .into_iter()
        .map(|n| n.to_lowercase())
        .collect();
    let mut required = BTreeSet::new();
    for c in alter.find_all(&code) {
        let (name, target) = (group(&c, 1), group(&c, 2));
        if !target.is_empty() {
            required.insert(target.into());
            if !created.contains(&name.to_lowercase()) {
                required.insert(name.into());
            }
        } else {
            required.insert(name.into());
        }
    }
    for name in drop.findall1(&code) {
        if !created.contains(&name.to_lowercase()) {
            required.insert(name);
        }
    }
    (required, Re::new(r"\brebuild_\w+\s*\(").is_match(&code))
}
pub fn validate_altered(files: &Files, released: Option<i64>, table: Option<&str>) -> Result<()> {
    let (Some(released), Some(table)) = (released, table) else {
        return Ok(());
    };
    let listed = altered_objects_table(table)?;
    for (path, text) in files {
        let Some(v) = migration_file_version(path)?.filter(|v| *v > released) else {
            continue;
        };
        let (required, rebuilds) = altered_by_migration(text);
        let have = listed.get(&v).cloned().unwrap_or_default();
        let missing: Vec<_> = required.difference(&have).cloned().collect();
        if missing.is_empty() && !(rebuilds && have.is_empty()) {
            continue;
        }
        let names = have
            .union(&missing.iter().cloned().collect())
            .map(|s| format!("\"{s}\""))
            .collect::<Vec<_>>();
        let quoted = if names.is_empty() {
            "\"<object the rebuild rewrites>\"".into()
        } else {
            names.join(", ")
        };
        let reason = if missing.is_empty() {
            format!("V{v} ({path}) calls a rebuild_* helper")
        } else {
            format!(
                "V{v} ({path}) alters existing objects: {}",
                missing.join(", ")
            )
        };
        return Err(format!(
            "{reason}, but MIGRATION_ALTERED_OBJECTS has no entry for it. Add `({v}, &[{quoted}]),` to {ALTERED_OBJECTS}. Catalog-pin tests then accept the changed SQL of exactly those objects. This check is conservative (it matches ALTER TABLE, DROP and rebuild_* calls in the migration file, ignoring objects the file itself creates): a false refusal costs one line, a missed entry costs a merge-queue gate round."
        ));
    }
    Ok(())
}

pub fn git_show(ctx: &Ctx, rev: &str, path: &str) -> Option<String> {
    ctx.git()
        .args(["show", &format!("{rev}:{path}")])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.replace("\r\n", "\n"))
}
/// Read `rev:path`, telling "the path does not exist at that revision" (`Ok(None)`) from every
/// other failure (unresolvable revision, unreadable or corrupt object, process error: `Err`), so a
/// guard fails closed instead of treating an inconclusive read as an absent file.
pub fn git_show_opt(ctx: &Ctx, rev: &str, path: &str) -> Result<Option<String>> {
    let spec = format!("{rev}:{path}");
    let shown = ctx
        .git()
        .args(["show", &spec])
        .output()
        .map_err(|e| format!("cannot run git show {spec}: {e}"))?;
    if shown.status.success() {
        let text =
            String::from_utf8(shown.stdout).map_err(|_| format!("{spec} is not valid UTF-8"))?;
        return Ok(Some(text.replace("\r\n", "\n")));
    }
    let why = String::from_utf8_lossy(&shown.stderr).trim().to_string();
    git_lines(
        ctx,
        &["rev-parse", "--verify", &format!("{rev}^{{commit}}")],
    )
    .map_err(|e| format!("cannot read {spec}: revision does not resolve ({e})"))?;
    let listed = git_lines(ctx, &["ls-tree", "--name-only", rev, "--", path])
        .map_err(|e| format!("cannot read {spec}: {e}"))?;
    if listed.is_empty() {
        Ok(None)
    } else {
        Err(format!("cannot read {spec}: {why}"))
    }
}
fn git_lines(ctx: &Ctx, args: &[&str]) -> Result<Vec<String>> {
    let o = ctx.git().args(args).output().map_err(|e| e.to_string())?;
    if !o.status.success() {
        return Err(String::from_utf8_lossy(&o.stderr).trim().into());
    }
    Ok(String::from_utf8_lossy(&o.stdout)
        .lines()
        .map(String::from)
        .collect())
}
pub fn load_manifest(text: &str, label: &str) -> Result<Value> {
    let v: Value = serde_json::from_str(text).map_err(|e| format!("invalid {label}: {e}"))?;
    if v["schema_version"] != json!(1) {
        return Err(format!("unsupported {label} schema_version"));
    }
    object(&v["blocks"])?;
    head(&v)?;
    Ok(v)
}
pub fn tracked_source_paths(manifest: Option<&Value>) -> BTreeSet<String> {
    let root = manifest
        .and_then(|m| {
            m["migration_dir"]
                .as_str()
                .or_else(|| m["migration_file"].as_str())
        })
        .and_then(|p| p.rsplit_once('/').map(|(d, _)| d))
        .filter(|d| STORE_ROOTS.contains(d));
    let roots = root.map_or_else(|| STORE_ROOTS.to_vec(), |r| vec![r]);
    let mut paths = BTreeSet::new();
    for r in roots {
        for f in ["mod.rs", "cohort_settlement.rs"] {
            paths.insert(format!("{r}/{f}"));
        }
        if manifest.is_none() {
            paths.insert(format!("{r}/manager_prepared_actions.rs"));
        }
    }
    if let Some(m) = manifest {
        for s in sections(m).values() {
            if let Some(p) = s["path"].as_str() {
                paths.insert(p.into());
            }
        }
    }
    paths
}
pub fn load_revision_files(ctx: &Ctx, rev: &str, manifest: &Value) -> Result<Files> {
    let mut paths = tracked_source_paths(Some(manifest));
    let listing = git_lines(
        ctx,
        &[
            "ls-tree",
            "-r",
            "--name-only",
            rev,
            "--",
            "crates/rsid-store/src/store/migrations/",
            "crates/rsid/src/store/migrations/",
        ],
    )?;
    paths.extend(listing);
    let mut files = Files::new();
    for p in paths {
        if let Some(s) = git_show_opt(ctx, rev, &p)? {
            files.insert(p, s);
        }
    }
    if !files.keys().any(|p| is_mod(p)) {
        return Err(format!("{MIGRATION_PATH} is missing at {rev}"));
    }
    Ok(files)
}
fn read_declaration(ctx: &Ctx, rev: &str, name: &str) -> Result<Value> {
    let text =
        git_show_opt(ctx, rev, name)?.ok_or_else(|| format!("{name} is missing at {rev}"))?;
    let d: Value = serde_json::from_str(&text)
        .map_err(|e| format!("provisional declaration {name} is not valid JSON: {e}"))?;
    if !d.is_object() {
        return Err(format!(
            "provisional declaration {name} must be a JSON object"
        ));
    }
    Ok(d)
}
pub fn validated_base_inventory(
    ctx: &Ctx,
    manifest: &Value,
    actual: &Value,
    rev: &str,
) -> Result<Value> {
    let Err(original) = validate_manifest(manifest, actual, "base migration manifest") else {
        return Ok(manifest.clone());
    };
    let extra = version_keys(actual)?
        .difference(&version_keys(manifest)?)
        .copied()
        .collect::<BTreeSet<_>>();
    if extra.is_empty() {
        return Err(original);
    }
    let mut declarations = Vec::new();
    let mut covered = BTreeSet::new();
    for name in git_lines(
        ctx,
        &["ls-tree", "-r", "--name-only", rev, "--", DECLARATIONS],
    )? {
        if !name.ends_with(".json") {
            continue;
        }
        let d = read_declaration(ctx, rev, &name)?;
        if !d["version"].as_i64().is_some_and(|v| extra.contains(&v)) {
            continue;
        }
        validate_declaration_shape(&d, &name, None)?;
        if let Some(v) = d["version"].as_i64() {
            covered.insert(v);
        }
        declarations.push(d.to_string());
    }
    if covered != extra {
        return Err(original);
    }
    let derived = refreshed_manifest(manifest, actual)?;
    validate_append_only(
        manifest,
        &derived,
        &parse_pending_predecessors(&declarations, head(manifest)?)?,
    )?;
    validate_manifest(&derived, actual, "base migration inventory")?;
    Ok(derived)
}
fn declared_gaps(ctx: &Ctx, base: &str, new: &str, latest: i64) -> Result<Vec<i64>> {
    let mut texts = Vec::new();
    for name in git_lines(
        ctx,
        &[
            "diff",
            "--name-only",
            "--diff-filter=A",
            base,
            new,
            "--",
            DECLARATIONS,
        ],
    )
    .unwrap_or_default()
    {
        if !name.ends_with(".json") {
            continue;
        }
        let d = read_declaration(ctx, new, &name)?;
        validate_declaration_shape(&d, &name, None)?;
        texts.push(d.to_string());
    }
    parse_pending_predecessors(&texts, latest)
}
fn worktree_text(ctx: &Ctx, path: &str) -> Result<Option<String>> {
    match std::fs::read_to_string(ctx.path(path)) {
        Ok(s) => Ok(Some(s.replace("\r\n", "\n"))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}
fn merge_conflicted_manifest(ctx: &Ctx) -> Result<Value> {
    let mut sides = Vec::new();
    for stage in [":2", ":3"] {
        let text = git_show(ctx, stage, MANIFEST).ok_or_else(|| {
            format!("{MANIFEST} has conflict markers but no merge stages; resolve the merge first")
        })?;
        sides.push(load_manifest(
            &text,
            &format!("{MANIFEST} (stage {})", stage.trim_start_matches(':')),
        )?);
    }
    let mut merged = sides[0].clone();
    for (v, fp) in object(&sides[1]["blocks"])? {
        if let Some(old) = merged["blocks"].get(v) {
            if old != fp {
                return Err(format!("merged manifests pin different content for V{v}"));
            }
        }
        merged["blocks"][v] = fp.clone();
    }
    let sorted = version_keys(&merged)?
        .into_iter()
        .map(|v| {
            let k = v.to_string();
            let fp = merged["blocks"][&k].clone();
            (k, fp)
        })
        .collect();
    merged["blocks"] = Value::Object(sorted);
    for (name, sec) in sections(&sides[1]) {
        if let Some(old) = merged["protected_sections"].get(&name) {
            if old["sha256"] != sec["sha256"] {
                return Err(format!(
                    "merged manifests pin different content for section {name}"
                ));
            }
        } else {
            merged["protected_sections"][&name] = sec;
        }
    }
    merged["latest_schema_version"] = json!(head(&sides[0])?.max(head(&sides[1])?));
    Ok(merged)
}

/// A rewind test file without the covered-through constant is the old-tree skip only when the base
/// never had the constant either; once the base uses it, neither the file nor the constant may
/// silently disappear.
fn require_rewind_tests(ctx: &Ctx, base: Option<&str>, tests: Option<&str>) -> Result<()> {
    if tests.is_some_and(|t| t.contains("REWIND_TEARDOWN_COVERED_THROUGH")) {
        return Ok(());
    }
    if let Some(base) = base {
        let used = git_show_opt(ctx, base, REWIND_TESTS)?
            .is_some_and(|t| t.contains("REWIND_TEARDOWN_COVERED_THROUGH"));
        if used {
            return Err(format!(
                "{REWIND_TESTS} is missing or lacks the constant, but {base} uses REWIND_TEARDOWN_COVERED_THROUGH; restore the file so the rewind coverage check can run"
            ));
        }
    }
    Ok(())
}

pub fn check(ctx: &Ctx, base: &str, new: &str) -> Result<String> {
    let head_text = git_show_opt(ctx, new, MANIFEST)?
        .ok_or_else(|| format!("{MANIFEST} is missing at {new}"))?;
    let manifest = load_manifest(&head_text, &format!("{new}:{MANIFEST}"))?;
    let files = load_revision_files(ctx, new, &manifest)?;
    let actual = inventory(&files)?;
    let base_text = git_show_opt(ctx, base, MANIFEST)?;
    let bm = base_text
        .as_deref()
        .map(|s| load_manifest(s, &format!("{base}:{MANIFEST}")))
        .transpose()?;
    let (manifest, deferred) =
        validated_inventory(&manifest, &actual, "head migration manifest", bm.as_ref())?;
    let head_tests = git_show_opt(ctx, new, REWIND_TESTS)?;
    require_rewind_tests(ctx, Some(base), head_tests.as_deref())?;
    validate_rewind(&files, head_tests.as_deref())?;
    let Some(bm) = bm else {
        return Ok(format!(
            "released-migration guard: bootstrap manifest valid through V{} (base has no manifest)",
            head(&manifest)?
        ));
    };
    let ba = inventory(&load_revision_files(ctx, base, &bm)?)?;
    let bm = validated_base_inventory(ctx, &bm, &ba, base)?;
    validate_append_only(&bm, &manifest, &declared_gaps(ctx, base, new, head(&bm)?)?)?;
    validate_altered(
        &files,
        Some(head(&bm)?),
        git_show_opt(ctx, new, ALTERED_OBJECTS)?.as_deref(),
    )?;
    Ok(format!(
        "released-migration guard: PASS {base}..{new}; V0-V{} immutable, head V{}{}",
        head(&bm)?,
        head(&manifest)?,
        if deferred {
            "; pin deferred to the lander"
        } else {
            ""
        }
    ))
}

pub fn refresh(ctx: &Ctx, base: &str, extra: &[String]) -> Result<String> {
    if git_lines(
        ctx,
        &["rev-parse", "--verify", &format!("{base}^{{commit}}")],
    )
    .is_err()
    {
        return Err(format!(
            "refresh base revision {base} cannot be resolved; fetch it first"
        ));
    }
    let bm = git_show_opt(ctx, base, MANIFEST)?
        .map(|s| load_manifest(&s, &format!("{base}:{MANIFEST}")))
        .transpose()?;
    if let Some(m) = &bm {
        validate_manifest(
            m,
            &inventory(&load_revision_files(ctx, base, m)?)?,
            "refresh base migration manifest",
        )?;
    }
    let text = worktree_text(ctx, MANIFEST)?.unwrap_or_default();
    let existing = if text.is_empty() {
        bm.clone()
    } else if text
        .lines()
        .any(|l| l.starts_with("<<<<<<< ") || l.starts_with(">>>>>>> "))
    {
        Some(merge_conflicted_manifest(ctx)?)
    } else {
        Some(load_manifest(&text, MANIFEST)?)
    };
    let mut paths = tracked_source_paths(existing.as_ref());
    if let Some(m) = &bm {
        paths.extend(tracked_source_paths(Some(m)));
    }
    paths.extend(extra.iter().cloned());
    let output = ctx
        .git()
        .args([
            "grep",
            "--untracked",
            "-l",
            "-e",
            "RSI-RELEASED-MIGRATION-BEGIN",
            "--",
            ":(glob)crates/**/*.rs",
        ])
        .output()
        .map_err(|e| e.to_string())?;
    if !output.status.success() && output.status.code() != Some(1) {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    paths.extend(
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(String::from),
    );
    for root in STORE_ROOTS {
        let dir = format!("{root}/migrations");
        match std::fs::read_dir(ctx.path(&dir)) {
            Ok(entries) => {
                for entry in entries {
                    let entry = entry.map_err(|e| e.to_string())?;
                    if entry.path().is_file() && entry.path().extension().is_some_and(|e| e == "rs")
                    {
                        paths.insert(format!("{dir}/{}", entry.file_name().to_string_lossy()));
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.to_string()),
        }
    }
    let mut files = Files::new();
    for p in paths {
        if ctx.path(&p).is_file() {
            if let Some(t) = worktree_text(ctx, &p)? {
                files.insert(p, t);
            }
        }
    }
    let actual = inventory(&files)?;
    let work_tests = worktree_text(ctx, REWIND_TESTS)?;
    require_rewind_tests(ctx, Some(base), work_tests.as_deref())?;
    validate_rewind(&files, work_tests.as_deref())?;
    if let Some(b) = &bm {
        validate_altered(
            &files,
            Some(head(b)?),
            worktree_text(ctx, ALTERED_OBJECTS)?.as_deref(),
        )?;
    }
    let released = bm.as_ref().or(existing.as_ref());
    let updated = if let Some(released) = released {
        let updated = refreshed_manifest(released, &actual)?;
        let mut declarations = Vec::new();
        if ctx.path(DECLARATIONS).is_dir() {
            let mut entries = std::fs::read_dir(ctx.path(DECLARATIONS))
                .map_err(|e| e.to_string())?
                .collect::<std::io::Result<Vec<_>>>()
                .map_err(|e| e.to_string())?;
            entries.sort_by_key(|e| e.file_name());
            for e in entries {
                if e.path().extension().is_some_and(|e| e == "json") {
                    declarations
                        .push(std::fs::read_to_string(e.path()).map_err(|e| e.to_string())?);
                }
            }
        }
        validate_append_only(
            released,
            &updated,
            &parse_pending_predecessors(&declarations, head(released)?)?,
        )?;
        updated
    } else {
        actual
    };
    let updated_text = format!(
        "{}\n",
        serde_json::to_string_pretty(&updated).map_err(|e| e.to_string())?
    );
    if updated_text != text {
        std::fs::write(ctx.path(MANIFEST), updated_text).map_err(|e| e.to_string())?;
    }
    Ok(format!("refreshed {MANIFEST} through V{}", head(&updated)?))
}

pub fn main(args: &[String], ctx: &Ctx, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let mut refresh_mode = false;
    let mut extra = Vec::new();
    let mut positional = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--refresh" => refresh_mode = true,
            "--include-path" => {
                let Some(p) = it.next() else {
                    let _ = writeln!(err, "--include-path requires a value");
                    return 2;
                };
                extra.push(p.clone());
            }
            "-h" | "--help" => {
                let _ = writeln!(
                    out,
                    "usage: released-migrations [--refresh] [--include-path PATH] [base] [head]"
                );
                return 0;
            }
            s if s.starts_with('-') => {
                let _ = writeln!(err, "unrecognized argument: {s}");
                return 2;
            }
            _ => positional.push(a.clone()),
        }
    }
    if positional.len() > 2 || (!extra.is_empty() && !refresh_mode) {
        let _ = writeln!(
            err,
            "--include-path requires --refresh; expected at most base and head"
        );
        return 2;
    }
    let new = positional.get(1).map_or("HEAD", String::as_str);
    let default = if refresh_mode {
        "origin/rolling".into()
    } else {
        format!("{new}^")
    };
    let base = positional.first().map_or(default.as_str(), String::as_str);
    let result = if refresh_mode {
        refresh(ctx, base, &extra)
    } else {
        check(ctx, base, new)
    };
    match result {
        Ok(message) => {
            let _ = writeln!(out, "{message}");
            0
        }
        Err(error) => {
            let _ = writeln!(err, "released-migration guard: FAIL: {error}");
            if !refresh_mode
                && positional.is_empty()
                && git_lines(ctx, &["rev-list", "--parents", "-n", "1", new])
                    .is_ok_and(|v| v.join(" ").split_whitespace().count() > 2)
            {
                let _ = writeln!(
                    err,
                    "The default base {new}^ is the first parent of a merge commit, not the base you branched from. Rerun against the real base: `scripts/repo-guard released-migrations origin/rolling {new}`."
                );
            }
            let _ = writeln!(
                err,
                "Released migration DDL, catalog projections, and fingerprints are immutable. Never repair them in place; add a forward migration. The pin file is derived: never edit it by hand or resolve its conflicts. The lander derives it when it publishes; to see it earlier run `scripts/repo-guard released-migrations --refresh`."
            );
            1
        }
    }
}

#[cfg(test)]
mod tests;

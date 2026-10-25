//! The receipt fields daemon decisions read, and the checks built on them.
//!
//! A field outside [`FIELDS`] is diagnostic: a runner may print it (`tool`,
//! `log_dir`, `selection`, ...), but no daemon decision depends on it, so two
//! runners may disagree on it. A field inside the table must have its shape,
//! and two runners given the same inputs must agree on its value
//! ([`consumer_projection`], [`compare`]).

use serde_json::{Map, Value};
use std::cmp::Ordering;
use std::fmt;

/// The JSON shape a field has when it is present and not null.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Bool,
    /// A non-negative integer.
    Count,
    /// Any integer.
    Integer,
    Text,
    /// An array of strings.
    TextList,
    /// An array of test failures: objects whose `package`, `kind`, `target`
    /// and `test`, when present, are strings.
    FailureList,
    /// An array of package outcomes: objects with a `package` and a `status`
    /// string, and, when present, a `completed` count and a `phase` string.
    PackageList,
    /// An array of anything.
    List,
    /// An object of anything.
    Object,
    /// The warning gate: an object whose `status`, when present, is a string.
    WarningGate,
}

impl Kind {
    const fn name(self) -> &'static str {
        match self {
            Self::Bool => "a bool",
            Self::Count => "a non-negative integer",
            Self::Integer => "an integer",
            Self::Text => "a string",
            Self::TextList => "an array of strings",
            Self::FailureList => "an array of failure objects",
            Self::PackageList => "an array of package objects",
            Self::List => "an array",
            Self::Object | Self::WarningGate => "an object",
        }
    }
}

/// Who writes a field into the stored receipt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Writer {
    /// The verification tool prints it in its receipt line.
    Runner,
    /// The daemon adds it to the stored receipt after the run
    /// (`agent_jobs`); a runner never prints it.
    Daemon,
}

/// One field of the contract.
#[derive(Debug, Clone, Copy)]
pub struct Field {
    pub key: &'static str,
    pub kind: Kind,
    /// Whether `null` is an accepted value.
    pub nullable: bool,
    /// Whether every receipt a runner prints carries it.
    pub required: bool,
    pub writer: Writer,
    /// The daemon code whose decision reads it; empty for a field every
    /// receipt carries that no decision reads.
    pub readers: &'static [&'static str],
}

const fn field(key: &'static str, kind: Kind, readers: &'static [&'static str]) -> Field {
    Field {
        key,
        kind,
        nullable: false,
        required: false,
        writer: Writer::Runner,
        readers,
    }
}

const fn required(mut field: Field) -> Field {
    field.required = true;
    field
}

const fn nullable(mut field: Field) -> Field {
    field.nullable = true;
    field
}

const fn daemon(mut field: Field) -> Field {
    field.writer = Writer::Daemon;
    field
}

const ASSESS: &str = "rsi_common::agent_landing_candidate::assess_candidate";
const CLASSIFY: &str = "rsid::agent_jobs::classify_scoped_test";
const REVIEW: &str = "rsid::topology::review_receipts";
const VERIFY: &str = "rsid::topology::verify";

/// Every receipt field a daemon decision reads (schema [`crate::SCHEMA`]).
pub const FIELDS: &[Field] = &[
    required(field(
        "schema",
        Kind::Count,
        &["rsi_scoped_receipt::contract::check"],
    )),
    required(field("ok", Kind::Bool, &[CLASSIFY, ASSESS, REVIEW])),
    required(field("exit_code", Kind::Integer, &[])),
    required(nullable(field(
        "head",
        Kind::Text,
        &["rsi_common::agent_landing_candidate::receipt_head", VERIFY],
    ))),
    required(nullable(field("base", Kind::Text, &[]))),
    required(field("dry_run", Kind::Bool, &[])),
    required(field(
        "packages",
        Kind::PackageList,
        &[ASSESS, VERIFY, "rsid::agent_jobs::scoped_receipt_summary"],
    )),
    field(
        "failures",
        Kind::FailureList,
        &[
            "rsid::agent_jobs::scoped_receipt_failing_tests",
            VERIFY,
            REVIEW,
        ],
    ),
    nullable(field("new_failures", Kind::FailureList, &[ASSESS])),
    field("baseline_reds", Kind::FailureList, &[ASSESS]),
    field("failure_class", Kind::Text, &[ASSESS]),
    field("plan", Kind::Text, &[ASSESS, REVIEW]),
    field("uncertain", Kind::Text, &[ASSESS]),
    field("stale_base", Kind::Text, &[ASSESS]),
    field("gate_covers_rest", Kind::Bool, &[ASSESS, REVIEW]),
    nullable(field("selected_tests", Kind::Count, &[ASSESS, REVIEW])),
    nullable(field("run_tests", Kind::Count, &[ASSESS])),
    nullable(field("merges_cleanly_onto_rolling", Kind::Bool, &[ASSESS])),
    field("conflict_paths", Kind::TextList, &[ASSESS]),
    nullable(field("derive_failure", Kind::List, &[VERIFY])),
    field("warnings", Kind::WarningGate, &[REVIEW]),
    daemon(field("partial", Kind::Bool, &[VERIFY])),
    daemon(field("termination", Kind::Object, &[])),
    daemon(field("job_timeout_minutes", Kind::Count, &[])),
    daemon(field("queue_wait_sec", Kind::Count, &[])),
];

/// The table entry for `key`.
#[must_use]
pub fn field_named(key: &str) -> Option<&'static Field> {
    FIELDS.iter().find(|field| field.key == key)
}

/// Where a receipt was read: the runner's own line, or the daemon's stored
/// copy, which may also carry the [`Writer::Daemon`] fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Runner,
    Stored,
}

/// One way a receipt breaks the contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    /// A JSON path such as `packages[2].status`.
    pub path: String,
    pub problem: String,
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path, self.problem)
    }
}

fn json_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a bool",
        Value::Number(number) if number.is_i64() || number.is_u64() => "an integer",
        Value::Number(_) => "a float",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

struct Checker {
    found: Vec<Violation>,
}

impl Checker {
    fn report(&mut self, path: String, problem: String) {
        self.found.push(Violation { path, problem });
    }

    fn expect(&mut self, path: &str, value: &Value, kind: Kind) {
        let fits = match kind {
            Kind::Bool => value.is_boolean(),
            Kind::Count => value.is_u64(),
            Kind::Integer => value.is_i64() || value.is_u64(),
            Kind::Text => value.is_string(),
            Kind::TextList | Kind::FailureList | Kind::PackageList | Kind::List => value.is_array(),
            Kind::Object | Kind::WarningGate => value.is_object(),
        };
        if !fits {
            self.report(
                path.to_owned(),
                format!("expected {}, found {}", kind.name(), json_type(value)),
            );
            return;
        }
        match kind {
            Kind::TextList => {
                for (index, item) in value.as_array().into_iter().flatten().enumerate() {
                    self.expect(&format!("{path}[{index}]"), item, Kind::Text);
                }
            }
            Kind::FailureList => {
                for (index, item) in value.as_array().into_iter().flatten().enumerate() {
                    self.entry(&format!("{path}[{index}]"), item, FAILURE_KEYS, &[]);
                }
            }
            Kind::PackageList => {
                for (index, item) in value.as_array().into_iter().flatten().enumerate() {
                    let at = format!("{path}[{index}]");
                    self.entry(&at, item, &["package", "status", "phase"], &["completed"]);
                    if let Some(entry) = item.as_object() {
                        for key in ["package", "status"] {
                            if !entry.contains_key(key) {
                                self.report(format!("{at}.{key}"), "missing".into());
                            }
                        }
                    }
                }
            }
            Kind::WarningGate => {
                if let Some(status) = value.get("status") {
                    self.expect(&format!("{path}.status"), status, Kind::Text);
                }
            }
            _ => {}
        }
    }

    /// An object whose `texts` keys, when present, are strings and whose
    /// `counts` keys, when present, are non-negative integers.
    fn entry(&mut self, path: &str, value: &Value, texts: &[&str], counts: &[&str]) {
        let Some(entry) = value.as_object() else {
            self.report(
                path.to_owned(),
                format!("expected an object, found {}", json_type(value)),
            );
            return;
        };
        for (keys, kind) in [(texts, Kind::Text), (counts, Kind::Count)] {
            for key in keys {
                if let Some(item) = entry.get(*key) {
                    self.expect(&format!("{path}.{key}"), item, kind);
                }
            }
        }
    }
}

const FAILURE_KEYS: &[&str] = &["package", "kind", "target", "test"];

/// Every way `receipt` breaks the contract; empty when it conforms.
///
/// Checked: the receipt is an object of schema [`crate::SCHEMA`]; every
/// required field is present; every present field of [`FIELDS`] has its kind
/// (or is null where the table allows it); a runner's own line carries no
/// daemon-owned field; and the verdict agrees with itself: a green receipt
/// (`ok` true) names no new failure and no interrupted infrastructure.
#[must_use]
pub fn check(receipt: &Value, origin: Origin) -> Vec<Violation> {
    let mut checker = Checker { found: Vec::new() };
    let Some(object) = receipt.as_object() else {
        checker.report(
            "$".into(),
            format!("expected an object, found {}", json_type(receipt)),
        );
        return checker.found;
    };
    for field in FIELDS {
        match object.get(field.key) {
            // Required fields are runner-written, so a stored copy needs them too.
            None if field.required => {
                checker.report(field.key.into(), "missing".into());
            }
            None => {}
            Some(_) if origin == Origin::Runner && field.writer == Writer::Daemon => {
                checker.report(
                    field.key.into(),
                    "the daemon writes this field after the run; a runner never prints it".into(),
                );
            }
            Some(Value::Null) if field.nullable => {}
            Some(value) => checker.expect(field.key, value, field.kind),
        }
    }
    if let Some(schema) = object.get("schema").and_then(Value::as_u64)
        && schema != crate::SCHEMA
    {
        checker.report(
            "schema".into(),
            format!("schema {schema}; this contract is schema {}", crate::SCHEMA),
        );
    }
    if object.get("ok").and_then(Value::as_bool) == Some(true) {
        if object
            .get("new_failures")
            .and_then(Value::as_array)
            .is_some_and(|list| !list.is_empty())
        {
            checker.report("ok".into(), "true while new_failures names failures".into());
        }
        if object.get("failure_class").and_then(Value::as_str) == Some("infrastructure_interrupted")
        {
            checker.report(
                "ok".into(),
                "true with failure_class infrastructure_interrupted".into(),
            );
        }
    }
    checker.found
}

/// The runner-written contract fields of `receipt`, in a canonical form.
///
/// List entries keep only their contract keys; failure lists, package lists and
/// conflict paths are sorted; the warning gate keeps only its `status`. Two
/// runners given the same inputs must produce equal projections; fields outside
/// the table are left out.
#[must_use]
pub fn consumer_projection(receipt: &Value) -> Value {
    let Some(object) = receipt.as_object() else {
        return Value::Object(Map::new());
    };
    Value::Object(sorted_map(
        FIELDS
            .iter()
            .filter(|field| field.writer == Writer::Runner)
            .filter_map(|field| {
                let value = object.get(field.key)?;
                Some((field.key.to_owned(), canonical(field.kind, value)))
            })
            .collect(),
    ))
}

/// A map built in key order, so it serializes the same whether or not another
/// crate of the build turns on `serde_json`'s `preserve_order`.
fn sorted_map(mut entries: Vec<(String, Value)>) -> Map<String, Value> {
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    entries.into_iter().collect()
}

fn keep(value: &Value, keys: &[&str]) -> Value {
    value.as_object().map_or_else(
        || value.clone(),
        |entry| {
            Value::Object(sorted_map(
                keys.iter()
                    .filter_map(|key| Some(((*key).to_owned(), entry.get(*key)?.clone())))
                    .collect(),
            ))
        },
    )
}

fn sorted(mut items: Vec<Value>) -> Value {
    items.sort_by(order);
    Value::Array(items)
}

/// A total order over JSON values that only needs to be deterministic.
fn order(a: &Value, b: &Value) -> Ordering {
    a.to_string().cmp(&b.to_string())
}

fn canonical(kind: Kind, value: &Value) -> Value {
    let Some(items) = value.as_array() else {
        return match (kind, value) {
            (Kind::WarningGate, Value::Object(_)) => keep(value, &["status"]),
            _ => value.clone(),
        };
    };
    match kind {
        Kind::FailureList => sorted(items.iter().map(|item| keep(item, FAILURE_KEYS)).collect()),
        Kind::PackageList => sorted(
            items
                .iter()
                .map(|item| keep(item, &["package", "status", "completed", "phase"]))
                .collect(),
        ),
        Kind::TextList => sorted(items.clone()),
        _ => value.clone(),
    }
}

/// One field on which two receipts disagree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Difference {
    pub field: &'static str,
    /// The field in the reference receipt (`None`: absent).
    pub expected: Option<Value>,
    /// The field in the receipt under test (`None`: absent).
    pub actual: Option<Value>,
}

impl fmt::Display for Difference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let show = |value: &Option<Value>| {
            value
                .as_ref()
                .map_or_else(|| "absent".to_owned(), Value::to_string)
        };
        write!(
            f,
            "{}: expected {}, found {}",
            self.field,
            show(&self.expected),
            show(&self.actual)
        )
    }
}

/// The contract fields on which `actual` differs from `expected`, in table
/// order, compared through [`consumer_projection`].
#[must_use]
pub fn compare(expected: &Value, actual: &Value) -> Vec<Difference> {
    let (expected, actual) = (consumer_projection(expected), consumer_projection(actual));
    FIELDS
        .iter()
        .filter_map(|field| {
            let (left, right) = (expected.get(field.key), actual.get(field.key));
            (left != right).then(|| Difference {
                field: field.key,
                expected: left.cloned(),
                actual: right.cloned(),
            })
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use serde_json::json;

    fn green() -> Value {
        json!({
            "schema": 1, "ok": true, "exit_code": 0, "head": "a".repeat(40),
            "base": "origin/rolling", "dry_run": false,
            "packages": [{"package": "rsid", "status": "passed", "completed": 3}],
            "failures": [], "new_failures": null, "baseline_reds": [],
            "plan": "precise", "merges_cleanly_onto_rolling": true, "conflict_paths": [],
            "warnings": {"status": "ok", "packages": {}}, "tool": {"source": "rolling"}
        })
    }

    fn problems(receipt: &Value, origin: Origin) -> Vec<String> {
        check(receipt, origin)
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    #[test]
    fn a_complete_green_receipt_conforms_from_the_runner_and_as_stored() {
        assert_eq!(problems(&green(), Origin::Runner), Vec::<String>::new());
        let mut stored = green();
        stored["job_timeout_minutes"] = json!(60);
        stored["queue_wait_sec"] = json!(12);
        assert_eq!(problems(&stored, Origin::Stored), Vec::<String>::new());
    }

    #[test]
    fn the_staged_runner_receipt_without_a_head_conforms() {
        // The shape the #423 staged runner prints: head and base unknown, red.
        let staged = json!({
            "schema": 1, "ok": false, "exit_code": 2, "dry_run": true, "head": null,
            "base": null, "packages": [], "failure_class": "runner_unavailable",
            "new_failures": null, "staged": true
        });
        assert_eq!(problems(&staged, Origin::Runner), Vec::<String>::new());
    }

    #[test]
    fn missing_required_fields_and_wrong_shapes_are_named_by_path() {
        let mut receipt = green();
        let object = receipt.as_object_mut().unwrap();
        object.remove("head");
        object.insert("ok".into(), json!("yes"));
        object.insert("selected_tests".into(), json!(-1));
        object.insert("conflict_paths".into(), json!(["a.rs", 7]));
        object.insert(
            "packages".into(),
            json!([{"package": "rsid", "completed": "3"}, "rsi"]),
        );
        object.insert("new_failures".into(), json!([{"test": 5}]));
        object.insert("warnings".into(), json!({"status": false}));
        assert_eq!(
            problems(&receipt, Origin::Runner),
            [
                "ok: expected a bool, found a string",
                "head: missing",
                "packages[0].completed: expected a non-negative integer, found a string",
                "packages[0].status: missing",
                "packages[1]: expected an object, found a string",
                "new_failures[0].test: expected a string, found an integer",
                "selected_tests: expected a non-negative integer, found an integer",
                "conflict_paths[1]: expected a string, found an integer",
                "warnings.status: expected a string, found a bool",
            ]
        );
    }

    #[test]
    fn a_runner_line_never_carries_daemon_stamps_or_another_schema() {
        let mut receipt = green();
        receipt["queue_wait_sec"] = json!(5);
        receipt["partial"] = json!(true);
        receipt["schema"] = json!(2);
        assert_eq!(
            problems(&receipt, Origin::Runner),
            [
                "partial: the daemon writes this field after the run; a runner never prints it",
                "queue_wait_sec: the daemon writes this field after the run; a runner never prints it",
                "schema: schema 2; this contract is schema 1",
            ]
        );
    }

    #[test]
    fn a_green_verdict_with_new_failures_or_interrupted_infrastructure_contradicts_itself() {
        let mut receipt = green();
        receipt["new_failures"] =
            json!([{"package": "rsid", "kind": "lib", "target": "", "test": "a::b"}]);
        receipt["failure_class"] = json!("infrastructure_interrupted");
        assert_eq!(
            problems(&receipt, Origin::Runner),
            [
                "ok: true while new_failures names failures",
                "ok: true with failure_class infrastructure_interrupted",
            ]
        );
        assert_eq!(
            problems(&json!([1]), Origin::Stored),
            ["$: expected an object, found an array"]
        );
    }

    #[test]
    fn the_projection_keeps_contract_fields_in_canonical_order_only() {
        let mut receipt = green();
        receipt["failures"] = json!([
            {"package": "rsid", "kind": "lib", "target": "", "test": "z", "log": "x"},
            {"package": "rsi", "kind": "lib", "target": "", "test": "a"}
        ]);
        receipt["queue_wait_sec"] = json!(9);
        let projection = consumer_projection(&receipt);
        assert_eq!(
            projection["failures"],
            json!([
                {"package": "rsi", "kind": "lib", "target": "", "test": "a"},
                {"package": "rsid", "kind": "lib", "target": "", "test": "z"}
            ])
        );
        assert_eq!(projection["warnings"], json!({"status": "ok"}));
        let keys: Vec<&str> = projection
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        let expected: Vec<&str> = FIELDS
            .iter()
            .filter(|field| field.writer == Writer::Runner)
            .map(|field| field.key)
            .filter(|key| receipt.get(*key).is_some())
            .collect();
        let mut expected_sorted = expected.clone();
        expected_sorted.sort_unstable();
        assert_eq!(keys, expected_sorted);
        assert_eq!(check(&projection, Origin::Runner), Vec::new());
    }

    #[test]
    fn compare_names_each_disagreeing_field_and_ignores_diagnostics_and_order() {
        let python = green();
        let mut rust = green();
        rust["tool"] = json!({"source": "rust"});
        rust["log_dir"] = json!("/tmp/elsewhere");
        assert_eq!(compare(&python, &rust), Vec::new());
        rust["packages"] = json!([{"package": "rsid", "status": "passed", "completed": 2}]);
        rust.as_object_mut().unwrap().remove("plan");
        let differences: Vec<String> = compare(&python, &rust)
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(
            differences,
            [
                "packages: expected [{\"completed\":3,\"package\":\"rsid\",\"status\":\"passed\"}], found [{\"completed\":2,\"package\":\"rsid\",\"status\":\"passed\"}]",
                "plan: expected \"precise\", found absent",
            ]
        );
    }

    #[test]
    fn every_field_is_listed_once_and_daemon_fields_are_never_required() {
        for (index, field) in FIELDS.iter().enumerate() {
            assert_eq!(
                FIELDS.iter().position(|other| other.key == field.key),
                Some(index),
                "{} is listed twice",
                field.key
            );
            if field.writer == Writer::Daemon {
                assert!(
                    !field.required,
                    "{} is daemon-owned and required",
                    field.key
                );
            }
        }
        assert_eq!(
            field_named("new_failures").map(|field| field.nullable),
            Some(true)
        );
    }
}

//! #3177 (program #3135 M3 S3): the `harness_build_digest` of a bench session.
//!
//! `compute_harness_version_hash` hashes the kind, the system prompt and the
//! query, so it changes with every task and is no identity for the harness
//! itself. The build digest names the harness configuration a benchmark run
//! measured: the same digest means the same rsid build, the same prompts, the
//! same tool surface, the same policy and loop knobs and the same
//! provider/model/effort/route, so two runs are comparable and a run is
//! reproducible. It does NOT contain the rsid build SHA (#3192): the hub daemon is
//! redeployed often and most redeploys leave the harness alone, so the digest names the
//! harness source (`harness_source_digest`, a digest of the git tree or blob ids of the
//! committed path list in `rsi_common::harness_source_digest`, taken at the build commit)
//! instead, and the build SHA is provenance only. It is a SHA-256 over canonical JSON (sorted keys, no
//! whitespace) of [`BuildDigestInputs`].
//!
//! The Harness launch records the digest as a `bench_build_digest` stream event
//! that the monitor stores as a System row
//! (`metadata.provider_health.kind = "bench_build_digest"`), where the bench
//! task (S4) reads it back with `session::bench_route::read_build_digest`.

use super::types::HarnessToolSpec;
use rsi_common::bench_overlay::OverlayIdentity;
use rsi_common::harness_tool_policy::HarnessToolPolicy;
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

/// The stream event a bench launch emits with its digest.
pub(crate) const BUILD_DIGEST_STREAM_EVENT: &str = "bench_build_digest";

/// Version of the digest's own input layout. Adding or renaming an input bumps
/// it, so a digest never silently equals one computed over another layout.
const DIGEST_SCHEMA_VERSION: u32 = 2;

/// Every input of the build digest.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct BuildDigestInputs {
    /// The harness source at the build commit (`daemon_info::HARNESS_SOURCE_DIGEST`); changes
    /// only when a path in the committed list changes, not on every redeploy.
    pub harness_source_digest: String,
    pub base_prompt_version: u32,
    pub base_prompt_digest: String,
    pub bench_prompt_version: u32,
    pub bench_prompt_digest: String,
    /// [`tool_spec_digest`] of the session's registry.
    pub tool_spec_digest: String,
    /// The effective tool policy the session runs under.
    pub tool_policy: HarnessToolPolicy,
    /// The effective loop knobs: the per-launch value, else the global.
    pub max_iterations: u32,
    pub compaction_budget_tokens: Option<u64>,
    pub rotation_off: bool,
    /// #4028: the length-stop recovery ceiling. Absent when the launch sets none, so
    /// such a digest is byte-identical to the one computed before the knob existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery_max_output_tokens: Option<u32>,
    pub provider: String,
    pub model: String,
    pub effort: Option<String>,
    /// The execution route, for example `SessionHarnessOpenRouterHttp`.
    pub route: String,
    /// #3191: the run overlay's identity (its canonical digest, the sha256 of
    /// each file it references and the effective rendered prompt digest). Absent
    /// for a run with no overlay, so such a digest is byte-identical to the one
    /// computed before overlays existed, and an empty overlay (which resolves to
    /// none) is the same run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overlay: Option<OverlayIdentity>,
}

/// `value` as canonical JSON: object keys sorted, no insignificant whitespace.
fn canonical_json(value: &Value) -> String {
    let mut out = String::new();
    write_canonical(value, &mut out);
    out
}

fn write_canonical(value: &Value, out: &mut String) {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(key.clone()).to_string());
                out.push(':');
                write_canonical(&map[key], out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        scalar => out.push_str(&scalar.to_string()),
    }
}

fn sha256_hex(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

/// Digest of a session's advertised tool catalog (`registry.specs()`): each
/// spec's name, description, parameter schema and kind, ordered by name so the
/// registration order does not matter.
pub(crate) fn tool_spec_digest(specs: &[HarnessToolSpec]) -> String {
    let mut sorted: Vec<&HarnessToolSpec> = specs.iter().collect();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));
    let values: Vec<Value> = sorted
        .into_iter()
        .map(|spec| serde_json::to_value(spec).unwrap_or(Value::Null))
        .collect();
    sha256_hex(&canonical_json(&Value::Array(values)))
}

/// The canonical JSON the digest is taken over, schema version included.
pub(crate) fn canonical_inputs(inputs: &BuildDigestInputs) -> String {
    let mut value = serde_json::to_value(inputs).unwrap_or(Value::Null);
    if let Value::Object(map) = &mut value {
        map.insert("schema_version".into(), Value::from(DIGEST_SCHEMA_VERSION));
    }
    canonical_json(&value)
}

/// The digests of the effective inputs a bench run's harness interpreted, one per
/// `rsi_common::bench_record::EffectiveInputs` field: base prompt, bench overlay, tool catalog,
/// tool policy and loop knobs. Each is the SHA-256 of the canonical JSON of just its inputs, so
/// two runs differing in one input differ in exactly one digest.
pub(crate) fn effective_input_digests(inputs: &BuildDigestInputs) -> Value {
    let hash = |value: Value| Value::String(sha256_hex(&canonical_json(&value)));
    serde_json::json!({
        "prompt": hash(serde_json::json!({
            "version": inputs.base_prompt_version,
            "digest": inputs.base_prompt_digest,
        })),
        "overlay": hash(match &inputs.overlay {
            None => serde_json::json!({
                "version": inputs.bench_prompt_version,
                "digest": inputs.bench_prompt_digest,
            }),
            Some(identity) => serde_json::json!({
                "version": inputs.bench_prompt_version,
                "digest": inputs.bench_prompt_digest,
                "overlay": serde_json::to_value(identity).unwrap_or(Value::Null),
            }),
        }),
        "tool_spec": hash(Value::String(inputs.tool_spec_digest.clone())),
        "policy": hash(serde_json::to_value(&inputs.tool_policy).unwrap_or(Value::Null)),
        "loop_knobs": hash({
            let mut knobs = serde_json::json!({
                "max_iterations": inputs.max_iterations,
                "compaction_budget_tokens": inputs.compaction_budget_tokens,
                "rotation_off": inputs.rotation_off,
            });
            if let Some(cap) = inputs.recovery_max_output_tokens {
                knobs["recovery_max_output_tokens"] = cap.into();
            }
            knobs
        }),
    })
}

/// The `harness_build_digest`: lowercase hex SHA-256 of the canonical inputs.
pub(crate) fn harness_build_digest(inputs: &BuildDigestInputs) -> String {
    sha256_hex(&canonical_inputs(inputs))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::harness::tools::HarnessToolRegistry;

    fn inputs() -> BuildDigestInputs {
        BuildDigestInputs {
            harness_source_digest: format!("sha256:{}", "a".repeat(64)),
            base_prompt_version: 1,
            base_prompt_digest: "b".repeat(64),
            bench_prompt_version: 1,
            bench_prompt_digest: "c".repeat(64),
            tool_spec_digest: "d".repeat(64),
            tool_policy: HarnessToolPolicy::bench(),
            max_iterations: 150,
            compaction_budget_tokens: Some(200_000),
            rotation_off: true,
            recovery_max_output_tokens: None,
            provider: "openai-compatible".into(),
            model: "vendor/model".into(),
            effort: Some("high".into()),
            route: "SessionHarnessOpenRouterHttp".into(),
            overlay: None,
        }
    }

    fn overlay_identity(tag: &str) -> OverlayIdentity {
        OverlayIdentity {
            digest: format!("sha256:{}", tag.repeat(64)),
            files: std::collections::BTreeMap::from([(
                "instructions.md".to_string(),
                format!("sha256:{}", "9".repeat(64)),
            )]),
            rendered_prompt_digest: "8".repeat(64),
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn canonical_json_sorts_keys_and_drops_whitespace() {
        let value = serde_json::json!({"b": [1, {"z": null, "a": "x"}], "a": true});
        assert_eq!(
            canonical_json(&value),
            r#"{"a":true,"b":[1,{"a":"x","z":null}]}"#
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn the_digest_is_stable_for_equal_inputs() {
        let a = harness_build_digest(&inputs());
        assert_eq!(a, harness_build_digest(&inputs()));
        assert_eq!(a.len(), 64);
        assert!(
            a.bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        );
        // The canonical text carries every input exactly once.
        let text = canonical_inputs(&inputs());
        for key in [
            "harness_source_digest",
            "base_prompt_version",
            "base_prompt_digest",
            "bench_prompt_version",
            "bench_prompt_digest",
            "tool_spec_digest",
            "tool_policy",
            "max_iterations",
            "compaction_budget_tokens",
            "rotation_off",
            "provider",
            "model",
            "effort",
            "route",
            "schema_version",
        ] {
            assert_eq!(text.matches(&format!("\"{key}\":")).count(), 1, "{key}");
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn the_digest_changes_when_any_single_input_changes() {
        let base = harness_build_digest(&inputs());
        let mut seen = std::collections::BTreeSet::from([base.clone()]);
        let mut check = |mutate: &dyn Fn(&mut BuildDigestInputs)| {
            let mut changed = inputs();
            mutate(&mut changed);
            let digest = harness_build_digest(&changed);
            assert_ne!(digest, base);
            assert!(seen.insert(digest), "two single-input changes collide");
        };
        check(&|i| i.harness_source_digest = format!("sha256:{}", "f".repeat(64)));
        check(&|i| i.base_prompt_version = 2);
        check(&|i| i.base_prompt_digest = "e".repeat(64));
        check(&|i| i.bench_prompt_version = 2);
        check(&|i| i.bench_prompt_digest = "e".repeat(64));
        check(&|i| i.tool_spec_digest = "e".repeat(64));
        check(&|i| i.tool_policy.web_access = None);
        check(&|i| i.tool_policy.denied_tools = vec!["shell".into()]);
        check(&|i| i.max_iterations = 151);
        check(&|i| i.compaction_budget_tokens = None);
        check(&|i| i.rotation_off = false);
        check(&|i| i.recovery_max_output_tokens = Some(65_536));
        check(&|i| i.provider = "anthropic".into());
        check(&|i| i.model = "vendor/other".into());
        check(&|i| i.effort = Some("low".into()));
        check(&|i| i.effort = None);
        check(&|i| i.route = "SessionHarnessOpenAiHttp".into());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn each_effective_input_moves_exactly_its_own_component_digest() {
        let base = effective_input_digests(&inputs());
        let keys = ["prompt", "overlay", "tool_spec", "policy", "loop_knobs"];
        assert_eq!(base.as_object().unwrap().len(), keys.len());
        assert_eq!(base, effective_input_digests(&inputs()));
        let check = |key: &str, mutate: &dyn Fn(&mut BuildDigestInputs)| {
            let mut changed = inputs();
            mutate(&mut changed);
            let moved = effective_input_digests(&changed);
            for other in keys {
                assert_eq!(
                    moved[other] != base[other],
                    other == key,
                    "{key} vs {other}"
                );
            }
        };
        check("prompt", &|i| i.base_prompt_digest = "e".repeat(64));
        check("prompt", &|i| i.base_prompt_version = 2);
        check("overlay", &|i| i.bench_prompt_digest = "e".repeat(64));
        check("tool_spec", &|i| i.tool_spec_digest = "e".repeat(64));
        check("policy", &|i| {
            i.tool_policy.denied_tools = vec!["shell".into()]
        });
        check("loop_knobs", &|i| i.max_iterations = 151);
        check("loop_knobs", &|i| i.rotation_off = false);
        check("loop_knobs", &|i| {
            i.recovery_max_output_tokens = Some(65_536)
        });
        // Model, effort, route and the harness source are not effective-input components.
        check("none", &|i| i.model = "vendor/other".into());
        check("none", &|i| i.harness_source_digest = "x".into());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn the_tool_spec_digest_follows_the_catalog_not_the_registration_order() {
        let registry = HarnessToolRegistry::default_tools_with_execution_scratch(
            None, None, None, None, None, None, None, None, None, true, None,
        );
        let specs = registry.specs();
        assert!(specs.len() > 2);
        let digest = tool_spec_digest(&specs);
        let mut reversed = specs.clone();
        reversed.reverse();
        assert_eq!(digest, tool_spec_digest(&reversed));
        // A different catalog, a changed description or schema: a different digest.
        assert_ne!(digest, tool_spec_digest(&specs[1..]));
        let mut edited = specs.clone();
        edited[0].description.push_str(" changed");
        assert_ne!(digest, tool_spec_digest(&edited));
        let mut reschemaed = specs;
        reschemaed[0].parameters_json = "{}".into();
        assert_ne!(digest, tool_spec_digest(&reschemaed));
    }

    /// #3191: no overlay leaves the digest and its canonical text exactly as they
    /// were before overlays existed; an overlay moves the digest and only the
    /// `overlay` component, whichever of its three identity parts changes.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn an_overlay_folds_into_the_digest_and_no_overlay_changes_nothing() {
        let bare = inputs();
        let text = canonical_inputs(&bare);
        assert!(!text.contains("\"overlay\""), "{text}");
        assert!(!text.contains("rendered_prompt_digest"));
        let base_digest = harness_build_digest(&bare);
        let base_components = effective_input_digests(&bare);

        let mut with = inputs();
        with.overlay = Some(overlay_identity("1"));
        let digest = harness_build_digest(&with);
        assert_ne!(digest, base_digest);
        let text = canonical_inputs(&with);
        for key in ["digest", "files", "rendered_prompt_digest"] {
            assert_eq!(text.matches(&format!("\"{key}\":")).count(), 1, "{key}");
        }
        let components = effective_input_digests(&with);
        for (key, value) in components.as_object().unwrap() {
            assert_eq!(*value != base_components[key], key == "overlay", "{key}");
        }

        let mut seen = std::collections::BTreeSet::from([digest.clone()]);
        let mut check = |mutate: &dyn Fn(&mut OverlayIdentity)| {
            let mut changed = inputs();
            let mut identity = overlay_identity("1");
            mutate(&mut identity);
            changed.overlay = Some(identity);
            let moved = harness_build_digest(&changed);
            assert_ne!(moved, digest);
            assert!(seen.insert(moved), "two single changes collide");
            assert_ne!(
                effective_input_digests(&changed)["overlay"],
                components["overlay"]
            );
        };
        check(&|i| i.digest = format!("sha256:{}", "2".repeat(64)));
        check(&|i| i.rendered_prompt_digest = "7".repeat(64));
        check(&|i| {
            i.files.insert(
                "instructions.md".into(),
                format!("sha256:{}", "6".repeat(64)),
            );
        });
        check(&|i| i.files.clear());
    }
}

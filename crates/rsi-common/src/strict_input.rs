//! Shared response DTOs accept additive daemon fields. Input boundaries still
//! reject every ignored field, including nested policy and launch fields.

use serde::{Deserialize, Deserializer, de::Error};

pub(crate) fn deserialize<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    let mut unknown = None;
    let value = serde_ignored::deserialize(deserializer, |path| {
        if unknown.is_none() {
            unknown = Some(path.to_string());
        }
    })?;
    if let Some(path) = unknown {
        return Err(D::Error::custom(format!("unknown field {path}")));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use crate::harness_manager_v2::{ConfigureHarnessManagerPolicyRequestV2, ManagerPolicyV2};
    use serde_json::json;

    #[test]
    fn policy_reads_accept_additions_but_operator_writes_reject_them_at_every_depth() {
        let request = json!({
            "project_id": uuid::Uuid::new_v4(), "expected_scope_version": 1,
            "expected_policy_version": 1, "idempotency_key": "skew-regression",
            "policy": {
                "allowed_launches": [{"provider": "Claude", "model": "claude-opus-5-5"}],
                "provider_limits": [{"provider": "Claude", "max_active": 2}],
                "provider_grants": [{"provider": "Claude", "budget_usd": 10.0, "efforts": ["high"]}],
                "daemon_setting_bounds": [{"key": "unused", "min": 1, "max": 2}]
            }
        });
        assert!(
            serde_json::from_value::<ConfigureHarnessManagerPolicyRequestV2>(request.clone())
                .is_ok()
        );
        for path in [
            "/policy",
            "/policy/allowed_launches/0",
            "/policy/provider_limits/0",
            "/policy/provider_grants/0",
            "/policy/daemon_setting_bounds/0",
        ] {
            let mut newer = request.clone();
            newer.pointer_mut(path).unwrap()["future_field"] = json!(true);
            let read: ManagerPolicyV2 = serde_json::from_value(newer["policy"].clone()).unwrap();
            assert_eq!(read.allowed_launches[0].model, "claude-opus-5-5");
            let error = serde_json::from_value::<ConfigureHarnessManagerPolicyRequestV2>(newer)
                .unwrap_err();
            assert!(
                error.to_string().contains("future_field"),
                "{path}: {error}"
            );
        }
        let mut typo = request;
        typo["expected_polcy_version"] = json!(1);
        assert!(serde_json::from_value::<ConfigureHarnessManagerPolicyRequestV2>(typo).is_err());
    }
}

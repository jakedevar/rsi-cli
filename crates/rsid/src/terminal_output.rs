//! Shared policy boundary for selecting a terminal provider output.
//!
//! Recursive DAG and Closure intentionally apply different provenance rules,
//! but both need the same stable "last accepted record" ordering semantics.

pub(crate) fn select_last_terminal_output<T>(
    records: &[T],
    mut accepts: impl FnMut(&T) -> bool,
) -> Option<&T> {
    records.iter().rev().find(|record| accepts(record))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selector_returns_last_policy_accepted_record() {
        let records = [
            ("provider", "first"),
            ("diagnostic", "stderr"),
            ("provider", "last"),
        ];
        assert_eq!(
            select_last_terminal_output(&records, |record| record.0 == "provider"),
            Some(&("provider", "last"))
        );
    }
}

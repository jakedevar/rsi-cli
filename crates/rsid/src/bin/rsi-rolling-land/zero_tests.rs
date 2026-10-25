//! Visible zero-test gates (hub #3887).
//!
//! The `rsid_unfiltered_selection=closure selected=0 reached=0` line describes
//! the rsid family only, so it reads as "no tests" for a gate that ran the
//! touched package's own tests. The opposite case is the real hazard: a gate
//! that ran no test command at all for a source that touched Rust packages,
//! desktop JavaScript or script suites lands on its compile checks alone. This
//! module names that case in the lander log and the `gate_timing=` line the
//! merge queue keeps in the outcome, with the fix or the lane to run instead.
//! A docs-only source legitimately selects nothing and stays quiet.

/// What a source touched that a test run should have covered.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct ZeroTestScope {
    /// Cargo packages the plan marked affected.
    packages: Vec<String>,
    /// Desktop front-end sources (`desktop/**` JavaScript, TypeScript, markup).
    desktop_js: usize,
    /// Bash TAP suites and Rust fixtures under `scripts/tests/`.
    script_suites: usize,
}

/// The desktop lane to run when the Rust gate selects nothing for desktop JS.
pub(super) const DESKTOP_LANE: &str = "recipe:desktop-npm";

fn desktop_front_end(path: &str) -> bool {
    path.starts_with("desktop/")
        && !path.starts_with("desktop/docs/")
        && path.rsplit_once('.').is_some_and(|(_, ext)| {
            matches!(
                ext,
                "js" | "mjs" | "cjs" | "ts" | "tsx" | "jsx" | "svelte" | "vue" | "html" | "css"
            )
        })
}

fn script_suite(path: &str) -> bool {
    path.starts_with("scripts/tests/") && path.ends_with(".sh")
}

impl ZeroTestScope {
    pub(super) fn from_diff<'a>(
        packages: &[String],
        paths: impl IntoIterator<Item = &'a str>,
    ) -> Self {
        let mut scope = Self {
            packages: packages.to_vec(),
            ..Self::default()
        };
        for path in paths {
            if desktop_front_end(path) {
                scope.desktop_js += 1;
            } else if script_suite(path) {
                scope.script_suites += 1;
            }
        }
        scope
    }

    /// The `gate_warning=` text when the gate ran `tests_run` test commands and
    /// that is none although the source touched testable code; `None` when the
    /// gate ran tests or the source touched nothing a test covers.
    pub(super) fn warning(&self, tests_run: usize) -> Option<String> {
        if tests_run > 0
            || (self.packages.is_empty() && self.desktop_js == 0 && self.script_suites == 0)
        {
            return None;
        }
        let mut out = format!("gate_warning=zero_tests tests_run={tests_run}");
        if !self.packages.is_empty() {
            out.push_str(&format!(
                " packages={} fix=enqueue_with_derive_filters:true_or_explicit_test_filters",
                self.packages.join(",")
            ));
        }
        if self.desktop_js > 0 {
            out.push_str(&format!(
                " desktop_js={} desktop_lane={DESKTOP_LANE}",
                self.desktop_js
            ));
        }
        if self.script_suites > 0 {
            out.push_str(&format!(
                " script_suites={} script_lane=scoped_test_runs_scripts/tests",
                self.script_suites
            ));
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rust_package_with_no_test_command_warns_and_names_the_fix() {
        let scope = ZeroTestScope::from_diff(
            &["rsi-repo-guards".to_owned()],
            ["crates/rsi-repo-guards/src/lib.rs"],
        );
        let warning = scope.warning(0).expect("zero tests must warn");
        assert!(warning.starts_with("gate_warning=zero_tests tests_run=0 "));
        assert!(warning.contains("packages=rsi-repo-guards"));
        assert!(warning.contains("derive_filters:true"));
        assert!(!warning.contains("desktop_lane"));
    }

    #[test]
    fn a_desktop_only_source_warns_with_the_desktop_lane_and_no_rust_fix() {
        let scope = ZeroTestScope::from_diff(&[], ["desktop/scripts/smoke-firstrun.mjs"]);
        let warning = scope.warning(0).expect("desktop JS must warn");
        assert!(warning.contains("desktop_js=1"));
        assert!(warning.contains("desktop_lane=recipe:desktop-npm"));
        assert!(!warning.contains("packages="));
        assert!(!warning.contains("fix="));
    }

    #[test]
    fn a_script_suite_addition_is_named() {
        let scope = ZeroTestScope::from_diff(&[], ["scripts/tests/test_check_host_warnings.sh"]);
        let warning = scope.warning(0).expect("a script suite must warn");
        assert!(warning.contains("script_suites=1"));
    }

    #[test]
    fn a_docs_only_source_selects_nothing_quietly() {
        let scope = ZeroTestScope::from_diff(&[], ["docs/agents/routing.md", "desktop/docs/a.md"]);
        assert_eq!(scope.warning(0), None);
    }

    #[test]
    fn a_gate_that_ran_a_test_command_is_quiet() {
        let scope = ZeroTestScope::from_diff(
            &["rsi-repo-guards".to_owned()],
            ["crates/rsi-repo-guards/src/lib.rs"],
        );
        assert_eq!(scope.warning(3), None);
    }
}

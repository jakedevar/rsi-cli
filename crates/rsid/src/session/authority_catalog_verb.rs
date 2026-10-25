//! `AgentGetAuthorityCatalog`: the agent'XX callable operator's manual.
//!
//! Every RSI-managed session may call it. The caller is transport-bound (RPC
//! token or native-tool registration); the answer is rendered from one durable
//! [`agent_authority_projection`](crate::store::Store::agent_authority_projection)
//! read, the same projection role-gated verbs are advertised from. It is
//! read-only and grants nothing: every other verb keeps its own guard.

use super::agent_verbs::AgentControlHandle;
use crate::error::{DaemonError, Result};
use rsi_common::agent_authority_catalog::{
    AgentAuthorityCatalogFieldsV1, AgentAuthorityCatalogV1, AgentGetAuthorityCatalogRequestV1,
};
use uuid::Uuid;

impl AgentControlHandle {
    /// # Errors
    /// `authority_catalog_unknown_verb` for an unknown `verb`, the projection's
    /// caller refusal, or a persistence error.
    pub async fn agent_get_authority_catalog(
        &self,
        caller: Uuid,
        request: AgentGetAuthorityCatalogRequestV1,
    ) -> Result<AgentAuthorityCatalogV1> {
        let requested = request
            .requested_verb()
            .map_err(|code| DaemonError::InvalidParam(code.into()))?;
        let (projection, rotation, coordinator, directives) = {
            let store = self.store.lock().await;
            let _admission = super::rpc_admission::admit_current_rpc(caller).await?;
            let projection = store.agent_authority_projection(caller)?;
            (
                projection,
                self.rotation_note(&store, caller),
                requested.is_none()
                    && super::context_cap::capped_seat(&store, caller)
                        .ok()
                        .flatten()
                        .is_some(),
                super::standing_directives::seat_directives_block(&store, caller),
            )
        };
        let mut catalog =
            super::preamble::render_authority_catalog(caller, &projection, requested)?;
        // #308: the default whole-manual answer carries one-line control
        // descriptions so a manager's catalog stays inlineable; `fields:
        // "full"` is the tooling form. A `verb` answer has no controls list.
        if requested.is_none()
            && request.fields.unwrap_or_default() == AgentAuthorityCatalogFieldsV1::Summary
        {
            catalog.summarize();
        }
        // #2558: the caller's live threshold, from the daemon's settings. A
        // single-verb request carries no guidance and gets no note.
        if let Some(note) = rotation.filter(|_| !catalog.guidance.is_empty()) {
            catalog.guidance.push_str("\n\n");
            catalog.guidance.push_str(&note);
        }
        if coordinator {
            let seat = self.get_session(caller).await;
            let budget = match seat {
                Some(seat) => self.seat_coordinator_budget(&seat).await,
                None => None,
            };
            if let Some(budget) = budget {
                catalog
                    .guidance
                    .push_str("\n\nResolved coordinator budget: ");
                catalog.guidance.push_str(&budget.summary());
                catalog.coordinator_budget = Some(budget);
            }
        }
        // #2649: the manager scope's standing operator directives, verbatim. A
        // seat reads them here at the start of every seat, so no handoff has to
        // carry them.
        if let Some(block) = directives.filter(|_| !catalog.guidance.is_empty()) {
            catalog.guidance.push_str("\n\n");
            catalog.guidance.push_str(&block);
        }
        Ok(catalog)
    }

    /// The caller's effective context rotation threshold as guidance text;
    /// `None` when the handle has no settings or nothing applies.
    fn rotation_note(&self, store: &crate::store::Store, caller: Uuid) -> Option<String> {
        let runtime = self.runtime_config.as_deref()?;
        let session = store.get_session(caller).ok().flatten()?;
        let coordinator = super::context_cap::capped_seat(store, caller)
            .ok()
            .flatten()
            .is_some();
        let threshold = super::context_cap::rotation_threshold_for(runtime, &session, coordinator)?;
        Some(rotation_note_text(threshold, coordinator))
    }
}

/// The guidance paragraph that states a caller's rotation threshold. It is
/// the only place a rotation percentage reaches an agent: playbooks name the
/// rule and point here (#2558).
pub(super) fn rotation_note_text(
    threshold: crate::config::RotationThreshold,
    coordinator: bool,
) -> String {
    let who = if coordinator {
        "Your seat is rotated by the daemon at"
    } else {
        "You are told to pass the baton (commit, append a handoff to your Issue, end your turn) at"
    };
    format!(
        "## Your context rotation threshold\n\n{who} {pct}% of your context window \
         (`{setting}`, a live operator setting). Hand off voluntarily only before that point. \
         `context_fill_pct` in `AgentManagerOverview` is measured prompt tokens over the same \
         active window, so it compares directly with this number. Read the threshold here; \
         do not use a remembered one.",
        pct = format_pct(threshold.pct),
        setting = threshold.setting,
    )
}

fn format_pct(pct: f64) -> String {
    if (pct - pct.round()).abs() < 0.05 {
        format!("{}", pct.round() as i64)
    } else {
        format!("{pct:.1}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RotationThreshold;
    use std::path::Path;

    const KEYWORDS: [&str; 7] = [
        "context", "window", "fill", "rotat", "baton", "handoff", "hand off",
    ];

    /// True when `line` states a number followed by `%` or `percent`.
    fn has_percentage(line: &str) -> bool {
        let bytes = line.as_bytes();
        (0..bytes.len()).any(|i| {
            bytes[i].is_ascii_digit() && !bytes.get(i + 1).is_some_and(u8::is_ascii_digit) && {
                let rest = line[i + 1..].trim_start();
                rest.starts_with('%') || rest.starts_with("percent")
            }
        })
    }

    /// 1-based lines of `text` that state a percentage next to context,
    /// rotation or handoff wording (the line itself or its neighbours).
    fn rotation_percentage_lines(text: &str) -> Vec<usize> {
        let lines: Vec<String> = text.lines().map(str::to_lowercase).collect();
        (0..lines.len())
            .filter(|&i| has_percentage(&lines[i]))
            .filter(|&i| {
                let near = lines[i.saturating_sub(1)..(i + 2).min(lines.len())].join(" ");
                KEYWORDS.iter().any(|word| near.contains(word))
            })
            .map(|i| i + 1)
            .collect()
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[test]
    fn guard_flags_a_hard_coded_rotation_percentage() {
        assert_eq!(
            rotation_percentage_lines("Keep your context small: when it\npasses 60%, hand off."),
            vec![2]
        );
        assert_eq!(
            rotation_percentage_lines("PM context fill at 60% or more"),
            vec![1]
        );
        assert_eq!(
            rotation_percentage_lines("past about 60 percent of the window"),
            vec![1]
        );
        assert!(rotation_percentage_lines("the daemon rotates you at the threshold").is_empty());
        assert!(rotation_percentage_lines("coverage is 90% of lines").is_empty());
    }

    /// #2558: no playbook, guidance file or worker preamble may state a
    /// context rotation percentage. The daemon serves the caller's live
    /// threshold in the authority catalog (`rotation_note_text`).
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[test]
    fn no_guidance_file_hard_codes_a_context_rotation_percentage_2558() {
        let root = Path::new(rsi_common::manifest_dir!()).join("../..");
        let worker_contract = root.join("docs/agents/worker-contract.md");
        let mut files = vec![
            root.join(".claude/commands/_shared/worker_preamble.md"),
            worker_contract.clone(),
        ];
        for dir in [
            root.join("crates/rsid/src/session/guidance"),
            root.join(".claude/skills/rsi-portfolio-manager"),
            root.join(".claude/skills/rsi-project-manager"),
            root.join(".claude/skills/rsi-agent-control"),
            root.join(".claude/skills/rsi-global-manager"),
            root.join(".agents/skills/rsi-portfolio-manager"),
            root.join(".agents/skills/rsi-project-manager"),
            root.join(".agents/skills/rsi-agent-control"),
            root.join(".agents/skills/rsi-global-manager"),
        ] {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            files.extend(
                entries
                    .flatten()
                    .map(|entry| entry.path())
                    .filter(|path| path.extension().is_some_and(|ext| ext == "md")),
            );
        }
        let mut violations = Vec::new();
        for file in &files {
            let text = std::fs::read_to_string(file)
                .unwrap_or_else(|err| panic!("read guidance {}: {err}", file.display()));
            if file == &worker_contract {
                assert!(
                    text.contains("`AgentGetAuthorityCatalog` serves `worker_context_cap_tokens`")
                );
            }
            for line in rotation_percentage_lines(&text) {
                violations.push(format!("{}:{line}", file.display()));
            }
        }
        assert!(
            violations.is_empty(),
            "guidance hard-codes a context rotation percentage; state the rule and point to the \
             authority catalog instead (#2558): {violations:#?}"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[test]
    fn rotation_note_states_the_live_threshold_and_its_setting_2558() {
        let seat = rotation_note_text(
            RotationThreshold {
                pct: 40.0,
                setting: "coordinator_context_cap_tokens",
            },
            true,
        );
        assert!(seat.contains("rotated by the daemon at 40% of your context window"));
        assert!(seat.contains("`coordinator_context_cap_tokens`"));
        assert!(seat.contains("`context_fill_pct`"));
        let worker = rotation_note_text(
            RotationThreshold {
                pct: 37.5,
                setting: "worker_context_cap_tokens",
            },
            false,
        );
        assert!(worker.contains("pass the baton"));
        assert!(worker.contains("37.5%"));
    }
}

//! Harness base instructions: the static, versioned prefix of the system
//! prompt of every Harness-loop session (#2258, plan §6 S8; slice 1 of #795,
//! the prompt seam).
//!
//! A Harness system prompt has two parts that stay distinct:
//!
//! - the **base**: `assets/harness/base_instructions.md`, immutable at
//!   runtime, identical for every session and every turn, so it forms the
//!   stable prefix a provider can cache (Mercury bills cached input at a
//!   tenth); it holds no date, path or tool list;
//! - the **supplemental** text: whatever the launch passed as its system
//!   prompt (the sandbox custody instruction and similar). A later #795 slice
//!   adds reviewed notes here; the base is never editable.
//!
//! [`PromptParts::render`] joins them in that order, with the supplemental
//! part wrapped in `<session_instructions>`. Both parts and the whole have a
//! SHA-256 digest, so a prompt version can be named and compared without
//! storing the text.
//!
//! Only providers that run the Harness loop get the base; the launch path of
//! every other provider never reaches this module.
//
// Adapted from openai/codex codex-rs/protocol/src/prompts/base_instructions/default.md @ dfdb40cd (Apache-2.0); see THIRD_PARTY_NOTICES.md
// Adapted from PrimeIntellect-ai/prime-agent packages/coding-agent/src/core/prompts/rlm.ts @ cd1f215c (MIT); see THIRD_PARTY_NOTICES.md
// Modified: rewritten for RSI's typed tools and sandbox rules (Apache-2.0 section 4(b)).

use super::types::{ChatMessage, MessageRole};
use rsi_common::harness_tool_policy::provider_runs_harness_loop;
use rsi_common::types::SessionProvider;
use sha2::{Digest, Sha256};
use std::sync::OnceLock;

/// Version of the base text. Bump it together with the digest pinned in the
/// tests whenever `assets/harness/base_instructions.md` changes.
pub(crate) const BASE_PROMPT_VERSION: u32 = 1;

const ASSET: &str = include_str!("../../../assets/harness/base_instructions.md");

const SUPPLEMENTAL_OPEN: &str = "<session_instructions>";
const SUPPLEMENTAL_CLOSE: &str = "</session_instructions>";

/// The base text: the asset without its leading HTML-comment header, with line
/// endings normalized (a Windows checkout may carry CRLF) and no surrounding
/// blank lines.
pub(crate) fn base_instructions() -> &'static str {
    static BASE: OnceLock<String> = OnceLock::new();
    BASE.get_or_init(|| {
        let normalized = ASSET.replace("\r\n", "\n");
        let body = match normalized.strip_prefix("<!--") {
            Some(rest) => rest.split_once("-->").map_or(rest, |(_, after)| after),
            None => normalized.as_str(),
        };
        body.trim().to_string()
    })
}

fn digest_hex(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

/// Digest of the base text alone.
pub(crate) fn base_digest() -> String {
    digest_hex(base_instructions())
}

/// The two parts of one Harness system prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PromptParts {
    supplemental: Option<String>,
}

impl PromptParts {
    /// A blank supplemental part is no part.
    pub(crate) fn new(supplemental: Option<String>) -> Self {
        Self {
            supplemental: supplemental.filter(|text| !text.trim().is_empty()),
        }
    }

    /// The prompt text sent to the model: base, then the wrapped supplemental
    /// part when there is one.
    pub(crate) fn render(&self) -> String {
        match &self.supplemental {
            None => base_instructions().to_string(),
            Some(text) => format!(
                "{}\n\n{SUPPLEMENTAL_OPEN}\n{}\n{SUPPLEMENTAL_CLOSE}",
                base_instructions(),
                text.trim()
            ),
        }
    }

    /// Digest of the rendered prompt: names the exact prompt a turn ran with.
    pub(crate) fn digest(&self) -> String {
        digest_hex(&self.render())
    }

    /// Digest of the supplemental part alone, when present.
    pub(crate) fn supplemental_digest(&self) -> Option<String> {
        self.supplemental
            .as_deref()
            .map(|text| digest_hex(text.trim()))
    }
}

/// Whether a launch for `provider` runs the Harness loop. `None` is the
/// default Harness route, chosen from the model name.
fn applies_to(provider: Option<SessionProvider>) -> bool {
    provider.is_none_or(provider_runs_harness_loop)
}

/// The system prompt a Harness-loop launch hands to the loop: base ⊕ the
/// caller's prompt. Any other provider gets the caller's prompt untouched.
pub(crate) fn compose_system_prompt(
    provider: Option<SessionProvider>,
    caller: Option<String>,
) -> Option<String> {
    if !applies_to(provider) {
        return caller;
    }
    let parts = PromptParts::new(caller);
    tracing::debug!(
        base_version = BASE_PROMPT_VERSION,
        base_digest = %base_digest(),
        prompt_digest = %parts.digest(),
        supplemental_digest = ?parts.supplemental_digest(),
        "harness system prompt composed"
    );
    Some(parts.render())
}

/// A resumed run rebuilds its history from persisted events, which never hold
/// a system message, and the loop sends a resumed history as it is. Put the
/// launch's composed system prompt (the base, then the supplemental text the
/// continue path assembled: the sandbox custody instruction and, on a fresh
/// relaunch, the router and preamble parts) at the front, once, so a continued
/// session keeps its instructions. Compaction keeps a leading system message,
/// so the prompt survives it (#2347).
pub(crate) fn seed_resumed_history(
    provider: Option<SessionProvider>,
    system_prompt: &str,
    history: Option<Vec<ChatMessage>>,
) -> Option<Vec<ChatMessage>> {
    let mut history = history?;
    if applies_to(provider)
        && history
            .first()
            .is_none_or(|first| first.role != MessageRole::System)
    {
        history.insert(0, ChatMessage::system(system_prompt));
    }
    Some(history)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::harness::tools::HarnessToolRegistry;
    use std::collections::BTreeSet;
    use std::sync::Arc;

    /// Pinned with [`BASE_PROMPT_VERSION`]: an edit to the asset fails the
    /// test that compares it, which is the cue to bump both.
    const PINNED_BASE_DIGEST: &str =
        "b2de35b1578b389bcee60559f4f0ef2dda8801761ede774671b8f721e866438d";
    /// Digest of the base plus [`FIXTURE_CALLER`] as rendered.
    const PINNED_RENDER_DIGEST: &str =
        "cdfd8941ec9c7632fd406b76e9851537dcf5de79e18fd2d660f49798ae24d1fc";
    const FIXTURE_CALLER: &str = "Work in the sandbox you were given.\n";

    /// Tools the prompt names that a launch need not advertise, with why.
    const CONDITIONAL_TOOLS: &[(&str, &str)] = &[
        (
            "web_search",
            "registered only when a search backend key resolves",
        ),
        (
            "web_fetch",
            "registered on chat routes; a hosted route supplies its own",
        ),
    ];

    fn registry() -> HarnessToolRegistry {
        HarnessToolRegistry::default_tools_with_execution_scratch(
            None, None, None, None, None, None, None, None, None, true, None,
        )
    }

    /// `ChatMessage` has no equality; compare what the seam may change.
    fn pairs(history: Option<&[ChatMessage]>) -> Vec<(MessageRole, String)> {
        history
            .unwrap_or_default()
            .iter()
            .map(|message| (message.role, message.content.clone()))
            .collect()
    }

    fn registered_names(registry: &HarnessToolRegistry) -> BTreeSet<String> {
        registry.specs().into_iter().map(|spec| spec.name).collect()
    }

    /// Every backticked token of the base text. The asset reserves backticks
    /// for tool names.
    fn mentioned_names() -> BTreeSet<String> {
        base_instructions()
            .split('`')
            .skip(1)
            .step_by(2)
            .map(str::to_string)
            .collect()
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn the_render_is_deterministic_and_pinned_by_digest() {
        assert_eq!(BASE_PROMPT_VERSION, 1);
        assert_eq!(
            base_digest(),
            PINNED_BASE_DIGEST,
            "the base text changed: bump BASE_PROMPT_VERSION and re-pin both digests"
        );

        let parts = PromptParts::new(Some(FIXTURE_CALLER.to_string()));
        let rendered = parts.render();
        assert_eq!(rendered, parts.render());
        assert_eq!(
            rendered,
            format!(
                "{}\n\n<session_instructions>\nWork in the sandbox you were given.\n</session_instructions>",
                base_instructions()
            )
        );
        assert_eq!(parts.digest(), PINNED_RENDER_DIGEST);
        assert_eq!(parts.digest(), digest_hex(&rendered));
        assert_eq!(
            parts.supplemental_digest().as_deref(),
            Some(digest_hex("Work in the sandbox you were given.").as_str())
        );

        // No supplemental part: the base alone, and a blank one counts as none.
        for blank in [None, Some(String::new()), Some("  \n".to_string())] {
            let bare = PromptParts::new(blank);
            assert_eq!(bare.render(), base_instructions());
            assert_eq!(bare.digest(), base_digest());
            assert_eq!(bare.supplemental_digest(), None);
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn the_base_is_static_sectioned_and_inside_its_budget() {
        let base = base_instructions();
        assert!(
            base.starts_with("<role>"),
            "header not stripped: {base:.40}"
        );
        assert!(!base.contains("<!--"));
        assert!(!base.contains('\r'));
        // Critical rules come last (Inception prompt guide).
        assert!(base.ends_with("</critical_rules>"));
        for section in [
            "role",
            "exploring",
            "editing",
            "planning",
            "web",
            "sandbox",
            "finishing",
            "critical_rules",
        ] {
            assert!(
                base.contains(&format!("<{section}>")),
                "missing <{section}>"
            );
            assert!(
                base.contains(&format!("</{section}>")),
                "missing </{section}>"
            );
        }
        // About 4 characters a token; the plan caps the base at about 1,200.
        assert!(
            base.len() <= 4_800,
            "the base is {} characters, about {} tokens",
            base.len(),
            base.len() / 4
        );
        // Static for prefix caching: nothing that varies per session or day.
        for dynamic in ["{{", "}}", "$HOME", "/home/", "cwd:", "today"] {
            assert!(!base.contains(dynamic), "dynamic text: {dynamic}");
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test]
    async fn every_tool_the_prompt_names_is_registered_or_conditional() {
        let registered = registered_names(&registry());
        let mentioned = mentioned_names();
        assert!(!mentioned.is_empty());
        let conditional: BTreeSet<&str> = CONDITIONAL_TOOLS.iter().map(|(name, _)| *name).collect();
        for name in &mentioned {
            assert!(
                registered.contains(name) || conditional.contains(name.as_str()),
                "the base prompt names `{name}`, which no launch registers; \
                 register it or list it in CONDITIONAL_TOOLS with the reason"
            );
        }

        // A conditional name must still be real: `web_fetch` is in the default
        // set and `web_search` appears once its backend key resolves.
        assert!(registered.contains("web_fetch"));
        let mut with_search = registry();
        let vault =
            crate::vault::VaultHandleBuilder::new(Arc::new(crate::vault::VaultSettings::default()))
                .env(|name| {
                    (name == "SERPER_API_KEY").then(|| "serper-fake-key-0123456789".to_string())
                })
                .open()
                .expect("test vault");
        let notice = crate::session::harness::HarnessClient::new().register_web_search(
            &mut with_search,
            &vault,
            false,
        );
        assert!(notice.is_none());
        assert!(registered_names(&with_search).contains("web_search"));

        // The tools the plan names for S8 are all mentioned.
        for expected in [
            "read_file",
            "grep",
            "glob",
            "apply_patch",
            "file_edit",
            "update_plan",
        ] {
            assert!(mentioned.contains(expected), "`{expected}` is not covered");
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn only_harness_loop_providers_get_the_base() {
        let caller = Some("custody text".to_string());
        // `SessionProvider` is non-exhaustive: a new provider is covered once it
        // is listed here and answers `provider_runs_harness_loop`.
        let all = [
            SessionProvider::Claude,
            SessionProvider::Codex,
            SessionProvider::Pioneer,
            SessionProvider::OpenRouter,
            SessionProvider::Bedrock,
            SessionProvider::Local,
            SessionProvider::Antigravity,
            SessionProvider::CodexAppServer,
            SessionProvider::Harness,
        ];
        for provider in all {
            let composed = compose_system_prompt(Some(provider), caller.clone());
            if provider_runs_harness_loop(provider) {
                let text = composed.expect("a Harness-loop prompt");
                assert!(text.starts_with(base_instructions()), "{provider:?}");
                assert!(text.contains("custody text"), "{provider:?}");
            } else {
                assert_eq!(composed, caller, "{provider:?} must be unchanged");
                assert_eq!(compose_system_prompt(Some(provider), None), None);
            }
        }
        // The default route (no provider named) is the Harness.
        assert_eq!(
            compose_system_prompt(None, None).as_deref(),
            Some(base_instructions())
        );

        // A history for a provider outside the loop is returned as given.
        let history = vec![ChatMessage::user("hi")];
        let kept = seed_resumed_history(
            Some(SessionProvider::Local),
            "prompt",
            Some(history.clone()),
        );
        assert_eq!(pairs(kept.as_deref()), pairs(Some(&history)));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn a_resumed_history_gets_the_base_once_at_the_front() {
        assert!(
            seed_resumed_history(Some(SessionProvider::Harness), base_instructions(), None)
                .is_none()
        );

        let history = vec![ChatMessage::user("task"), ChatMessage::assistant("done")];
        let prompt = compose_system_prompt(
            Some(SessionProvider::Harness),
            Some("custody text".to_string()),
        )
        .expect("a Harness prompt");
        let seeded = seed_resumed_history(
            Some(SessionProvider::Harness),
            &prompt,
            Some(history.clone()),
        )
        .expect("a seeded history");
        assert_eq!(seeded.len(), history.len() + 1);
        assert_eq!(seeded[0].role, MessageRole::System);
        assert_eq!(seeded[0].content, prompt);
        assert!(seeded[0].content.starts_with(base_instructions()));
        assert!(seeded[0].content.contains("custody text"));
        assert_eq!(pairs(Some(&seeded[1..])), pairs(Some(&history)));

        // Seeding again changes nothing: the front is already a system message.
        let again = seed_resumed_history(
            Some(SessionProvider::Harness),
            &prompt,
            Some(seeded.clone()),
        )
        .expect("history");
        assert_eq!(pairs(Some(&again)), pairs(Some(&seeded)));

        // An empty history still starts with the base.
        let empty = seed_resumed_history(None, &prompt, Some(Vec::new())).expect("history");
        assert_eq!(empty.len(), 1);
        assert_eq!(empty[0].role, MessageRole::System);
    }
}

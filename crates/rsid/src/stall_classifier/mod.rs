//! Stall classifier: opt-in small-LLM that decides whether an idle session
//! is genuinely stalled and what (if anything) the daemon should do about it.
//!
//! Architecture mirrors `dreamer/`: a sibling tokio task receives session IDs
//! on a bounded `mpsc::Receiver<Uuid>` from the stall detector, fetches a
//! conversation excerpt + child inventory via a read-only input builder,
//! prompts a cheap OpenAI-compatible model, parses a strict-JSON verdict,
//! and either:
//!   - emits telemetry only (`Finished` / `NeedsUser`, or any verdict below
//!     `confidence_floor`), or
//!   - dispatches a `NudgeAction::Continue` to a `nudge_consumer` loop that
//!     calls `SessionManager::continue_session()` with the model-authored
//!     nudge prompt (interrupt → wait → relaunch with `--resume`, preserving
//!     session UUID + conversation history).
//!
//! Off by default (`RSI_STALL_CLASSIFIER_ENABLED=false`). When disabled the
//! daemon behaves exactly as before the classifier shipped: the static
//! `stall_detector` action selector is the sole path.
//!
//! Phase 1 lands the scaffolding — module, config knobs, LLM client, prompt
//! template, scheduler skeleton. Detector signal channel arrives in Phase 2;
//! the classifier loop is fleshed out in Phase 4.

pub mod input;
pub mod llm_client;
pub mod prompts;
pub mod scheduler;
pub mod types;

pub use llm_client::StallClassifierLlmClient;
pub use scheduler::{ClassifierConfig, spawn_classifier};
pub use types::{ChildSummary, ClassificationInput, ClassifierVerdict, NudgeAction, Verdict};

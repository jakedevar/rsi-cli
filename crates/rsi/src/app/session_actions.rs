//! Session lifecycle actions for App (launch, continue, delete, archive, etc.).

use super::App;
use crate::types::{PHASE1_PLACEHOLDER_TAG, Pane};
use rsi_common::types::{SandboxSpec, SessionKind, SessionProvider};
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// The complete set of options that describes one session launch.
///
/// Both session-creation entry points build the same `LaunchOptions` payload:
/// - the input-bar quick-new path via [`App::quick_new_launch_options`], then
///   an App-owned response-bearing task, and
/// - the create-entity modal via `overlay::create_entity_form::leaf_launch_options`,
///   which uses the same response-bearing owner with an exact form snapshot.
///
/// This is the "one option builder" of ST-NEWSESSION-UNIFY (P1-3 / #7): for
/// equivalent inputs both paths produce an identical `LaunchOptions`, so the
/// resulting `LaunchSession` RPC params — and the sessions they create — are
/// identical regardless of entry point. `Serialize` exists so the unification
/// can be pinned by an order-independent equality test.
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct LaunchOptions {
    pub query: String,
    pub title: Option<String>,
    pub working_dir: Option<PathBuf>,
    pub provider: SessionProvider,
    pub model: Option<String>,
    pub system_prompt: Option<String>,
    pub session_kind: Option<SessionKind>,
    pub project_id: Option<Uuid>,
    pub max_retries: Option<u8>,
    pub effort: Option<String>,
    pub parent_id: Option<Uuid>,
    pub sandbox: Option<SandboxSpec>,
    pub tags: Vec<String>,
    pub workflow_id: Option<Uuid>,
    pub workflow_id_override: Option<Uuid>,
}

/// A fully owned launch request that can move onto a short-lived client task.
/// Inline credentials are deliberately not `Debug` so task diagnostics cannot
/// accidentally expose them.
#[derive(Clone)]
pub(crate) enum OwnedLaunchRequest {
    Standard(LaunchOptions),
    CustomSimple {
        options: LaunchOptions,
        base_url: String,
        api_key: String,
    },
    CustomWithOptions {
        options: LaunchOptions,
        base_url: String,
        api_key: String,
    },
}

impl OwnedLaunchRequest {
    pub(crate) fn options(&self) -> &LaunchOptions {
        match self {
            Self::Standard(options)
            | Self::CustomSimple { options, .. }
            | Self::CustomWithOptions { options, .. } => options,
        }
    }

    pub(crate) async fn execute(self, socket_path: PathBuf) -> Result<Uuid, String> {
        let mut client = crate::client::DaemonClient::new(socket_path);
        client.connect().await.map_err(|error| error.to_string())?;
        match self {
            Self::Standard(options) => client
                .launch_session_with_opts_response(
                    &options.query,
                    options.title.as_deref(),
                    options.working_dir.as_deref(),
                    options.provider,
                    options.model.as_deref(),
                    options.system_prompt.as_deref(),
                    options.session_kind,
                    options.project_id,
                    options.max_retries,
                    options.effort.as_deref(),
                    options.parent_id,
                    options.sandbox,
                    &options.tags,
                    options.workflow_id,
                    options.workflow_id_override,
                )
                .await
                .map_err(|error| error.to_string()),
            Self::CustomSimple {
                options,
                base_url,
                api_key,
            } => client
                .launch_session_custom_provider_response(
                    &options.query,
                    options.title.as_deref(),
                    options.working_dir.as_deref(),
                    options.provider,
                    options.model.as_deref(),
                    options.system_prompt.as_deref(),
                    options.project_id,
                    &base_url,
                    &api_key,
                    options.workflow_id,
                    options.effort.as_deref(),
                    &options.tags,
                    options.workflow_id_override,
                )
                .await
                .map_err(|error| error.to_string()),
            Self::CustomWithOptions {
                options,
                base_url,
                api_key,
            } => client
                .launch_session_with_opts_custom_provider_response(
                    &options.query,
                    options.title.as_deref(),
                    options.working_dir.as_deref(),
                    options.provider,
                    options.model.as_deref(),
                    options.system_prompt.as_deref(),
                    options.session_kind,
                    options.project_id,
                    &base_url,
                    &api_key,
                    options.max_retries,
                    options.effort.as_deref(),
                    options.parent_id,
                    &options.tags,
                    options.workflow_id_override,
                )
                .await
                .map_err(|error| error.to_string()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LaunchPlacement {
    CurrentPane,
    NewTab,
    NewSplit,
}

#[derive(Debug, Clone)]
pub(crate) enum InteractiveLaunchOrigin {
    QuickInput {
        query: String,
    },
    LegacyPrompt {
        overlay_id: Uuid,
        purpose: crate::types::PromptPurpose,
        draft_lines: Vec<String>,
        corrected_preview: Option<String>,
    },
    StackedPrompt {
        overlay_id: Uuid,
        purpose: crate::types::PromptPurpose,
        draft_lines: Vec<String>,
        corrected_preview: Option<String>,
    },
    CreateEntityLeaf {
        form: Box<crate::overlay::create_entity_form::PendingCreateEntityLeafForm>,
    },
    Detached,
}

#[derive(Clone)]
pub(crate) struct InteractiveLaunchResult {
    pub generation: u64,
    pub accepted: Result<Uuid, String>,
    pub model_warning: Option<String>,
}

pub(crate) struct PendingInteractiveLaunch {
    pub generation: u64,
    pub origin: InteractiveLaunchOrigin,
    pub placement: LaunchPlacement,
}

fn parse_slash_command_name(query: &str) -> Option<&str> {
    let rest = query.trim_start().strip_prefix('/')?;
    let end = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(rest.len());
    if end == 0 { None } else { Some(&rest[..end]) }
}

fn declared_claude_family(model: &str) -> Option<&'static str> {
    let lower = model.trim().to_ascii_lowercase();
    if lower.contains("fable") {
        Some("fable")
    } else if lower.contains("opus") {
        Some("opus")
    } else if lower.contains("sonnet") {
        Some("sonnet")
    } else if lower.contains("haiku") {
        Some("haiku")
    } else {
        None
    }
}

fn selected_claude_family(model: &str) -> Option<&'static str> {
    use rsi_common::model_utils::ModelFamily;

    match rsi_common::model_utils::parse_model_version(model) {
        Some((ModelFamily::Fable, _, _)) => Some("fable"),
        Some((ModelFamily::Opus, _, _)) => Some("opus"),
        Some((ModelFamily::Sonnet, _, _)) => Some("sonnet"),
        Some((ModelFamily::Haiku, _, _)) => Some("haiku"),
        None => None,
    }
}

pub(crate) fn command_frontmatter_model_conflict(
    query: &str,
    working_dir: Option<&Path>,
    provider: SessionProvider,
    selected_model: Option<&str>,
) -> Option<String> {
    if provider != SessionProvider::Claude {
        return None;
    }

    let selected_model = selected_model?;
    let selected_family = selected_claude_family(selected_model)?;
    let command_name = parse_slash_command_name(query)?;
    let search_root = working_dir
        .map(Path::to_path_buf)
        .or_else(|| std::env::current_dir().ok())?;

    for dir in search_root.ancestors() {
        let command_path = dir
            .join(".claude")
            .join("commands")
            .join(format!("{command_name}.md"));
        if !command_path.is_file() {
            continue;
        }
        let declared_model = rsi_common::command_meta::parse_command_file(&command_path)
            .ok()?
            .model?;
        let declared_family = declared_claude_family(&declared_model)?;
        if declared_family == selected_family {
            return None;
        }
        return Some(format!(
            "/{} declares model {} in .claude/commands/{}.md; Claude may ignore selected model {}.",
            command_name,
            declared_model,
            command_name,
            rsi_common::model_utils::abbreviate_model(selected_model),
        ));
    }

    None
}

impl App {
    fn custom_provider_for_modal_launch(
        &self,
        provider_override: Option<SessionProvider>,
        custom_provider_index: Option<usize>,
    ) -> Option<&crate::settings::CustomProviderEntry> {
        let index = custom_provider_index.or_else(|| {
            if provider_override.is_none() {
                self.custom_provider_index
            } else {
                None
            }
        });
        index.and_then(|index| self.settings.custom_providers.get(index))
    }

    pub(crate) fn launch_prerequisite_error(&self, provider: SessionProvider) -> Option<String> {
        if !self.authoritative_config_ready() {
            return Some(self.daemon_config_unavailable_reason());
        }
        if !self.poll.connected {
            return Some("Not connected to daemon".to_string());
        }
        if self.custom_provider_index.is_none() && !self.is_provider_available(provider) {
            return Some(format!(
                "{} provider unavailable (check daemon health)",
                Self::provider_label(provider)
            ));
        }
        None
    }

    /// Build the launch options for the input-bar quick-new-session path.
    ///
    /// Pure (no I/O, no `&mut self`) so the unification is unit-testable
    /// against the modal's `leaf_launch_options`. `working_dir` is already
    /// resolved by the caller (cwd fallback applied). The input bar has no
    /// kind/tag/parent pickers, so quick-new always creates a `Standard` leaf
    /// with the placeholder tag and no parent — these are the only fields that
    /// differ from a modal launch by design; everything a user *can* control
    /// (provider, model, effort, project) resolves identically to the modal.
    pub(crate) fn quick_new_launch_options(
        &self,
        query: &str,
        working_dir: Option<PathBuf>,
        workflow_id: Option<Uuid>,
    ) -> LaunchOptions {
        LaunchOptions {
            query: query.to_string(),
            title: None,
            working_dir,
            provider: self.selected_provider,
            model: self.selected_model.clone(),
            system_prompt: self
                .settings
                .system_prompt_preset
                .content()
                .map(str::to_string),
            // Explicit Standard (not relying on the daemon default) so a
            // quick-new session matches a modal Standard launch byte-for-byte.
            session_kind: Some(SessionKind::Standard),
            project_id: self.current_project_id,
            max_retries: None,
            effort: self.selected_effort.clone(),
            parent_id: None,
            sandbox: None, // No sandbox for direct launches via normal mode.
            tags: vec![PHASE1_PLACEHOLDER_TAG.to_string()],
            workflow_id,
            workflow_id_override: None,
        }
    }

    fn begin_interactive_launch(
        &mut self,
        request: OwnedLaunchRequest,
        origin: InteractiveLaunchOrigin,
        placement: LaunchPlacement,
    ) -> Result<(), String> {
        if self.interactive_launch_pending.is_some() {
            let error = "A session launch is awaiting daemon acceptance; draft preserved";
            self.notify_error(error);
            return Err(error.to_string());
        }
        let options = request.options();
        if let Some(reason) = self.launch_prerequisite_error(options.provider) {
            self.notify_error(reason.clone());
            return Err(reason);
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            let error = "Launch failed: async runtime unavailable; draft preserved";
            self.notify_error(error);
            return Err(error.to_string());
        };
        let model_warning = command_frontmatter_model_conflict(
            &options.query,
            options.working_dir.as_deref(),
            options.provider,
            options.model.as_deref(),
        );
        self.interactive_launch_generation = self.interactive_launch_generation.saturating_add(1);
        let generation = self.interactive_launch_generation;
        self.interactive_launch_pending = Some(PendingInteractiveLaunch {
            generation,
            origin,
            placement,
        });
        let socket_path = self.client.socket_path().to_path_buf();
        let tx = self.interactive_launch_tx.clone();
        self.interactive_launch_handle = Some(runtime.spawn(async move {
            let accepted = request.execute(socket_path).await;
            let _ = tx
                .send(InteractiveLaunchResult {
                    generation,
                    accepted,
                    model_warning,
                })
                .await;
        }));
        Ok(())
    }

    pub(crate) fn request_quick_launch(
        &mut self,
        query: &str,
        working_dir: Option<&Path>,
        workflow_id: Option<Uuid>,
        origin: InteractiveLaunchOrigin,
        placement: LaunchPlacement,
    ) -> bool {
        let cwd = working_dir
            .map(Path::to_path_buf)
            .or_else(|| std::env::current_dir().ok());
        let request = self.owned_quick_launch_request(query, cwd, workflow_id);
        self.begin_interactive_launch(request, origin, placement)
            .is_ok()
    }

    pub(crate) fn owned_quick_launch_request(
        &self,
        query: &str,
        working_dir: Option<PathBuf>,
        workflow_id: Option<Uuid>,
    ) -> OwnedLaunchRequest {
        let mut options = self.quick_new_launch_options(query, working_dir, workflow_id);
        let request = if let Some(entry) = self
            .custom_provider_index
            .and_then(|index| self.settings.custom_providers.get(index))
        {
            if !entry.default_model.is_empty() {
                options.model = Some(entry.default_model.clone());
            }
            OwnedLaunchRequest::CustomSimple {
                options,
                base_url: entry.base_url.clone(),
                api_key: entry.api_key.clone(),
            }
        } else {
            OwnedLaunchRequest::Standard(options)
        };
        request
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn request_taskrabbit_launch(
        &mut self,
        query: &str,
        working_dir: Option<&Path>,
        model_override: Option<&str>,
        provider_override: Option<SessionProvider>,
        custom_provider_index: Option<usize>,
        sandbox: Option<SandboxSpec>,
        origin: InteractiveLaunchOrigin,
        placement: LaunchPlacement,
    ) -> bool {
        let provider = provider_override.unwrap_or(self.selected_provider);
        let model = model_override
            .map(str::to_string)
            .or_else(|| self.selected_model.clone());
        let system_prompt = match self.settings.system_prompt_preset.content() {
            Some(preset) => format!("{}\n\n{}", preset, super::TASKRABBIT_SYSTEM_PROMPT),
            None => super::TASKRABBIT_SYSTEM_PROMPT.to_string(),
        };
        let mut options = LaunchOptions {
            query: query.to_string(),
            title: None,
            working_dir: working_dir
                .map(Path::to_path_buf)
                .or_else(|| std::env::current_dir().ok()),
            provider,
            model,
            system_prompt: Some(system_prompt),
            session_kind: Some(SessionKind::TaskRabbit),
            project_id: self.current_project_id,
            max_retries: Some(3),
            effort: self.selected_effort.clone(),
            parent_id: None,
            sandbox,
            tags: vec![PHASE1_PLACEHOLDER_TAG.to_string()],
            workflow_id: None,
            workflow_id_override: None,
        };
        let request = if let Some(entry) =
            self.custom_provider_for_modal_launch(provider_override, custom_provider_index)
        {
            if options.model.is_none() && !entry.default_model.is_empty() {
                options.model = Some(entry.default_model.clone());
            }
            OwnedLaunchRequest::CustomWithOptions {
                options,
                base_url: entry.base_url.clone(),
                api_key: entry.api_key.clone(),
            }
        } else {
            OwnedLaunchRequest::Standard(options)
        };
        self.begin_interactive_launch(request, origin, placement)
            .is_ok()
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn request_blank_launch(
        &mut self,
        query: &str,
        working_dir: Option<&Path>,
        model_override: Option<&str>,
        provider_override: Option<SessionProvider>,
        custom_provider_index: Option<usize>,
        sandbox: Option<SandboxSpec>,
        origin: InteractiveLaunchOrigin,
        placement: LaunchPlacement,
    ) -> bool {
        let provider = provider_override.unwrap_or(self.selected_provider);
        let mut options = LaunchOptions {
            query: query.to_string(),
            title: None,
            working_dir: working_dir
                .map(Path::to_path_buf)
                .or_else(|| std::env::current_dir().ok()),
            provider,
            model: model_override
                .map(str::to_string)
                .or_else(|| self.selected_model.clone()),
            system_prompt: self
                .settings
                .system_prompt_preset
                .content()
                .map(str::to_string),
            session_kind: Some(SessionKind::Standard),
            project_id: self.current_project_id,
            max_retries: None,
            effort: self.selected_effort.clone(),
            parent_id: None,
            sandbox,
            tags: vec![PHASE1_PLACEHOLDER_TAG.to_string()],
            workflow_id: None,
            workflow_id_override: None,
        };
        let request = if let Some(entry) =
            self.custom_provider_for_modal_launch(provider_override, custom_provider_index)
        {
            if options.model.is_none() && !entry.default_model.is_empty() {
                options.model = Some(entry.default_model.clone());
            }
            OwnedLaunchRequest::CustomSimple {
                options,
                base_url: entry.base_url.clone(),
                api_key: entry.api_key.clone(),
            }
        } else {
            OwnedLaunchRequest::Standard(options)
        };
        self.begin_interactive_launch(request, origin, placement)
            .is_ok()
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn request_typed_launch(
        &mut self,
        query: &str,
        working_dir: &Path,
        model_override: Option<&str>,
        provider_override: Option<SessionProvider>,
        custom_provider_index: Option<usize>,
        sandbox: Option<SandboxSpec>,
        kind: SessionKind,
        parent_id: Option<Uuid>,
        origin: InteractiveLaunchOrigin,
        placement: LaunchPlacement,
    ) -> bool {
        let provider = provider_override.unwrap_or(self.selected_provider);
        let mut options = LaunchOptions {
            query: query.to_string(),
            title: None,
            working_dir: Some(working_dir.to_path_buf()),
            provider,
            model: model_override
                .map(str::to_string)
                .or_else(|| self.selected_model.clone()),
            system_prompt: self
                .settings
                .system_prompt_preset
                .content()
                .map(str::to_string),
            session_kind: Some(kind),
            project_id: self.current_project_id,
            max_retries: None,
            effort: self.selected_effort.clone(),
            parent_id,
            sandbox,
            tags: vec![PHASE1_PLACEHOLDER_TAG.to_string()],
            workflow_id: None,
            workflow_id_override: None,
        };
        let request = if let Some(entry) =
            self.custom_provider_for_modal_launch(provider_override, custom_provider_index)
        {
            if options.model.is_none() && !entry.default_model.is_empty() {
                options.model = Some(entry.default_model.clone());
            }
            OwnedLaunchRequest::CustomSimple {
                options,
                base_url: entry.base_url.clone(),
                api_key: entry.api_key.clone(),
            }
        } else {
            OwnedLaunchRequest::Standard(options)
        };
        self.begin_interactive_launch(request, origin, placement)
            .is_ok()
    }

    pub(crate) fn request_create_entity_leaf_launch(
        &mut self,
        options: LaunchOptions,
        form: crate::overlay::create_entity_form::PendingCreateEntityLeafForm,
    ) -> Result<(), String> {
        self.begin_interactive_launch(
            OwnedLaunchRequest::Standard(options),
            InteractiveLaunchOrigin::CreateEntityLeaf {
                form: Box::new(form),
            },
            LaunchPlacement::CurrentPane,
        )
    }

    pub(crate) fn interactive_launch_pending_for_overlay(&self, overlay_id: Uuid) -> bool {
        self.interactive_launch_pending
            .as_ref()
            .is_some_and(|pending| match &pending.origin {
                InteractiveLaunchOrigin::LegacyPrompt {
                    overlay_id: pending_id,
                    ..
                }
                | InteractiveLaunchOrigin::StackedPrompt {
                    overlay_id: pending_id,
                    ..
                } => *pending_id == overlay_id,
                _ => false,
            })
    }

    pub(crate) fn interactive_quick_launch_pending(&self) -> bool {
        self.interactive_launch_pending
            .as_ref()
            .is_some_and(|pending| {
                matches!(pending.origin, InteractiveLaunchOrigin::QuickInput { .. })
            })
    }

    pub(crate) fn interactive_create_entity_launch_pending(&self) -> bool {
        self.interactive_launch_pending
            .as_ref()
            .is_some_and(|pending| {
                matches!(
                    pending.origin,
                    InteractiveLaunchOrigin::CreateEntityLeaf { .. }
                )
            })
    }

    pub(crate) fn apply_interactive_launch_result(
        &mut self,
        result: InteractiveLaunchResult,
    ) -> bool {
        let Some(pending) = self.interactive_launch_pending.as_ref() else {
            return false;
        };
        if pending.generation != result.generation {
            return false;
        }
        self.interactive_launch_handle = None;
        let pending = self
            .interactive_launch_pending
            .take()
            .expect("generation-matched pending launch");
        match result.accepted {
            Ok(session_id) => {
                let create_entity_leaf = matches!(
                    &pending.origin,
                    InteractiveLaunchOrigin::CreateEntityLeaf { .. }
                );
                self.place_accepted_launch(session_id, pending.placement);
                match pending.origin {
                    InteractiveLaunchOrigin::QuickInput { query } => {
                        if self.input_buffer == query {
                            self.input_buffer.clear();
                            self.input_mode = crate::types::InputMode::Normal;
                            self.input_purpose = crate::types::InputPurpose::NewSession;
                        }
                    }
                    InteractiveLaunchOrigin::LegacyPrompt {
                        overlay_id,
                        purpose,
                        draft_lines,
                        corrected_preview,
                    } => {
                        let unchanged = matches!(
                            &self.overlay,
                            crate::types::OverlayState::Prompt {
                                overlay_id: current,
                                surface,
                                ..
                            } if *current == overlay_id
                                && surface.textarea.lines() == draft_lines.as_slice()
                                && surface.corrected_preview == corrected_preview
                        );
                        if unchanged {
                            self.restore_previous_overlay();
                            self.clear_prompt_draft(&purpose);
                        } else {
                            self.notify("Session accepted; edited prompt draft preserved");
                        }
                    }
                    InteractiveLaunchOrigin::StackedPrompt {
                        overlay_id,
                        purpose,
                        draft_lines,
                        corrected_preview,
                    } => {
                        if let Some(index) = self.input_overlays.iter().position(|overlay| {
                            matches!(
                                overlay,
                                crate::types::OverlayState::Prompt {
                                    overlay_id: current,
                                    surface,
                                    ..
                                } if *current == overlay_id
                                    && surface.textarea.lines() == draft_lines.as_slice()
                                    && surface.corrected_preview == corrected_preview
                            )
                        }) {
                            self.input_overlays.remove(index);
                            self.focused_input_idx = self
                                .focused_input_idx
                                .min(self.input_overlays.len().saturating_sub(1));
                            self.clear_prompt_draft(&purpose);
                        } else {
                            self.notify("Session accepted; edited prompt draft preserved");
                        }
                    }
                    InteractiveLaunchOrigin::CreateEntityLeaf { form } => {
                        crate::overlay::create_entity_form::finish_create_entity_leaf_launch(
                            self, *form, session_id,
                        );
                    }
                    InteractiveLaunchOrigin::Detached => {}
                }
                if !create_entity_leaf {
                    self.push_notification(
                        crate::types::NotificationKind::SessionLaunching,
                        crate::types::NotificationPriority::Low,
                        "Session launching...".to_string(),
                        Some(session_id),
                    );
                }
                if let Some(message) = result.model_warning {
                    self.notify(message);
                }
                true
            }
            Err(error) => {
                match pending.origin {
                    InteractiveLaunchOrigin::CreateEntityLeaf { form } => {
                        crate::overlay::create_entity_form::restore_create_entity_leaf_launch(
                            self,
                            *form,
                            error.clone(),
                        );
                        self.notify_error(format!("Create failed: {error}; form preserved"));
                    }
                    _ => self.notify_error(format!("Launch failed: {error}; draft preserved")),
                }
                true
            }
        }
    }

    fn clear_prompt_draft(&mut self, purpose: &crate::types::PromptPurpose) {
        match purpose {
            crate::types::PromptPurpose::TaskRabbit => self.taskrabbit_draft.clear(),
            crate::types::PromptPurpose::Blank => self.blank_draft.clear(),
            _ => {}
        }
    }

    /// Launch a new session via the daemon (input-bar quick-new path).
    pub async fn launch_session(
        &mut self,
        query: &str,
        working_dir: Option<&std::path::Path>,
        workflow_id: Option<uuid::Uuid>,
    ) -> bool {
        self.request_quick_launch(
            query,
            working_dir,
            workflow_id,
            InteractiveLaunchOrigin::Detached,
            LaunchPlacement::CurrentPane,
        )
    }

    /// Launch a TaskRabbit one-shot session with system prompt injection.
    /// When `model_override`/`provider_override` are provided (per-session overrides from
    /// the prompt dropdown), they take precedence over the global defaults.
    pub async fn launch_taskrabbit(
        &mut self,
        query: &str,
        working_dir: Option<&std::path::Path>,
        model_override: Option<&str>,
        provider_override: Option<rsi_common::types::SessionProvider>,
        sandbox: Option<rsi_common::types::SandboxSpec>,
    ) -> bool {
        self.request_taskrabbit_launch(
            query,
            working_dir,
            model_override,
            provider_override,
            None,
            sandbox,
            InteractiveLaunchOrigin::Detached,
            LaunchPlacement::CurrentPane,
        )
    }

    /// Launch a Blank session.
    /// When `model_override`/`provider_override` are provided (per-session overrides from
    /// the prompt dropdown), they take precedence over the global defaults.
    /// `sandbox` is `Some(SandboxSpec)` when the user toggled sandbox mode in the prompt overlay.
    pub async fn launch_blank(
        &mut self,
        query: &str,
        working_dir: Option<&std::path::Path>,
        model_override: Option<&str>,
        provider_override: Option<rsi_common::types::SessionProvider>,
        sandbox: Option<rsi_common::types::SandboxSpec>,
    ) -> bool {
        self.request_blank_launch(
            query,
            working_dir,
            model_override,
            provider_override,
            None,
            sandbox,
            InteractiveLaunchOrigin::Detached,
            LaunchPlacement::CurrentPane,
        )
    }

    /// Continue a completed/interrupted session with a follow-up query.
    ///
    /// Returns `true` when the daemon accepted the continue, `false` when it
    /// was rejected (not connected, or the RPC returned an error). Callers that
    /// destroy a user-typed input surface before calling this MUST restore that
    /// text on `false` — the daemon's interrupt-then-wait path can legitimately
    /// time out while a provider tears down, and silently eating the user's
    /// prompt on that path is what made the failure feel like data loss.
    pub async fn continue_session(&mut self, session_id: Uuid, query: &str) -> bool {
        if !self.poll.connected {
            self.notify_error("Not connected to daemon");
            return false;
        }
        match self.client.continue_session(session_id, query).await {
            Ok(()) => {
                self.push_notification(
                    crate::types::NotificationKind::SessionResuming,
                    crate::types::NotificationPriority::Low,
                    "Session resuming...".to_string(),
                    Some(session_id),
                );

                // Optimistically update local state to Starting so loading bar appears immediately
                if let Some(state) = self.sessions.get_mut(&session_id) {
                    state.session.status = rsi_common::types::SessionStatus::Starting;
                    state.session.updated_at = chrono::Utc::now();
                    self.mark_dirty();
                }
                true
            }
            Err(e) => {
                if let Some(retry_after_ms) = e.reclaim_prepared_retry_ms() {
                    self.notify_error(format!(
                        "Target reclaim pending; retry continue in {} ms",
                        retry_after_ms
                    ));
                } else {
                    self.notify_error(format!("Continue failed: {}", e));
                }
                false
            }
        }
    }

    /// Restore a rejected continue's prompt into a session's input bar.
    ///
    /// Mirrors the existing modal-cancel transfer at `overlay/mod.rs` (extract
    /// text, then rebuild the surface with `new_insert_with_content`) so a
    /// failed continue leaves the user's prompt exactly where they typed it
    /// instead of discarding it. Left in Normal mode so the restored text is
    /// editable/resubmittable without a stray insert-mode surprise.
    ///
    /// Takes the surface's RAW lines, not the query string sent to the daemon:
    /// `content_for_send` collapses visual wraps into spaces, so restoring from
    /// the query would silently reflow a multi-line prompt into one line.
    pub fn restore_continue_lines(&mut self, session_id: Uuid, lines: Vec<String>) {
        if lines.iter().all(String::is_empty) {
            return;
        }
        if let Some(state) = self.sessions.get_mut(&session_id) {
            state.input_bar.surface =
                crate::input_surface::InputSurface::new_insert_with_content(lines);
            state.input_bar.surface.mode = crate::types::PopupMode::Normal;
            self.mark_dirty();
        }
    }

    /// Delete the currently selected session.
    pub async fn delete_focused_session(&mut self) {
        let session_id = match self.selected_session_id() {
            Some(id) => id,
            None => {
                self.notify("No session selected");
                return;
            }
        };

        if let Some(state) = self.sessions.get(&session_id) {
            if matches!(
                state.session.status,
                rsi_common::types::SessionStatus::Running
                    | rsi_common::types::SessionStatus::Starting
            ) {
                self.notify("Cannot delete active session. Interrupt first (x).");
                return;
            }
            if state.session.status == rsi_common::types::SessionStatus::Archived {
                self.notify("Session is already archived.");
                return;
            }
        }

        let query_preview = self
            .sessions
            .get(&session_id)
            .map(|s| {
                let q = &s.session.query;
                if q.len() > 40 {
                    format!("{}...", &q[..40])
                } else {
                    q.clone()
                }
            })
            .unwrap_or_default();

        if let Err(e) = self.client.delete_session(session_id).await {
            self.notify_error(format!("Delete failed: {}", e));
            return;
        }

        self.sessions.remove(&session_id);
        self.clean_jumplist(session_id);
        if let Some(pos) = self.session_order.iter().position(|id| *id == session_id) {
            self.session_order.remove(pos);
        }

        self.recalculate_filtered_order();
        self.reconcile_all_session_list_selections(false);

        self.revert_detail_panes_for(session_id);
        self.notify_success(format!("Deleted: {}", query_preview));
    }

    /// Archive the currently selected session (soft delete).
    /// For active sessions: marks for auto-archive on completion.
    /// For inactive sessions: archives immediately.
    pub async fn archive_focused_session(&mut self) {
        let session_id = match self.selected_session_id() {
            Some(id) => id,
            None => {
                self.notify("No session selected");
                return;
            }
        };

        // For active sessions: mark for auto-archive on completion
        if let Some(state) = self.sessions.get(&session_id)
            && matches!(
                state.session.status,
                rsi_common::types::SessionStatus::Running
                    | rsi_common::types::SessionStatus::Starting
            )
        {
            let desired = !state.session.pending_archive;
            match self.client.mark_pending_archive(session_id, desired).await {
                Ok(()) => {
                    if let Some(state) = self.sessions.get_mut(&session_id) {
                        state.session.pending_archive = desired;
                    }
                    self.mark_dirty();
                    if desired {
                        self.notify_success("Will archive when complete");
                    } else {
                        self.notify_success("Auto-archive on completion cleared");
                    }
                }
                Err(e) => {
                    self.notify_error(format!("Mark pending archive failed: {}", e));
                }
            }
            return;
        }

        // For inactive sessions: archive immediately (existing behavior unchanged)
        let query_preview = self
            .sessions
            .get(&session_id)
            .map(|s| {
                let q = &s.session.query;
                if q.len() > 40 {
                    format!("{}...", &q[..40])
                } else {
                    q.clone()
                }
            })
            .unwrap_or_default();

        let archive_result = match self.client.archive_session(session_id).await {
            Ok(result) => result,
            Err(error) => {
                if let Some(cleanup) = error.archive_cleanup_error() {
                    let phase = cleanup
                        .phase
                        .map(|value| value.as_str())
                        .unwrap_or("preflight");
                    self.notify_error(format!(
                        "Archive retained ({phase}/{}; retryable={}): {}",
                        cleanup.safe_code.as_str(),
                        cleanup.retryable,
                        cleanup.next_action
                    ));
                } else {
                    self.notify_error(format!("Archive failed: {error}"));
                }
                return;
            }
        };

        self.sessions.remove(&session_id);
        self.clean_jumplist(session_id);
        if let Some(pos) = self.session_order.iter().position(|id| *id == session_id) {
            self.session_order.remove(pos);
        }

        self.recalculate_filtered_order();
        self.reconcile_all_session_list_selections(false);

        self.revert_detail_panes_for(session_id);
        if let Some(receipt) = archive_result.receipt {
            let run = receipt.run_id.to_string();
            let oid = receipt.source_oid.as_str();
            self.notify_success(format!(
                "Archived: {} — settled run {} branch {}@{} preserved",
                query_preview,
                &run[..8],
                receipt.source_branch,
                &oid[..oid.len().min(12)]
            ));
        } else {
            self.notify_success(format!("Archived: {}", query_preview));
        }
    }

    /// Unarchive the focused session in the archive zone or detail view.
    pub async fn unarchive_focused_session(&mut self) {
        let session_id = match self.focused_pane() {
            Some(Pane::SessionDetail { session_id }) => {
                if let Some(state) = self.sessions.get(session_id) {
                    if state.session.status == rsi_common::types::SessionStatus::Archived {
                        Some(*session_id)
                    } else {
                        None
                    }
                } else {
                    None
                }
            }
            Some(Pane::SessionList {
                active_zone: crate::types::SessionListZone::Archive,
                selected_session: Some(sid),
                ..
            }) => Some(*sid),
            _ => None,
        };

        let Some(session_id) = session_id else {
            self.notify("No archived session focused");
            return;
        };

        match self.client.unarchive_session(session_id).await {
            Ok(session) => {
                if let Some(state) = self.sessions.get_mut(&session_id) {
                    state.session.status = rsi_common::types::SessionStatus::Completed;
                }
                if !self.session_order.contains(&session_id) {
                    self.session_order.push(session_id);
                }
                // Remove from archive order
                self.filtered_archived_order.retain(|id| *id != session_id);
                self.recalculate_filtered_order();
                self.reconcile_all_session_list_selections(false);

                let query_preview = if session.query.len() > 30 {
                    format!("{}...", &session.query[..30])
                } else {
                    session.query.clone()
                };
                self.notify_success(format!("Unarchived: {}", query_preview));
            }
            Err(e) => {
                self.notify_error(format!("Unarchive failed: {}", e));
            }
        }
    }

    /// Refresh the archived sessions list from the daemon.
    pub async fn refresh_archived_sessions(&mut self) {
        let project_id = self.current_project_id;
        match self.client.list_archived_sessions(project_id).await {
            Ok(sessions) => {
                let mut order = Vec::with_capacity(sessions.len());
                for session in sessions {
                    let id = session.id;
                    order.push(id);
                    self.sessions
                        .entry(id)
                        .or_insert_with(|| crate::types::SessionState::new(session));
                }
                self.filtered_archived_order = order;
            }
            Err(e) => {
                self.notify_error(format!("Failed to load archives: {}", e));
            }
        }
    }

    /// Toggle pin state of the currently selected session.
    pub async fn toggle_pin_focused_session(&mut self) {
        let session_id = match self.selected_session_id() {
            Some(id) => id,
            None => {
                self.notify("No session selected");
                return;
            }
        };

        match self.client.toggle_pin(session_id).await {
            Ok(pinned_at) => {
                if let Some(state) = self.sessions.get_mut(&session_id) {
                    state.session.pinned_at = pinned_at.as_ref().and_then(|s| {
                        chrono::DateTime::parse_from_rfc3339(s)
                            .ok()
                            .map(|dt| dt.with_timezone(&chrono::Utc))
                    });
                }
                self.sort_sessions(false);
                let msg = if pinned_at.is_some() {
                    "Session pinned"
                } else {
                    "Session unpinned"
                };
                self.notify_success(msg);
            }
            Err(e) => {
                self.notify_error(format!("Pin failed: {}", e));
            }
        }
    }

    /// Cancel a pending automatic retry on the currently selected session.
    ///
    /// The daemon's `CancelRetry` returns `true` only when a retry timer was
    /// actually armed, so the client trusts that bool rather than pre-checking
    /// client-visible state (there is no wire field that reflects whether a
    /// retry is currently armed — only `retry_attempt`/`max_retries` counts).
    pub async fn cancel_retry_focused_session(&mut self) {
        let session_id = match self.selected_session_id() {
            Some(id) => id,
            None => {
                self.notify("No session selected");
                return;
            }
        };

        match self.client.cancel_retry(session_id).await {
            Ok(true) => {
                self.notify_success("Retry cancelled");
            }
            Ok(false) => {
                self.notify("No pending retry to cancel");
            }
            Err(e) => {
                self.notify_error(format!("Cancel retry failed: {}", e));
            }
        }
    }

    /// Toggle "testing needed" marker on the currently selected session.
    pub async fn toggle_testing_needed_focused_session(&mut self) {
        let session_id = match self.selected_session_id() {
            Some(id) => id,
            None => {
                self.notify("No session selected");
                return;
            }
        };

        match self.client.toggle_testing_needed(session_id).await {
            Ok(testing_needed_at) => {
                if let Some(state) = self.sessions.get_mut(&session_id) {
                    state.session.testing_needed_at = testing_needed_at.as_ref().and_then(|s| {
                        chrono::DateTime::parse_from_rfc3339(s)
                            .ok()
                            .map(|dt| dt.with_timezone(&chrono::Utc))
                    });
                }
                let msg = if testing_needed_at.is_some() {
                    "Marked for testing"
                } else {
                    "Testing mark removed"
                };
                self.notify_success(msg);
            }
            Err(e) => {
                self.notify_error(format!("Toggle testing needed failed: {}", e));
            }
        }
    }

    /// Toggle auto-rotation disabled state on the currently selected session.
    pub async fn toggle_rotation_disabled_focused_session(&mut self) {
        let session_id = match self.selected_session_id() {
            Some(id) => id,
            None => {
                self.notify("No session selected");
                return;
            }
        };

        match self.client.toggle_rotation_disabled(session_id).await {
            Ok(rotation_disabled_at) => {
                if let Some(state) = self.sessions.get_mut(&session_id) {
                    state.session.rotation_disabled_at =
                        rotation_disabled_at.as_ref().and_then(|s| {
                            chrono::DateTime::parse_from_rfc3339(s)
                                .ok()
                                .map(|dt| dt.with_timezone(&chrono::Utc))
                        });
                }
                let msg = if rotation_disabled_at.is_some() {
                    "Auto-rotation disabled"
                } else {
                    "Auto-rotation enabled"
                };
                self.notify_success(msg);
            }
            Err(e) => {
                self.notify_error(format!("Toggle rotation failed: {}", e));
            }
        }
    }

    /// Reassign the focused session to a different project.
    pub async fn reassign_session_project(
        &mut self,
        session_id: uuid::Uuid,
        project_id: Option<uuid::Uuid>,
    ) {
        match self
            .client
            .update_session_project(session_id, project_id)
            .await
        {
            Ok(()) => {
                if let Some(state) = self.sessions.get_mut(&session_id) {
                    state.session.project_id = project_id;
                }
                self.sort_sessions(false);
                let msg = match project_id {
                    Some(pid) => {
                        let name = self
                            .projects
                            .iter()
                            .find(|p| p.id == pid)
                            .map(|p| p.name.as_str())
                            .unwrap_or("unknown");
                        format!("Session → {}", name)
                    }
                    None => "Session unassigned".to_string(),
                };
                self.notify_success(msg);
            }
            Err(e) => {
                self.notify_error(format!("Reassign failed: {}", e));
            }
        }
    }

    /// Manually trigger context rotation on the focused session.
    pub async fn rotate_focused_session(&mut self) {
        let session_id = match self.focused_pane().cloned() {
            Some(Pane::SessionDetail { session_id }) => Some(session_id),
            _ => self.selected_session_id(),
        };

        if let Some(sid) = session_id {
            if let Some(state) = self.sessions.get(&sid) {
                use rsi_common::types::SessionStatus;
                if !matches!(
                    state.session.status,
                    SessionStatus::Running
                        | SessionStatus::Completed
                        | SessionStatus::Interrupted
                        | SessionStatus::Failed
                ) {
                    self.notify("Can only rotate running or stopped sessions");
                    return;
                }
            }
            if let Err(e) = self.client.rotate_session(sid).await {
                self.notify_error(format!("Rotate failed: {}", e));
            }
        } else {
            self.notify("No session selected");
        }
    }

    /// Interrupt the session in the focused pane.
    pub async fn interrupt_focused_session(&mut self) {
        if let Some(session_id) = self.selected_session_id()
            && let Err(e) = self.client.interrupt_session(session_id).await
        {
            self.notify_error(format!("Interrupt failed: {}", e));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{App, command_frontmatter_model_conflict};
    use crate::{client::DaemonClient, settings::CustomProviderEntry};
    use rsi_common::types::SessionProvider;
    use std::fs;
    use std::path::PathBuf;
    use tempfile::tempdir;
    use uuid::Uuid;

    #[test]
    fn modal_custom_provider_overrides_global_custom_provider() {
        let mut app = App::new(DaemonClient::new(PathBuf::from("/tmp/rsi-test.sock")));
        app.settings.custom_providers = vec![
            CustomProviderEntry {
                id: Uuid::new_v4(),
                name: "Global custom".to_string(),
                base_url: "https://global.example/v1".to_string(),
                api_key: "global-key".to_string(),
                default_model: "global-model".to_string(),
            },
            CustomProviderEntry {
                id: Uuid::new_v4(),
                name: "Modal custom".to_string(),
                base_url: "https://modal.example/v1".to_string(),
                api_key: "modal-key".to_string(),
                default_model: "modal-model".to_string(),
            },
        ];
        app.custom_provider_index = Some(0);

        let selected = app
            .custom_provider_for_modal_launch(Some(SessionProvider::Local), Some(1))
            .expect("modal selection should resolve its custom provider");
        assert_eq!(selected.name, "Modal custom");

        assert!(
            app.custom_provider_for_modal_launch(Some(SessionProvider::Claude), None)
                .is_none()
        );
    }

    #[test]
    fn warns_when_slash_command_declares_different_claude_family() {
        let temp = tempdir().unwrap();
        let commands_dir = temp.path().join(".claude").join("commands");
        fs::create_dir_all(&commands_dir).unwrap();
        fs::write(
            commands_dir.join("master_orchestrate.md"),
            "---\nmodel: opus\n---\n# Body\n",
        )
        .unwrap();

        let warning = command_frontmatter_model_conflict(
            "/master_orchestrate thoughts/shared/orchestration/board.md",
            Some(temp.path()),
            SessionProvider::Claude,
            Some("claude-fable-5-1"),
        );

        assert!(
            warning.is_some(),
            "expected a warning when command frontmatter and selected model disagree"
        );
        let message = warning.unwrap();
        assert!(message.contains("/master_orchestrate"));
        assert!(message.contains("model opus"));
        assert!(message.contains("Fable 5.1"));
    }

    #[test]
    fn no_warning_when_selected_model_matches_command_family() {
        let temp = tempdir().unwrap();
        let commands_dir = temp.path().join(".claude").join("commands");
        fs::create_dir_all(&commands_dir).unwrap();
        fs::write(
            commands_dir.join("master_orchestrate.md"),
            "---\nmodel: opus\n---\n# Body\n",
        )
        .unwrap();

        let warning = command_frontmatter_model_conflict(
            "/master_orchestrate thoughts/shared/orchestration/board.md",
            Some(temp.path()),
            SessionProvider::Claude,
            Some("claude-opus-4-8"),
        );

        assert!(warning.is_none());
    }
}

//! Custom command parser for flywheel.
//!
//! Parses `:` commands, handling flywheel-specific commands first and
//! falling through to standard vim command handling for the rest.

use crate::action_registry::{ActionId, resolve_command};
use crate::modalkit_types::LcAction;

/// Result of parsing a command string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandResult {
    /// A flywheel-specific action to execute.
    LcAction(LcAction),
    /// Command was not recognized — fall through to modalkit.
    Unhandled(String),
}

/// Parse a command string (without the leading `:`).
///
/// Custom commands are checked first. If the command doesn't match any
/// custom prefix, it returns `Unhandled` for modalkit to process.
pub fn parse_command(input: &str) -> CommandResult {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return CommandResult::Unhandled(String::new());
    }

    let Some((descriptor, args)) = resolve_command(trimmed) else {
        return CommandResult::Unhandled(trimmed.to_string());
    };
    if !args.is_empty() && descriptor.command_aliases[0].starts_with("manager ") {
        return CommandResult::Unhandled(
            "Use :manager [appoint|scope|clear|policy|board|decisions|inbox|inspect]".to_string(),
        );
    }
    if !args.is_empty()
        && matches!(
            descriptor.id,
            ActionId::ContextHelp
                | ActionId::AllCommands
                | ActionId::Refresh
                | ActionId::OpenIssuesWorkspace
        )
    {
        return CommandResult::Unhandled(trimmed.to_string());
    }
    let args = if args.is_empty() { None } else { Some(args) };

    match descriptor.id {
        // :continue / :cont — continue session
        ActionId::ContinueSession => match args {
            Some(query) if !query.is_empty() => {
                CommandResult::LcAction(LcAction::ContinueSession(Some(query.to_string())))
            }
            _ => CommandResult::LcAction(LcAction::ContinueSession(None)),
        },

        // :kill / :ki — interrupt session
        ActionId::InterruptSession => CommandResult::LcAction(LcAction::InterruptSession),

        // :delete / :del — delete selected session
        ActionId::DeleteSession => CommandResult::LcAction(LcAction::DeleteSession),

        // :archive / :arc — archive selected session (soft delete)
        ActionId::ArchiveSession => CommandResult::LcAction(LcAction::ArchiveSession),

        // :rotate / :rot — manually trigger context rotation
        ActionId::RotateSession => CommandResult::LcAction(LcAction::RotateSession),

        // :archives — switch to archive zone
        ActionId::Archives => CommandResult::LcAction(LcAction::GoToArchiveZone),

        // :model / :mod — select model or open model selector
        ActionId::Model => match args {
            Some(model) if !model.is_empty() => {
                CommandResult::LcAction(LcAction::SelectModel(Some(model.to_string())))
            }
            _ => CommandResult::LcAction(LcAction::ToggleModelDropdown),
        },

        // :theme — select theme flavor or open picker
        ActionId::OpenThemePicker => match args {
            Some(name) if !name.is_empty() => {
                CommandResult::LcAction(LcAction::SelectTheme(name.to_string()))
            }
            _ => CommandResult::LcAction(LcAction::OpenThemePicker),
        },

        // :sessions / :ls — show session list
        ActionId::Sessions => CommandResult::LcAction(LcAction::ListSessions),

        // :alerts / :att / :attention — show attention queue
        ActionId::Alerts => CommandResult::LcAction(LcAction::ToggleNotifications),

        // :quit / :q — quit
        ActionId::Quit => CommandResult::LcAction(LcAction::Quit),

        // :projects — open project picker
        ActionId::Projects => CommandResult::LcAction(LcAction::OpenProjectPicker),

        // :project <name> — switch to project by name (opens/focuses workspace)
        ActionId::Project => match args {
            Some(name) if !name.is_empty() => {
                CommandResult::LcAction(LcAction::SwitchProject(name.to_string()))
            }
            _ => CommandResult::LcAction(LcAction::OpenProjectPicker),
        },

        // :task [query] — launch TaskRabbit or open popup
        ActionId::Task => match args {
            Some(query) if !query.is_empty() => {
                CommandResult::LcAction(LcAction::LaunchTaskRabbit(query.to_string()))
            }
            _ => CommandResult::LcAction(LcAction::TaskRabbitPrompt),
        },

        // :blank [query] — launch Blank general-purpose session or open popup
        ActionId::Blank => match args {
            Some(query) if !query.is_empty() => {
                CommandResult::LcAction(LcAction::LaunchBlank(query.to_string()))
            }
            _ => CommandResult::LcAction(LcAction::BlankPrompt),
        },

        // :project-new <name> [path] — create project
        ActionId::ProjectNew => match args {
            Some(args_str) if !args_str.is_empty() => {
                let (name, path) = match args_str.split_once(char::is_whitespace) {
                    Some((n, p)) => (n.to_string(), Some(p.trim().to_string())),
                    None => (args_str.to_string(), None),
                };
                CommandResult::LcAction(LcAction::CreateProject { name, path })
            }
            _ => CommandResult::LcAction(LcAction::OpenProjectPicker), // Open picker if no args
        },

        // :project-edit <name> — edit project
        ActionId::ProjectEdit => match args {
            Some(name) if !name.is_empty() => {
                CommandResult::LcAction(LcAction::EditProjectByName(name.to_string()))
            }
            _ => CommandResult::LcAction(LcAction::OpenProjectPicker),
        },

        // :project-delete <name> — delete project
        ActionId::ProjectDelete => match args {
            Some(name) if !name.is_empty() => {
                CommandResult::LcAction(LcAction::DeleteProjectByName(name.to_string()))
            }
            _ => {
                // No name given — ignore
                CommandResult::Unhandled(trimmed.to_string())
            }
        },

        // :set / :settings — open settings pane
        ActionId::SettingsCommand => CommandResult::LcAction(LcAction::OpenSettings),

        // :stopall / :stop-all — immediate global model-control stop
        ActionId::StopAll => CommandResult::LcAction(LcAction::EmergencyStopAll),

        // :hooks — open Settings -> Hooks (Claude Code) directly in items focus
        ActionId::Hooks => CommandResult::LcAction(LcAction::OpenSettingsAt(
            crate::settings_registry::SettingsSection::ClaudeHooks,
        )),

        // :skills — open Settings -> Skills (Claude Code) directly in items focus
        ActionId::Skills => CommandResult::LcAction(LcAction::OpenSettingsAt(
            crate::settings_registry::SettingsSection::ClaudeSkills,
        )),

        // :group — open label picker overlay
        ActionId::Group => CommandResult::LcAction(LcAction::OpenLabelPicker),

        // :diagnostics / :diag — open diagnostics overlay (RSI_PROFILE metrics)
        ActionId::Diagnostics => CommandResult::LcAction(LcAction::OpenDiagnostics),

        // :graph — open graph review overlay (visual workflow editor)
        ActionId::Graph => CommandResult::LcAction(LcAction::OpenGraphReview),

        // :topology-resolve [<execution_id>] inspect|accept|retry|discard [<commit>]
        // (a bare command reports its usage from the handler).
        ActionId::TopologyResolve => CommandResult::LcAction(LcAction::ResolveTopologyAttempt(
            args.map(str::trim).unwrap_or_default().to_string(),
        )),

        // :dag — open read-only recursive DAG browser
        ActionId::Dag => CommandResult::LcAction(LcAction::OpenRecursiveDagBrowser),

        // :context [text] / :ctx [text] — set or clear active task context
        ActionId::Context => match args {
            Some(text) if !text.is_empty() => {
                // Cap at 500 characters
                let capped = if text.len() > 500 { &text[..500] } else { text };
                CommandResult::LcAction(LcAction::SetActiveTask(Some(capped.to_string())))
            }
            _ => CommandResult::LcAction(LcAction::SetActiveTask(None)),
        },

        // :card — entity card editor
        // :card          → open card editor for current project
        // :card user     → open user card editor
        // :card add <f>  → add fact to current project card
        // :card user add <f> → add fact to user card
        ActionId::Card => match args {
            None | Some("") => CommandResult::LcAction(LcAction::OpenProjectCard),
            Some(arg) => {
                let arg = arg.trim();
                if arg == "user" {
                    CommandResult::LcAction(LcAction::OpenUserCard)
                } else if let Some(fact) = arg.strip_prefix("add ") {
                    let fact = fact.trim().trim_matches('"');
                    if fact.is_empty() {
                        CommandResult::Unhandled(trimmed.to_string())
                    } else {
                        CommandResult::LcAction(LcAction::AddProjectCardFact(fact.to_string()))
                    }
                } else if let Some(rest) = arg.strip_prefix("user ") {
                    if let Some(fact) = rest.strip_prefix("add ") {
                        let fact = fact.trim().trim_matches('"');
                        if fact.is_empty() {
                            CommandResult::Unhandled(trimmed.to_string())
                        } else {
                            CommandResult::LcAction(LcAction::AddUserCardFact(fact.to_string()))
                        }
                    } else {
                        CommandResult::LcAction(LcAction::OpenUserCard)
                    }
                } else {
                    CommandResult::LcAction(LcAction::OpenProjectCard)
                }
            }
        },

        // :ask [query] — open dialectic query overlay or query directly
        ActionId::Ask => match args {
            Some(query) if !query.is_empty() => {
                CommandResult::LcAction(LcAction::AskQuery(query.to_string()))
            }
            _ => CommandResult::LcAction(LcAction::OpenDialectic),
        },

        // :term / :terminal / :shell — toggle embedded terminal
        ActionId::Terminal => CommandResult::LcAction(LcAction::ToggleTerminal),

        // :lead / :setlead — set focused leaf as lead of its parent Epic
        ActionId::Lead => CommandResult::LcAction(LcAction::SetEpicLead),

        ActionId::Manager => match args {
            None => CommandResult::LcAction(LcAction::OpenHarnessManager),
            Some(_) => CommandResult::Unhandled(
                "Use :manager [appoint|scope|clear|policy|board|decisions|inbox|inspect]"
                    .to_string(),
            ),
        },
        ActionId::ManagerAppoint => CommandResult::LcAction(LcAction::AppointHarnessManager),
        ActionId::ManagerScope => CommandResult::LcAction(LcAction::EditHarnessManagerScope),
        ActionId::ManagerClear => CommandResult::LcAction(LcAction::ClearHarnessManagerScope),
        ActionId::ManagerPolicy => CommandResult::LcAction(LcAction::EditHarnessManagerPolicy),
        ActionId::ManagerBoard => CommandResult::LcAction(LcAction::OpenHarnessManagerBoard),
        ActionId::ManagerDecisions => {
            CommandResult::LcAction(LcAction::OpenHarnessManagerDecisions)
        }
        ActionId::ManagerInbox => CommandResult::LcAction(LcAction::OpenHarnessManagerInbox),
        ActionId::ManagerInspect => CommandResult::LcAction(LcAction::OpenHarnessManagerInspect),
        ActionId::ContextHelp => CommandResult::LcAction(LcAction::ToggleKeybindingsHelp),
        ActionId::AllCommands => CommandResult::LcAction(LcAction::OpenAllCommands),
        ActionId::OpenManual => match args {
            None => CommandResult::LcAction(LcAction::OpenManual(
                crate::manual::open::ManualMode::Browser,
            )),
            Some("pager") => CommandResult::LcAction(LcAction::OpenManual(
                crate::manual::open::ManualMode::Pager,
            )),
            Some("pdf") => {
                CommandResult::LcAction(LcAction::OpenManual(crate::manual::open::ManualMode::Pdf))
            }
            Some(_) => CommandResult::Unhandled("Use :manual [pager|pdf]".to_string()),
        },
        ActionId::Refresh => CommandResult::LcAction(LcAction::RefreshNavigation),
        ActionId::OpenIssuesWorkspace => CommandResult::LcAction(LcAction::OpenIssuesWorkspace),

        // :rate / :r [N] — rate the focused session (1–10) or open rating overlay
        ActionId::Rate => match args {
            Some(num_str) if !num_str.is_empty() => match num_str.parse::<u32>() {
                Ok(n) if (1..=10).contains(&n) => CommandResult::LcAction(LcAction::RateSession(n)),
                _ => CommandResult::Unhandled("Invalid rating. Use :rate <1-10>".to_string()),
            },
            _ => CommandResult::LcAction(LcAction::OpenRatingOverlay),
        },

        // Everything else: fall through to modalkit
        _ => CommandResult::Unhandled(trimmed.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_alias_parses_pager_and_pdf_arguments() {
        use crate::manual::open::ManualMode;
        for alias in ["manual", "man"] {
            assert_eq!(
                parse_command(alias),
                CommandResult::LcAction(LcAction::OpenManual(ManualMode::Browser))
            );
            assert_eq!(
                parse_command(&format!("{alias} pager")),
                CommandResult::LcAction(LcAction::OpenManual(ManualMode::Pager))
            );
            assert_eq!(
                parse_command(&format!("{alias} pdf")),
                CommandResult::LcAction(LcAction::OpenManual(ManualMode::Pdf))
            );
        }
        assert_eq!(
            parse_command("manual foo"),
            CommandResult::Unhandled("Use :manual [pager|pdf]".to_string())
        );
    }

    #[test]
    fn archives_command_keeps_archive_zone_navigation() {
        assert_eq!(
            parse_command("archives"),
            CommandResult::LcAction(LcAction::GoToArchiveZone)
        );
    }

    #[tokio::test]
    async fn commands_alias_opens_all_view() {
        assert_eq!(
            parse_command("commands"),
            CommandResult::LcAction(LcAction::OpenAllCommands)
        );
        let mut app = crate::app::app_test_helpers::with_session_list(0);
        crate::action_handler::dispatch_lc_action(&mut app, LcAction::OpenAllCommands).await;
        assert!(matches!(
            app.overlay,
            crate::types::OverlayState::KeybindingsHelp {
                view: crate::overlay::keybindings_help::HelpView::All,
                ..
            }
        ));
    }

    #[test]
    fn every_catalog_command_has_an_execution_path() {
        for descriptor in crate::action_registry::ACTION_DESCRIPTORS {
            for alias in descriptor.command_aliases {
                let result = parse_command(alias);
                let is_vim = matches!(
                    descriptor.id,
                    ActionId::Split
                        | ActionId::VSplit
                        | ActionId::ClosePane
                        | ActionId::OnlyPane
                        | ActionId::TabNew
                        | ActionId::TabClose
                        | ActionId::TabNext
                        | ActionId::TabPrev
                );
                if is_vim || descriptor.id == ActionId::ProjectDelete {
                    assert!(matches!(result, CommandResult::Unhandled(_)), "{alias}");
                } else {
                    assert!(matches!(result, CommandResult::LcAction(_)), "{alias}");
                }
            }
        }
    }

    #[test]
    fn issues_alias_uses_the_registered_workspace_action() {
        assert_eq!(
            parse_command("issues"),
            CommandResult::LcAction(LcAction::OpenIssuesWorkspace)
        );
    }

    #[test]
    fn harness_manager_commands_are_explicit() {
        for (command, action) in [
            ("manager", LcAction::OpenHarnessManager),
            (" manager appoint ", LcAction::AppointHarnessManager),
            ("manager scope", LcAction::EditHarnessManagerScope),
            ("manager clear", LcAction::ClearHarnessManagerScope),
            ("manager policy", LcAction::EditHarnessManagerPolicy),
            ("manager board", LcAction::OpenHarnessManagerBoard),
            ("manager decisions", LcAction::OpenHarnessManagerDecisions),
            ("manager inbox", LcAction::OpenHarnessManagerInbox),
            ("manager inspect", LcAction::OpenHarnessManagerInspect),
        ] {
            assert_eq!(parse_command(command), CommandResult::LcAction(action));
        }
        for command in ["manager delete", "manager appoint extra"] {
            assert_eq!(
                parse_command(command),
                CommandResult::Unhandled(
                    "Use :manager [appoint|scope|clear|policy|board|decisions|inbox|inspect]"
                        .to_string()
                )
            );
        }
    }

    // --- :continue / :cont ---

    #[test]
    fn test_continue_with_query() {
        assert_eq!(
            parse_command("continue fix remaining tests"),
            CommandResult::LcAction(LcAction::ContinueSession(Some(
                "fix remaining tests".to_string()
            )))
        );
    }

    #[test]
    fn test_cont_alias() {
        assert_eq!(
            parse_command("cont follow up"),
            CommandResult::LcAction(LcAction::ContinueSession(Some("follow up".to_string())))
        );
    }

    #[test]
    fn test_continue_without_query() {
        assert_eq!(
            parse_command("continue"),
            CommandResult::LcAction(LcAction::ContinueSession(None))
        );
    }

    // --- :kill / :ki ---

    #[test]
    fn test_kill() {
        assert_eq!(
            parse_command("kill"),
            CommandResult::LcAction(LcAction::InterruptSession)
        );
    }

    #[test]
    fn test_ki_alias() {
        assert_eq!(
            parse_command("ki"),
            CommandResult::LcAction(LcAction::InterruptSession)
        );
    }

    // --- :delete / :del ---

    #[test]
    fn test_delete() {
        assert_eq!(
            parse_command("delete"),
            CommandResult::LcAction(LcAction::DeleteSession)
        );
    }

    #[test]
    fn test_del_alias() {
        assert_eq!(
            parse_command("del"),
            CommandResult::LcAction(LcAction::DeleteSession)
        );
    }

    // --- :model / :mod ---

    #[test]
    fn test_model_with_arg() {
        assert_eq!(
            parse_command("model claude-opus-4-6"),
            CommandResult::LcAction(LcAction::SelectModel(Some("claude-opus-4-6".to_string())))
        );
    }

    #[test]
    fn test_mod_alias_with_arg() {
        assert_eq!(
            parse_command("mod claude-sonnet-5"),
            CommandResult::LcAction(LcAction::SelectModel(Some("claude-sonnet-5".to_string())))
        );
    }

    #[test]
    fn test_model_without_arg_opens_selector() {
        assert_eq!(
            parse_command("model"),
            CommandResult::LcAction(LcAction::ToggleModelDropdown)
        );
    }

    #[test]
    fn test_stopall_command() {
        assert_eq!(
            parse_command("stopall"),
            CommandResult::LcAction(LcAction::EmergencyStopAll)
        );
        assert_eq!(
            parse_command("stop-all"),
            CommandResult::LcAction(LcAction::EmergencyStopAll)
        );
    }

    // --- :sessions / :ls ---

    #[test]
    fn test_sessions() {
        assert_eq!(
            parse_command("sessions"),
            CommandResult::LcAction(LcAction::ListSessions)
        );
    }

    #[test]
    fn test_ls_alias() {
        assert_eq!(
            parse_command("ls"),
            CommandResult::LcAction(LcAction::ListSessions)
        );
    }

    // --- :alerts / :att ---

    #[test]
    fn test_alerts() {
        assert_eq!(
            parse_command("alerts"),
            CommandResult::LcAction(LcAction::ToggleNotifications)
        );
    }

    #[test]
    fn test_att_alias() {
        assert_eq!(
            parse_command("att"),
            CommandResult::LcAction(LcAction::ToggleNotifications)
        );
    }

    #[test]
    fn test_attention_alias() {
        assert_eq!(
            parse_command("attention"),
            CommandResult::LcAction(LcAction::ToggleNotifications)
        );
    }

    // --- :dag ---

    #[test]
    fn test_dag_opens_recursive_dag_browser() {
        assert_eq!(
            parse_command("dag"),
            CommandResult::LcAction(LcAction::OpenRecursiveDagBrowser)
        );
    }

    // --- :quit / :q ---

    #[test]
    fn test_quit() {
        assert_eq!(
            parse_command("quit"),
            CommandResult::LcAction(LcAction::Quit)
        );
    }

    #[test]
    fn test_q_alias() {
        assert_eq!(parse_command("q"), CommandResult::LcAction(LcAction::Quit));
    }

    #[test]
    fn test_qall() {
        assert_eq!(
            parse_command("qall"),
            CommandResult::LcAction(LcAction::Quit)
        );
    }

    #[test]
    fn test_force_quit() {
        assert_eq!(parse_command("q!"), CommandResult::LcAction(LcAction::Quit));
    }

    // --- Fallthrough ---

    #[test]
    fn test_split_falls_through() {
        assert_eq!(
            parse_command("split"),
            CommandResult::Unhandled("split".to_string())
        );
    }

    #[test]
    fn test_vsplit_falls_through() {
        assert_eq!(
            parse_command("vsplit"),
            CommandResult::Unhandled("vsplit".to_string())
        );
    }

    #[test]
    fn test_tabnew_falls_through() {
        assert_eq!(
            parse_command("tabnew"),
            CommandResult::Unhandled("tabnew".to_string())
        );
    }

    #[test]
    fn test_unknown_command_falls_through() {
        assert_eq!(
            parse_command("foobar"),
            CommandResult::Unhandled("foobar".to_string())
        );
    }

    // --- Edge cases ---

    // --- :rotate / :rot ---

    #[test]
    fn test_rotate() {
        assert_eq!(
            parse_command("rotate"),
            CommandResult::LcAction(LcAction::RotateSession)
        );
    }

    #[test]
    fn test_rot_alias() {
        assert_eq!(
            parse_command("rot"),
            CommandResult::LcAction(LcAction::RotateSession)
        );
    }

    // --- :group / :groups ---

    #[test]
    fn test_group() {
        assert_eq!(
            parse_command("group"),
            CommandResult::LcAction(LcAction::OpenLabelPicker)
        );
    }

    #[test]
    fn test_groups_alias() {
        assert_eq!(
            parse_command("groups"),
            CommandResult::LcAction(LcAction::OpenLabelPicker)
        );
    }

    // --- Edge cases ---

    #[test]
    fn test_empty_input() {
        assert_eq!(parse_command(""), CommandResult::Unhandled(String::new()));
    }

    #[test]
    fn test_whitespace_trimming() {
        assert_eq!(
            parse_command("  task  fix bug  "),
            CommandResult::LcAction(LcAction::LaunchTaskRabbit("fix bug".to_string()))
        );
    }

    // --- :card ---

    #[test]
    fn test_card_opens_project_editor() {
        assert_eq!(
            parse_command("card"),
            CommandResult::LcAction(LcAction::OpenProjectCard)
        );
    }

    #[test]
    fn test_card_user_opens_user_editor() {
        assert_eq!(
            parse_command("card user"),
            CommandResult::LcAction(LcAction::OpenUserCard)
        );
    }

    #[test]
    fn test_card_add_fact() {
        assert_eq!(
            parse_command("card add Uses Rust with ratatui"),
            CommandResult::LcAction(LcAction::AddProjectCardFact(
                "Uses Rust with ratatui".to_string()
            ))
        );
    }

    #[test]
    fn test_card_add_quoted_fact() {
        assert_eq!(
            parse_command(r#"card add "Vim-first TUI""#),
            CommandResult::LcAction(LcAction::AddProjectCardFact("Vim-first TUI".to_string()))
        );
    }

    #[test]
    fn test_card_user_add_fact() {
        assert_eq!(
            parse_command("card user add Prefers dark theme"),
            CommandResult::LcAction(LcAction::AddUserCardFact("Prefers dark theme".to_string()))
        );
    }

    #[test]
    fn test_card_user_with_trailing_args_opens_user() {
        // :card user xyz → opens user editor (unknown subcommand)
        assert_eq!(
            parse_command("card user xyz"),
            CommandResult::LcAction(LcAction::OpenUserCard)
        );
    }

    // --- :ask ---

    #[test]
    fn test_ask_with_query() {
        assert_eq!(
            parse_command("ask what did I work on today"),
            CommandResult::LcAction(LcAction::AskQuery("what did I work on today".to_string()))
        );
    }

    #[test]
    fn test_ask_without_query() {
        assert_eq!(
            parse_command("ask"),
            CommandResult::LcAction(LcAction::OpenDialectic)
        );
    }

    #[test]
    fn test_ask_with_whitespace_only_query() {
        assert_eq!(
            parse_command("ask   "),
            CommandResult::LcAction(LcAction::OpenDialectic)
        );
    }

    #[test]
    fn test_ask_with_multiword_query() {
        assert_eq!(
            parse_command("ask summarize my recent sessions"),
            CommandResult::LcAction(LcAction::AskQuery(
                "summarize my recent sessions".to_string()
            ))
        );
    }

    // --- :rate / :r ---

    #[test]
    fn test_rate_with_valid_number() {
        assert_eq!(
            parse_command("rate 7"),
            CommandResult::LcAction(LcAction::RateSession(7))
        );
    }

    #[test]
    fn test_rate_alias_with_valid_number() {
        assert_eq!(
            parse_command("r 10"),
            CommandResult::LcAction(LcAction::RateSession(10))
        );
    }

    #[test]
    fn test_rate_zero_is_unhandled() {
        assert!(matches!(
            parse_command("rate 0"),
            CommandResult::Unhandled(_)
        ));
    }

    #[test]
    fn test_rate_eleven_is_unhandled() {
        assert!(matches!(
            parse_command("rate 11"),
            CommandResult::Unhandled(_)
        ));
    }

    #[test]
    fn test_rate_non_numeric_is_unhandled() {
        assert!(matches!(
            parse_command("rate abc"),
            CommandResult::Unhandled(_)
        ));
    }

    #[test]
    fn test_rate_without_arg_opens_overlay() {
        assert_eq!(
            parse_command("rate"),
            CommandResult::LcAction(LcAction::OpenRatingOverlay)
        );
    }

    #[test]
    fn test_r_alias_without_arg_opens_overlay() {
        assert_eq!(
            parse_command("r"),
            CommandResult::LcAction(LcAction::OpenRatingOverlay)
        );
    }
}

pub mod action_handler;
pub mod action_registry;
pub mod app;
pub mod browser;
pub mod claude_config;
pub mod client;
pub mod clipboard;
pub mod cold_storage;
pub mod commands;
#[cfg(test)]
mod ctrl_c_tests;
pub mod daemon_resources;
pub mod draw_stats;
pub mod editing_mode_prompt;
pub mod efficiency_stats;
pub mod event;
pub mod external_reports; // #3789: `:external-reports` operator view.
pub mod field_edit;
pub mod file_utils;
pub mod file_viewer;
pub mod file_viewer_commands;
pub mod form_text;
pub mod git_gutter;
pub mod input_bar;
pub mod input_surface;
pub mod key_tables;
pub mod keybindings;
pub mod land;
pub mod manual;
pub mod mcp_server_rows;
pub mod modalkit_types;
pub mod model_control_budgets;
pub mod model_control_stats;
pub mod notification_stream;
pub mod overlay;
pub mod poll_controller;
pub mod portable_cli;
pub mod profiling;
pub mod prompt_creator;
pub mod prompt_creator_keys;
pub mod prompt_processor;
mod provider_billing_view;
pub mod provider_credential_rows;
pub mod provider_profile_view;
pub mod report_issue; // #3789: `rsi report-issue`, upstream harness reports via gh.
pub mod settings;
pub mod settings_keys;
pub mod settings_registry;
pub mod settings_staging;
#[cfg(test)]
mod standard_inputs_3b_tests;
#[cfg(test)]
mod standard_inputs_tests;
pub mod state;
pub mod suggestions;
pub mod terminal;
#[cfg(test)]
pub(crate) mod test_support;
pub mod text_objects;
pub mod todo_panel;
pub mod types;
pub mod ui;
pub mod vim_textarea;
pub mod widget;

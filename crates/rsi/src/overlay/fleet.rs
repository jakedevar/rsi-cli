//! Full-screen fleet workspace: aggregate table, active agents and inspector.
use crate::{app::App, types::OverlayState};
use crossterm::event::{KeyCode, KeyEvent};
use rsi_common::fleet::{FleetAgent, FleetGroup, FleetOverview};
use std::time::{Duration, Instant};

#[derive(Debug, Default)]
pub struct FleetState {
    pub snapshot: Option<FleetOverview>,
    pub error: Option<String>,
    pub last_attempt: Option<Instant>,
    pub selected: usize,
    pub selected_group: usize,
    pub focus_groups: bool,
    pub group_sort: usize,
    pub filter: String,
    pub editing: bool,
    pub sorting: bool,
    pub group: usize,
    pub window: usize,
    pub sort: usize,
    pub reverse: bool,
    pub pending_y: bool,
}
impl FleetState {
    pub fn dimension(&self) -> &'static str {
        ["project", "provider", "model"][self.group]
    }
    pub fn sort_label(&self) -> &'static str {
        if self.focus_groups {
            ["group", "active", "tokens", "cost", "errors"][self.group_sort]
        } else {
            ["project", "provider", "model", "status", "context"][self.sort]
        }
    }
    pub fn agents(&self) -> Vec<&FleetAgent> {
        let mut rows: Vec<_> = self
            .snapshot
            .iter()
            .flat_map(|s| &s.agents)
            .filter(|a| {
                format!(
                    "{} {} {:?} {} {} {:?} {}",
                    a.project,
                    a.role,
                    a.session.provider,
                    a.session.model.as_deref().unwrap_or("Unknown"),
                    a.session.effort.as_deref().unwrap_or("default"),
                    a.session.status,
                    a.session.title.as_deref().unwrap_or(&a.session.query)
                )
                .to_lowercase()
                .contains(&self.filter.to_lowercase())
            })
            .collect();
        rows.sort_by(|a, b| {
            let order = match self.sort {
                0 => a.project.cmp(&b.project),
                1 => format!("{:?}", a.session.provider).cmp(&format!("{:?}", b.session.provider)),
                2 => a.session.model.cmp(&b.session.model),
                3 => format!("{:?}", a.session.status).cmp(&format!("{:?}", b.session.status)),
                _ => a
                    .session
                    .context_fill_pct
                    .partial_cmp(&b.session.context_fill_pct)
                    .unwrap_or(std::cmp::Ordering::Equal),
            }
            .then(a.session.id.cmp(&b.session.id));
            if self.reverse { order.reverse() } else { order }
        });
        rows
    }
    pub fn groups(&self) -> Vec<&FleetGroup> {
        let mut rows: Vec<_> = self
            .snapshot
            .iter()
            .flat_map(|s| &s.groups)
            .filter(|g| {
                g.dimension == self.dimension()
                    && g.label.to_lowercase().contains(&self.filter.to_lowercase())
            })
            .collect();
        rows.sort_by(|a, b| {
            let au = &a.windows[self.window];
            let bu = &b.windows[self.window];
            let order = match self.group_sort {
                0 => a.label.cmp(&b.label),
                1 => b.active.cmp(&a.active),
                2 => bu
                    .tokens_per_minute(300)
                    .total_cmp(&au.tokens_per_minute(300)),
                3 => bu.cost.total_cmp(&au.cost),
                _ => bu.error_pct().total_cmp(&au.error_pct()),
            }
            .then(a.key.cmp(&b.key));
            if self.reverse { order.reverse() } else { order }
        });
        rows
    }
    pub fn install(&mut self, snapshot: FleetOverview) {
        let id = self.agents().get(self.selected).map(|a| a.session.id);
        let group = self
            .groups()
            .get(self.selected_group)
            .map(|g| g.key.clone());
        self.snapshot = Some(snapshot);
        self.selected_group = group
            .and_then(|key| self.groups().iter().position(|g| g.key == key))
            .unwrap_or(self.selected_group)
            .min(self.groups().len().saturating_sub(1));
        self.error = None;
        self.selected = id
            .and_then(|id| self.agents().iter().position(|a| a.session.id == id))
            .unwrap_or(self.selected)
            .min(self.agents().len().saturating_sub(1));
    }
}
pub async fn open(app: &mut App) {
    app.overlay = OverlayState::Fleet(Box::default());
    refresh(app).await;
}
pub async fn refresh(app: &mut App) {
    let OverlayState::Fleet(state) = &mut app.overlay else {
        return;
    };
    state.last_attempt = Some(Instant::now());
    let result = app.client.get_fleet_overview().await;
    let OverlayState::Fleet(state) = &mut app.overlay else {
        return;
    };
    match result {
        Ok(snapshot) => state.install(snapshot),
        Err(e) => state.error = Some(format!("Refresh failed: {e}. Press r to retry.")),
    }
    app.mark_dirty();
}
pub async fn tick(app: &mut App) {
    if matches!(&app.overlay,OverlayState::Fleet(s) if s.last_attempt.is_none_or(|t|t.elapsed()>=Duration::from_secs(5)))
    {
        refresh(app).await;
    }
}
pub async fn handle_key(app: &mut App, key: KeyEvent) {
    let OverlayState::Fleet(s) = &mut app.overlay else {
        return;
    };
    if s.editing {
        match key.code {
            KeyCode::Esc | KeyCode::Enter => s.editing = false,
            KeyCode::Backspace => {
                s.filter.pop();
                s.selected = 0;
                s.selected_group = 0;
            }
            KeyCode::Char(c) => {
                s.filter.push(c);
                s.selected = 0;
                s.selected_group = 0;
            }
            _ => {}
        }
    } else if s.sorting {
        match key.code {
            KeyCode::Esc => s.sorting = false,
            KeyCode::Char(c @ '1'..='5') => {
                if s.focus_groups {
                    s.group_sort = (c as usize) - ('1' as usize);
                } else {
                    s.sort = (c as usize) - ('1' as usize);
                }
                s.selected = 0;
                s.selected_group = 0;
                s.sorting = false;
            }
            _ => {}
        }
    } else {
        let copy = s.pending_y && key.code == KeyCode::Char('y');
        s.pending_y = false;
        if copy {
            if let Some(id) = s.agents().get(s.selected).map(|a| a.session.id) {
                crate::clipboard::osc52_copy(&id.to_string());
                app.notify(format!("Session {id}: copy request sent"));
            }
            return;
        }
        match key.code {
            KeyCode::Char('y') if !s.focus_groups => s.pending_y = true,
            KeyCode::Esc | KeyCode::Char('q') => app.overlay = OverlayState::None,
            KeyCode::Char('/') => s.editing = true,
            KeyCode::Char('s') => s.sorting = true,
            KeyCode::Char('S') => {
                s.reverse = !s.reverse;
                s.selected = 0;
                s.selected_group = 0;
            }
            KeyCode::Tab => s.focus_groups = !s.focus_groups,
            KeyCode::Char('b') => {
                s.group = (s.group + 1) % 3;
                s.selected_group = 0;
            }
            KeyCode::Char('w') => s.window = (s.window + 1) % 3,
            KeyCode::Char('r') => refresh(app).await,
            KeyCode::Char(':') => crate::overlay::command_palette::open_command_palette(app),
            KeyCode::Enter if s.focus_groups => s.focus_groups = false,
            KeyCode::Enter => {
                if let Some(id) = s.agents().get(s.selected).map(|a| a.session.id) {
                    if !crate::overlay::global_manager_workspace::jump_to_session(app, id).await {
                        if let OverlayState::Fleet(s) = &mut app.overlay {
                            s.error = Some("Session unavailable; refresh to retry".into());
                        }
                    }
                }
            }
            _ => {
                if s.focus_groups {
                    let len = s.groups().len();
                    crate::overlay::list::handle_list_nav_key(&mut s.selected_group, len, &key);
                } else {
                    let len = s.agents().len();
                    crate::overlay::list::handle_list_nav_key(&mut s.selected, len, &key);
                }
            }
        }
    }
    app.mark_dirty();
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use rsi_common::{fleet::*, types::SessionStatus};
    use uuid::Uuid;
    pub(crate) fn snapshot() -> FleetOverview {
        let mut agents = vec![];
        for (i, project) in ["Atlas", "Beacon"].into_iter().enumerate() {
            let mut session = crate::app::app_test_helpers::baseline_session(
                Uuid::from_u128(i as u128 + 1),
                rsi_common::types::SessionKind::Standard,
            );
            session.id = Uuid::from_u128(i as u128 + 1);
            session.title = Some(format!("Research {project}"));
            session.model = Some("gpt-6.1-sol".into());
            session.status = SessionStatus::Running;
            session.context_fill_pct = Some(42.0);
            session.effort = Some("high".into());
            agents.push(FleetAgent {
                session,
                project: project.into(),
                role: "worker".into(),
                turn_started_at: None,
            });
        }
        let usage = FleetUsage {
            invocations: 12,
            input: 12000,
            output: 4000,
            cache_read: 3000,
            cache_write: 1000,
            cost: 0.25,
            errors: 1,
            unknown_usage: 0,
        };
        let groups = ["project", "provider", "model"]
            .into_iter()
            .map(|dimension| FleetGroup {
                dimension: dimension.into(),
                key: "atlas".into(),
                label: "Atlas".into(),
                active: 2,
                windows: [usage.clone(), usage.clone(), usage.clone()],
            })
            .collect();
        FleetOverview {
            as_of: Utc.with_ymd_and_hms(2026, 10, 5, 12, 0, 0).unwrap(),
            agents,
            groups,
            totals: [usage.clone(), usage.clone(), usage],
            agents_truncated: false,
            usage_truncated: false,
        }
    }
    #[test]
    fn fleet_refresh_preserves_uuid_filter_and_sort() {
        let mut state = FleetState::default();
        state.install(snapshot());
        state.selected = 1;
        let mut next = snapshot();
        next.agents.reverse();
        next.agents[0].project = "Aardvark".into();
        state.install(next);
        assert_eq!(
            state.agents()[state.selected].session.id,
            Uuid::from_u128(2)
        );
        state.filter = "atlas".into();
        assert_eq!(state.agents().len(), 1);
        assert_eq!(state.groups().len(), 1);
        state.filter.clear();
        state.reverse = true;
        assert_eq!(state.agents()[0].project, "Atlas");
        state.filter = "no results".into();
        assert!(state.agents().is_empty());
        assert!(state.groups().is_empty());
    }
    #[tokio::test]
    async fn fleet_keys_filter_sort_group_and_open_session() {
        let mut app = crate::app::app_test_helpers::with_session_list(0);
        let snapshot = snapshot();
        let id = snapshot.agents[0].session.id;
        app.upsert_session(snapshot.agents[0].session.clone());
        let mut state = FleetState::default();
        state.install(snapshot);
        app.overlay = OverlayState::Fleet(Box::new(state));
        for code in [
            KeyCode::Char('/'),
            KeyCode::Char('A'),
            KeyCode::Enter,
            KeyCode::Char('s'),
            KeyCode::Char('3'),
            KeyCode::Char('b'),
            KeyCode::Char('w'),
        ] {
            handle_key(
                &mut app,
                KeyEvent::new(code, crossterm::event::KeyModifiers::NONE),
            )
            .await;
        }
        let OverlayState::Fleet(s) = &app.overlay else {
            panic!("fleet")
        };
        assert_eq!(s.filter, "A");
        assert_eq!(s.sort, 2);
        assert_eq!(s.group, 1);
        assert_eq!(s.window, 1);
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Enter, crossterm::event::KeyModifiers::NONE),
        )
        .await;
        assert!(matches!(app.overlay, OverlayState::None));
        assert!(app.sessions.contains_key(&id));
        assert_eq!(
            crate::commands::parse_command("fleet"),
            crate::commands::CommandResult::LcAction(crate::modalkit_types::LcAction::OpenFleet)
        );
    }
}

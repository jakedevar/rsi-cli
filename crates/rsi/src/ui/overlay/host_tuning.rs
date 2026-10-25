use crate::overlay::host_tuning::{Edit, State};
use crate::ui::theme;
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    text::Line,
    widgets::{Clear, Paragraph, Wrap},
};
use rsi_common::host_tuning::{HostProfile, RESOURCE_KEYS, Reading};

fn reading<T>(value: &Reading<T>, show: impl FnOnce(&T) -> String) -> String {
    match value {
        Reading::Known { value } => show(value),
        Reading::Unknown { reason } => format!("unknown ({reason})"),
        Reading::Unsupported => "unsupported".into(),
    }
}
fn bytes(value: &Reading<u64>) -> String {
    reading(value, |n| format!("{:.1} GiB", *n as f64 / 1_073_741_824.0))
}

pub(super) fn render(frame: &mut Frame, area: Rect, state: &State) {
    let popup = super::fixed_centered_rect(
        area,
        area.width.saturating_sub(4).min(130),
        area.height.saturating_sub(2),
    );
    frame.render_widget(Clear, popup);
    let dirty = if state.draft != state.view.state.configuration {
        " [draft]"
    } else {
        ""
    };
    let block = theme::overlay_block().title(format!(" Host tuning{dirty} "));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let zones = Layout::vertical([
        Constraint::Length(9),
        Constraint::Length(4),
        Constraint::Min(2),
        Constraint::Length(5),
    ])
    .split(inner);
    let p = &state.view.probe;
    let profile = match state.draft.profile {
        HostProfile::Unconfigured => "unconfigured".into(),
        HostProfile::Dedicated => "dedicated (0% operator reserve)".into(),
        HostProfile::Shared { reserve_percent } => {
            format!("shared ({reserve_percent}% operator reserve)")
        }
    };
    let heading = vec![
        Line::from(format!(
            "Profile: {profile}   Session cap guidance: {}",
            state
                .draft
                .live_session_cap_guidance
                .map_or("unset".into(), |v| v.to_string())
        )),
        Line::from(format!(
            "Effective CPUs: {}   Cgroup quota (CPUs): {}",
            reading(&p.effective_cpus, |v| format!("{v:.2}")),
            reading(&p.cgroup_cpu_quota, |v| format!("{v:.2}"))
        )),
        Line::from(format!(
            "RAM total: {}   available: {}   CPU temperature: {}",
            bytes(&p.total_ram_bytes),
            bytes(&p.available_ram_bytes),
            reading(&p.cpu_temperature_c, |v| format!("{v:.1} C"))
        )),
        Line::from(format!(
            "Disk free — data: {}   sandboxes: {}",
            bytes(&p.data_dir_free_bytes),
            bytes(&p.sandbox_base_free_bytes)
        )),
        Line::from(format!(
            "Load (1/5/15 min): {}",
            reading(&p.load_average, |v| format!(
                "{:.2} / {:.2} / {:.2}",
                v[0], v[1], v[2]
            ))
        )),
        Line::from(format!(
            "Cgroup: {}",
            reading(&p.cgroup, |v| v.replace('\n', "; "))
        )),
        Line::from(format!(
            "Active: {}   Undo: {}   Pending recovery: {}",
            state.view.state.active.as_deref().unwrap_or("none"),
            state.view.state.undo.is_some(),
            state.view.state.pending
        )),
        Line::from(
            "Reserve and session cap are preferences; this slice does not derive values. RAM is host RAM.",
        ),
        Line::from(
            "Test threads: per-job/load-adaptive. Worker quotas: new scopes. Build slice: Linux/systemd only.",
        ),
    ];
    frame.render_widget(Paragraph::new(heading), zones[0]);
    let mut presets = vec![Line::from(if state.settings_focus {
        "Presets (Tab to select):"
    } else {
        "Presets [focused] — j/k select:"
    })];
    for (index, name) in state
        .draft
        .presets
        .keys()
        .enumerate()
        .skip(state.preset.saturating_sub(1))
        .take(3)
    {
        presets.push(Line::from(format!(
            "{} {name}",
            if index == state.preset { ">" } else { " " }
        )));
    }
    if state.draft.presets.is_empty() {
        presets.push(Line::from(
            "No saved presets. n captures current settings; w saves.",
        ));
    }
    frame.render_widget(Paragraph::new(presets), zones[1]);
    let name = state.name();
    let values = name.as_ref().and_then(|n| state.draft.presets.get(n));
    let mut rows = vec![Line::from(format!(
        "{}   preset value / actual    operator min..max    applies",
        if state.settings_focus {
            "Settings [focused]"
        } else {
            "Settings (Tab to edit)"
        }
    ))];
    let count = usize::from(zones[2].height.saturating_sub(1)).max(1);
    let first = state.key.saturating_sub(count.saturating_sub(1));
    for (index, (key, class)) in RESOURCE_KEYS.iter().enumerate().skip(first).take(count) {
        let value = values
            .and_then(|v| v.get(*key))
            .map_or("—".into(), |v| v.to_string());
        let current = state
            .view
            .actual
            .get(*key)
            .map_or("unknown".into(), |v| v.to_string());
        let bounds = state
            .draft
            .bounds
            .get(*key)
            .map_or("unset".into(), |b| format!("{}..{}", b.min, b.max));
        rows.push(Line::from(format!(
            "{} {key}: {value} / {current}    {bounds}    {class}",
            if index == state.key { ">" } else { " " }
        )));
    }
    frame.render_widget(Paragraph::new(rows), zones[2]);
    let mut footer = vec![
        Line::from(
            "Tab focus  j/k select  n new from current  Enter edit value  m/M min/max  b reserve  d dedicated  g session guidance",
        ),
        Line::from(
            "w save draft  a apply saved preset  u undo last switch  r refresh/discard draft  Esc close",
        ),
        Line::from(
            "New bounds start at the captured value; widen min/max before changing a value. No automatic retuning.",
        ),
    ];
    if let Some(edit) = state.editing {
        let label = match edit {
            Edit::Name => "Preset name",
            Edit::Reserve => "Operator reserve % (0..100)",
            Edit::Guidance => "Live-session cap guidance (blank = unset)",
            Edit::Value => "Preset value",
            Edit::Min => "Operator minimum",
            Edit::Max => "Operator maximum",
        };
        footer.push(Line::from(format!(
            "{label}: {}  [Enter accept, Esc cancel]",
            state.input
        )));
    } else {
        footer.push(Line::from(state.message.clone()));
    }
    frame.render_widget(Paragraph::new(footer).wrap(Wrap { trim: false }), zones[3]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};
    #[test]
    fn resource_panel_shows_preferences_unknown_probe_and_selected_setting() {
        let unsupported = serde_json::json!({"status":"unsupported"});
        let probe = serde_json::from_value(serde_json::json!({
            "effective_cpus":{"status":"known","value":2.5},"cgroup_cpu_quota":unsupported,"cgroup":unsupported,
            "total_ram_bytes":unsupported,"available_ram_bytes":unsupported,"data_dir_free_bytes":unsupported,
            "sandbox_base_free_bytes":unsupported,"load_average":unsupported,"cpu_temperature_c":{"status":"unknown","reason":"no sensor"}
        })).unwrap();
        let mut state = State::new(rsi_common::host_tuning::TuningView {
            state: Default::default(),
            probe,
            actual: Default::default(),
            apply_classes: Default::default(),
            notes: vec![],
        });
        state.draft.profile = HostProfile::Shared {
            reserve_percent: 15,
        };
        state.key = RESOURCE_KEYS.len() - 1;
        let mut terminal = Terminal::new(TestBackend::new(120, 36)).unwrap();
        terminal
            .draw(|frame| render(frame, frame.area(), &state))
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        for expected in [
            "shared (15% operator reserve)",
            "2.50",
            "unknown (no sensor)",
            "sandbox_build_cache_reclaim_ttl_secs",
            "w save draft",
        ] {
            assert!(text.contains(expected), "{expected}");
        }
    }
}

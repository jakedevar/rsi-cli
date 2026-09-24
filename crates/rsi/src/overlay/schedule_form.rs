use chrono::{NaiveDate, NaiveDateTime, NaiveTime, Utc};
use crossterm::event::{KeyCode, KeyEvent};

use rsi_common::rpc::CreateScheduledJobParams;
use rsi_common::types::{Recurrence, ScheduleSpec, ScheduledJob};

use crate::app::App;
use crate::types::OverlayState;

pub const RECURRENCE_TYPES: &[(&str, &str)] = &[
    ("Once", "once"),
    ("Seconds", "s"),
    ("Minutes", "m"),
    ("Hours", "h"),
    ("Days", "d"),
    ("Weeks", "w"),
    ("Months", "mo"),
    ("Years", "y"),
];

const FIELD_COUNT: usize = 6;

pub fn open_schedule_form_new(app: &mut App) {
    let today = Utc::now().format("%Y-%m-%d").to_string();
    app.overlay = OverlayState::ScheduleForm {
        focused_field: 0,
        name: String::new(),
        message: String::new(),
        recurrence_index: 0,
        interval: "1".into(),
        anchor_date: today,
        anchor_time: "00:00:00".into(),
        editing_id: None,
    };
}

pub fn open_schedule_form_edit(app: &mut App, job: &ScheduledJob) {
    let (recurrence_index, interval) = recurrence_to_form(&job.schedule.recurrence);
    let anchor_date = job.schedule.anchor.format("%Y-%m-%d").to_string();
    let anchor_time = job.schedule.anchor.format("%H:%M:%S").to_string();

    app.overlay = OverlayState::ScheduleForm {
        focused_field: 0,
        name: job.name.clone(),
        message: job.message.clone(),
        recurrence_index,
        interval: interval.to_string(),
        anchor_date,
        anchor_time,
        editing_id: Some(job.id),
    };
}

pub async fn handle_schedule_form_key(app: &mut App, key: KeyEvent) -> bool {
    let (_focused_field, editing_id) = if let OverlayState::ScheduleForm {
        focused_field,
        editing_id,
        ..
    } = &app.overlay
    {
        (*focused_field, *editing_id)
    } else {
        return false;
    };

    match key.code {
        KeyCode::Tab => {
            if let OverlayState::ScheduleForm {
                ref mut focused_field,
                ..
            } = app.overlay
            {
                *focused_field = (*focused_field + 1) % FIELD_COUNT;
            }
        }
        KeyCode::BackTab => {
            if let OverlayState::ScheduleForm {
                ref mut focused_field,
                ..
            } = app.overlay
            {
                *focused_field = (*focused_field + FIELD_COUNT - 1) % FIELD_COUNT;
            }
        }
        KeyCode::Enter => {
            submit_schedule_form(app, editing_id).await;
        }
        KeyCode::Esc => {
            // Return to browser
            crate::overlay::schedule_browser::open_schedule_browser(app).await;
        }
        KeyCode::Char(c) => {
            if let OverlayState::ScheduleForm {
                focused_field,
                ref mut name,
                ref mut message,
                ref mut recurrence_index,
                ref mut interval,
                ref mut anchor_date,
                ref mut anchor_time,
                ..
            } = app.overlay
            {
                match focused_field {
                    0 => name.push(c),
                    1 => message.push(c),
                    2 => {
                        // Cycle recurrence type forward on any char
                        *recurrence_index = (*recurrence_index + 1) % RECURRENCE_TYPES.len();
                    }
                    3 => {
                        if c.is_ascii_digit() {
                            interval.push(c);
                        }
                    }
                    4 => {
                        if c.is_ascii_digit() || c == '-' {
                            anchor_date.push(c);
                        }
                    }
                    5 => {
                        if c.is_ascii_digit() || c == ':' {
                            anchor_time.push(c);
                        }
                    }
                    _ => {}
                }
            }
        }
        KeyCode::Backspace => {
            if let OverlayState::ScheduleForm {
                focused_field,
                ref mut name,
                ref mut message,
                ref mut interval,
                ref mut anchor_date,
                ref mut anchor_time,
                ..
            } = app.overlay
            {
                match focused_field {
                    0 => {
                        name.pop();
                    }
                    1 => {
                        message.pop();
                    }
                    2 => {} // recurrence_index, no backspace
                    3 => {
                        interval.pop();
                    }
                    4 => {
                        anchor_date.pop();
                    }
                    5 => {
                        anchor_time.pop();
                    }
                    _ => {}
                }
            }
        }
        KeyCode::Left => {
            if let OverlayState::ScheduleForm {
                focused_field: 2,
                ref mut recurrence_index,
                ..
            } = app.overlay
            {
                *recurrence_index =
                    (*recurrence_index + RECURRENCE_TYPES.len() - 1) % RECURRENCE_TYPES.len();
            }
        }
        KeyCode::Right => {
            if let OverlayState::ScheduleForm {
                focused_field: 2,
                ref mut recurrence_index,
                ..
            } = app.overlay
            {
                *recurrence_index = (*recurrence_index + 1) % RECURRENCE_TYPES.len();
            }
        }
        _ => {}
    }
    true
}

async fn submit_schedule_form(app: &mut App, editing_id: Option<uuid::Uuid>) {
    let form_data: Option<(String, String, usize, String, String, String)> =
        if let OverlayState::ScheduleForm {
            name,
            message,
            recurrence_index,
            interval,
            anchor_date,
            anchor_time,
            ..
        } = &app.overlay
        {
            Some((
                name.clone(),
                message.clone(),
                *recurrence_index,
                interval.clone(),
                anchor_date.clone(),
                anchor_time.clone(),
            ))
        } else {
            None
        };
    let Some((name, message, recurrence_index, interval_str, anchor_date_str, anchor_time_str)) =
        form_data
    else {
        return;
    };

    let name = name.trim().to_string();
    if name.is_empty() {
        app.notify_error("Job name is required");
        return;
    }
    let message = message.trim().to_string();
    if message.is_empty() {
        app.notify_error("Job message/prompt is required");
        return;
    }

    // Parse recurrence
    let interval_val: u64 = interval_str.trim().parse().unwrap_or(1).max(1);
    let recurrence = form_to_recurrence(recurrence_index, interval_val);

    // Parse anchor date/time
    let date = match NaiveDate::parse_from_str(anchor_date_str.trim(), "%Y-%m-%d") {
        Ok(d) => d,
        Err(_) => {
            app.notify_error("Invalid date format (use YYYY-MM-DD)");
            return;
        }
    };
    let time_str = anchor_time_str.trim();
    let time = if time_str.is_empty() {
        NaiveTime::from_hms_opt(0, 0, 0).unwrap()
    } else {
        match NaiveTime::parse_from_str(time_str, "%H:%M:%S")
            .or_else(|_| NaiveTime::parse_from_str(time_str, "%H:%M"))
        {
            Ok(t) => t,
            Err(_) => {
                app.notify_error("Invalid time format (use HH:MM:SS or HH:MM)");
                return;
            }
        }
    };

    let anchor = NaiveDateTime::new(date, time).and_utc();
    let schedule = ScheduleSpec { recurrence, anchor };

    if let Some(id) = editing_id {
        // Update existing job
        let params = rsi_common::rpc::UpdateScheduledJobParams {
            id,
            name: Some(name),
            message: Some(message),
            schedule: Some(schedule),
            enabled: None,
            working_dir: None,
            provider: None,
            model: None,
            project_id: None,
        };
        match app.client.update_scheduled_job(params).await {
            Ok(()) => app.notify_success("Scheduled job updated"),
            Err(e) => {
                app.notify_error(format!("Update failed: {e}"));
                return;
            }
        }
    } else {
        // Create new job
        let params = CreateScheduledJobParams {
            name,
            message,
            schedule,
            working_dir: std::env::current_dir().ok(),
            provider: Some(app.selected_provider),
            model: app.selected_model.clone(),
            project_id: app.current_project_id,
        };
        match app.client.create_scheduled_job(params).await {
            Ok(_job) => app.notify_success("Scheduled job created"),
            Err(e) => {
                app.notify_error(format!("Create failed: {e}"));
                return;
            }
        }
    }

    // Return to browser
    crate::overlay::schedule_browser::open_schedule_browser(app).await;
}

fn form_to_recurrence(index: usize, interval: u64) -> Recurrence {
    match index {
        0 => Recurrence::Once,
        1 => Recurrence::EverySeconds(interval),
        2 => Recurrence::EveryMinutes(interval),
        3 => Recurrence::EveryHours(interval),
        4 => Recurrence::EveryDays(interval),
        5 => Recurrence::EveryWeeks(interval),
        6 => Recurrence::EveryMonths(interval as u32),
        7 => Recurrence::EveryYears(interval as u32),
        _ => Recurrence::Once,
    }
}

fn recurrence_to_form(r: &Recurrence) -> (usize, u64) {
    match r {
        Recurrence::Once => (0, 1),
        Recurrence::EverySeconds(n) => (1, *n),
        Recurrence::EveryMinutes(n) => (2, *n),
        Recurrence::EveryHours(n) => (3, *n),
        Recurrence::EveryDays(n) => (4, *n),
        Recurrence::EveryWeeks(n) => (5, *n),
        Recurrence::EveryMonths(n) => (6, *n as u64),
        Recurrence::EveryYears(n) => (7, *n as u64),
        _ => (0, 1),
    }
}

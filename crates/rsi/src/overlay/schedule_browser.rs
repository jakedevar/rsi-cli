use crossterm::event::{KeyCode, KeyEvent};

use crate::app::App;
use crate::types::OverlayState;

pub async fn open_schedule_browser(app: &mut App) {
    app.overlay = OverlayState::ScheduleBrowser {
        jobs: Vec::new(),
        selected_index: 0,
        loading: true,
        pending_delete: false,
    };

    // Fetch jobs from daemon
    match app.client.list_scheduled_jobs().await {
        Ok(jobs) => {
            app.overlay = OverlayState::ScheduleBrowser {
                jobs,
                selected_index: 0,
                loading: false,
                pending_delete: false,
            };
        }
        Err(e) => {
            app.notify_error(format!("Failed to load scheduled jobs: {e}"));
            app.overlay = OverlayState::ScheduleBrowser {
                jobs: Vec::new(),
                selected_index: 0,
                loading: false,
                pending_delete: false,
            };
        }
    }
}

pub async fn handle_schedule_browser_key(app: &mut App, key: KeyEvent) -> bool {
    // Extract what we need from the overlay state without holding a borrow.
    let state = if let OverlayState::ScheduleBrowser {
        ref jobs,
        selected_index,
        ..
    } = app.overlay
    {
        Some((jobs.len(), selected_index))
    } else {
        None
    };

    let Some((job_count, selected_index)) = state else {
        return false;
    };

    let pending_delete = matches!(
        &app.overlay,
        OverlayState::ScheduleBrowser {
            pending_delete: true,
            ..
        }
    );
    if pending_delete {
        if let OverlayState::ScheduleBrowser { pending_delete, .. } = &mut app.overlay {
            *pending_delete = false;
        }
        if key.code == KeyCode::Char('d') && key.modifiers.is_empty() {
            delete_selected_job(app).await;
            return true;
        }
    }

    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.overlay = OverlayState::None;
        }
        KeyCode::Char('j') | KeyCode::Down => {
            if let OverlayState::ScheduleBrowser {
                ref mut selected_index,
                ref jobs,
                ..
            } = app.overlay
            {
                if !jobs.is_empty() {
                    *selected_index = (*selected_index + 1).min(jobs.len() - 1);
                }
            }
        }
        KeyCode::Char('k') | KeyCode::Up => {
            if let OverlayState::ScheduleBrowser {
                ref mut selected_index,
                ..
            } = app.overlay
            {
                *selected_index = selected_index.saturating_sub(1);
            }
        }
        KeyCode::Char('d') if key.modifiers.is_empty() => {
            if let OverlayState::ScheduleBrowser { pending_delete, .. } = &mut app.overlay {
                *pending_delete = true;
            }
        }
        KeyCode::Char(' ') => {
            // Toggle: extract ID first, then call, then modify state
            let job_info = if let OverlayState::ScheduleBrowser {
                ref jobs,
                selected_index,
                ..
            } = app.overlay
            {
                jobs.get(selected_index).map(|j| (j.id, j.name.clone()))
            } else {
                None
            };
            if let Some((id, name)) = job_info {
                match app.client.toggle_scheduled_job(id).await {
                    Ok(new_state) => {
                        if let OverlayState::ScheduleBrowser {
                            ref mut jobs,
                            selected_index,
                            ..
                        } = app.overlay
                        {
                            if let Some(job) = jobs.get_mut(selected_index) {
                                job.enabled = new_state;
                            }
                        }
                        let label = if new_state { "enabled" } else { "disabled" };
                        app.notify_success(format!("Job '{name}' {label}"));
                    }
                    Err(e) => app.notify_error(format!("Toggle failed: {e}")),
                }
            }
        }
        KeyCode::Char('t') => {
            // Trigger: extract ID first
            let job_info = if let OverlayState::ScheduleBrowser {
                ref jobs,
                selected_index,
                ..
            } = app.overlay
            {
                jobs.get(selected_index).map(|j| (j.id, j.name.clone()))
            } else {
                None
            };
            if let Some((id, name)) = job_info {
                match app.client.trigger_scheduled_job(id).await {
                    Ok(()) => app.notify_success(format!("Triggered '{name}'")),
                    Err(e) => app.notify_error(format!("Trigger failed: {e}")),
                }
            }
        }
        KeyCode::Enter | KeyCode::Char('e') => {
            // Edit: clone the job, then open form
            let job = if let OverlayState::ScheduleBrowser {
                ref jobs,
                selected_index,
                ..
            } = app.overlay
            {
                jobs.get(selected_index).cloned()
            } else {
                None
            };
            if let Some(job) = job {
                crate::overlay::schedule_form::open_schedule_form_edit(app, &job);
            }
        }
        KeyCode::Char('n') => {
            crate::overlay::schedule_form::open_schedule_form_new(app);
        }
        KeyCode::Char('r') => {
            open_schedule_browser(app).await;
        }
        _ => {}
    }

    let _ = (job_count, selected_index); // suppress unused warnings
    true
}

async fn delete_selected_job(app: &mut App) {
    let job_id = if let OverlayState::ScheduleBrowser {
        ref jobs,
        selected_index,
        ..
    } = app.overlay
    {
        jobs.get(selected_index).map(|job| job.id)
    } else {
        None
    };

    let Some(id) = job_id else {
        return;
    };

    match app.client.delete_scheduled_job(id).await {
        Ok(()) => {
            if let OverlayState::ScheduleBrowser {
                ref mut jobs,
                ref mut selected_index,
                ..
            } = app.overlay
            {
                jobs.retain(|job| job.id != id);
                if *selected_index >= jobs.len() && *selected_index > 0 {
                    *selected_index -= 1;
                }
            }
            app.notify_success("Scheduled job deleted");
        }
        Err(error) => app.notify_error(format!("Delete failed: {error}")),
    }
}

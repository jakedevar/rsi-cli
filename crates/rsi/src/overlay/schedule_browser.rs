use crossterm::event::{KeyCode, KeyEvent};

use crate::app::App;
use crate::types::OverlayState;

/// Fresh open: always the default (active) filter.
pub async fn open_schedule_browser(app: &mut App) {
    load_schedule_browser(app, false).await;
}

/// Returns to the browser (e.g. after the schedule form) with the filter the
/// operator last had active.
pub async fn reopen_schedule_browser(app: &mut App) {
    let include_history = app.schedule_include_history;
    load_schedule_browser(app, include_history).await;
}

/// (Re)loads the first page with the given history filter. `H` and `r` reuse it
/// so a refresh keeps the operator's filter.
async fn load_schedule_browser(app: &mut App, include_history: bool) {
    app.schedule_include_history = include_history;
    let paging = crate::types::SchedulePaging {
        include_history,
        next_cursor: None,
    };
    app.overlay = OverlayState::ScheduleBrowser {
        jobs: Vec::new(),
        selected_index: 0,
        loading: true,
        pending_delete: false,
        paging: paging.clone(),
    };

    // Held wakes are a best-effort read-side annotation: an older daemon without
    // the method simply shows none.
    app.schedule_holds = app
        .client
        .list_scheduled_job_holds()
        .await
        .map(|holds| holds.into_iter().map(|hold| (hold.job_id, hold)).collect())
        .unwrap_or_default();

    // Fetch the first page from the daemon (default filter unless history).
    match app
        .client
        .list_scheduled_jobs_page(include_history, None)
        .await
    {
        Ok(page) => {
            app.overlay = OverlayState::ScheduleBrowser {
                jobs: page.jobs,
                selected_index: 0,
                loading: false,
                pending_delete: false,
                paging: crate::types::SchedulePaging {
                    include_history,
                    next_cursor: page.next_cursor,
                },
            };
        }
        Err(e) => {
            app.notify_error(format!("Failed to load scheduled jobs: {e}"));
            app.overlay = OverlayState::ScheduleBrowser {
                jobs: Vec::new(),
                selected_index: 0,
                loading: false,
                pending_delete: false,
                paging,
            };
        }
    }
}

/// Appends the next page. Rows already loaded (a job that moved across the
/// filter between pages) are skipped so the list never shows a duplicate.
async fn load_more_schedule_jobs(app: &mut App) {
    let request = match &app.overlay {
        OverlayState::ScheduleBrowser { paging, .. } => paging
            .next_cursor
            .clone()
            .map(|cursor| (paging.include_history, cursor)),
        _ => None,
    };
    let Some((include_history, cursor)) = request else {
        app.notify("All scheduled jobs are loaded");
        return;
    };
    let page = match app
        .client
        .list_scheduled_jobs_page(include_history, Some(cursor))
        .await
    {
        Ok(page) => page,
        Err(e) => {
            app.notify_error(format!("Failed to load more scheduled jobs: {e}"));
            return;
        }
    };
    let OverlayState::ScheduleBrowser { jobs, paging, .. } = &mut app.overlay else {
        return;
    };
    append_page(jobs, paging, page);
}

fn append_page(
    jobs: &mut Vec<rsi_common::types::ScheduledJob>,
    paging: &mut crate::types::SchedulePaging,
    page: rsi_common::rpc::ListScheduledJobsResult,
) {
    let loaded: std::collections::HashSet<uuid::Uuid> = jobs.iter().map(|job| job.id).collect();
    jobs.extend(
        page.jobs
            .into_iter()
            .filter(|job| !loaded.contains(&job.id)),
    );
    paging.next_cursor = page.next_cursor;
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
            let include_history = matches!(
                &app.overlay,
                OverlayState::ScheduleBrowser { paging, .. } if paging.include_history
            );
            load_schedule_browser(app, include_history).await;
        }
        KeyCode::Char('H') => {
            let include_history = matches!(
                &app.overlay,
                OverlayState::ScheduleBrowser { paging, .. } if paging.include_history
            );
            load_schedule_browser(app, !include_history).await;
        }
        KeyCode::Char('m') => {
            load_more_schedule_jobs(app).await;
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

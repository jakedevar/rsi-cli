//! Notify callbacks carry no source bytes or database effects. Every callback
//! collapses to a complete-workspace request; lost events are repaired by the
//! manager's periodic content scan.

use super::{IndexError, RegisteredWorkspace, Result, manager::IndexHandle};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use uuid::Uuid;

pub struct IndexWatcher {
    _watcher: RecommendedWatcher,
}

impl IndexWatcher {
    pub fn new(workspace: &RegisteredWorkspace, handle: IndexHandle) -> Result<Self> {
        let id = workspace.workspace_id;
        let root = workspace.root.clone();
        let error_handle = handle.clone();
        let mut watcher = notify::recommended_watcher(move |event: notify::Result<Event>| {
            route_event(&handle, id, &root, event);
        })
        .map_err(|error| {
            IndexError::UnsafeWorkspace(format!("watcher creation failed: {error}"))
        })?;
        if let Err(error) = watcher.watch(&workspace.root, RecursiveMode::Recursive) {
            let _ = error_handle.force_rescan(workspace.workspace_id);
            return Err(IndexError::UnsafeWorkspace(format!(
                "watch failed: {error}"
            )));
        }
        Ok(Self { _watcher: watcher })
    }
}

pub(super) fn route_event(
    handle: &IndexHandle,
    workspace_id: Uuid,
    root: &std::path::Path,
    event: notify::Result<Event>,
) {
    match event {
        Ok(event) if relevant(&event, root) => {
            let _ = handle.request(workspace_id);
        }
        Ok(_) => {}
        Err(_) => {
            let _ = handle.force_rescan(workspace_id);
        }
    }
}

fn relevant(event: &Event, root: &std::path::Path) -> bool {
    if !matches!(
        event.kind,
        EventKind::Create(_)
            | EventKind::Modify(_)
            | EventKind::Remove(_)
            | EventKind::Any
            | EventKind::Other
    ) {
        return false;
    }
    if event.paths.is_empty() {
        return true;
    }
    event.paths.iter().any(|path| {
        let Ok(relative) = path.strip_prefix(root) else { return true; };
        if relative.components().any(|component| matches!(component, std::path::Component::Normal(name) if [".git", ".rsi", "target", "node_modules", "vendor", ".venv"].iter().any(|ignored| name == *ignored))) {
            return false;
        }
        // Paths can be deleted by the time a callback runs; extension and
        // directory hints are enough to request a full scan.
        relative.extension().and_then(|ext| ext.to_str()).is_none_or(|ext| matches!(ext.to_ascii_lowercase().as_str(), "rs" | "md" | "markdown" | "toml" | "lock"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relevant_changes_are_hints_for_full_scan() {
        let root = std::path::Path::new("/work");
        let event = Event::new(EventKind::Any).add_path(root.join("src/lib.rs"));
        assert!(relevant(&event, root));
        let ignored = Event::new(EventKind::Any).add_path(root.join("target/generated.rs"));
        assert!(!relevant(&ignored, root));
    }
}

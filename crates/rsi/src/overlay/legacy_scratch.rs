//! Operator legacy-scratch adoption overlay (#1147).
//!
//! #1140's reclaim deletes only scratch directories that carry a creation
//! record. Worker TMPDIRs, `/var/tmp/rsi-*` and lander workspaces made before it
//! have none and are retained forever. This overlay lists them with the
//! daemon's verdict for each and lets the operator adopt chosen ones. Adopting
//! only RECORDS a directory (after the daemon re-runs the full reclaim proof);
//! it deletes nothing. The daemon's reclaim pass deletes recorded scratch later.

use crate::app::App;
use crate::types::{LegacyScratchOverlayState, OverlayState};
use crossterm::event::{KeyCode, KeyEvent};
use rsi_common::scratch_adopt::{
    AdoptLegacyScratchResponseV1, LegacyScratchCandidateV1, ListLegacyScratchResponseV1,
    MAX_ADOPT_PATHS, ScratchAdoptRefusal,
};

/// What `a`/`A` ask to adopt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AdoptScope {
    Selected,
    AllAdoptable,
}

/// Operator wording for a refusal; also used by the renderer.
pub(crate) const fn refusal_label(refusal: ScratchAdoptRefusal) -> &'static str {
    match refusal {
        ScratchAdoptRefusal::OutsideRoots => "outside the scratch roots",
        ScratchAdoptRefusal::Missing => "no longer exists",
        ScratchAdoptRefusal::Symlink => "is a symlink",
        ScratchAdoptRefusal::NotDirectory => "is not a directory",
        ScratchAdoptRefusal::Held => "held by a live process",
        ScratchAdoptRefusal::Young => "written too recently",
        ScratchAdoptRefusal::DirtyWorktree => "has a dirty git worktree",
        ScratchAdoptRefusal::Unpublished => "has unpublished git work",
        ScratchAdoptRefusal::Unproven => "not provably safe",
        ScratchAdoptRefusal::AlreadyRecorded => "already recorded",
        ScratchAdoptRefusal::Changed => "changed during the proof",
        ScratchAdoptRefusal::Failed => "recording failed",
    }
}

impl LegacyScratchOverlayState {
    pub(crate) fn from_listing(listing: ListLegacyScratchResponseV1) -> Self {
        Self {
            candidates: listing.candidates,
            refused_roots: listing.refused_roots,
            budget_exhausted: listing.budget_exhausted,
            ..Self::default()
        }
    }

    /// Replace the listing after an adoption, keeping the cursor in range and
    /// the result lines.
    fn reload(&mut self, listing: ListLegacyScratchResponseV1) {
        self.candidates = listing.candidates;
        self.refused_roots = listing.refused_roots;
        self.budget_exhausted = listing.budget_exhausted;
        self.selected_index = self
            .selected_index
            .min(self.candidates.len().saturating_sub(1));
    }

    pub(crate) fn move_selection(&mut self, delta: isize) {
        let max = self.candidates.len().saturating_sub(1);
        self.selected_index = self.selected_index.saturating_add_signed(delta).min(max);
    }

    pub(crate) fn selected(&self) -> Option<&LegacyScratchCandidateV1> {
        self.candidates.get(self.selected_index)
    }

    /// The paths to send for `scope`. Only candidates the daemon reported as
    /// adoptable are sent; the daemon decides again when it runs the proof.
    pub(crate) fn paths_for(&self, scope: AdoptScope) -> Vec<String> {
        match scope {
            AdoptScope::Selected => self
                .selected()
                .filter(|c| c.blocker.is_none())
                .map(|c| c.path.clone())
                .into_iter()
                .collect(),
            AdoptScope::AllAdoptable => self
                .candidates
                .iter()
                .filter(|c| c.blocker.is_none())
                .take(MAX_ADOPT_PATHS)
                .map(|c| c.path.clone())
                .collect(),
        }
    }

    /// Result lines for the daemon's per-path outcomes.
    pub(crate) fn describe(response: &AdoptLegacyScratchResponseV1) -> Vec<String> {
        let adopted = response.results.iter().filter(|r| r.adopted).count();
        let mut lines = vec![format!(
            "Adopted {adopted} of {} (recorded only; nothing deleted)",
            response.results.len()
        )];
        for result in &response.results {
            lines.push(match result.refusal {
                None => format!("adopted  {}", result.path),
                Some(refusal) => match &result.detail {
                    Some(detail) => format!(
                        "refused  {}: {} ({detail})",
                        result.path,
                        refusal_label(refusal)
                    ),
                    None => format!("refused  {}: {}", result.path, refusal_label(refusal)),
                },
            });
        }
        lines
    }

    /// Open the confirmation for `scope`, or say why there is nothing to
    /// confirm. Sends nothing.
    pub(crate) fn begin_confirm(&mut self, scope: AdoptScope) {
        let paths = self.paths_for(scope);
        self.last_error = None;
        self.confirm_scroll = 0;
        if paths.is_empty() {
            self.last_error = Some(self.nothing_to_adopt(scope));
            return;
        }
        self.confirm = Some(paths);
    }

    /// Why nothing was sent for `scope`.
    fn nothing_to_adopt(&self, scope: AdoptScope) -> String {
        match (scope, self.selected()) {
            (AdoptScope::Selected, Some(candidate)) => match candidate.blocker {
                Some(blocker) => format!(
                    "{} cannot be adopted: {}",
                    candidate.path,
                    refusal_label(blocker)
                ),
                None => "nothing selected".to_string(),
            },
            (AdoptScope::Selected, None) => "nothing selected".to_string(),
            (AdoptScope::AllAdoptable, _) => "no candidate is adoptable now".to_string(),
        }
    }
}

pub async fn open(app: &mut App) {
    match app.client.list_legacy_scratch().await {
        Ok(listing) => {
            app.overlay = OverlayState::LegacyScratch(Box::new(
                LegacyScratchOverlayState::from_listing(listing),
            ));
        }
        Err(error) => app.notify_error(format!("Legacy scratch listing failed: {error}")),
    }
}

/// What a key press asks the async layer to do. Everything that decides is
/// here, free of the client, so the confirmation step is unit-testable.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Step {
    Nothing,
    Close,
    Refresh,
    /// The operator confirmed these exact paths.
    Adopt(Vec<String>),
}

/// Map a key onto the overlay state. Adoption is two-step: `a`/`Enter`/`A`
/// only open the confirmation (naming every full path); only `y` inside it
/// returns [`Step::Adopt`].
pub(crate) fn step(state: &mut LegacyScratchOverlayState, code: KeyCode) -> Step {
    if state.confirm.is_some() {
        return confirm_step(state, code);
    }
    match code {
        KeyCode::Esc | KeyCode::Char('q') => Step::Close,
        KeyCode::Char('j') | KeyCode::Down => {
            state.move_selection(1);
            Step::Nothing
        }
        KeyCode::Char('k') | KeyCode::Up => {
            state.move_selection(-1);
            Step::Nothing
        }
        KeyCode::Char('r') => Step::Refresh,
        KeyCode::Enter | KeyCode::Char('a') => {
            state.begin_confirm(AdoptScope::Selected);
            Step::Nothing
        }
        KeyCode::Char('A') => {
            state.begin_confirm(AdoptScope::AllAdoptable);
            Step::Nothing
        }
        _ => Step::Nothing,
    }
}

fn confirm_step(state: &mut LegacyScratchOverlayState, code: KeyCode) -> Step {
    match code {
        KeyCode::Char('y') => {
            state.confirm_scroll = 0;
            state.confirm.take().map_or(Step::Nothing, Step::Adopt)
        }
        KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('q') => {
            state.confirm = None;
            state.confirm_scroll = 0;
            Step::Nothing
        }
        KeyCode::Char('j') | KeyCode::Down => {
            let last = state
                .confirm
                .as_ref()
                .map_or(0, |p| p.len().saturating_sub(1));
            state.confirm_scroll = (state.confirm_scroll + 1).min(last);
            Step::Nothing
        }
        KeyCode::Char('k') | KeyCode::Up => {
            state.confirm_scroll = state.confirm_scroll.saturating_sub(1);
            Step::Nothing
        }
        _ => Step::Nothing,
    }
}

pub(super) async fn handle_key(app: &mut App, key: KeyEvent) {
    let OverlayState::LegacyScratch(state) = &mut app.overlay else {
        return;
    };
    match step(state, key.code) {
        Step::Nothing => {}
        Step::Close => app.overlay = OverlayState::None,
        Step::Refresh => refresh(app).await,
        Step::Adopt(paths) => adopt(app, paths).await,
    }
}

async fn refresh(app: &mut App) {
    let listing = app.client.list_legacy_scratch().await;
    let OverlayState::LegacyScratch(state) = &mut app.overlay else {
        return;
    };
    match listing {
        Ok(listing) => {
            state.reload(listing);
            state.last_error = None;
        }
        Err(error) => state.last_error = Some(format!("Listing failed: {error}")),
    }
}

async fn adopt(app: &mut App, paths: Vec<String>) {
    let outcome = app.client.adopt_legacy_scratch(paths).await;
    let listing = match &outcome {
        Ok(_) => Some(app.client.list_legacy_scratch().await),
        Err(_) => None,
    };
    let OverlayState::LegacyScratch(state) = &mut app.overlay else {
        return;
    };
    state.last_error = None;
    match outcome {
        Ok(response) => state.last_result = LegacyScratchOverlayState::describe(&response),
        Err(error) => state.last_error = Some(format!("Adoption failed: {error}")),
    }
    if let Some(Ok(listing)) = listing {
        state.reload(listing);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsi_common::scratch_adopt::AdoptLegacyScratchResultV1;

    fn candidate(path: &str, blocker: Option<ScratchAdoptRefusal>) -> LegacyScratchCandidateV1 {
        LegacyScratchCandidateV1 {
            path: path.to_string(),
            kind: "worker_tmp".to_string(),
            blocker,
            bytes: 4096,
            detail: None,
        }
    }

    fn state() -> LegacyScratchOverlayState {
        LegacyScratchOverlayState::from_listing(ListLegacyScratchResponseV1 {
            candidates: vec![
                candidate("/h/.cache/rsi-a-tmp", None),
                candidate("/h/.cache/rsi-b-tmp", Some(ScratchAdoptRefusal::Held)),
                candidate("/h/.cache/rsi-c-tmp", None),
            ],
            ..ListLegacyScratchResponseV1::default()
        })
    }

    #[test]
    fn only_adoptable_candidates_are_sent() {
        let mut s = state();
        assert_eq!(
            s.paths_for(AdoptScope::Selected),
            vec!["/h/.cache/rsi-a-tmp"]
        );
        s.move_selection(1);
        assert!(s.paths_for(AdoptScope::Selected).is_empty());
        assert_eq!(
            s.nothing_to_adopt(AdoptScope::Selected),
            "/h/.cache/rsi-b-tmp cannot be adopted: held by a live process"
        );
        assert_eq!(
            s.paths_for(AdoptScope::AllAdoptable),
            vec!["/h/.cache/rsi-a-tmp", "/h/.cache/rsi-c-tmp"]
        );
    }

    #[test]
    fn selection_stays_inside_the_list() {
        let mut s = state();
        s.move_selection(-1);
        assert_eq!(s.selected_index, 0);
        s.move_selection(10);
        assert_eq!(s.selected_index, 2);
        s.reload(ListLegacyScratchResponseV1 {
            candidates: vec![candidate("/x/rsi-z-tmp", None)],
            ..ListLegacyScratchResponseV1::default()
        });
        assert_eq!(s.selected_index, 0);
    }

    #[test]
    fn the_adopt_result_names_each_path_and_its_typed_refusal() {
        let lines = LegacyScratchOverlayState::describe(&AdoptLegacyScratchResponseV1 {
            results: vec![
                AdoptLegacyScratchResultV1 {
                    path: "/a".to_string(),
                    adopted: true,
                    refusal: None,
                    detail: None,
                },
                AdoptLegacyScratchResultV1 {
                    path: "/b".to_string(),
                    adopted: false,
                    refusal: Some(ScratchAdoptRefusal::Symlink),
                    detail: None,
                },
                AdoptLegacyScratchResultV1 {
                    path: "/c".to_string(),
                    adopted: false,
                    refusal: Some(ScratchAdoptRefusal::Unproven),
                    detail: Some(
                        "holder proof incomplete (process-unreadable); pid 7 comm sshd uid 1000"
                            .to_string(),
                    ),
                },
            ],
        });
        assert_eq!(
            lines,
            vec![
                "Adopted 1 of 3 (recorded only; nothing deleted)".to_string(),
                "adopted  /a".to_string(),
                "refused  /b: is a symlink".to_string(),
                "refused  /c: not provably safe (holder proof incomplete (process-unreadable); pid 7 comm sshd uid 1000)".to_string(),
            ]
        );
    }

    #[test]
    fn opening_selecting_and_the_first_action_send_nothing() {
        let mut s = state();
        assert_eq!(step(&mut s, KeyCode::Char('j')), Step::Nothing);
        assert_eq!(step(&mut s, KeyCode::Char('k')), Step::Nothing);
        // Enter, a and A only open the confirmation.
        assert_eq!(step(&mut s, KeyCode::Enter), Step::Nothing);
        assert_eq!(s.confirm, Some(vec!["/h/.cache/rsi-a-tmp".to_string()]));
        s.confirm = None;
        assert_eq!(step(&mut s, KeyCode::Char('a')), Step::Nothing);
        assert!(s.confirm.is_some());
        s.confirm = None;
        assert_eq!(step(&mut s, KeyCode::Char('A')), Step::Nothing);
        assert!(s.confirm.is_some());
    }

    #[test]
    fn only_an_explicit_y_in_the_confirmation_adopts_exactly_the_listed_paths() {
        let mut s = state();
        step(&mut s, KeyCode::Char('A'));
        // Neither Enter, a nor A confirms.
        for code in [KeyCode::Enter, KeyCode::Char('a'), KeyCode::Char('A')] {
            assert_eq!(step(&mut s, code), Step::Nothing);
            assert!(s.confirm.is_some());
        }
        assert_eq!(
            step(&mut s, KeyCode::Char('y')),
            Step::Adopt(vec![
                "/h/.cache/rsi-a-tmp".to_string(),
                "/h/.cache/rsi-c-tmp".to_string()
            ])
        );
        assert_eq!(s.confirm, None);
    }

    #[test]
    fn cancelling_the_confirmation_sends_nothing_and_returns_to_the_list() {
        for cancel in [KeyCode::Esc, KeyCode::Char('n'), KeyCode::Char('q')] {
            let mut s = state();
            step(&mut s, KeyCode::Enter);
            assert_eq!(step(&mut s, cancel), Step::Nothing);
            assert_eq!(s.confirm, None);
            // Back in the list: Esc now closes.
            assert_eq!(step(&mut s, KeyCode::Esc), Step::Close);
        }
    }

    #[test]
    fn a_blocked_directory_never_reaches_the_confirmation() {
        let mut s = state();
        s.move_selection(1);
        assert_eq!(step(&mut s, KeyCode::Enter), Step::Nothing);
        assert_eq!(s.confirm, None);
        assert_eq!(
            s.last_error.as_deref(),
            Some("/h/.cache/rsi-b-tmp cannot be adopted: held by a live process")
        );
    }

    #[test]
    fn the_confirmation_scrolls_through_every_path() {
        let mut s = LegacyScratchOverlayState::from_listing(ListLegacyScratchResponseV1 {
            candidates: (0..5)
                .map(|n| candidate(&format!("/v/rsi-{n}"), None))
                .collect(),
            ..ListLegacyScratchResponseV1::default()
        });
        step(&mut s, KeyCode::Char('A'));
        assert_eq!(s.confirm.as_ref().map(Vec::len), Some(5));
        for _ in 0..10 {
            step(&mut s, KeyCode::Char('j'));
        }
        assert_eq!(s.confirm_scroll, 4);
        step(&mut s, KeyCode::Char('k'));
        assert_eq!(s.confirm_scroll, 3);
    }
}

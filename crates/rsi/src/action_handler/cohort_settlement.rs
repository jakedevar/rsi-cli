use crate::app::App;

pub(super) async fn open(app: &mut App) {
    crate::overlay::open_source_worktree_settlement(app).await;
}

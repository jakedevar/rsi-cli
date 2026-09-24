use super::SessionManager;
use crate::error::{DaemonError, Result};
use rsi_common::types::EspGame;

impl SessionManager {
    pub async fn save_esp_game(&self, game: EspGame) -> Result<()> {
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            store
                .insert_esp_game(&game)
                .map_err(|e| DaemonError::Store(e.to_string()))
        })
        .await
        .map_err(|e| DaemonError::Store(e.to_string()))??;
        Ok(())
    }

    pub async fn list_esp_games(&self, limit: usize) -> Result<Vec<EspGame>> {
        let store = self.store.clone();
        let games = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            store
                .list_esp_games(limit)
                .map_err(|e| DaemonError::Store(e.to_string()))
        })
        .await
        .map_err(|e| DaemonError::Store(e.to_string()))??;
        Ok(games)
    }
}

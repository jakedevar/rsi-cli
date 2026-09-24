use crate::store::Store;
use crate::store::row_mappers::map_esp_game_row;
use anyhow::Result;
use rsi_common::types::EspGame;

impl Store {
    pub fn insert_esp_game(&self, game: &EspGame) -> Result<()> {
        self.conn.execute(
            "INSERT INTO esp_games (id, played_at, score, rounds_played, total_rounds, p_value, round_details)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                game.id.to_string(),
                game.played_at.to_rfc3339(),
                game.score as i64,
                game.rounds_played as i64,
                game.total_rounds as i64,
                game.p_value,
                game.round_details,
            ],
        )?;
        Ok(())
    }

    pub fn list_esp_games(&self, limit: usize) -> Result<Vec<EspGame>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, played_at, score, rounds_played, total_rounds, p_value, round_details
             FROM esp_games ORDER BY played_at DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(rusqlite::params![limit as i64], map_esp_game_row)?;
        let mut games = Vec::new();
        for row in rows {
            games.push(row?.into_esp_game()?);
        }
        Ok(games)
    }
}

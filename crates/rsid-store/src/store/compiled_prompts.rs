use rusqlite::params;
use uuid::Uuid;

use rsi_common::types::CompiledPrompt;

use super::Store;
use super::row_mappers::parse_timestamp;
use crate::error::{DaemonError, Result};

impl Store {
    pub fn insert_compiled_prompt(&self, prompt: &CompiledPrompt) -> Result<()> {
        self.conn.execute(
            "INSERT INTO compiled_prompts (
                id, session_id, original_input, compiled_output, contract_status,
                layer_semantic, layer_syntactic, layer_deictic, layer_discourse, layer_pragmatic,
                accepted, created_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                prompt.id.to_string(),
                prompt.session_id.map(|u| u.to_string()),
                prompt.original_input,
                prompt.compiled_output,
                prompt.contract_status,
                prompt.layer_semantic as i32,
                prompt.layer_syntactic as i32,
                prompt.layer_deictic as i32,
                prompt.layer_discourse as i32,
                prompt.layer_pragmatic as i32,
                prompt.accepted as i32,
                prompt
                    .created_at
                    .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
            ],
        )?;
        Ok(())
    }

    pub fn list_compiled_prompts(
        &self,
        session_id: Option<&Uuid>,
        limit: u32,
    ) -> Result<Vec<CompiledPrompt>> {
        let (sql, id_str);
        let params: Vec<Box<dyn rusqlite::types::ToSql>> = match session_id {
            Some(sid) => {
                id_str = sid.to_string();
                sql = "SELECT id, session_id, original_input, compiled_output, contract_status,
                       layer_semantic, layer_syntactic, layer_deictic, layer_discourse, layer_pragmatic,
                       accepted, created_at
                       FROM compiled_prompts WHERE session_id = ?1
                       ORDER BY created_at DESC LIMIT ?2".to_string();
                vec![Box::new(id_str.clone()), Box::new(limit as i64)]
            }
            None => {
                sql = "SELECT id, session_id, original_input, compiled_output, contract_status,
                       layer_semantic, layer_syntactic, layer_deictic, layer_discourse, layer_pragmatic,
                       accepted, created_at
                       FROM compiled_prompts
                       ORDER BY created_at DESC LIMIT ?1".to_string();
                vec![Box::new(limit as i64)]
            }
        };

        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            params.iter().map(|p| p.as_ref()).collect();
        let mut stmt = self.conn.prepare(&sql)?;
        let prompts = stmt
            .query_map(param_refs.as_slice(), |row| {
                Ok(map_compiled_prompt_row(row))
            })?
            .filter_map(|r| r.ok().and_then(|p| p.ok()))
            .collect();
        Ok(prompts)
    }
}

fn map_compiled_prompt_row(row: &rusqlite::Row) -> Result<CompiledPrompt> {
    let id_str: String = row.get(0)?;
    let session_id_str: Option<String> = row.get(1)?;
    let original_input: String = row.get(2)?;
    let compiled_output: String = row.get(3)?;
    let contract_status: String = row.get(4)?;
    let layer_semantic: i32 = row.get(5)?;
    let layer_syntactic: i32 = row.get(6)?;
    let layer_deictic: i32 = row.get(7)?;
    let layer_discourse: i32 = row.get(8)?;
    let layer_pragmatic: i32 = row.get(9)?;
    let accepted: i32 = row.get(10)?;
    let created_at_str: String = row.get(11)?;

    let session_id = session_id_str
        .map(|s| Uuid::parse_str(&s).map_err(|e| DaemonError::Store(e.to_string())))
        .transpose()?;

    Ok(CompiledPrompt {
        id: Uuid::parse_str(&id_str).map_err(|e| DaemonError::Store(e.to_string()))?,
        session_id,
        original_input,
        compiled_output,
        contract_status,
        layer_semantic: layer_semantic != 0,
        layer_syntactic: layer_syntactic != 0,
        layer_deictic: layer_deictic != 0,
        layer_discourse: layer_discourse != 0,
        layer_pragmatic: layer_pragmatic != 0,
        accepted: accepted != 0,
        created_at: parse_timestamp(&created_at_str).map_err(DaemonError::Store)?,
    })
}

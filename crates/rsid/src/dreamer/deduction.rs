//! Deduction specialist: infers new facts from explicit observations,
//! identifies superseded observations, and detects contradictions.

use super::llm_client::DreamerLlmClient;
use crate::error::Result;
use rsi_common::types::{Observation, ObservationLevel};
use uuid::Uuid;

pub(crate) const DEDUCTION_SYSTEM_PROMPT: &str = r#"You are a deduction specialist. Given a set of explicit observations, your job is to:

1. INFER: Create new deductive observations -- logical necessities that follow from the explicit facts. Each must include the IDs of the source observations it was derived from.
2. SUPERSEDE: Identify observations that are now outdated or contradicted by newer observations.
3. CONTRADICT: Flag pairs of observations that directly contradict each other.

Output in this exact format:
INFER: [observation text] SOURCES: [comma-separated source IDs]
SUPERSEDE: [observation ID] REASON: [why it's outdated]
CONTRADICT: [observation ID 1] vs [observation ID 2] REASON: [nature of contradiction]

If there are no inferences, supersessions, or contradictions to report, output nothing."#;

pub struct DeductionSpecialist {
    llm: DreamerLlmClient,
}

pub struct DeductionResult {
    pub new_observations: Vec<Observation>,
    pub superseded_ids: Vec<Uuid>,
    pub contradictions: Vec<(Uuid, Uuid, String)>,
}

impl DeductionSpecialist {
    pub fn new(llm: DreamerLlmClient) -> Self {
        Self { llm }
    }

    pub async fn run(
        &self,
        explicit_observations: &[Observation],
        project_id: Option<Uuid>,
    ) -> Result<DeductionResult> {
        if explicit_observations.is_empty() {
            return Ok(DeductionResult {
                new_observations: Vec::new(),
                superseded_ids: Vec::new(),
                contradictions: Vec::new(),
            });
        }

        let prompt = Self::build_prompt(explicit_observations);
        let response = self.llm.complete(DEDUCTION_SYSTEM_PROMPT, &prompt).await?;
        Ok(Self::parse_response(
            &response,
            explicit_observations,
            project_id,
        ))
    }

    pub(crate) fn build_prompt(observations: &[Observation]) -> String {
        let mut lines = Vec::with_capacity(observations.len());
        for obs in observations {
            lines.push(format!("[{}] {}", obs.id, obs.content));
        }
        lines.join("\n")
    }

    pub(crate) fn parse_response(
        response: &str,
        _source_observations: &[Observation],
        project_id: Option<Uuid>,
    ) -> DeductionResult {
        let now = chrono::Utc::now();
        let mut new_observations = Vec::new();
        let mut superseded_ids = Vec::new();
        let mut contradictions = Vec::new();

        for line in response.lines() {
            let line = line.trim();
            if line.starts_with("INFER:") {
                if let Some((content, sources)) = parse_infer_line(line) {
                    new_observations.push(Observation {
                        id: Uuid::new_v4(),
                        session_id: Uuid::nil(),
                        project_id,
                        level: ObservationLevel::Deductive,
                        content,
                        source_ids: sources,
                        confidence: None,
                        times_derived: 1,
                        created_at: now,
                        updated_at: now,
                    });
                }
            } else if line.starts_with("SUPERSEDE:")
                && let Some(id) = parse_supersede_line(line)
            {
                superseded_ids.push(id);
            } else if line.starts_with("CONTRADICT:")
                && let Some((id1, id2, reason)) = parse_contradict_line(line)
            {
                contradictions.push((id1, id2, reason));
            }
        }

        DeductionResult {
            new_observations,
            superseded_ids,
            contradictions,
        }
    }
}

fn parse_infer_line(line: &str) -> Option<(String, Vec<Uuid>)> {
    let after_prefix = line.strip_prefix("INFER:")?.trim();
    let parts: Vec<&str> = after_prefix.splitn(2, "SOURCES:").collect();
    let content = parts.first()?.trim().to_string();
    let sources = parts
        .get(1)
        .map(|s| {
            s.trim()
                .split(',')
                .filter_map(|id_str| Uuid::parse_str(id_str.trim()).ok())
                .collect()
        })
        .unwrap_or_default();
    if content.is_empty() {
        return None;
    }
    Some((content, sources))
}

fn parse_supersede_line(line: &str) -> Option<Uuid> {
    let after_prefix = line.strip_prefix("SUPERSEDE:")?.trim();
    let id_str = after_prefix.split_whitespace().next()?;
    Uuid::parse_str(id_str).ok()
}

fn parse_contradict_line(line: &str) -> Option<(Uuid, Uuid, String)> {
    let after_prefix = line.strip_prefix("CONTRADICT:")?.trim();
    let parts: Vec<&str> = after_prefix.splitn(2, "REASON:").collect();
    let ids_part = parts.first()?.trim();
    let reason = parts
        .get(1)
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let id_parts: Vec<&str> = ids_part.split("vs").collect();
    let id1 = Uuid::parse_str(id_parts.first()?.trim()).ok()?;
    let id2 = Uuid::parse_str(id_parts.get(1)?.trim()).ok()?;
    Some((id1, id2, reason))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_infer_line_valid() {
        let id1 = Uuid::new_v4();
        let id2 = Uuid::new_v4();
        let line = format!("INFER: User prefers dark themes SOURCES: {id1}, {id2}");
        let (content, sources) = parse_infer_line(&line).unwrap();
        assert_eq!(content, "User prefers dark themes");
        assert_eq!(sources.len(), 2);
        assert!(sources.contains(&id1));
        assert!(sources.contains(&id2));
    }

    #[test]
    fn test_parse_infer_line_no_sources() {
        let line = "INFER: Standalone inference";
        let (content, sources) = parse_infer_line(line).unwrap();
        assert_eq!(content, "Standalone inference");
        assert!(sources.is_empty());
    }

    #[test]
    fn test_parse_infer_line_empty_content() {
        let line = "INFER:  SOURCES: some-uuid";
        assert!(parse_infer_line(line).is_none());
    }

    #[test]
    fn test_parse_supersede_line_valid() {
        let id = Uuid::new_v4();
        let line = format!("SUPERSEDE: {id} REASON: outdated by newer data");
        let parsed = parse_supersede_line(&line).unwrap();
        assert_eq!(parsed, id);
    }

    #[test]
    fn test_parse_supersede_line_invalid_uuid() {
        let line = "SUPERSEDE: not-a-uuid REASON: whatever";
        assert!(parse_supersede_line(line).is_none());
    }

    #[test]
    fn test_parse_contradict_line_valid() {
        let id1 = Uuid::new_v4();
        let id2 = Uuid::new_v4();
        let line = format!("CONTRADICT: {id1} vs {id2} REASON: one says X, other says Y");
        let (parsed_id1, parsed_id2, reason) = parse_contradict_line(&line).unwrap();
        assert_eq!(parsed_id1, id1);
        assert_eq!(parsed_id2, id2);
        assert_eq!(reason, "one says X, other says Y");
    }

    #[test]
    fn test_parse_contradict_line_invalid() {
        let line = "CONTRADICT: invalid format";
        assert!(parse_contradict_line(line).is_none());
    }

    #[test]
    fn test_parse_response_mixed() {
        let id1 = Uuid::new_v4();
        let id2 = Uuid::new_v4();
        let id3 = Uuid::new_v4();
        let response = format!(
            "INFER: New fact from data SOURCES: {id1}, {id2}\n\
             SUPERSEDE: {id3} REASON: outdated\n\
             CONTRADICT: {id1} vs {id2} REASON: conflicting info\n\
             Some random line that should be ignored"
        );

        let obs = vec![
            Observation {
                id: id1,
                session_id: Uuid::new_v4(),
                project_id: None,
                level: ObservationLevel::Explicit,
                content: "fact1".to_string(),
                source_ids: vec![],
                confidence: None,
                times_derived: 1,
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            },
            Observation {
                id: id2,
                session_id: Uuid::new_v4(),
                project_id: None,
                level: ObservationLevel::Explicit,
                content: "fact2".to_string(),
                source_ids: vec![],
                confidence: None,
                times_derived: 1,
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            },
        ];

        let result = DeductionSpecialist::parse_response(&response, &obs, None);
        assert_eq!(result.new_observations.len(), 1);
        assert_eq!(result.new_observations[0].content, "New fact from data");
        assert_eq!(
            result.new_observations[0].level,
            ObservationLevel::Deductive
        );
        assert_eq!(result.superseded_ids.len(), 1);
        assert_eq!(result.superseded_ids[0], id3);
        assert_eq!(result.contradictions.len(), 1);
        assert_eq!(result.contradictions[0].0, id1);
        assert_eq!(result.contradictions[0].1, id2);
    }

    #[test]
    fn test_parse_response_empty() {
        let result = DeductionSpecialist::parse_response("", &[], None);
        assert!(result.new_observations.is_empty());
        assert!(result.superseded_ids.is_empty());
        assert!(result.contradictions.is_empty());
    }
}

//! Induction specialist: identifies cross-cutting behavioral patterns
//! across multiple observations.

use super::llm_client::DreamerLlmClient;
use crate::error::Result;
use rsi_common::types::{Observation, ObservationConfidence, ObservationLevel};
use uuid::Uuid;

pub(crate) const INDUCTION_SYSTEM_PROMPT: &str = r#"You are an induction specialist. Given observations at all levels (explicit, deductive, and existing inductive), identify cross-cutting behavioral patterns.

For each pattern:
- It must be supported by at least 2 observations
- Assign confidence: LOW (2-3 sources), MEDIUM (4-6 sources), HIGH (7+ sources)
- Include the IDs of supporting observations

Output in this exact format:
PATTERN: [pattern description] CONFIDENCE: [LOW|MEDIUM|HIGH] SOURCES: [comma-separated source IDs]

If there are no patterns to identify, output nothing."#;

pub struct InductionSpecialist {
    llm: DreamerLlmClient,
}

pub struct InductionResult {
    pub patterns: Vec<Observation>,
}

impl InductionSpecialist {
    pub fn new(llm: DreamerLlmClient) -> Self {
        Self { llm }
    }

    pub async fn run(
        &self,
        all_observations: &[Observation],
        project_id: Option<Uuid>,
    ) -> Result<InductionResult> {
        if all_observations.len() < 3 {
            return Ok(InductionResult {
                patterns: Vec::new(),
            });
        }

        let prompt = Self::build_prompt(all_observations);
        let response = self.llm.complete(INDUCTION_SYSTEM_PROMPT, &prompt).await?;
        Ok(Self::parse_response(&response, project_id))
    }

    pub(crate) fn build_prompt(observations: &[Observation]) -> String {
        let mut lines = Vec::with_capacity(observations.len());
        for obs in observations {
            let level = match obs.level {
                ObservationLevel::Explicit => "explicit",
                ObservationLevel::Deductive => "deductive",
                ObservationLevel::Inductive => "inductive",
                ObservationLevel::Contradiction => "contradiction",
                _ => "unknown",
            };
            lines.push(format!("[{}] ({}) {}", obs.id, level, obs.content));
        }
        lines.join("\n")
    }

    pub(crate) fn parse_response(response: &str, project_id: Option<Uuid>) -> InductionResult {
        let now = chrono::Utc::now();
        let mut patterns = Vec::new();

        for line in response.lines() {
            let line = line.trim();
            if !line.starts_with("PATTERN:") {
                continue;
            }

            if let Some((content, confidence, sources)) = parse_pattern_line(line) {
                patterns.push(Observation {
                    id: Uuid::new_v4(),
                    session_id: Uuid::nil(),
                    project_id,
                    level: ObservationLevel::Inductive,
                    content,
                    source_ids: sources,
                    confidence: Some(confidence),
                    times_derived: 1,
                    created_at: now,
                    updated_at: now,
                });
            }
        }

        InductionResult { patterns }
    }
}

fn parse_pattern_line(line: &str) -> Option<(String, ObservationConfidence, Vec<Uuid>)> {
    let after_prefix = line.strip_prefix("PATTERN:")?.trim();

    // Split on CONFIDENCE:
    let parts: Vec<&str> = after_prefix.splitn(2, "CONFIDENCE:").collect();
    let content = parts.first()?.trim().to_string();

    let remainder = parts.get(1)?.trim();
    let conf_parts: Vec<&str> = remainder.splitn(2, "SOURCES:").collect();

    let confidence = match conf_parts.first()?.trim().to_uppercase().as_str() {
        "LOW" => ObservationConfidence::Low,
        "MEDIUM" => ObservationConfidence::Medium,
        "HIGH" => ObservationConfidence::High,
        _ => ObservationConfidence::Low,
    };

    let sources = conf_parts
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
    Some((content, confidence, sources))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_pattern_line_low_confidence() {
        let id1 = Uuid::new_v4();
        let id2 = Uuid::new_v4();
        let line =
            format!("PATTERN: User always uses vim bindings CONFIDENCE: LOW SOURCES: {id1}, {id2}");
        let (content, conf, sources) = parse_pattern_line(&line).unwrap();
        assert_eq!(content, "User always uses vim bindings");
        assert_eq!(conf, ObservationConfidence::Low);
        assert_eq!(sources.len(), 2);
    }

    #[test]
    fn test_parse_pattern_line_medium_confidence() {
        let ids: Vec<Uuid> = (0..5).map(|_| Uuid::new_v4()).collect();
        let ids_str: Vec<String> = ids.iter().map(|id| id.to_string()).collect();
        let line = format!(
            "PATTERN: Prefers dark themes CONFIDENCE: MEDIUM SOURCES: {}",
            ids_str.join(", ")
        );
        let (content, conf, sources) = parse_pattern_line(&line).unwrap();
        assert_eq!(content, "Prefers dark themes");
        assert_eq!(conf, ObservationConfidence::Medium);
        assert_eq!(sources.len(), 5);
    }

    #[test]
    fn test_parse_pattern_line_high_confidence() {
        let ids: Vec<Uuid> = (0..8).map(|_| Uuid::new_v4()).collect();
        let ids_str: Vec<String> = ids.iter().map(|id| id.to_string()).collect();
        let line = format!(
            "PATTERN: Consistent coding style CONFIDENCE: HIGH SOURCES: {}",
            ids_str.join(", ")
        );
        let (_, conf, sources) = parse_pattern_line(&line).unwrap();
        assert_eq!(conf, ObservationConfidence::High);
        assert_eq!(sources.len(), 8);
    }

    #[test]
    fn test_parse_pattern_line_empty_content() {
        let line = "PATTERN:  CONFIDENCE: LOW SOURCES: some-id";
        assert!(parse_pattern_line(line).is_none());
    }

    #[test]
    fn test_parse_pattern_line_missing_confidence() {
        let line = "PATTERN: Some observation";
        assert!(parse_pattern_line(line).is_none());
    }

    #[test]
    fn test_parse_response_multiple_patterns() {
        let id1 = Uuid::new_v4();
        let id2 = Uuid::new_v4();
        let id3 = Uuid::new_v4();
        let response = format!(
            "PATTERN: First pattern CONFIDENCE: LOW SOURCES: {id1}, {id2}\n\
             Some noise line\n\
             PATTERN: Second pattern CONFIDENCE: HIGH SOURCES: {id1}, {id2}, {id3}"
        );

        let result = InductionSpecialist::parse_response(&response, None);
        assert_eq!(result.patterns.len(), 2);
        assert_eq!(result.patterns[0].content, "First pattern");
        assert_eq!(
            result.patterns[0].confidence,
            Some(ObservationConfidence::Low)
        );
        assert_eq!(result.patterns[0].level, ObservationLevel::Inductive);
        assert_eq!(result.patterns[1].content, "Second pattern");
        assert_eq!(
            result.patterns[1].confidence,
            Some(ObservationConfidence::High)
        );
    }

    #[test]
    fn test_parse_response_empty() {
        let result = InductionSpecialist::parse_response("", None);
        assert!(result.patterns.is_empty());
    }
}

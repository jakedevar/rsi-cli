//! `:manager escalations` (#1238): the operator's queue at the top of every
//! manager chain. All calls are operator-only daemon RPCs.
//!
//! - `:manager escalations [list]` lists open escalations and unread
//!   top-of-chain reports;
//! - `:manager escalations all` also lists ruled, retired and read rows;
//! - `:manager escalations rule <hop> <text>` rules one open escalation (a
//!   hop id or a unique prefix of 4+ characters); the ruling returns down the
//!   recorded chain to the source seat. It is a manager decision and never
//!   answers a human approval;
//! - `:manager escalations ack <notice>` marks one report read;
//! - `:manager escalations undelivered [<cursor>]` pages tier mail whose
//!   delivery failed or is uncertain (#1295); a list names the next cursor.

use rsi_common::manager_tier_routing::{
    ListOperatorEscalationsResultV1, ManagerTierEscalationHopV1, OperatorNoticeV1,
    RuleOperatorEscalationRequestV1, UndeliveredTierMailV1,
};
use uuid::Uuid;

use crate::app::App;

const USAGE: &str =
    "Use :manager escalations [list|all|rule <hop> <text>|ack <notice>|undelivered [<cursor>]].";

/// One parsed `:manager escalations` command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EscalationsCommand {
    List {
        include_closed: bool,
    },
    Rule {
        hop: String,
        ruling: String,
    },
    Ack(String),
    /// One page of failed and uncertain tier mail after a cursor.
    Undelivered(Option<String>),
}

pub(crate) fn parse(command: &str) -> Result<EscalationsCommand, String> {
    let command = command.trim();
    let (verb, rest) = command
        .split_once(char::is_whitespace)
        .map_or((command, ""), |(verb, rest)| (verb, rest.trim()));
    match verb {
        "" | "list" if rest.is_empty() => Ok(EscalationsCommand::List {
            include_closed: false,
        }),
        "all" if rest.is_empty() => Ok(EscalationsCommand::List {
            include_closed: true,
        }),
        "ack" if !rest.is_empty() && !rest.contains(char::is_whitespace) => {
            Ok(EscalationsCommand::Ack(rest.into()))
        }
        "undelivered" if !rest.contains(char::is_whitespace) => Ok(
            EscalationsCommand::Undelivered((!rest.is_empty()).then(|| rest.to_string())),
        ),
        "rule" => {
            let (hop, ruling) = rest
                .split_once(char::is_whitespace)
                .map(|(hop, ruling)| (hop, ruling.trim()))
                .ok_or_else(|| USAGE.to_string())?;
            if ruling.is_empty() {
                return Err(USAGE.into());
            }
            Ok(EscalationsCommand::Rule {
                hop: hop.into(),
                ruling: ruling.into(),
            })
        }
        _ => Err(USAGE.into()),
    }
}

/// Resolve an id argument among `ids`: a full id or a unique prefix of 4+
/// characters.
pub(crate) fn resolve_id(ids: &[Uuid], arg: &str, what: &str) -> Result<Uuid, String> {
    if let Ok(id) = Uuid::parse_str(arg) {
        return Ok(id);
    }
    if arg.len() < 4 {
        return Err(format!(
            "Name the {what} by its id or a 4+ character prefix"
        ));
    }
    let matches: Vec<&Uuid> = ids
        .iter()
        .filter(|id| id.to_string().starts_with(arg))
        .collect();
    match matches.as_slice() {
        [one] => Ok(**one),
        [] => Err(format!("No open {what} matches {arg}")),
        _ => Err(format!(
            "{arg} names {} {what}s; use more characters",
            matches.len()
        )),
    }
}

fn short(id: Uuid) -> String {
    id.to_string()[..8].to_string()
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or_default()
}

/// One line per escalation hop: id, state, origin and reason.
pub(crate) fn hop_summary(hop: &ManagerTierEscalationHopV1) -> String {
    let ruling = hop
        .ruling
        .as_deref()
        .map(|ruling| format!(" -> ruled: {}", first_line(ruling)))
        .unwrap_or_default();
    format!(
        "escalation {} ({}, hop {}) from {} in project {}: {}{}",
        short(hop.hop_id),
        hop.state,
        hop.hop,
        hop.source_ref,
        short(hop.project_id),
        first_line(&hop.reason),
        ruling,
    )
}

/// One line per top-of-chain report.
pub(crate) fn notice_summary(notice: &OperatorNoticeV1) -> String {
    format!(
        "report {} ({}) from {}: {}",
        short(notice.message_id),
        if notice.state == "queued" {
            "unread"
        } else {
            "read"
        },
        notice.source_ref,
        first_line(&notice.body),
    )
}

/// One line per tier message whose delivery failed or is uncertain (#1266).
pub(crate) fn undelivered_summary(mail: &UndeliveredTierMailV1) -> String {
    format!(
        "{} {} {} ({} -> {}): {} [{}]",
        mail.kind,
        short(mail.message_id),
        mail.state,
        mail.source_ref,
        mail.target_ref,
        first_line(&mail.settle_reason),
        first_line(&mail.body),
    )
}

pub(crate) fn queue_summary(queue: &ListOperatorEscalationsResultV1) -> String {
    if queue.escalations.is_empty() && queue.notices.is_empty() && queue.undelivered.is_empty() {
        return "No escalations or reports wait for the operator.".into();
    }
    queue
        .escalations
        .iter()
        .map(hop_summary)
        .chain(queue.notices.iter().map(notice_summary))
        .chain(queue.undelivered.iter().map(undelivered_summary))
        .chain(queue.next_undelivered_after.iter().map(|cursor| {
            format!("More undelivered mail: :manager escalations undelivered {cursor}")
        }))
        .collect::<Vec<_>>()
        .join("\n")
}

/// One page of failed and uncertain tier mail (#1295).
pub(crate) fn undelivered_page_summary(queue: &ListOperatorEscalationsResultV1) -> String {
    if queue.undelivered.is_empty() {
        return "No undelivered tier mail on this page.".into();
    }
    queue
        .undelivered
        .iter()
        .map(undelivered_summary)
        .chain(queue.next_undelivered_after.iter().map(|cursor| {
            format!("More undelivered mail: :manager escalations undelivered {cursor}")
        }))
        .collect::<Vec<_>>()
        .join("\n")
}

pub(crate) async fn dispatch_escalations_command(app: &mut App, command: &str) {
    match run(app, command).await {
        Ok(message) => app.notify_success(message),
        Err(error) => app.notify_error(error),
    }
    app.mark_dirty();
}

fn rpc_error(error: impl std::fmt::Display) -> String {
    format!("Escalations: {error}")
}

pub(crate) async fn run(app: &mut App, command: &str) -> Result<String, String> {
    match parse(command)? {
        EscalationsCommand::List { include_closed } => {
            let queue = app
                .client
                .list_operator_escalations(include_closed, None)
                .await
                .map_err(rpc_error)?;
            Ok(queue_summary(&queue))
        }
        EscalationsCommand::Undelivered(after) => {
            let queue = app
                .client
                .list_operator_escalations(false, after)
                .await
                .map_err(rpc_error)?;
            Ok(undelivered_page_summary(&queue))
        }
        EscalationsCommand::Rule { hop, ruling } => {
            let queue = app
                .client
                .list_operator_escalations(false, None)
                .await
                .map_err(rpc_error)?;
            let ids: Vec<Uuid> = queue.escalations.iter().map(|hop| hop.hop_id).collect();
            let hop_id = resolve_id(&ids, &hop, "escalation")?;
            let ruled = app
                .client
                .rule_operator_escalation(RuleOperatorEscalationRequestV1 {
                    hop_id,
                    ruling,
                    idempotency_key: Uuid::new_v4().to_string(),
                })
                .await
                .map_err(rpc_error)?;
            Ok(format!("Escalation ruled: {}", hop_summary(&ruled)))
        }
        EscalationsCommand::Ack(notice) => {
            let queue = app
                .client
                .list_operator_escalations(false, None)
                .await
                .map_err(rpc_error)?;
            let ids: Vec<Uuid> = queue.notices.iter().map(|n| n.message_id).collect();
            let message_id = resolve_id(&ids, &notice, "report")?;
            let read = app
                .client
                .acknowledge_operator_notice(message_id)
                .await
                .map_err(rpc_error)?;
            Ok(format!("Report read: {}", notice_summary(&read)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_lists_rules_and_acknowledges() {
        assert_eq!(
            parse("").unwrap(),
            EscalationsCommand::List {
                include_closed: false
            }
        );
        assert_eq!(
            parse("all").unwrap(),
            EscalationsCommand::List {
                include_closed: true
            }
        );
        assert_eq!(
            parse("rule 1a2b3c4d Split the branch.").unwrap(),
            EscalationsCommand::Rule {
                hop: "1a2b3c4d".into(),
                ruling: "Split the branch.".into(),
            }
        );
        assert_eq!(
            parse("ack 9f8e").unwrap(),
            EscalationsCommand::Ack("9f8e".into())
        );
        assert_eq!(
            parse("undelivered").unwrap(),
            EscalationsCommand::Undelivered(None)
        );
        assert_eq!(
            parse("undelivered 2026-10-06T00:00:00.000000000Z|abc").unwrap(),
            EscalationsCommand::Undelivered(Some("2026-10-06T00:00:00.000000000Z|abc".into()))
        );
        assert_eq!(parse("rule 1a2b3c4d").unwrap_err(), USAGE);
        assert_eq!(parse("revoke x").unwrap_err(), USAGE);
    }

    #[test]
    fn ids_resolve_by_unique_prefix() {
        let a = Uuid::parse_str("1a2b3c4d-0000-4000-8000-000000000001").unwrap();
        let b = Uuid::parse_str("1a2b9999-0000-4000-8000-000000000002").unwrap();
        assert_eq!(resolve_id(&[a, b], "1a2b3", "escalation").unwrap(), a);
        assert_eq!(
            resolve_id(&[a, b], &b.to_string(), "escalation").unwrap(),
            b
        );
        assert!(resolve_id(&[a, b], "1a2b", "escalation").is_err());
        assert!(resolve_id(&[a, b], "1a", "escalation").is_err());
    }

    #[test]
    fn the_queue_summary_names_each_row() {
        let now = chrono::Utc::now();
        let hop = ManagerTierEscalationHopV1 {
            hop_id: Uuid::new_v4(),
            escalation_id: Uuid::new_v4(),
            project_id: Uuid::new_v4(),
            subject_id: Uuid::new_v4(),
            reason: "Two areas claim the release branch".into(),
            hop: 3,
            source_ref: "portfolio:p".into(),
            target_ref: "operator".into(),
            target_session_id: None,
            target_grant_version: None,
            state: "open".into(),
            ruling: None,
            created_at: now,
            updated_at: now,
        };
        let notice = OperatorNoticeV1 {
            message_id: Uuid::new_v4(),
            source_ref: "portfolio:p".into(),
            source_session_id: Some(Uuid::new_v4()),
            project_id: None,
            body: "Portfolio green.".into(),
            state: "queued".into(),
            created_at: now,
        };
        let undelivered = UndeliveredTierMailV1 {
            message_id: Uuid::new_v4(),
            role: String::new(),
            kind: "message".into(),
            source_ref: "portfolio:p".into(),
            target_ref: "project:q".into(),
            source_session_id: Some(Uuid::new_v4()),
            target_session_id: Some(Uuid::new_v4()),
            project_id: None,
            body: "Rebase onto rolling.".into(),
            state: "failed".into(),
            settle_reason: "admission refused".into(),
            settled_at: now,
        };
        let text = queue_summary(&ListOperatorEscalationsResultV1 {
            escalations: vec![hop],
            notices: vec![notice],
            undelivered: vec![undelivered],
            next_undelivered_after: Some("2026-10-06T00:00:00.000000000Z|abc".into()),
        });
        assert!(
            text.contains(":manager escalations undelivered 2026-10-06T00:00:00.000000000Z|abc")
        );
        assert!(text.contains("message"));
        assert!(text.contains("failed"));
        assert!(text.contains("admission refused"));
        assert!(text.contains("Two areas claim the release branch"));
        assert!(text.contains("hop 3"));
        assert!(text.contains("report"));
        assert!(text.contains("Portfolio green."));
        assert!(text.contains("unread"));
        assert_eq!(
            queue_summary(&ListOperatorEscalationsResultV1::default()),
            "No escalations or reports wait for the operator."
        );
    }
}

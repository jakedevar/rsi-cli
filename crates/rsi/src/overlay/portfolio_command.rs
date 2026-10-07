//! `:manager portfolio` (#1236): the operator lists, inspects, appoints,
//! configures and revokes portfolio nodes (managers above project level, any
//! tier). All calls are operator-only daemon RPCs.
//!
//! - `:manager portfolio [list]` lists the active nodes;
//! - `:manager portfolio show <node>` shows one node (id, id prefix or a
//!   unique tier label);
//! - `:manager portfolio appoint <label> [project names...]` creates a root
//!   labelled `<label>` with the focused session as its seat over the named
//!   projects (comma-separated, or space-separated single words; default:
//!   every project not covered by another node), with the operator's default
//!   launches and the Execute policy;
//! - `:manager portfolio appoint <label> --adopt <node,...> [projects...]`
//!   (#1237) appoints a manager above existing nodes: the new node covers the
//!   adopted nodes' projects (plus any named ones), sits where they sat (a
//!   root, or under their shared parent) and is granted at least one unit
//!   more than each of them in every allowance, so they narrow it;
//! - `:manager portfolio configure <JSON>` sends a full typed request;
//! - `:manager portfolio revoke <node>` revokes one node at its current
//!   versions.

use rsi_common::harness_manager_v2::{ManagerLaunchChoiceV2, ManagerPolicyV2};
use rsi_common::portfolio_nodes::{
    ConfigurePortfolioNodeRequestV1, PORTFOLIO_DEFAULT_MAX_DIRECT_REPORTS,
    PORTFOLIO_MAX_DIRECT_REPORTS, PortfolioNodeV1, RevokePortfolioNodeRequestV1, valid_tier_label,
};
use rsi_common::types::Project;
use uuid::Uuid;

use crate::app::App;
use crate::overlay::global_manager_command::{
    default_allowed_launches, default_project_policy, resolve_projects,
};

const USAGE: &str = "Use :manager portfolio [list|show <node>|appoint <label> [--adopt <node,...>] [projects...]|configure <JSON>|revoke <node>].";

/// One parsed `:manager portfolio` command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PortfolioCommand {
    List,
    Show(String),
    Appoint {
        label: String,
        projects: String,
        /// Node arguments (ids, id prefixes or unique labels) to adopt.
        adopt: Vec<String>,
    },
    Configure(String),
    Revoke(String),
}

pub(crate) fn parse(command: &str) -> Result<PortfolioCommand, String> {
    let command = command.trim();
    let (verb, rest) = command
        .split_once(char::is_whitespace)
        .map_or((command, ""), |(verb, rest)| (verb, rest.trim()));
    match verb {
        "" | "list" if rest.is_empty() => Ok(PortfolioCommand::List),
        "show" if !rest.is_empty() => Ok(PortfolioCommand::Show(rest.into())),
        "revoke" if !rest.is_empty() => Ok(PortfolioCommand::Revoke(rest.into())),
        "configure" if !rest.is_empty() => Ok(PortfolioCommand::Configure(rest.into())),
        "appoint" if !rest.is_empty() => {
            let (label, projects) = rest
                .split_once(char::is_whitespace)
                .map_or((rest, ""), |(label, projects)| (label, projects.trim()));
            if !valid_tier_label(label) {
                return Err(format!("Invalid tier label: {label}"));
            }
            let (projects, adopt) = split_adopt(projects)?;
            Ok(PortfolioCommand::Appoint {
                label: label.into(),
                projects,
                adopt,
            })
        }
        _ => Err(USAGE.into()),
    }
}

/// Split `--adopt <node,...>` (anywhere after the label) from the project
/// arguments.
fn split_adopt(args: &str) -> Result<(String, Vec<String>), String> {
    let mut projects = Vec::new();
    let mut adopt = Vec::new();
    let mut words = args.split_whitespace();
    while let Some(word) = words.next() {
        if word == "--adopt" {
            let nodes = words
                .next()
                .ok_or_else(|| "--adopt needs <node,...>".to_string())?;
            adopt.extend(
                nodes
                    .split(',')
                    .map(str::trim)
                    .filter(|node| !node.is_empty())
                    .map(str::to_string),
            );
        } else if let Some(nodes) = word.strip_prefix("--adopt=") {
            adopt.extend(
                nodes
                    .split(',')
                    .map(str::trim)
                    .filter(|node| !node.is_empty())
                    .map(str::to_string),
            );
        } else {
            projects.push(word);
        }
    }
    Ok((projects.join(" "), adopt))
}

/// #1237: a grant every `children` node narrows (the daemon's
/// `grant_narrows`): `base` widened to each child's capabilities and to one
/// unit more than each child in every finite allowance, with no provider
/// ceilings and a spend cap only when every child and `base` are capped.
#[must_use]
pub(crate) fn covering_policy(
    base: &ManagerPolicyV2,
    children: &[&PortfolioNodeV1],
) -> ManagerPolicyV2 {
    let mut policy = base.clone();
    for child in children {
        let child = &child.grant.project_policy;
        for capability in &child.capabilities {
            if !policy.capabilities.contains(capability) {
                policy.capabilities.push(*capability);
            }
        }
        policy.max_created_containers = policy
            .max_created_containers
            .max(child.max_created_containers.saturating_add(1));
        policy.max_created_sessions = policy
            .max_created_sessions
            .max(child.max_created_sessions.saturating_add(1));
        policy.max_active_sessions = policy
            .max_active_sessions
            .max(child.max_active_sessions.saturating_add(1));
        policy.max_spend_usd = match (policy.max_spend_usd, child.max_spend_usd) {
            (Some(own), Some(child)) => Some(own.max(child)),
            _ => None,
        };
        for launch in &child.allowed_launches {
            if !policy.allowed_launches.is_empty() && !policy.allowed_launches.contains(launch) {
                policy.allowed_launches.push(launch.clone());
            }
        }
    }
    policy.provider_limits.clear();
    policy
}

/// `base` launches plus every child's.
#[must_use]
pub(crate) fn covering_launches(
    base: &[ManagerLaunchChoiceV2],
    children: &[&PortfolioNodeV1],
) -> Vec<ManagerLaunchChoiceV2> {
    let mut launches = base.to_vec();
    for child in children {
        for launch in &child.grant.allowed_launches {
            if !launches.contains(launch) {
                launches.push(launch.clone());
            }
        }
    }
    launches
}

/// A direct-report limit that fits `children` and is no lower than any of
/// theirs.
#[must_use]
pub(crate) fn covering_reports(base: u16, children: &[&PortfolioNodeV1]) -> u16 {
    children
        .iter()
        .map(|child| child.max_direct_reports)
        .chain([base, u16::try_from(children.len()).unwrap_or(u16::MAX)])
        .max()
        .unwrap_or(base)
        .min(PORTFOLIO_MAX_DIRECT_REPORTS)
}

/// The request appointing a new node labelled `label`, seated on `seat`,
/// above `adopted` (which must share one parent) plus `extra` projects.
pub(crate) fn appoint_above_request(
    label: &str,
    seat: Uuid,
    adopted: &[&PortfolioNodeV1],
    extra: &[Uuid],
) -> Result<ConfigurePortfolioNodeRequestV1, String> {
    let parent = adopted.first().and_then(|node| node.parent_node_id);
    if adopted.iter().any(|node| node.parent_node_id != parent) {
        return Err("The adopted nodes must share one parent (or all be roots).".into());
    }
    let mut project_ids: Vec<Uuid> = Vec::new();
    for id in adopted
        .iter()
        .flat_map(|node| node.grant.project_ids.iter())
        .chain(extra)
    {
        if !project_ids.contains(id) {
            project_ids.push(*id);
        }
    }
    Ok(ConfigurePortfolioNodeRequestV1 {
        node_id: None,
        parent_node_id: parent,
        adopt_node_ids: adopted.iter().map(|node| node.node_id).collect(),
        expected_parent_grant_version: None,
        tier_label: label.into(),
        seat_session_id: seat,
        project_ids,
        allowed_launches: covering_launches(&default_allowed_launches(), adopted),
        policy: covering_policy(&default_project_policy(), adopted),
        child_policy: None,
        max_direct_reports: covering_reports(PORTFOLIO_DEFAULT_MAX_DIRECT_REPORTS, adopted),
        expected_node_grant_version: 0,
        expected_authority_epoch: 0,
        idempotency_key: Uuid::new_v4().to_string(),
    })
}

/// Resolve a node argument: a full id, a unique id prefix (4+ characters) or
/// a unique tier label among `nodes`.
pub(crate) fn resolve_node(nodes: &[PortfolioNodeV1], arg: &str) -> Result<Uuid, String> {
    if let Ok(id) = Uuid::parse_str(arg) {
        return Ok(id);
    }
    let by_prefix: Vec<&PortfolioNodeV1> = if arg.len() >= 4 {
        nodes
            .iter()
            .filter(|node| node.node_id.to_string().starts_with(arg))
            .collect()
    } else {
        Vec::new()
    };
    let matches = if by_prefix.is_empty() {
        nodes
            .iter()
            .filter(|node| node.state == "active" && node.tier_label == arg)
            .collect()
    } else {
        by_prefix
    };
    match matches.as_slice() {
        [one] => Ok(one.node_id),
        [] => Err(format!("No portfolio node matches {arg}")),
        _ => Err(format!(
            "{arg} names {} portfolio nodes; use the node id",
            matches.len()
        )),
    }
}

fn short(id: Uuid) -> String {
    id.to_string()[..8].to_string()
}

fn project_names(ids: &[Uuid], projects: &[Project]) -> String {
    ids.iter()
        .map(|id| {
            projects
                .iter()
                .find(|project| project.id == *id)
                .map_or_else(|| short(*id), |project| project.name.clone())
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// One line per node: label, id, state, versions, seat and projects.
pub(crate) fn node_summary(node: &PortfolioNodeV1, projects: &[Project]) -> String {
    format!(
        "{} {} ({}) v{} epoch {} seat {}: projects [{}]; PM policy {:?}",
        node.tier_label,
        short(node.node_id),
        node.state,
        node.grant.grant_version,
        node.authority_epoch,
        short(node.grant.seat_session_id),
        project_names(&node.grant.project_ids, projects),
        node.grant.project_policy.mode,
    )
}

/// Projects for `appoint`: the named ones, else every project no active node
/// covers (the daemon refuses an overlap).
fn appoint_projects(
    projects: &[Project],
    nodes: &[PortfolioNodeV1],
    args: &str,
) -> Result<Vec<Uuid>, String> {
    if !args.trim().is_empty() {
        return resolve_projects(projects, args);
    }
    let free: Vec<Uuid> = projects
        .iter()
        .map(|project| project.id)
        .filter(|id| {
            !nodes
                .iter()
                .any(|node| node.state == "active" && node.grant.project_ids.contains(id))
        })
        .collect();
    if free.is_empty() {
        return Err(
            "Every project is already covered by a portfolio node; name the projects.".into(),
        );
    }
    Ok(free)
}

pub(crate) async fn dispatch_portfolio_command(app: &mut App, command: &str) {
    match run(app, command).await {
        Ok(message) => app.notify_success(message),
        Err(error) => app.notify_error(error),
    }
    app.mark_dirty();
}

fn rpc_error(error: impl std::fmt::Display) -> String {
    format!("Portfolio: {error}")
}

pub(crate) async fn run(app: &mut App, command: &str) -> Result<String, String> {
    match parse(command)? {
        PortfolioCommand::List => {
            let nodes = app
                .client
                .list_portfolio_nodes(false)
                .await
                .map_err(rpc_error)?;
            if nodes.is_empty() {
                return Ok("No portfolio nodes. Focus a session and run :manager portfolio appoint <label> [projects...].".into());
            }
            Ok(nodes
                .iter()
                .map(|node| node_summary(node, &app.projects))
                .collect::<Vec<_>>()
                .join("\n"))
        }
        PortfolioCommand::Show(arg) => {
            let nodes = app
                .client
                .list_portfolio_nodes(true)
                .await
                .map_err(rpc_error)?;
            let id = resolve_node(&nodes, &arg)?;
            let node = app
                .client
                .get_portfolio_node(id)
                .await
                .map_err(rpc_error)?
                .ok_or_else(|| format!("No portfolio node {arg}"))?;
            Ok(node_summary(&node, &app.projects))
        }
        PortfolioCommand::Appoint {
            label,
            projects,
            adopt,
        } => {
            let seat = app
                .selected_session_id()
                .ok_or_else(|| "Focus the session to appoint as the node's seat.".to_string())?;
            let nodes = app
                .client
                .list_portfolio_nodes(false)
                .await
                .map_err(rpc_error)?;
            let request = if adopt.is_empty() {
                let project_ids = appoint_projects(&app.projects, &nodes, &projects)?;
                ConfigurePortfolioNodeRequestV1 {
                    node_id: None,
                    parent_node_id: None,
                    adopt_node_ids: Vec::new(),
                    expected_parent_grant_version: None,
                    tier_label: label,
                    seat_session_id: seat,
                    project_ids,
                    allowed_launches: default_allowed_launches(),
                    policy: default_project_policy(),
                    child_policy: None,
                    max_direct_reports: PORTFOLIO_DEFAULT_MAX_DIRECT_REPORTS,
                    expected_node_grant_version: 0,
                    expected_authority_epoch: 0,
                    idempotency_key: Uuid::new_v4().to_string(),
                }
            } else {
                let ids = adopt
                    .iter()
                    .map(|arg| resolve_node(&nodes, arg))
                    .collect::<Result<Vec<_>, _>>()?;
                let adopted: Vec<&PortfolioNodeV1> = ids
                    .iter()
                    .map(|id| {
                        nodes
                            .iter()
                            .find(|node| node.node_id == *id)
                            .ok_or_else(|| format!("No active portfolio node {id}"))
                    })
                    .collect::<Result<_, _>>()?;
                let extra = if projects.trim().is_empty() {
                    Vec::new()
                } else {
                    resolve_projects(&app.projects, &projects)?
                };
                appoint_above_request(&label, seat, &adopted, &extra)?
            };
            let adopted = request.adopt_node_ids.len();
            let node = match app.client.configure_portfolio_node(request.clone()).await {
                Ok(node) => node,
                Err(error) => {
                    let error = error.to_string();
                    // #1544: a cap-reduction refusal opens the tree's confirm
                    // step (preview, then `y` resends with
                    // confirm_cap_reductions:true).
                    if crate::overlay::manager_tree::offer_cap_confirmation(
                        app,
                        "Appoint the portfolio node",
                        crate::overlay::manager_tree::PreparedRequest::ConfigurePortfolio(
                            Box::new(request),
                        ),
                        &error,
                    )
                    .await
                    {
                        return Ok(
                            crate::overlay::global_manager_command::CAP_CONFIRM_NOTICE.into()
                        );
                    }
                    return Err(rpc_error(error));
                }
            };
            Ok(if adopted == 0 {
                format!(
                    "Portfolio node appointed: {}",
                    node_summary(&node, &app.projects)
                )
            } else {
                format!(
                    "Portfolio node appointed above {adopted} node(s): {}",
                    node_summary(&node, &app.projects)
                )
            })
        }
        PortfolioCommand::Configure(raw) => {
            let confirmed: rsi_common::portfolio_nodes::PortfolioCapConfirmation<
                ConfigurePortfolioNodeRequestV1,
            > = serde_json::from_str(&raw)
                .map_err(|error| format!("Portfolio request JSON: {error}"))?;
            let request = confirmed.request;
            request.validate().map_err(rpc_error)?;
            let node = app
                .client
                .configure_portfolio_node_confirmed(request, confirmed.confirm_cap_reductions)
                .await
                .map_err(rpc_error)?;
            Ok(format!(
                "Portfolio node saved: {}",
                node_summary(&node, &app.projects)
            ))
        }
        PortfolioCommand::Revoke(arg) => {
            let nodes = app
                .client
                .list_portfolio_nodes(false)
                .await
                .map_err(rpc_error)?;
            let id = resolve_node(&nodes, &arg)?;
            let node = app
                .client
                .get_portfolio_node(id)
                .await
                .map_err(rpc_error)?
                .ok_or_else(|| format!("No portfolio node {arg}"))?;
            let revoked = app
                .client
                .revoke_portfolio_node(RevokePortfolioNodeRequestV1 {
                    node_id: node.node_id,
                    expected_grant_version: node.grant.grant_version,
                    expected_authority_epoch: node.authority_epoch,
                    idempotency_key: Uuid::new_v4().to_string(),
                })
                .await
                .map_err(rpc_error)?;
            Ok(format!(
                "Portfolio node revoked: {}",
                node_summary(&revoked, &app.projects)
            ))
        }
    }
}

#[cfg(test)]
#[path = "portfolio_command_tests.rs"]
mod tests;

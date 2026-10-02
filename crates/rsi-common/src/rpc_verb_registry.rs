//! The single per-verb declaration for the attributed (session-token) RPC
//! surface (#1011).
//!
//! Every method a session-attributed caller may reach is declared exactly
//! once, with its audience:
//!
//! - `Agent` verbs are the descriptors of
//!   [`agent_control_catalog_v1`]: the descriptor carries the method name, the
//!   native tool (if any) and the CLI exposure (every catalogued verb is in
//!   the `rsi-rpc agent` listing).
//! - `Read`, `UnscopedRead` and `Hook` verbs are declared in
//!   [`NON_AGENT_ATTRIBUTED_VERBS`] below.
//!
//! Anything else is operator-only and default-denied for a tokened caller
//! (AGENTS.md hard rule 10). `rsid`'s `agent_gate` derives `AGENT_VERBS`,
//! `READ_VERBS`, `UNSCOPED_READ_VERBS` and `HOOK_VERBS` from this module;
//! nothing else lists these names.

use crate::agent_control_schema::agent_control_catalog_v1;
use std::sync::LazyLock;

/// Who may reach a declared method with a session token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RpcVerbAudience {
    /// Model-facing `Agent*` verb: native tool and `rsi-rpc agent` listing.
    Agent,
    /// Read-only verb that resolves the token caller and scopes its target
    /// (#241 `agent_read_scope_denied`).
    Read,
    /// Read-only verb that names no target session (a subset of reads).
    UnscopedRead,
    /// Only the session's own tool-boundary hook calls it (#1049); not part
    /// of the model-facing catalog, native tools or the CLI listing.
    Hook,
}

/// One declared attributed verb.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RpcVerbDeclaration {
    pub method: &'static str,
    pub audience: RpcVerbAudience,
    /// Native tool name, for agent verbs that have one.
    pub native_tool: Option<&'static str>,
    /// Listed by `rsi-rpc agent` (the CLI catalog).
    pub cli: bool,
}

const fn decl(method: &'static str, audience: RpcVerbAudience) -> RpcVerbDeclaration {
    RpcVerbDeclaration {
        method,
        audience,
        native_tool: None,
        cli: false,
    }
}

/// Attributed verbs that are not `Agent*` catalog verbs. Order is the order of
/// the derived lists: extend by adding an entry after review, never by widening
/// to a prefix match.
pub const NON_AGENT_ATTRIBUTED_VERBS: &[RpcVerbDeclaration] = &[
    decl("GetSession", RpcVerbAudience::Read),
    decl("GetSessionSummary", RpcVerbAudience::Read),
    decl("GetHealthStatus", RpcVerbAudience::UnscopedRead),
    decl("GetDaemonCapabilities", RpcVerbAudience::UnscopedRead),
    decl("ListSessionChildren", RpcVerbAudience::Read),
    decl("GetConversation", RpcVerbAudience::Read),
    decl("GetSessionDiagnostics", RpcVerbAudience::Read),
    decl("GetConversationsSince", RpcVerbAudience::Read),
    decl("GetTurnMetrics", RpcVerbAudience::Read),
    decl("ClaimBoundaryMail", RpcVerbAudience::Hook),
];

const fn bytes_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

const fn starts_with_agent(name: &str) -> bool {
    let n = name.as_bytes();
    n.len() >= 5 && n[0] == b'A' && n[1] == b'g' && n[2] == b'e' && n[3] == b'n' && n[4] == b't'
}

/// Build-time audience rule: a non-`Agent*` declaration never uses the
/// `Agent` prefix, names are unique, and only agent verbs carry native tools
/// or CLI exposure.
const _: () = {
    let table = NON_AGENT_ATTRIBUTED_VERBS;
    let mut i = 0;
    while i < table.len() {
        assert!(!starts_with_agent(table[i].method));
        assert!(!matches!(table[i].audience, RpcVerbAudience::Agent));
        assert!(table[i].native_tool.is_none());
        assert!(!table[i].cli);
        let mut j = i + 1;
        while j < table.len() {
            assert!(!bytes_eq(table[i].method, table[j].method));
            j += 1;
        }
        i += 1;
    }
};

static DECLARATIONS: LazyLock<Vec<RpcVerbDeclaration>> = LazyLock::new(|| {
    let mut all: Vec<RpcVerbDeclaration> = agent_control_catalog_v1()
        .iter()
        .map(|descriptor| RpcVerbDeclaration {
            method: descriptor.method,
            audience: RpcVerbAudience::Agent,
            native_tool: descriptor.native_tool.map(|tool| tool.name()),
            cli: true,
        })
        .collect();
    all.extend_from_slice(NON_AGENT_ATTRIBUTED_VERBS);
    all
});

/// Every declared attributed verb: the agent catalog first (catalog order),
/// then [`NON_AGENT_ATTRIBUTED_VERBS`].
#[must_use]
pub fn rpc_verb_declarations() -> &'static [RpcVerbDeclaration] {
    &DECLARATIONS
}

fn methods_for(pick: fn(RpcVerbAudience) -> bool) -> Vec<&'static str> {
    rpc_verb_declarations()
        .iter()
        .filter(|declaration| pick(declaration.audience))
        .map(|declaration| declaration.method)
        .collect()
}

static AGENT: LazyLock<Vec<&'static str>> =
    LazyLock::new(|| methods_for(|a| a == RpcVerbAudience::Agent));
static READ: LazyLock<Vec<&'static str>> = LazyLock::new(|| {
    methods_for(|a| matches!(a, RpcVerbAudience::Read | RpcVerbAudience::UnscopedRead))
});
static UNSCOPED_READ: LazyLock<Vec<&'static str>> =
    LazyLock::new(|| methods_for(|a| a == RpcVerbAudience::UnscopedRead));
static HOOK: LazyLock<Vec<&'static str>> =
    LazyLock::new(|| methods_for(|a| a == RpcVerbAudience::Hook));

/// `Agent*` verbs (the `AGENT_VERBS` gate list), in catalog order.
#[must_use]
pub fn agent_verb_methods() -> &'static [&'static str] {
    &AGENT
}

/// Read-only verbs safe for an attributed caller (the `READ_VERBS` gate list).
#[must_use]
pub fn read_verb_methods() -> &'static [&'static str] {
    &READ
}

/// The reads that name no target session (`UNSCOPED_READ_VERBS`).
#[must_use]
pub fn unscoped_read_verb_methods() -> &'static [&'static str] {
    &UNSCOPED_READ
}

/// Tool-boundary hook verbs (`HOOK_VERBS`).
#[must_use]
pub fn hook_verb_methods() -> &'static [&'static str] {
    &HOOK
}

/// Methods listed by the `rsi-rpc agent` CLI catalog, in declaration order.
#[must_use]
pub fn cli_verb_methods() -> Vec<&'static str> {
    rpc_verb_declarations()
        .iter()
        .filter(|declaration| declaration.cli)
        .map(|declaration| declaration.method)
        .collect()
}

/// Native tool names (provider tools, harness tools, MCP gateway), in
/// declaration order.
#[must_use]
pub fn native_tool_names() -> Vec<&'static str> {
    rpc_verb_declarations()
        .iter()
        .filter_map(|declaration| declaration.native_tool)
        .collect()
}

/// Declared audience of `method`; `None` means operator-only (default-deny).
#[must_use]
pub fn audience_of(method: &str) -> Option<RpcVerbAudience> {
    rpc_verb_declarations()
        .iter()
        .find(|declaration| declaration.method == method)
        .map(|declaration| declaration.audience)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manager_operator_delegation::{
        DELEGABLE_OPERATOR_METHODS, NEVER_DELEGABLE_OPERATOR_METHODS_V1,
    };
    use std::collections::BTreeSet;

    #[test]
    fn declared_methods_are_unique() {
        let all = rpc_verb_declarations();
        let unique: BTreeSet<_> = all.iter().map(|d| d.method).collect();
        assert_eq!(unique.len(), all.len());
    }

    #[test]
    fn operator_only_methods_are_never_agent_visible() {
        for method in DELEGABLE_OPERATOR_METHODS
            .iter()
            .chain(NEVER_DELEGABLE_OPERATOR_METHODS_V1)
        {
            assert_eq!(audience_of(method), None, "{method} must be operator-only");
            assert!(!agent_verb_methods().contains(method));
            assert!(!read_verb_methods().contains(method));
            assert!(!hook_verb_methods().contains(method));
        }
    }

    #[test]
    fn agent_visible_declarations_carry_the_catalog_exposure() {
        for declaration in rpc_verb_declarations() {
            match declaration.audience {
                RpcVerbAudience::Agent => {
                    assert!(declaration.method.starts_with("Agent"));
                    assert!(declaration.cli);
                }
                _ => {
                    assert!(!declaration.method.starts_with("Agent"));
                    assert!(declaration.native_tool.is_none() && !declaration.cli);
                }
            }
        }
        let native = rpc_verb_declarations()
            .iter()
            .filter(|d| d.native_tool.is_some())
            .count();
        assert_eq!(
            native,
            agent_control_catalog_v1()
                .iter()
                .filter(|d| d.native_tool.is_some())
                .count()
        );
    }

    #[test]
    fn unscoped_reads_are_a_subset_of_reads() {
        for method in unscoped_read_verb_methods() {
            assert!(read_verb_methods().contains(method), "{method}");
        }
    }

    #[test]
    fn cli_and_native_lists_derive_from_the_declarations() {
        let catalog = agent_control_catalog_v1();
        assert_eq!(
            cli_verb_methods(),
            catalog.iter().map(|d| d.method).collect::<Vec<_>>()
        );
        assert_eq!(
            cli_verb_methods(),
            agent_verb_methods(),
            "every agent verb is listed by the CLI and nothing else is"
        );
        assert_eq!(
            native_tool_names(),
            catalog
                .iter()
                .filter_map(|d| d.native_tool.map(|t| t.name()))
                .collect::<Vec<_>>()
        );
        for method in read_verb_methods().iter().chain(hook_verb_methods()) {
            assert!(!cli_verb_methods().contains(method), "{method}");
        }
    }
}

# Agent verb parameter names (#2313)

One name per concept across the `Agent*` verb set. The catalog schemas in
`crates/rsi-common/src/agent_control_schema.rs` are the source; the
`parameter_names` schema test (`agent_control_schema/parameter_names.rs`) fails
when a verb adds a session-identifying name that is not in the tables below.

## Canonical names

| Concept | Canonical | Verbs | Deprecated alias (one release) |
| --- | --- | --- | --- |
| Session the verb acts on (optional = this session) | `session_id` | `AgentGetStatus`, `AgentHalt`, `AgentListWakes`, `AgentReadSessionEvents` | none |
| Session the verb acts on (required child) | `session_id` | `AgentSendMessage`, `AgentContinueChild`, `AgentArchiveChild` | `target_session_id` |
| A set of sessions | `session_ids` (array) | `AgentGetProgress` | none |
| Session whose sandbox/branch is run in or landed | `source_session_id` | `AgentEnqueueLandingSource`, `AgentCheckLandingCandidate`, `AgentSubmitJob` | `sandbox_session_id` (`AgentSubmitJob`) |
| Worktree path to run in (managers) | `worktree` | `AgentSubmitJob` | none (a path, not a session) |

Plural parameters are `<concept>_ids` arrays; a singular `<concept>_id` is never
an array.

## Names that stay because they are a different concept

| Name | Verbs | Concept |
| --- | --- | --- |
| `expected_tip_session_id`, `expected_target_session_id` | `AgentContinueChild`, `AgentArchiveChild`, manager escalation verbs | Staleness fences: the value the caller last observed |
| `watch_session_id` | `AgentScheduleWake` | The session a wake watches |
| `remote_session_id` | satellite verbs | A satellite's own session id |
| `lead_session_id` | `AgentManagerControl` | The lead session of an Epic (selector) |
| `owner_manager_session_id` | `AgentManagerUpdate` | The manager session that owns a decision |
| `seat_root_session_id` | `AgentManagerDelegateNode` | Root session of a manager seat |

## Deprecation

The old names are accepted by the request DTOs (serde `alias`) and listed in
each verb's schema with `"deprecated": true` and a note naming the canonical
name (visible through `AgentGetAuthorityCatalog {verb}`). Sending both names is
a duplicate-field refusal. The canonical name is not in `required`: each pair is
a flat name group in the schema, `x-exactly-one-of` (the canonical name is
mandatory) or `x-at-most-one-of` (it is optional), so a schema-driven client
accepts an alias-only request and refuses both names (#2636). The groups are
`x-` keywords because provider tool schemas reject a top-level `oneOf`/`not`.
Remove an alias, its schema property and its
`DEPRECATED_PARAMETER_ALIASES` entry one release after it shipped.

Result and receipt objects keep their field names (`AgentContinueChild` and
`AgentArchiveChild` results still echo `target_session_id`); only request
parameters are in scope.

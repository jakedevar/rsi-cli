//! RSI Remote V1 observation wire. This module is deliberately unregistered in
//! rsi-common; remote/contracts/v1/rust compiles this exact file for qualification.
//! All inbound bytes must use `decode`; serde alone is not the wire boundary.
//! Validation proves neither authorization, source truth, CAS nor transport state.

use serde::{Deserialize, Deserializer, Serialize, de};
use serde_json::Value;
use std::{collections::BTreeSet, fmt};

pub const VERSION: &str = "1.0";
pub const MAX_ENVELOPE_BYTES: usize = 512 * 1024;
pub const MAX_REQUEST_BYTES: usize = 16 * 1024;
pub const MAX_ITEM_BYTES: usize = 64 * 1024;

/// Deliberately carries no input, path, field name or serde diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidWire;
impl fmt::Display for InvalidWire {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("invalid Remote V1 wire value")
    }
}
impl std::error::Error for InvalidWire {}
type Result<T> = std::result::Result<T, InvalidWire>;
fn require(ok: bool) -> Result<()> {
    if ok { Ok(()) } else { Err(InvalidWire) }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct Text<const MAX: usize, const NONEMPTY: bool = true>(String);
impl<const M: usize, const N: bool> Text<M, N> {
    pub fn new(value: String) -> Result<Self> {
        require(value.len() <= M && (!N || !value.is_empty()))?;
        Ok(Self(value))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl<'de, const M: usize, const N: bool> Deserialize<'de> for Text<M, N> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        Self::new(String::deserialize(d)?).map_err(de::Error::custom)
    }
}

macro_rules! scalar {
    ($name:ident, $check:expr) => {
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);
        impl $name {
            pub fn new(s: String) -> Result<Self> {
                require(($check)(&s))?;
                Ok(Self(s))
            }
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
                Self::new(String::deserialize(d)?).map_err(de::Error::custom)
            }
        }
    };
}
scalar!(DecimalU64, |s: &str| s
    .parse::<u64>()
    .is_ok_and(|n| n.to_string() == s));
scalar!(DecimalI64, |s: &str| s
    .parse::<i64>()
    .is_ok_and(|n| n.to_string() == s));
scalar!(WireUuid, |s: &str| uuid::Uuid::parse_str(s)
    .is_ok_and(|u| u.hyphenated().to_string() == s));
scalar!(Timestamp, |s: &str| {
    let b = s.as_bytes();
    (b.len() == 30 || b.len() == 35)
        && b.get(19) == Some(&b'.')
        && b[20..29].iter().all(u8::is_ascii_digit)
        && b.get(10) == Some(&b'T')
        && (b.get(29) == Some(&b'Z') || matches!(b.get(29), Some(b'+' | b'-')))
        && chrono::DateTime::parse_from_rfc3339(s).is_ok()
});
scalar!(DecisionId, |s: &str| s.len() <= 160
    && decision_identity(s).is_ok());
scalar!(OpaqueToken, |s: &str| !s.is_empty()
    && s.len() <= 2048
    && s.bytes().all(|b| b.is_ascii_alphanumeric()
        || b == b'-'
        || b == b'_'));

impl DecimalU64 {
    pub fn get(&self) -> u64 {
        self.0.parse().expect("validated decimal")
    }
}
impl DecimalI64 {
    pub fn get(&self) -> i64 {
        self.0.parse().expect("validated decimal")
    }
}

fn required<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    d: D,
) -> std::result::Result<T, D::Error> {
    T::deserialize(d)
}

// Serde's derived struct visitor also accepts positional sequences. The wire
// contract is map-only, including empty structs and nested response objects.
fn object_only<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    d: D,
) -> std::result::Result<T, D::Error> {
    struct Object<T>(std::marker::PhantomData<T>);
    impl<'de, T: Deserialize<'de>> de::Visitor<'de> for Object<T> {
        type Value = T;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a wire object")
        }
        fn visit_map<A: de::MapAccess<'de>>(self, map: A) -> std::result::Result<T, A::Error> {
            T::deserialize(de::value::MapAccessDeserializer::new(map))
        }
    }
    d.deserialize_map(Object(std::marker::PhantomData))
}

// deserialize_with makes nullable fields required too. Only explicitly marked
// defaults below may be omitted. Nested DTOs validate while being decoded.
macro_rules! wire {
    ($name:ident { $( $(#[$attr:meta])* $field:ident : $ty:ty ),* $(,)? } => $check:expr) => {
        #[derive(Debug, Clone, PartialEq, Eq, Serialize)]
        pub struct $name { $( pub $field: $ty, )* }
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Fields { $( $(#[$attr])* #[serde(deserialize_with = "required")] $field: $ty, )* }
                let fields: Fields = object_only(d)?;
                let _ = &fields;
                let v = Self { $( $field: fields.$field, )* };
                let checked: Result<()> = ($check)(&v);
                checked.map_err(de::Error::custom)?;
                Ok(v)
            }
        }
    };
}
// All tagged unions are objects. One declaration supplies the public enum and
// the internal derived visitor, restricted to map input for either tag layout.
macro_rules! object_enum {
    ($(#[$attr:meta])* pub enum $name:ident { $($variant:ident($ty:ty)),* $(,)? }) => {
        #[derive(Debug, Clone, PartialEq, Eq, Serialize)]
        $(#[$attr])* pub enum $name { $($variant($ty)),* }
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
                #[derive(Deserialize)]
                $(#[$attr])* enum Fields { $($variant($ty)),* }
                let fields: Fields = object_only(d)?;
                Ok(match fields { $(Fields::$variant(v) => Self::$variant(v)),* })
            }
        }
    };
    ($(#[$attr:meta])* pub enum $name:ident {
        $($variant:ident { $($(#[$field_attr:meta])* $field:ident: $ty:ty),* $(,)? }),* $(,)?
    }) => {
        #[derive(Debug, Clone, PartialEq, Eq, Serialize)]
        $(#[$attr])* pub enum $name {
            $($variant { $($(#[$field_attr])* $field: $ty),* }),*
        }
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
                #[derive(Deserialize)]
                $(#[$attr])* enum Fields {
                    $($variant { $($(#[$field_attr])* $field: $ty),* }),*
                }
                let fields: Fields = object_only(d)?;
                Ok(match fields { $(Fields::$variant { $($field),* } => Self::$variant { $($field),* }),* })
            }
        }
    };
}
macro_rules! labels {
    ($name:ident, $known:ident { $($label:ident),+ }) => {
        #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
        pub enum $known { $($label),+ }
        object_enum! {
            #[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
            pub enum $name { Known { value: $known }, Unknown { label: Text<128> } }
        }
        impl $name {
            pub fn display(&self) -> String {
                match self { Self::Known { value } => format!("{value:?}"), Self::Unknown { label } => format!("Unknown: {}", label.as_str()) }
            }
        }
    };
}
labels!(
    ProviderV1,
    KnownProviderV1 {
        Claude,
        Codex,
        Pioneer,
        OpenRouter,
        Bedrock,
        Local,
        Antigravity,
        CodexAppServer,
        Harness
    }
);
labels!(
    SessionKindV1,
    KnownSessionKindV1 {
        Standard,
        TaskRabbit,
        Bug,
        Group,
        Epic,
        Story,
        Task,
        Feature,
        Refactor,
        Research
    }
);
labels!(
    SessionStatusV1,
    KnownSessionStatusV1 {
        Starting,
        Running,
        WaitingApproval,
        Completed,
        Failed,
        Interrupted,
        Archived,
        Deleted
    }
);
labels!(
    EventKindV1,
    KnownEventKindV1 {
        Message,
        ToolUse,
        ToolResult,
        System,
        Thinking,
        Compressed
    }
);
labels!(RoleV1, KnownRoleV1 { User, Assistant });
#[allow(non_camel_case_types)]
mod publication_labels {
    use super::*;
    labels!(
        PublicationStateV1,
        KnownPublicationStateV1 {
            unresolved,
            published,
            cleared,
            enqueued,
            expired,
            superseded,
            Pending,
            Approved,
            Denied
        }
    );
}
pub use publication_labels::{KnownPublicationStateV1, PublicationStateV1};

macro_rules! closed {
    ($name:ident { $($variant:ident),+ }) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum $name { $($variant),+ }
    };
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ReadMethodV1 {
    RemoteGetInfoV1,
    RemoteListProjectsV1,
    RemoteListSessionsV1,
    RemoteGetSessionV1,
    RemoteGetHistoryPageV1,
    RemoteGetDecisionsV1,
}
pub const REQUIRED_CAPABILITIES: [ReadMethodV1; 6] = [
    ReadMethodV1::RemoteGetInfoV1,
    ReadMethodV1::RemoteListProjectsV1,
    ReadMethodV1::RemoteListSessionsV1,
    ReadMethodV1::RemoteGetSessionV1,
    ReadMethodV1::RemoteGetHistoryPageV1,
    ReadMethodV1::RemoteGetDecisionsV1,
];
closed!(CoverageStateV1 {
    Complete,
    Busy,
    Limited,
    Unavailable
});
closed!(SourceV1 {
    ConfiguredProjects,
    StoreProjects,
    ActiveSessions,
    CompletedSessions,
    StoreSessions,
    StoreHistory,
    QuestionPublications,
    TrackedQuestionSlot,
    SessionQuestionSlot,
    DurableQuestionFallback,
    NativeRuntime,
    NativePublications,
    NativeHistoricalFallback,
    LegacyApprovals
});
closed!(DegradationV1 {
    Busy,
    Limited,
    Unavailable,
    Disagreement,
    Truncated,
    Stale,
    SourceChanged,
    HistoryChanged
});
closed!(PreviewStateV1 {
    Complete,
    Truncated,
    Unavailable
});
closed!(SourceExtentV1 {
    FullField,
    BoundedSnapshot,
    Unknown
});
closed!(DisplaySourceFieldV1 {
    Method,
    Description,
    ToolName,
    Query,
    Model
});
closed!(PairingStateV1 {
    Exact,
    Ambiguous,
    Oversized,
    Missing
});
closed!(ContentStateV1 {
    Complete,
    Preview,
    Offloaded,
    Unavailable
});
closed!(IdentityClassV1 {
    Publication,
    Slot,
    Legacy
});
closed!(DecisionKindV1 {
    GenericQuestions,
    NativeApproval,
    LegacyApproval
});
closed!(ClosureStateV1 {
    Open,
    Closed,
    Ambiguous,
    Unknown
});
closed!(DeliveryStateV1 { Enqueued, Unknown });
closed!(DecisionModeV1 {
    Attention,
    Retained
});
closed!(CursorKindV1 {
    Projects,
    Sessions,
    History,
    Decisions
});
closed!(ErrorCodeV1 {
    InvalidRequest,
    NotFound,
    StaleCursor,
    Admission,
    ResourceLimit,
    Busy,
    SourceUnavailable,
    UpgradeRequired,
    AuthRequired,
    AccessDenied,
    SelectionRequired,
    SelectionUnavailable,
    Conflict
});
closed!(RetryActionV1 {
    None,
    Retry,
    Reauthenticate,
    Rediscover,
    Resync,
    Relocate,
    Upgrade
});
closed!(CacheStatusV1 {
    Resident,
    Refetched,
    Stale
});
closed!(ViewSlotV1 {
    Info,
    Projects,
    Sessions,
    Detail,
    Decisions,
    History,
    Tail,
    Foreground
});
closed!(LeaseStateV1 {
    Allocation,
    Attached,
    Disconnected
});
closed!(ResetReasonV1 {
    AnchorMissing,
    CursorExpired,
    CatchupGap,
    EpochChanged,
    ReplayLost,
    ResourceLimit
});

object_enum! {
    #[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
    pub enum CursorV1 {
        Projects { token: OpaqueToken },
        Sessions { token: OpaqueToken },
        History { token: OpaqueToken },
        Decisions { token: OpaqueToken },
    }
}
impl CursorV1 {
    pub fn kind(&self) -> CursorKindV1 {
        match self {
            Self::Projects { .. } => CursorKindV1::Projects,
            Self::Sessions { .. } => CursorKindV1::Sessions,
            Self::History { .. } => CursorKindV1::History,
            Self::Decisions { .. } => CursorKindV1::Decisions,
        }
    }
}
wire!(EventKeyV1 { sequence: i32, id: DecimalI64 } => |v: &EventKeyV1| require(v.id.get() > 0));
impl EventKeyV1 {
    fn key(&self) -> (i32, i64) {
        (self.sequence, self.id.get())
    }
}
object_enum! {
    #[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
    pub enum HistoryWindowV1 {
        Latest {},
        Older {
            anchor: EventKeyV1,
        },
        Newer {
            anchor: EventKeyV1,
            #[serde(default)]
            through: Option<EventKeyV1>,
        },
        Interval {
            lower_exclusive: EventKeyV1,
            upper_inclusive: EventKeyV1,
        },
        Locate {
            event_id: DecimalI64,
            old_key: EventKeyV1,
            offset: u32,
        },
    }
}
impl HistoryWindowV1 {
    fn validate(&self) -> Result<()> {
        match self {
            Self::Newer {
                anchor,
                through: Some(through),
            } => require(anchor.key() < through.key()),
            Self::Interval {
                lower_exclusive,
                upper_inclusive,
            } => require(lower_exclusive.key() < upper_inclusive.key()),
            Self::Locate {
                event_id,
                old_key,
                offset,
            } => require(event_id == &old_key.id && *offset <= 8192),
            _ => Ok(()),
        }
    }
}
fn limit25() -> u32 {
    25
}
fn limit50() -> u32 {
    50
}
fn limit16() -> u32 {
    16
}
fn attention() -> DecisionModeV1 {
    DecisionModeV1::Attention
}
fn cursor_kind(cursor: &Option<CursorV1>, kind: CursorKindV1) -> bool {
    cursor.as_ref().is_none_or(|c| c.kind() == kind)
}
fn unique<T: Ord>(items: impl IntoIterator<Item = T>) -> bool {
    let mut set = BTreeSet::new();
    items.into_iter().all(|v| set.insert(v))
}

wire!(RemoteGetInfoV1 { } => |_: &RemoteGetInfoV1| Ok(()));
wire!(RemoteListProjectsV1 {
    project_ids: Vec<WireUuid>, #[serde(default = "limit25")] limit: u32, #[serde(default)] cursor: Option<CursorV1>
} => |v: &RemoteListProjectsV1| require(v.project_ids.len() <= 32 && unique(v.project_ids.iter()) && (1..=50).contains(&v.limit) && cursor_kind(&v.cursor, CursorKindV1::Projects)));
wire!(RemoteListSessionsV1 {
    project_id: WireUuid, #[serde(default = "limit50")] limit: u32, #[serde(default)] cursor: Option<CursorV1>
} => |v: &RemoteListSessionsV1| require((1..=100).contains(&v.limit) && cursor_kind(&v.cursor, CursorKindV1::Sessions)));
wire!(RemoteGetSessionV1 { project_id: WireUuid, session_id: WireUuid } => |_: &RemoteGetSessionV1| Ok(()));
wire!(RemoteGetHistoryPageV1 {
    project_id: WireUuid, session_id: WireUuid, window: HistoryWindowV1,
    #[serde(default = "limit25")] limit: u32, #[serde(default)] cursor: Option<CursorV1>
} => |v: &RemoteGetHistoryPageV1| { require((1..=50).contains(&v.limit) && cursor_kind(&v.cursor, CursorKindV1::History))?; v.window.validate() });
wire!(RemoteGetDecisionsV1 {
    project_id: WireUuid, session_id: WireUuid, #[serde(default = "limit16")] limit: u32,
    #[serde(default = "attention")] mode: DecisionModeV1, #[serde(default)] cursor: Option<CursorV1>,
    #[serde(default)] selected_decision_id: Option<DecisionId>
} => |v: &RemoteGetDecisionsV1| { require((1..=32).contains(&v.limit) && cursor_kind(&v.cursor, CursorKindV1::Decisions))?; if let Some(id) = &v.selected_decision_id { decision_scope(id, &v.session_id)?; } Ok(()) });

object_enum! {
    #[serde(tag = "method", content = "params", deny_unknown_fields)]
    pub enum ReadRequestV1 {
        RemoteGetInfoV1(RemoteGetInfoV1),
        RemoteListProjectsV1(RemoteListProjectsV1),
        RemoteListSessionsV1(RemoteListSessionsV1),
        RemoteGetSessionV1(RemoteGetSessionV1),
        RemoteGetHistoryPageV1(RemoteGetHistoryPageV1),
        RemoteGetDecisionsV1(RemoteGetDecisionsV1),
    }
}

// Public discovery has no project-ID prerequisite. Only the future trusted
// gateway supplies its configured IDs to RemoteListProjectsV1 at the source.
wire!(ProjectsDiscoveryV1 {
    #[serde(default = "limit25")] limit: u32, #[serde(default)] cursor: Option<CursorV1>
} => |v: &ProjectsDiscoveryV1| require((1..=50).contains(&v.limit) && cursor_kind(&v.cursor, CursorKindV1::Projects)));
object_enum! {
    #[serde(tag = "method", content = "params", deny_unknown_fields)]
    pub enum ViewReadRequestV1 {
        RemoteGetInfoV1(RemoteGetInfoV1),
        RemoteListProjectsV1(ProjectsDiscoveryV1),
        RemoteListSessionsV1(RemoteListSessionsV1),
        RemoteGetSessionV1(RemoteGetSessionV1),
        RemoteGetHistoryPageV1(RemoteGetHistoryPageV1),
        RemoteGetDecisionsV1(RemoteGetDecisionsV1),
    }
}
impl ViewReadRequestV1 {
    fn object_or_info_request(&self) -> Option<ReadRequestV1> {
        Some(match self {
            Self::RemoteGetInfoV1(v) => ReadRequestV1::RemoteGetInfoV1(v.clone()),
            Self::RemoteListProjectsV1(_) => return None,
            Self::RemoteListSessionsV1(v) => ReadRequestV1::RemoteListSessionsV1(v.clone()),
            Self::RemoteGetSessionV1(v) => ReadRequestV1::RemoteGetSessionV1(v.clone()),
            Self::RemoteGetHistoryPageV1(v) => ReadRequestV1::RemoteGetHistoryPageV1(v.clone()),
            Self::RemoteGetDecisionsV1(v) => ReadRequestV1::RemoteGetDecisionsV1(v.clone()),
        })
    }
}

wire!(ProjectionLimitsV1 {
    page_items: u32, name_bytes: u32, event_text_bytes: u32, decision_text_bytes: u32, item_bytes: u32, envelope_bytes: u32
} => |v: &ProjectionLimitsV1| require((1..=100).contains(&v.page_items) && (1..=4096).contains(&v.name_bytes) && (1..=8192).contains(&v.event_text_bytes) && (1..=8192).contains(&v.decision_text_bytes) && (1..=65536).contains(&v.item_bytes) && (1..=524288).contains(&v.envelope_bytes)));
wire!(SourceCoverageV1 {
    source: SourceV1, state: CoverageStateV1, has_more: bool, lower_bound: DecimalU64, observed_at: Timestamp, observation_order: u32
} => |v: &SourceCoverageV1| require((1..=32).contains(&v.observation_order)));
wire!(ObservationV1 {
    version: Text<3>, daemon_epoch: WireUuid, observed_at: Timestamp, next_cursor: Option<CursorV1>,
    complete: bool, projection_limits: ProjectionLimitsV1, degraded: Vec<DegradationV1>, coverage: Vec<SourceCoverageV1>
} => |v: &ObservationV1| {
    require(v.version.as_str() == VERSION && v.coverage.len() <= 14 && v.degraded.len() <= 8 && unique(v.degraded.iter())
        && unique(v.coverage.iter().map(|c| c.source)) && unique(v.coverage.iter().map(|c| c.observation_order)))?;
    let incomplete = v.coverage.iter().any(|c| c.state != CoverageStateV1::Complete);
    require(!v.complete || !incomplete)?;
    require(!v.complete || v.next_cursor.is_some() == v.coverage.iter().any(|c| c.has_more))?;
    for c in &v.coverage {
        let reason = match c.state { CoverageStateV1::Busy => Some(DegradationV1::Busy), CoverageStateV1::Limited => Some(DegradationV1::Limited), CoverageStateV1::Unavailable => Some(DegradationV1::Unavailable), CoverageStateV1::Complete => None };
        if let Some(reason) = reason { require(v.degraded.contains(&reason))?; }
    }
    require(v.complete || !v.degraded.is_empty())
});
wire!(ProjectV1 { id: WireUuid, name: Text<512> } => |_: &ProjectV1| Ok(()));
wire!(AttentionV1 { requires_local_action: bool, incomplete: bool, live_signals_lower_bound: DecimalU64 } => |v: &AttentionV1| require((!v.incomplete && v.live_signals_lower_bound.get() == 0) || v.requires_local_action));
wire!(SessionSummaryV1 {
    id: WireUuid, project_id: WireUuid, parent_id: Option<WireUuid>, continued_from: Option<WireUuid>, kind: SessionKindV1,
    own_title: Text<512>, provider: ProviderV1, status: SessionStatusV1, updated_at: Timestamp, attention: AttentionV1
} => |v: &SessionSummaryV1| require(v.parent_id.as_ref() != Some(&v.id) && v.continued_from.as_ref() != Some(&v.id)));
wire!(DisplayFieldV1 {
    text: Text<4096>, state: PreviewStateV1, source_field: DisplaySourceFieldV1, source_extent: SourceExtentV1,
    #[serde(default)] observed_bytes: Option<DecimalU64>
} => |v: &DisplayFieldV1| {
    let max = match v.source_field { DisplaySourceFieldV1::Method | DisplaySourceFieldV1::ToolName => 512, DisplaySourceFieldV1::Description => 2048, _ => 4096 };
    require(v.text.as_str().len() <= max)?;
    if let Some(bytes) = &v.observed_bytes {
        if v.state != PreviewStateV1::Unavailable {
            require(bytes.get() >= v.text.as_str().len() as u64)?;
            if v.state == PreviewStateV1::Complete { require(bytes.get() == v.text.as_str().len() as u64)?; }
        }
    }
    if v.state == PreviewStateV1::Unavailable { require(v.text.as_str() == unavailable_label(v.source_field))?; }
    Ok(())
});
pub fn unavailable_label(field: DisplaySourceFieldV1) -> &'static str {
    match field {
        DisplaySourceFieldV1::Method => "Native approval — method unavailable",
        DisplaySourceFieldV1::Description => "Description unavailable",
        DisplaySourceFieldV1::ToolName => "Legacy approval — tool name unavailable",
        DisplaySourceFieldV1::Query => "Query unavailable",
        DisplaySourceFieldV1::Model => "Model unavailable",
    }
}
impl DisplayFieldV1 {
    pub fn notices(&self) -> Vec<&'static str> {
        let mut result = Vec::new();
        if self.state == PreviewStateV1::Truncated {
            result.push("Truncated preview");
        }
        if self.source_extent == SourceExtentV1::BoundedSnapshot {
            result.push("Source-provided preview");
        }
        result
    }
}
wire!(SequenceObservationV1 { source: SourceV1, sequence: Option<i32>, event_id: Option<DecimalI64> } => |v: &SequenceObservationV1| require(matches!(v.source, SourceV1::ActiveSessions | SourceV1::CompletedSessions | SourceV1::StoreHistory) && v.event_id.as_ref().is_none_or(|id| id.get() > 0)));
wire!(SessionDetailV1 {
    summary: SessionSummaryV1, own_title: Text<4096>, query: DisplayFieldV1, model: DisplayFieldV1,
    sequences: Vec<SequenceObservationV1>, pending_coverage: Vec<SourceCoverageV1>
} => |v: &SessionDetailV1| require(v.own_title.as_str().starts_with(v.summary.own_title.as_str()) && v.query.source_field == DisplaySourceFieldV1::Query && v.model.source_field == DisplaySourceFieldV1::Model && v.sequences.len() <= 3 && unique(v.sequences.iter().map(|s| s.source)) && pending_coverage(&v.pending_coverage)));

wire!(HistoryEventV1 {
    id: DecimalI64, sequence: i32, kind: EventKindV1, role: Option<RoleV1>, created_at: Timestamp,
    text: Text<8192, false>, content_bytes: DecimalU64, truncated: bool, tool_name: Option<Text<512>>,
    tool_pair_key: Option<Text<256>>, tool_id_display: Option<Text<256>>, pairing_state: PairingStateV1, content_state: ContentStateV1
} => |v: &HistoryEventV1| {
    require(v.id.get() > 0)?;
    if v.content_state == ContentStateV1::Complete { require(!v.truncated && v.content_bytes.get() == v.text.as_str().len() as u64)?; }
    if v.content_state == ContentStateV1::Preview { require(v.truncated && v.content_bytes.get() >= v.text.as_str().len() as u64)?; }
    match v.pairing_state {
        PairingStateV1::Exact | PairingStateV1::Ambiguous => require(v.tool_pair_key.is_some() && v.tool_pair_key == v.tool_id_display),
        PairingStateV1::Oversized => require(v.tool_pair_key.is_none() && v.tool_id_display.is_some()),
        PairingStateV1::Missing => require(v.tool_pair_key.is_none() && v.tool_id_display.is_none()),
    }
});
object_enum! {
    #[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
    pub enum HistoryPositionV1 {
        Unchanged {},
        Relocated {
            event_id: DecimalI64,
            old_key: EventKeyV1,
            new_key: EventKeyV1,
            offset: u32,
        },
        Reset {
            #[serde(deserialize_with = "required")]
            anchor: Option<EventKeyV1>,
            reason: ResetReasonV1,
        },
    }
}
wire!(HistoryIntervalV1 { lower_exclusive: Option<EventKeyV1>, upper_inclusive: Option<EventKeyV1> } => |v: &HistoryIntervalV1| require(match (&v.lower_exclusive, &v.upper_inclusive) { (Some(l), Some(u)) => l.key() < u.key(), (Some(_), None) => false, _ => true }));

fn decision_identity(s: &str) -> Result<(IdentityClassV1, DecisionKindV1)> {
    let parts: Vec<_> = s.split(':').collect();
    let uuid = |p: &str| WireUuid::new(p.to_owned()).is_ok();
    match parts.as_slice() {
        ["question", id] if uuid(id) => Ok((
            IdentityClassV1::Publication,
            DecisionKindV1::GenericQuestions,
        )),
        ["native", id] if uuid(id) => {
            Ok((IdentityClassV1::Publication, DecisionKindV1::NativeApproval))
        }
        ["legacy", id] if uuid(id) => Ok((IdentityClassV1::Legacy, DecisionKindV1::LegacyApproval)),
        ["question-fallback", id] if uuid(id) => {
            Ok((IdentityClassV1::Slot, DecisionKindV1::GenericQuestions))
        }
        ["question-slot", id, generation, mirror]
            if uuid(id)
                && (*generation == "completed"
                    || DecimalU64::new((*generation).to_owned()).is_ok())
                && matches!(*mirror, "tracked" | "session") =>
        {
            Ok((IdentityClassV1::Slot, DecisionKindV1::GenericQuestions))
        }
        _ => Err(InvalidWire),
    }
}
object_enum! {
    #[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
    pub enum DecisionDisplayV1 {
        GenericQuestions {},
        NativeApproval {
            method: DisplayFieldV1,
            description: DisplayFieldV1,
        },
        LegacyApproval {
            tool_name: DisplayFieldV1,
        },
    }
}
impl DecisionDisplayV1 {
    fn kind(&self) -> DecisionKindV1 {
        match self {
            Self::GenericQuestions { .. } => DecisionKindV1::GenericQuestions,
            Self::NativeApproval { .. } => DecisionKindV1::NativeApproval,
            Self::LegacyApproval { .. } => DecisionKindV1::LegacyApproval,
        }
    }
    fn validate(&self) -> Result<()> {
        require(match self {
            Self::GenericQuestions { .. } => true,
            Self::NativeApproval {
                method,
                description,
            } => native_display(method, description),
            Self::LegacyApproval { tool_name } => {
                tool_name.source_field == DisplaySourceFieldV1::ToolName
            }
        })
    }
}
fn native_display(method: &DisplayFieldV1, description: &DisplayFieldV1) -> bool {
    method.source_field == DisplaySourceFieldV1::Method
        && description.source_field == DisplaySourceFieldV1::Description
        && method.source_extent == SourceExtentV1::BoundedSnapshot
        && description.source_extent == SourceExtentV1::BoundedSnapshot
}
wire!(QuestionOptionV1 { label: Text<128>, description: Text<512, false> } => |_: &QuestionOptionV1| Ok(()));
wire!(QuestionV1 {
    header: Text<128, false>, question: Text<2048>, options: Vec<QuestionOptionV1>, multi_select: bool, omitted_options: DecimalU64
} => |v: &QuestionV1| require(v.options.len() <= 8));
fn question_projection(
    questions: &[QuestionV1],
    omitted: &DecimalU64,
    state: PreviewStateV1,
) -> Result<()> {
    require(
        questions.len() <= 8 && (state == PreviewStateV1::Unavailable || !questions.is_empty()),
    )?;
    if state == PreviewStateV1::Complete {
        require(omitted.get() == 0 && questions.iter().all(|q| q.omitted_options.get() == 0))?;
    }
    Ok(())
}

// Retained generic displays carry their last bounded question projection. Live
// summaries keep questions in their existing field, without a duplicate copy.
object_enum! {
    #[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
    pub enum RetainedDecisionDisplayV1 {
        GenericQuestions {
            questions: Vec<QuestionV1>,
            omitted_questions: DecimalU64,
            details_state: PreviewStateV1,
        },
        NativeApproval {
            method: DisplayFieldV1,
            description: DisplayFieldV1,
        },
        LegacyApproval {
            tool_name: DisplayFieldV1,
        },
    }
}
impl RetainedDecisionDisplayV1 {
    fn kind(&self) -> DecisionKindV1 {
        match self {
            Self::GenericQuestions { .. } => DecisionKindV1::GenericQuestions,
            Self::NativeApproval { .. } => DecisionKindV1::NativeApproval,
            Self::LegacyApproval { .. } => DecisionKindV1::LegacyApproval,
        }
    }
    fn validate(&self) -> Result<()> {
        match self {
            Self::GenericQuestions {
                questions,
                omitted_questions,
                details_state,
            } => {
                question_projection(questions, omitted_questions, *details_state)?;
            }
            Self::NativeApproval {
                method,
                description,
            } => require(native_display(method, description))?,
            Self::LegacyApproval { tool_name } => {
                require(tool_name.source_field == DisplaySourceFieldV1::ToolName)?
            }
        }
        require(text_bytes(&serde_json::to_value(self).map_err(|_| InvalidWire)?) <= 8192)
    }
}
fn decision_source(s: SourceV1) -> bool {
    matches!(
        s,
        SourceV1::QuestionPublications
            | SourceV1::TrackedQuestionSlot
            | SourceV1::SessionQuestionSlot
            | SourceV1::DurableQuestionFallback
            | SourceV1::NativeRuntime
            | SourceV1::NativePublications
            | SourceV1::NativeHistoricalFallback
            | SourceV1::LegacyApprovals
    )
}
fn pending_coverage(c: &[SourceCoverageV1]) -> bool {
    c.len() <= 8
        && unique(c.iter().map(|v| v.source))
        && unique(c.iter().map(|v| v.observation_order))
        && c.iter().all(|v| decision_source(v.source))
}
wire!(DecisionSourceObservationV1 {
    source: SourceV1, observed_at: Timestamp, state: CoverageStateV1, incarnation: Option<WireUuid>, spawn_generation: Option<DecimalU64>,
    witness_present: Option<bool>, resolution_observed: Option<bool>, resolution_persisted: Option<bool>, writer_live: Option<bool>, writer_capacity: Option<u32>,
    display_alternative: Option<DecisionDisplayV1>
} => |v: &DecisionSourceObservationV1| {
    require(decision_source(v.source) && v.writer_capacity.is_none_or(|n| n <= 64))?;
    if let Some(d) = &v.display_alternative { d.validate()?; }
    Ok(())
});
wire!(DecisionSummaryV1 {
    id: DecisionId, identity_class: IdentityClassV1, kind: DecisionKindV1, publication_state: Option<PublicationStateV1>,
    closure_state: ClosureStateV1, delivery_state: DeliveryStateV1, source_observations: Vec<DecisionSourceObservationV1>, omitted_source_observations: DecimalU64,
    disagreement: bool, display: DecisionDisplayV1, questions: Vec<QuestionV1>, omitted_questions: DecimalU64,
    details_state: PreviewStateV1, requires_local_action: bool, can_answer: bool
} => |v: &DecisionSummaryV1| {
    require(decision_identity(v.id.as_str())? == (v.identity_class, v.kind) && v.display.kind() == v.kind && !v.can_answer
        && (1..=8).contains(&v.source_observations.len()) && unique(v.source_observations.iter().map(|s| s.source)) && v.questions.len() <= 8)?;
    v.display.validate()?;
    validate_decision_sources(v)?;
    validate_publication(v)?;
    if v.kind != DecisionKindV1::GenericQuestions { require(v.questions.is_empty() && v.omitted_questions.get() == 0)?; }
    if v.kind == DecisionKindV1::GenericQuestions { question_projection(&v.questions, &v.omitted_questions, v.details_state)?; }
    if v.details_state == PreviewStateV1::Complete { require(v.omitted_questions.get() == 0 && v.omitted_source_observations.get() == 0 && v.questions.iter().all(|q| q.omitted_options.get() == 0))?; }
    if matches!(v.closure_state, ClosureStateV1::Open | ClosureStateV1::Ambiguous | ClosureStateV1::Unknown) || v.disagreement || v.details_state != PreviewStateV1::Complete || v.source_observations.iter().any(|s| s.state != CoverageStateV1::Complete) { require(v.requires_local_action)?; }
    for s in &v.source_observations {
        if let Some(d) = &s.display_alternative { require(v.disagreement && d.kind() == v.kind)?; }
    }
    let value = serde_json::to_value(v).map_err(|_| InvalidWire)?;
    require(text_bytes(&value) <= 8192)
});
object_enum! {
    #[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
    pub enum SelectedDecisionV1 {
        None {},
        Present {
            decision: Box<DecisionSummaryV1>,
            stale: bool,
        },
        Tombstone {
            id: DecisionId,
            identity_class: IdentityClassV1,
            #[serde(deserialize_with = "required")]
            last_display: Option<RetainedDecisionDisplayV1>,
            message: Text<64>,
        },
        Unavailable {
            id: DecisionId,
            #[serde(deserialize_with = "required")]
            last_display: Option<RetainedDecisionDisplayV1>,
            reason: DegradationV1,
        },
    }
}
impl SelectedDecisionV1 {
    fn id(&self) -> Option<&DecisionId> {
        match self {
            Self::None { .. } => None,
            Self::Present { decision, .. } => Some(&decision.id),
            Self::Tombstone { id, .. } | Self::Unavailable { id, .. } => Some(id),
        }
    }
    fn validate(&self) -> Result<()> {
        match self {
            Self::Tombstone {
                id,
                identity_class,
                last_display,
                message,
            } => {
                require(
                    decision_identity(id.as_str())?.0 == *identity_class
                        && message.as_str() == "Decision no longer available",
                )?;
                if let Some(d) = last_display {
                    d.validate()?;
                    require(d.kind() == decision_identity(id.as_str())?.1)?;
                }
            }
            Self::Unavailable {
                id,
                last_display: Some(d),
                ..
            } => {
                d.validate()?;
                require(d.kind() == decision_identity(id.as_str())?.1)?;
            }
            _ => (),
        }
        Ok(())
    }
}

wire!(InfoV1 { protocol: Text<3>, daemon_boot_id: WireUuid, required_capabilities: Vec<ReadMethodV1> } => |v: &InfoV1| require(v.protocol.as_str() == VERSION && v.required_capabilities == REQUIRED_CAPABILITIES));
// Flatten at the type declaration, so every envelope has the exact top-level
// observation fields and unknown fields remain rejected (no serde flatten).
macro_rules! envelope {
    ($name:ident { $($field:ident: $ty:ty),* $(,)? } => $check:expr) => {
        wire!($name {
            version: Text<3>, daemon_epoch: WireUuid, observed_at: Timestamp,
            next_cursor: Option<CursorV1>, complete: bool, projection_limits: ProjectionLimitsV1,
            degraded: Vec<DegradationV1>, coverage: Vec<SourceCoverageV1>, $($field: $ty),*
        } => |v: &$name| {
            let o = v.observation();
            // The common observation constructor is validated through its codec.
            serde_json::from_value::<ObservationV1>(serde_json::to_value(&o).map_err(|_| InvalidWire)?).map_err(|_| InvalidWire)?;
            ($check)(v)
        });
        impl $name {
            pub fn observation(&self) -> ObservationV1 {
                ObservationV1 { version: self.version.clone(), daemon_epoch: self.daemon_epoch.clone(), observed_at: self.observed_at.clone(), next_cursor: self.next_cursor.clone(), complete: self.complete, projection_limits: self.projection_limits.clone(), degraded: self.degraded.clone(), coverage: self.coverage.clone() }
            }
        }
    };
}
envelope!(InfoResponseV1 { item: InfoV1 } => |v: &InfoResponseV1| require(v.daemon_epoch == v.item.daemon_boot_id && v.next_cursor.is_none() && v.complete));
envelope!(ProjectsResponseV1 { items: Vec<ProjectV1> } => |v: &ProjectsResponseV1| {
    page(&v.observation(), v.items.len(), 50, CursorKindV1::Projects)?;
    require(v.items.windows(2).all(|p| p[0].id < p[1].id))
});
envelope!(SessionsResponseV1 { project: ProjectV1, items: Vec<SessionSummaryV1> } => |v: &SessionsResponseV1| {
    page(&v.observation(), v.items.len(), 100, CursorKindV1::Sessions)?;
    require(v.items.windows(2).all(|p| p[0].id < p[1].id) && v.items.iter().all(|s| s.project_id == v.project.id))
});
envelope!(SessionResponseV1 { item: SessionDetailV1 } => |v: &SessionResponseV1| {
    require(v.next_cursor.is_none())?;
    let observation = v.observation();
    for c in &v.item.pending_coverage { current_coverage(&observation, c.state)?; }
    if v.item.pending_coverage.iter().any(|s| s.state != CoverageStateV1::Complete) {
        require(v.item.summary.attention.incomplete && v.item.summary.attention.requires_local_action)?;
    }
    Ok(())
});
envelope!(HistoryResponseV1 {
    project_id: WireUuid, session_id: WireUuid, items: Vec<HistoryEventV1>, window: HistoryWindowV1,
    head: Option<EventKeyV1>, interval: HistoryIntervalV1, position: HistoryPositionV1
} => |v: &HistoryResponseV1| {
    page(&v.observation(), v.items.len(), 50, CursorKindV1::History)?;
    v.window.validate()?;
    require(unique(v.items.iter().map(|e| &e.id)) && v.items.windows(2).all(|p| (p[0].sequence, p[0].id.get()) < (p[1].sequence, p[1].id.get())))?;
    for e in &v.items {
        let key = (e.sequence, e.id.get());
        require(v.head.as_ref().is_some_and(|h| key <= h.key()) && v.interval.upper_inclusive.as_ref().is_some_and(|u| key <= u.key()) && v.interval.lower_exclusive.as_ref().is_none_or(|l| key > l.key()))?;
        if matches!(v.position, HistoryPositionV1::Unchanged { .. }) {
            require(match &v.window {
                HistoryWindowV1::Older { anchor } => key < anchor.key(),
                HistoryWindowV1::Newer { anchor, through } => key > anchor.key() && through.as_ref().is_none_or(|t| key <= t.key()),
                HistoryWindowV1::Interval { lower_exclusive, upper_inclusive } => key > lower_exclusive.key() && key <= upper_inclusive.key(), _ => true
            })?;
        }
    }
    if let HistoryPositionV1::Relocated { event_id, old_key, new_key, offset } = &v.position {
        require(event_id == &old_key.id && event_id == &new_key.id && old_key != new_key && *offset <= 8192 && v.items.iter().any(|e| &e.id == event_id && e.sequence == new_key.sequence && *offset as usize <= e.text.as_str().len() && e.text.as_str().is_char_boundary(*offset as usize)))?;
    }
    locate_result(&v.window, &v.position, &v.items)
});
envelope!(DecisionsResponseV1 {
    project_id: WireUuid, session_id: WireUuid, mode: DecisionModeV1, items: Vec<DecisionSummaryV1>, selected: SelectedDecisionV1
} => |v: &DecisionsResponseV1| {
    let observation = v.observation();
    page(&observation, v.items.len(), 32, CursorKindV1::Decisions)?; require(unique(v.items.iter().map(|d| &d.id)))?;
    if let SelectedDecisionV1::Unavailable { reason, .. } = &v.selected { require(!v.complete && v.degraded.contains(reason) && matches!(reason, DegradationV1::Busy | DegradationV1::Limited | DegradationV1::Unavailable | DegradationV1::SourceChanged))?; }
    for d in v.items.iter().chain(match &v.selected {
        SelectedDecisionV1::Present { decision, stale: false } => Some(decision.as_ref()), _ => None,
    }) {
        for s in &d.source_observations { current_coverage(&observation, s.state)?; }
    }
    if matches!(v.selected, SelectedDecisionV1::Present { stale: true, .. }) { require(v.degraded.contains(&DegradationV1::Stale))?; }
    v.selected.validate()
});
fn current_coverage(o: &ObservationV1, state: CoverageStateV1) -> Result<()> {
    let reason = match state {
        CoverageStateV1::Complete => return Ok(()),
        CoverageStateV1::Busy => DegradationV1::Busy,
        CoverageStateV1::Limited => DegradationV1::Limited,
        CoverageStateV1::Unavailable => DegradationV1::Unavailable,
    };
    require(!o.complete && o.degraded.contains(&reason))
}
fn locate_result(
    window: &HistoryWindowV1,
    position: &HistoryPositionV1,
    items: &[HistoryEventV1],
) -> Result<()> {
    if let HistoryWindowV1::Locate {
        event_id, old_key, ..
    } = window
    {
        require(match position {
            HistoryPositionV1::Unchanged {} => items
                .iter()
                .any(|e| &e.id == event_id && e.sequence == old_key.sequence),
            HistoryPositionV1::Relocated {
                event_id: found_id,
                old_key: found_old,
                ..
            } => found_id == event_id && found_old == old_key,
            HistoryPositionV1::Reset { .. } => true,
        })?;
    }
    Ok(())
}
fn page(o: &ObservationV1, len: usize, max: u32, kind: CursorKindV1) -> Result<()> {
    require(
        o.projection_limits.page_items <= max
            && len <= o.projection_limits.page_items as usize
            && cursor_kind(&o.next_cursor, kind),
    )
}
object_enum! {
    #[serde(tag = "method", content = "result", deny_unknown_fields)]
    pub enum ReadResponseV1 {
        RemoteGetInfoV1(InfoResponseV1),
        RemoteListProjectsV1(ProjectsResponseV1),
        RemoteListSessionsV1(SessionsResponseV1),
        RemoteGetSessionV1(SessionResponseV1),
        RemoteGetHistoryPageV1(HistoryResponseV1),
        RemoteGetDecisionsV1(DecisionsResponseV1),
    }
}
wire!(RetryGuidanceV1 { action: RetryActionV1, after_ms: Option<u32> } => |v: &RetryGuidanceV1| require(v.after_ms.is_none_or(|n| n <= 30000) && (v.action != RetryActionV1::None || v.after_ms.is_none())));
wire!(SafeErrorV1 { code: ErrorCodeV1, correlation_id: WireUuid, retry: RetryGuidanceV1 } => |_: &SafeErrorV1| Ok(()));
impl ErrorCodeV1 {
    pub fn http_status(self) -> u16 {
        match self {
            Self::InvalidRequest => 400,
            Self::NotFound | Self::SelectionUnavailable => 404,
            Self::StaleCursor | Self::Conflict | Self::SelectionRequired => 409,
            Self::Admission => 429,
            Self::ResourceLimit => 422,
            Self::Busy | Self::SourceUnavailable => 503,
            Self::UpgradeRequired => 426,
            Self::AuthRequired => 401,
            Self::AccessDenied => 403,
        }
    }
}

wire!(SessionSelectionV1 { session_id: WireUuid, history_window: HistoryWindowV1, #[serde(default)] selected_decision_id: Option<DecisionId> } => |v: &SessionSelectionV1| { v.history_window.validate()?; if let Some(id) = &v.selected_decision_id { decision_scope(id, &v.session_id)?; } Ok(()) });
object_enum! {
    #[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
    pub enum SelectionV1 {
        None {},
        Project {
            project_id: WireUuid,
            #[serde(default)]
            session: Option<SessionSelectionV1>,
        },
    }
}
wire!(ViewBindingV1 {
    gateway_epoch: WireUuid, policy_epoch: WireUuid, view_id: WireUuid, view_epoch: WireUuid,
    selection_generation: DecimalU64, attachment_generation: DecimalU64, cache_epoch: WireUuid
} => |v: &ViewBindingV1| require(v.selection_generation.get() > 0));
wire!(NativeBindingV1 { window_id: WireUuid, window_generation: DecimalU64, connection_generation: DecimalU64 } => |v: &NativeBindingV1| require(v.window_generation.get() > 0 && v.connection_generation.get() > 0));
wire!(ViewLeaseV1 { state: LeaseStateV1, expires_at: Timestamp, duration_ms: u32 } => |v: &ViewLeaseV1| require(v.duration_ms == if v.state == LeaseStateV1::Allocation { 10000 } else { 25000 }));
wire!(PageRevisionV1 { slot: ViewSlotV1, page_key: Text<2048>, entry_incarnation: DecimalU64, page_revision: DecimalU64 } => |v: &PageRevisionV1| require(v.entry_incarnation.get() > 0 && v.page_revision.get() > v.entry_incarnation.get()));
wire!(BarrierV1 { sequence: DecimalU64, pages: Vec<PageRevisionV1> } => |v: &BarrierV1| require(v.pages.len() <= 8 && unique(v.pages.iter().map(|p| p.slot)) && unique(v.pages.iter().map(|p| &p.page_key))));
wire!(ViewStateV1 { binding: ViewBindingV1, selection: SelectionV1, lease: ViewLeaseV1, barrier: BarrierV1, ready: bool } => |v: &ViewStateV1| {
    require(!v.ready || (v.binding.attachment_generation.get() > 0 && v.lease.state == LeaseStateV1::Attached))?;
    if matches!(v.selection, SelectionV1::None { .. }) { require(v.barrier.pages.iter().all(|p| matches!(p.slot, ViewSlotV1::Info | ViewSlotV1::Projects)))?; }
    if matches!(v.selection, SelectionV1::Project { session: None, .. }) { require(v.barrier.pages.iter().all(|p| matches!(p.slot, ViewSlotV1::Info | ViewSlotV1::Projects | ViewSlotV1::Sessions)))?; }
    Ok(())
});
wire!(CreateViewV1 { client_slot_id: WireUuid, selection: SelectionV1 } => |v: &CreateViewV1| require(matches!(v.selection, SelectionV1::None { .. })));
wire!(AllocatedViewV1 { state: ViewStateV1 } => |v: &AllocatedViewV1| require(matches!(v.state.selection, SelectionV1::None { .. }) && v.state.binding.selection_generation.get() == 1 && v.state.binding.attachment_generation.get() == 0 && !v.state.ready && v.state.lease.state == LeaseStateV1::Allocation && v.state.barrier.sequence.get() == 0 && v.state.barrier.pages.is_empty()));
wire!(AttachViewV1 { binding: ViewBindingV1, expected_attachment_generation: DecimalU64, operation_id: WireUuid } => |v: &AttachViewV1| require(v.expected_attachment_generation == v.binding.attachment_generation && v.expected_attachment_generation.get() < u64::MAX));
wire!(SelectViewV1 { binding: ViewBindingV1, expected_selection_generation: DecimalU64, operation_id: WireUuid, selection: SelectionV1 } => |v: &SelectViewV1| require(v.binding.attachment_generation.get() > 0 && v.expected_selection_generation == v.binding.selection_generation && v.expected_selection_generation.get() < u64::MAX));
wire!(SelectionAckV1 { state: ViewStateV1, operation_id: WireUuid, previous_selection_generation: DecimalU64 } => |v: &SelectionAckV1| require(v.state.ready && v.previous_selection_generation.get() > 0 && v.previous_selection_generation.get().checked_add(1) == Some(v.state.binding.selection_generation.get())));
wire!(AttachmentAckV1 { state: ViewStateV1, operation_id: WireUuid, previous_attachment_generation: DecimalU64 } => |v: &AttachmentAckV1| require(!v.state.ready && v.previous_attachment_generation.get().checked_add(1) == Some(v.state.binding.attachment_generation.get())));
wire!(StreamIdV1 { gateway_epoch: WireUuid, view_epoch: WireUuid, sequence: DecimalU64 } => |_: &StreamIdV1| Ok(()));
wire!(ReadyV1 { state: ViewStateV1, stream_id: StreamIdV1 } => |v: &ReadyV1| require(v.state.ready && v.stream_id.gateway_epoch == v.state.binding.gateway_epoch && v.stream_id.view_epoch == v.state.binding.view_epoch && v.stream_id.sequence == v.state.barrier.sequence));
wire!(AckViewV1 { binding: ViewBindingV1, highest_contiguous_sequence: DecimalU64, #[serde(default)] foreground_activity: bool, #[serde(default)] native: Option<NativeBindingV1> } => |v: &AckViewV1| require(v.binding.attachment_generation.get() > 0));
wire!(CloseViewV1 { binding: ViewBindingV1, operation_id: WireUuid } => |_: &CloseViewV1| Ok(()));
wire!(BoundReadV1 { binding: ViewBindingV1, request_id: WireUuid, page_key: Text<2048>, read: ViewReadRequestV1, #[serde(default)] native: Option<NativeBindingV1> } => |v: &BoundReadV1| require(v.binding.attachment_generation.get() > 0));
wire!(BoundResponseV1 {
    binding: ViewBindingV1, request_id: WireUuid, page_key: Text<2048>, entry_incarnation: DecimalU64, page_revision: DecimalU64,
    cache_status: CacheStatusV1, read: ReadResponseV1, #[serde(default)] native: Option<NativeBindingV1>
} => |v: &BoundResponseV1| require(v.binding.attachment_generation.get() > 0 && v.entry_incarnation.get() > 0 && v.page_revision.get() > v.entry_incarnation.get()));
object_enum! {
    #[serde(
        tag = "event",
        content = "data",
        rename_all = "snake_case",
        deny_unknown_fields
    )]
    pub enum NoticeV1 {
        Ready(ReadyV1),
        SelectionReset(ReadyV1),
        PageChanged(PageNoticeV1),
        PageRefetchRequired(PageNoticeV1),
        Reset(ResetNoticeV1),
        AuthRequired(StreamNoticeV1),
        ServiceUnavailable(StreamNoticeV1),
    }
}
wire!(PageNoticeV1 { binding: ViewBindingV1, barrier: BarrierV1 } => |v: &PageNoticeV1| require(v.binding.attachment_generation.get() > 0));
wire!(ResetNoticeV1 { binding: ViewBindingV1, sequence: DecimalU64, reason: ResetReasonV1 } => |v: &ResetNoticeV1| require(v.binding.attachment_generation.get() > 0));
wire!(StreamNoticeV1 { binding: ViewBindingV1, sequence: DecimalU64 } => |v: &StreamNoticeV1| require(v.binding.attachment_generation.get() > 0));
wire!(BootstrapV1 { nonce: Text<43>, expires_at: Timestamp } => |v: &BootstrapV1| token256(v.nonce.as_str()));
wire!(CreateAppSessionV1 { nonce: Text<43> } => |v: &CreateAppSessionV1| token256(v.nonce.as_str()));
wire!(AppSessionV1 { csrf_token: Text<43>, gateway_epoch: WireUuid, policy_epoch: WireUuid, idle_expires_at: Timestamp, absolute_expires_at: Timestamp } => |v: &AppSessionV1| token256(v.csrf_token.as_str()));
fn token256(s: &str) -> Result<()> {
    require(
        s.len() == 43
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            && s.as_bytes()
                .last()
                .is_some_and(|b| b"AEIMQUYcgkosw048".contains(b)),
    )
}

object_enum! {
    /// Closed fixture/codec entry points, not a dispatch or generic RPC surface.
    #[serde(
        tag = "type",
        content = "value",
        rename_all = "snake_case",
        deny_unknown_fields
    )]
    pub enum WireDocumentV1 {
        Request(ReadRequestV1),
        ViewRequest(ViewReadRequestV1),
        Response(ReadResponseV1),
        Error(SafeErrorV1),
        CreateView(CreateViewV1),
        AllocatedView(AllocatedViewV1),
        AttachView(AttachViewV1),
        AttachmentAck(AttachmentAckV1),
        SelectView(SelectViewV1),
        SelectionAck(SelectionAckV1),
        ViewState(ViewStateV1),
        Notice(NoticeV1),
        AckView(AckViewV1),
        CloseView(CloseViewV1),
        BoundRead(BoundReadV1),
        BoundResponse(BoundResponseV1),
        Bootstrap(BootstrapV1),
        CreateAppSession(CreateAppSessionV1),
        AppSession(AppSessionV1),
    }
}

/// Decode bounded bytes, reject duplicate keys/floating JSON numbers, then decode
/// a closed typed document. Optional defaults are emitted explicitly by `encode`.
pub fn decode(bytes: &[u8]) -> Result<WireDocumentV1> {
    require(bytes.len() <= MAX_ENVELOPE_BYTES)?;
    let value = serde_json::from_slice::<StrictValue>(bytes)
        .map_err(|_| InvalidWire)?
        .0;
    validate_tree(&value, 0, &mut 0)?;
    let document: WireDocumentV1 = serde_json::from_value(value).map_err(|_| InvalidWire)?;
    if is_request(&document) {
        require(bytes.len() <= MAX_REQUEST_BYTES)?;
    }
    validate_document(&document)?;
    Ok(document)
}
pub fn encode(document: &WireDocumentV1) -> Result<Vec<u8>> {
    // Revalidate programmatically constructed DTOs too, including nested enums.
    let bytes = serde_json::to_vec(document).map_err(|_| InvalidWire)?;
    let checked = decode(&bytes)?;
    serde_json::to_vec(&checked).map_err(|_| InvalidWire)
}
fn text_bytes(v: &Value) -> usize {
    match v {
        Value::String(s) => s.len(),
        Value::Array(a) => a.iter().map(text_bytes).sum(),
        Value::Object(o) => o.values().map(text_bytes).sum(),
        _ => 0,
    }
}
fn validate_tree(v: &Value, depth: usize, nodes: &mut usize) -> Result<()> {
    *nodes += 1;
    require(depth <= 32 && *nodes <= 32768)?;
    match v {
        Value::Number(n) => require(n.is_i64() || n.is_u64())?,
        Value::Array(a) => {
            for child in a {
                validate_tree(child, depth + 1, nodes)?;
            }
        }
        Value::Object(o) => {
            for child in o.values() {
                validate_tree(child, depth + 1, nodes)?;
            }
        }
        _ => (),
    }
    Ok(())
}

// The visitor detects duplicates before a map can erase them. serde_json also
// rejects lone Unicode surrogates and non-JSON numeric spellings.
struct StrictValue(Value);
impl<'de> Deserialize<'de> for StrictValue {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct Visitor;
        impl<'de> de::Visitor<'de> for Visitor {
            type Value = StrictValue;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("bounded JSON")
            }
            fn visit_bool<E: de::Error>(self, v: bool) -> std::result::Result<Self::Value, E> {
                Ok(StrictValue(v.into()))
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> std::result::Result<Self::Value, E> {
                Ok(StrictValue(v.into()))
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> std::result::Result<Self::Value, E> {
                Ok(StrictValue(v.into()))
            }
            fn visit_f64<E: de::Error>(self, _: f64) -> std::result::Result<Self::Value, E> {
                Err(E::custom(InvalidWire))
            }
            fn visit_str<E: de::Error>(self, v: &str) -> std::result::Result<Self::Value, E> {
                Ok(StrictValue(v.into()))
            }
            fn visit_unit<E: de::Error>(self) -> std::result::Result<Self::Value, E> {
                Ok(StrictValue(Value::Null))
            }
            fn visit_seq<A: de::SeqAccess<'de>>(
                self,
                mut a: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut values = Vec::new();
                while let Some(StrictValue(v)) = a.next_element()? {
                    values.push(v);
                }
                Ok(StrictValue(Value::Array(values)))
            }
            fn visit_map<A: de::MapAccess<'de>>(
                self,
                mut a: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut values = serde_json::Map::new();
                while let Some((key, StrictValue(value))) = a.next_entry::<String, StrictValue>()? {
                    if values.insert(key, value).is_some() {
                        return Err(de::Error::custom(InvalidWire));
                    }
                }
                Ok(StrictValue(Value::Object(values)))
            }
        }
        d.deserialize_any(Visitor)
    }
}

fn validate_document(d: &WireDocumentV1) -> Result<()> {
    let value = serde_json::to_value(d).map_err(|_| InvalidWire)?;
    let size = serde_json::to_vec(&value).map_err(|_| InvalidWire)?.len();
    let request = is_request(d);
    require(
        size <= if request {
            MAX_REQUEST_BYTES
        } else {
            MAX_ENVELOPE_BYTES
        },
    )?;
    let response = match d {
        WireDocumentV1::Response(r) => Some(r),
        WireDocumentV1::BoundResponse(r) => Some(&r.read),
        _ => None,
    };
    if let Some(r) = response {
        validate_response(r, size)?;
    }
    Ok(())
}
fn is_request(d: &WireDocumentV1) -> bool {
    matches!(
        d,
        WireDocumentV1::Request(_)
            | WireDocumentV1::ViewRequest(_)
            | WireDocumentV1::CreateView(_)
            | WireDocumentV1::AttachView(_)
            | WireDocumentV1::SelectView(_)
            | WireDocumentV1::AckView(_)
            | WireDocumentV1::CloseView(_)
            | WireDocumentV1::BoundRead(_)
            | WireDocumentV1::CreateAppSession(_)
    )
}

fn validate_response(r: &ReadResponseV1, size: usize) -> Result<()> {
    let value = serde_json::to_value(r).map_err(|_| InvalidWire)?;
    let result = &value["result"];
    let o = match r {
        ReadResponseV1::RemoteGetInfoV1(v) => v.observation(),
        ReadResponseV1::RemoteListProjectsV1(v) => v.observation(),
        ReadResponseV1::RemoteListSessionsV1(v) => v.observation(),
        ReadResponseV1::RemoteGetSessionV1(v) => v.observation(),
        ReadResponseV1::RemoteGetHistoryPageV1(v) => v.observation(),
        ReadResponseV1::RemoteGetDecisionsV1(v) => v.observation(),
    };
    require(size <= o.projection_limits.envelope_bytes as usize)?;
    if matches!(
        r,
        ReadResponseV1::RemoteListProjectsV1(_)
            | ReadResponseV1::RemoteListSessionsV1(_)
            | ReadResponseV1::RemoteGetHistoryPageV1(_)
    ) {
        require(o.complete)?;
    }
    for item in result
        .get("items")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .chain(result.get("item"))
        .chain(result.get("project"))
    {
        require(
            serde_json::to_vec(item).map_err(|_| InvalidWire)?.len()
                <= o.projection_limits.item_bytes as usize,
        )?;
    }
    let required_sources: &[SourceV1] = match r {
        ReadResponseV1::RemoteGetInfoV1(_) => &[],
        ReadResponseV1::RemoteListProjectsV1(_) => {
            &[SourceV1::ConfiguredProjects, SourceV1::StoreProjects]
        }
        ReadResponseV1::RemoteListSessionsV1(_) => &[
            SourceV1::ActiveSessions,
            SourceV1::CompletedSessions,
            SourceV1::StoreSessions,
        ],
        ReadResponseV1::RemoteGetSessionV1(_) => &[
            SourceV1::ActiveSessions,
            SourceV1::CompletedSessions,
            SourceV1::StoreSessions,
        ],
        ReadResponseV1::RemoteGetHistoryPageV1(_) => &[SourceV1::StoreHistory],
        ReadResponseV1::RemoteGetDecisionsV1(_) => &[
            SourceV1::QuestionPublications,
            SourceV1::TrackedQuestionSlot,
            SourceV1::SessionQuestionSlot,
            SourceV1::DurableQuestionFallback,
            SourceV1::NativeRuntime,
            SourceV1::NativePublications,
            SourceV1::NativeHistoricalFallback,
            SourceV1::LegacyApprovals,
        ],
    };
    let configured_empty = matches!(r, ReadResponseV1::RemoteListProjectsV1(v) if v.items.is_empty() && v.coverage.len() == 1 && v.coverage[0].source == SourceV1::ConfiguredProjects && v.coverage[0].state == CoverageStateV1::Complete && v.coverage[0].lower_bound.get() == 0 && !v.coverage[0].has_more && v.next_cursor.is_none());
    require(
        configured_empty
            || required_sources
                .iter()
                .all(|s| o.coverage.iter().any(|c| &c.source == s)),
    )?;
    match r {
        ReadResponseV1::RemoteListProjectsV1(v) => require(
            v.items
                .iter()
                .all(|p| p.name.as_str().len() <= o.projection_limits.name_bytes as usize),
        )?,
        ReadResponseV1::RemoteListSessionsV1(v) => require(
            v.project.name.as_str().len() <= o.projection_limits.name_bytes as usize
                && v.items
                    .iter()
                    .all(|s| s.own_title.as_str().len() <= o.projection_limits.name_bytes as usize),
        )?,
        ReadResponseV1::RemoteGetSessionV1(v) => {
            require(
                v.item.own_title.as_str().len() <= o.projection_limits.name_bytes as usize
                    && v.item.pending_coverage.len() == 8,
            )?;
        }
        ReadResponseV1::RemoteGetHistoryPageV1(v) => require(
            v.items
                .iter()
                .all(|e| e.text.as_str().len() <= o.projection_limits.event_text_bytes as usize),
        )?,
        ReadResponseV1::RemoteGetDecisionsV1(v) => {
            for d in v.items.iter().chain(match &v.selected {
                SelectedDecisionV1::Present { decision, .. } => Some(decision.as_ref()),
                _ => None,
            }) {
                let item = serde_json::to_value(d).map_err(|_| InvalidWire)?;
                require(
                    text_bytes(&item) <= o.projection_limits.decision_text_bytes as usize
                        && serde_json::to_vec(&item).map_err(|_| InvalidWire)?.len()
                            <= o.projection_limits.item_bytes as usize,
                )?;
                decision_scope(&d.id, &v.session_id)?;
            }
            if let Some(id) = v.selected.id() {
                decision_scope(id, &v.session_id)?;
                let selected = serde_json::to_value(&v.selected).map_err(|_| InvalidWire)?;
                require(
                    text_bytes(&selected) <= o.projection_limits.decision_text_bytes as usize
                        && serde_json::to_vec(&selected)
                            .map_err(|_| InvalidWire)?
                            .len()
                            <= o.projection_limits.item_bytes as usize,
                )?;
            }
            require(
                serde_json::to_vec(&v.selected)
                    .map_err(|_| InvalidWire)?
                    .len()
                    <= MAX_ITEM_BYTES,
            )?;
        }
        _ => (),
    }
    Ok(())
}

fn decision_scope(id: &DecisionId, session: &WireUuid) -> Result<()> {
    let parts: Vec<_> = id.as_str().split(':').collect();
    require(
        !matches!(parts[0], "question-slot" | "question-fallback") || parts[1] == session.as_str(),
    )
}
fn validate_decision_sources(d: &DecisionSummaryV1) -> Result<()> {
    let parts: Vec<_> = d.id.as_str().split(':').collect();
    let primary = match parts[0] {
        "question" => Some(SourceV1::QuestionPublications),
        "question-fallback" => Some(SourceV1::DurableQuestionFallback),
        "question-slot" => Some(if parts[3] == "tracked" {
            SourceV1::TrackedQuestionSlot
        } else {
            SourceV1::SessionQuestionSlot
        }),
        "legacy" => Some(SourceV1::LegacyApprovals),
        _ => None, // Native identity is witnessed by any of its three source variants.
    };
    require(primary.is_none_or(|p| d.source_observations.iter().any(|s| s.source == p)))?;
    for s in &d.source_observations {
        require(match d.kind {
            DecisionKindV1::GenericQuestions => {
                if parts[0] == "question-slot" {
                    matches!(
                        s.source,
                        SourceV1::TrackedQuestionSlot | SourceV1::SessionQuestionSlot
                    )
                } else if parts[0] == "question" {
                    matches!(
                        s.source,
                        SourceV1::QuestionPublications | SourceV1::DurableQuestionFallback
                    )
                } else {
                    Some(s.source) == primary
                }
            }
            DecisionKindV1::NativeApproval => matches!(
                s.source,
                SourceV1::NativeRuntime
                    | SourceV1::NativePublications
                    | SourceV1::NativeHistoricalFallback
            ),
            DecisionKindV1::LegacyApproval => s.source == SourceV1::LegacyApprovals,
        })?;
        if s.source != SourceV1::NativeRuntime {
            require(
                s.writer_live.is_none()
                    && s.writer_capacity.is_none()
                    && s.witness_present.is_none(),
            )?;
        }
        if parts[0] == "question-slot" && parts[2] != "completed" {
            require(
                s.spawn_generation
                    .as_ref()
                    .is_none_or(|g| g.as_str() == parts[2]),
            )?;
        }
    }
    if matches!(parts[0], "question-slot" | "question") && d.source_observations.len() > 1 {
        // Multiple generic sources share one projection only when its evidence
        // is complete and consistent. The producer must establish full source
        // equality before projection; matching truncated text is insufficient.
        require(
            !d.disagreement
                && d.details_state == PreviewStateV1::Complete
                && d.omitted_source_observations.get() == 0
                && d.omitted_questions.get() == 0
                && d.questions.iter().all(|q| q.omitted_options.get() == 0)
                && d.source_observations
                    .iter()
                    .all(|s| s.state == CoverageStateV1::Complete),
        )?;
    }
    Ok(())
}
fn validate_publication(d: &DecisionSummaryV1) -> Result<()> {
    let Some(PublicationStateV1::Known { value }) = &d.publication_state else {
        return Ok(());
    };
    use KnownPublicationStateV1::*;
    let closure = match d.kind {
        DecisionKindV1::GenericQuestions => match value {
            unresolved | published => Some(ClosureStateV1::Open),
            cleared => Some(ClosureStateV1::Closed),
            _ => return Err(InvalidWire),
        },
        DecisionKindV1::LegacyApproval => match value {
            Pending => Some(ClosureStateV1::Open),
            Approved | Denied => Some(ClosureStateV1::Closed),
            _ => return Err(InvalidWire),
        },
        DecisionKindV1::NativeApproval => {
            require(matches!(
                value,
                unresolved | published | enqueued | expired | superseded
            ))?;
            None
        }
    };
    require(closure.is_none_or(|c| d.closure_state == c))
}

/// Structural correlation only. Caller must separately establish authority and
/// source observations; these DTOs and comparison helpers confer no permission.
pub fn validate_exchange(q: &ReadRequestV1, r: &ReadResponseV1) -> Result<()> {
    match (q, r) {
        (ReadRequestV1::RemoteGetInfoV1(_), ReadResponseV1::RemoteGetInfoV1(_)) => Ok(()),
        (ReadRequestV1::RemoteListProjectsV1(q), ReadResponseV1::RemoteListProjectsV1(r)) => {
            require(
                r.items.len() <= q.limit as usize
                    && r.items.iter().all(|p| q.project_ids.contains(&p.id)),
            )
        }
        (ReadRequestV1::RemoteListSessionsV1(q), ReadResponseV1::RemoteListSessionsV1(r)) => {
            require(r.project.id == q.project_id && r.items.len() <= q.limit as usize)
        }
        (ReadRequestV1::RemoteGetSessionV1(q), ReadResponseV1::RemoteGetSessionV1(r)) => {
            require(r.item.summary.id == q.session_id && r.item.summary.project_id == q.project_id)
        }
        (ReadRequestV1::RemoteGetHistoryPageV1(q), ReadResponseV1::RemoteGetHistoryPageV1(r)) => {
            require(
                r.project_id == q.project_id
                    && r.session_id == q.session_id
                    && r.items.len() <= q.limit as usize
                    && r.window == q.window,
            )?;
            locate_result(&q.window, &r.position, &r.items)
        }
        (ReadRequestV1::RemoteGetDecisionsV1(q), ReadResponseV1::RemoteGetDecisionsV1(r)) => {
            require(
                r.project_id == q.project_id
                    && r.session_id == q.session_id
                    && r.mode == q.mode
                    && r.items.len() <= q.limit as usize
                    && r.selected.id() == q.selected_decision_id.as_ref(),
            )
        }
        _ => Err(InvalidWire),
    }
}
pub fn validate_bound_read(view: &ViewStateV1, q: &BoundReadV1) -> Result<()> {
    require(view.ready && view.binding == q.binding)?;
    match (&view.selection, &q.read) {
        (_, ViewReadRequestV1::RemoteGetInfoV1(_) | ViewReadRequestV1::RemoteListProjectsV1(_)) => {
            Ok(())
        }
        (SelectionV1::Project { project_id, .. }, ViewReadRequestV1::RemoteListSessionsV1(q)) => {
            require(project_id == &q.project_id)
        }
        (
            SelectionV1::Project {
                project_id,
                session: Some(s),
            },
            ViewReadRequestV1::RemoteGetSessionV1(q),
        ) => require(project_id == &q.project_id && s.session_id == q.session_id),
        (
            SelectionV1::Project {
                project_id,
                session: Some(s),
            },
            ViewReadRequestV1::RemoteGetHistoryPageV1(q),
        ) => require(project_id == &q.project_id && s.session_id == q.session_id),
        (
            SelectionV1::Project {
                project_id,
                session: Some(s),
            },
            ViewReadRequestV1::RemoteGetDecisionsV1(q),
        ) => require(
            project_id == &q.project_id
                && s.session_id == q.session_id
                && s.selected_decision_id == q.selected_decision_id,
        ),
        _ => Err(InvalidWire),
    }
}
pub fn validate_bound_response(q: &BoundReadV1, r: &BoundResponseV1) -> Result<()> {
    require(
        q.binding == r.binding
            && q.request_id == r.request_id
            && q.page_key == r.page_key
            && q.native == r.native,
    )?;
    match (&q.read, &r.read) {
        (ViewReadRequestV1::RemoteListProjectsV1(q), ReadResponseV1::RemoteListProjectsV1(r)) => {
            require(r.items.len() <= q.limit as usize)
        }
        _ => validate_exchange(
            &q.read.object_or_info_request().ok_or(InvalidWire)?,
            &r.read,
        ),
    }
}

/// Fixture/renderer hints from bounded fields; these are text, never markup.
pub fn display_hints(d: &WireDocumentV1) -> Result<Vec<String>> {
    fn walk(v: &Value, hints: &mut Vec<String>) {
        match v {
            Value::String(s) => hints.push(s.clone()),
            Value::Array(a) => {
                for x in a {
                    walk(x, hints);
                }
            }
            Value::Object(o) => {
                if o.get("state").and_then(Value::as_str) == Some("unknown") {
                    if let Some(label) = o.get("label").and_then(Value::as_str) {
                        hints.push(format!("Unknown: {label}"));
                    }
                }
                if o.get("state").and_then(Value::as_str) == Some("truncated")
                    || o.get("details_state").and_then(Value::as_str) == Some("truncated")
                {
                    hints.push("Truncated preview".into());
                }
                if o.get("source_extent").and_then(Value::as_str) == Some("bounded_snapshot") {
                    hints.push("Source-provided preview".into());
                }
                if o.get("identity_class").and_then(Value::as_str) == Some("slot") {
                    hints.push("Occurrence unknown".into());
                }
                if o.get("kind").and_then(Value::as_str) == Some("generic_questions")
                    && o.contains_key("details_state")
                {
                    hints.push("Answer not observed".into());
                    if o.get("details_state").and_then(Value::as_str) == Some("unavailable") {
                        hints.push("Question details unavailable".into());
                    }
                }
                if let Some(source) = o.get("source").and_then(Value::as_str) {
                    if let Some(label) = match source {
                        "native_runtime" => Some("Native runtime"),
                        "native_publications" => Some("Native publication"),
                        "native_historical_fallback" => Some("Native historical fallback"),
                        "legacy_approvals" => Some("Legacy approval"),
                        _ => None,
                    } {
                        hints.push(label.into());
                    }
                }
                for x in o.values() {
                    walk(x, hints);
                }
            }
            _ => (),
        }
    }
    let mut hints = Vec::new();
    walk(
        &serde_json::to_value(d).map_err(|_| InvalidWire)?,
        &mut hints,
    );
    hints.sort();
    hints.dedup();
    Ok(hints)
}

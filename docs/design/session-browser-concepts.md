# Session browser design study

Design scope: three browser layouts plus shared session detail and global chrome,
not a TUI implementation. Open
[interactive prototype](session-browser-concepts.html) locally in a browser.
No build, daemon connection, external fonts or network requests required.
Prototype data combines screenshot names with explicitly illustrative states;
it is not a report of current sessions. Copy controls copy their displayed fixture
values to the local clipboard, with a manual fallback if browser access fails.

## Recommendation

**Focus: 62% list, 38% selected-session inspector when browsing; compact navigator
and wide conversation when opened.** Preserve dense navigation, put selected flags
first, and replace the permanent activity rail with space for session names.
Preserve the global top bar and current dark terminal palette.
Ratios are proposed starting points, not measured optimal dimensions.

## Revision: global chrome and session detail

Operator correction: first draft appeared to remove the application top bar and
its global active-session count. A list-local activity summary cannot replace it.
The recommended Focus direction remains; this revision explicitly retains shared
application chrome in all three layouts and in conversation view.

Three scopes stay distinct:

| Scope | Placement | Meaning |
| --- | --- | --- |
| Across RSI | Persistent top row: `● 7 active │ ⚠ 178 │ ? 0`, project tabs | Active sessions and alert center independent of selection, list filter or detail pane |
| Current list | Breadcrumb/count and list groups | Sessions in current navigation scope; not global activity |
| Selected session | First inspector/conversation row | Flags belonging to this exact session |

Numbers are illustrative. Seven active deliberately differs from the visible list
to make scope evident. The attention count 178 and question count 0 retain the
operator screenshot's values without claiming they are current daemon state.

`render_top_chrome` in `crates/rsi/src/ui/mod.rs` already lives above the split
tree. It counts Running + Starting over `app.sessions`, uses
`attention_session_ids(app)` for attention, and counts `pending_question` for `?`.
Keep this existing global data scope; never recompute the active total from
visible rows or the selected session. This is a count of sessions, not processes,
workers, containers or a newly invented activity definition. The renderer alone
does not establish cache completeness: incomplete/disconnected state must remain
visibly stale or unknown, not quietly become zero. Centered transient notices and
right-side project tabs also retain their reserved space in production chrome.

The prototype's top counters open an illustrative alert-center dialog with
Active, Attention and Questions tabs. This makes proposed destinations reviewable;
it does not claim the current warning/question controls work. Keep those indicators
and their access point. Repairing their backend semantics or triage behavior is
separate implementation work, not silently bundled with the layout change. Zero
questions gets an explicit empty state. Global counts are not mutually exclusive.

### Session detail arrangement

- Shared global bar stays at top. Opening a conversation changes only the content
  region below it. All three browse concepts converge on this same detail design;
  the chosen browse arrangement returns on exit.
- Compact navigator gets roughly 30% width, conversation 70%. Preserve names,
  status, urgent flags, context and age; model/effort moves into selected header.
  Eliminate the large unused gap between list and transcript shown in screenshot.
- Conversation header stays fixed while transcript scrolls: flags first, then
  full title/status plus sandbox/branch/CWD copy controls, then runtime facts and
  Details disclosure. Keep full paths, ID and context provenance reachable.
- Inspector does not become a permanent third pane beside conversation. Transcript
  already provides full updates, tool evidence and outcome text; another summary
  column would repeat it and consume writing space.
- Preserve message identity, author/model, sequence, time, tool groups and input
  behavior. Prototype tool groups expand; no flattening or deletion of evidence.
  Text uses available width with a readable line-length cap and small padding.
- Composer remains anchored below transcript; drafts survive browse/detail and
  session switches. Prototype stores drafts per fixture session and sends nothing.
  Production must preserve draft ownership when list cursor and open detail target
  differ; copy controls and composer bind the detail pane's own session ID.
- Browser prototype follows selection in its single conversation pane. This is a
  demonstration, not authority to replace RSI's split-tree or multi-pane semantics.
  Preserve explicit pane targeting, independent detail panes and existing open/
  close/focus behavior when implementing.
- Narrow browser preview stacks a short scrollable navigator above conversation.
  Production terminal should prioritize conversation and offer list toggle when
  two useful panes cannot fit. Keep global active count and composer visible at
  short heights; reduce optional metadata before either.

Expert-lens consequence: app-wide state belongs outside pane layout (architecture),
active count stays visible during work (power-user UX), scope and freshness stay
honest (reliability), and existing aggregated state supplies the bar without extra
per-pane polling (performance/scalability). Shared detail avoids implementing three
different conversation systems (engineering/tech wizard); explicit pane/draft
ownership preserves session boundaries (harness architecture).

| Concept | Layout | Best fit | Cost |
| --- | --- | --- | --- |
| Focus | Wide list + compact inspector | Everyday navigation and acting on selected session | Recent cross-session changes need a separate view |
| Drawer | Full-width list + bottom detail drawer | Long names and narrower terminals | Drawer consumes visible session rows |
| Watch | List + inspector + small changes rail | Wide displays used for monitoring | Less room for names; requires timestamped event evidence |

Prototype controls switch layouts, select sessions, preview running/reply/failed
examples, toggle ASCII icons, reveal details and copy values. Prototype shortcuts
are browser-only; production keys require checking existing bindings and updating
`docs/keybindings.md`.

## What changes

1. **Flags first.** Selected inspector starts with flags, above title and prose.
   Drawer pins selected flags above the list so they remain at the top of the view.
   Prioritize failed/stalled/reply/approval signals, then testing/retry/archive,
   then unread/pinned/rotation-disabled/lead metadata. Preserve every active flag;
   wrap the strip if needed. Distinguish status, action requirements and preference
   flags; no new backend enum is implied. Urgent row markers remain visible even
   when inspector is hidden. Global top bar summarizes attention across RSI;
   selected strip describes only the selected session. Counts may overlap and
   must never imply exclusive partitions.
2. **Sandbox and branch become actions.** Place `□` and `⎇` beside selected
   identity. Click copies the entire sandbox path or branch, including truncated
   portions. Working-directory copy gets a separate `⌂` control because source
   directory and sandbox can differ. Hover or keyboard focus exposes purpose and
   full value. In a TUI, focus help must render visibly, not rely on browser-style
   tooltips. Provide `sb`, `br`, `wd` fallback glyphs; never require a patched font.
   Absent values render disabled controls with a reason, never copy empty strings.
3. **Replace “LATEST CACHED STATE” with conditional useful content.** Running
   sessions can show a concise last agent report with source and age. Do not infer
   current work from cached prose. If no meaningful report exists, omit that block;
   status already says running. Waiting sessions lead with next action and why;
   failed sessions lead with exact failure evidence and recovery status; completed
   sessions lead with outcome and known verification. Full text remains reachable.
4. **Keep brief, paths and diagnostics behind disclosure.** UUID, full directory,
   branch, context source/version/digest and observation details stay available in
   Details. Description lives in Brief. Default inspector shows title, flags,
   action/report and compact runtime facts. Relevant context uncertainty remains
   visible as `?`; missing usage must never masquerade as zero usage.
5. **Remove duplicated right-column content.** Focus and Drawer keep group counts
   and attention in the list/header. Watch uses its third column only for bounded,
   timestamped changes. Old unresolved work can stay in attention, but it must not
   be labeled live or now. A 31-day-old completed row in an attention queue needs
   an explicit reason; completion alone does not explain operator action.
6. **Preserve identity.** Container's own name remains primary. Role/ordinal and
   child activity can supplement it, never replace it. No session name is removed
   as a density optimization. Long names can truncate visually with full selected
   title available. Keep context, age and model/effort in list when space permits;
   hide model column before sacrificing identity and retain it in inspector.

## Seven expert lenses

These are design judgments applying repository's named expert framework,
not claims of external consultation or usability research.

| Lens | Judgment and consequence |
| --- | --- |
| Software engineer | Reuse existing inspector facts and selected identity projection. Change presentation before inventing new persistent state. |
| Tech wizard | One reusable selected-session inspector across three arrangements. Prototype alternate layouts before committing to a production layout. |
| UI/UX power user | Flags precede paragraphs. Symbols suit frequent actions; uncommon or dangerous states keep short text. Mouse copy must have keyboard parity. |
| Systems performance | Recompute view from existing session updates; copy only on activation. Avoid extra per-row RPCs or polling to populate tooltips. |
| Reliability | Timestamp reports, expose unknowns, preserve exact failures. Clipboard feedback must describe evidence honestly. Existing OSC 52 writer cannot prove host clipboard accepted content. |
| Scalability | Bound changes history and preserve list scrolling. At narrow widths, remove rail, then use drawer/compact inspector rather than squeezing every column. |
| Agentic harness architect | Separate agent reports, provider liveness, verification and accepted delivery. “Completed” does not establish tests passed or changes integrated. |

## Implementation implications

- `crates/rsi/src/ui/session.rs`: inspector currently appends FLAGS after context;
  running body uses LATEST CACHED STATE; activity renderer repeats operator queue,
  recent changes and flow. These are concrete presentation edit points.
- `crates/rsi/src/types/session_inspector.rs`: flags and sandbox/branch facts
  already exist. `latest_state` is a plain optional string, not a typed timestamped
  report. Source age needs real provenance or must display unknown. Native flag
  strings currently include pinned, new message, testing needed, rotation disabled,
  stalled, pending archive, epic lead and retry counts.
- `crates/rsi/src/clipboard.rs`: existing OSC 52 write can serve TUI copy actions.
  Browser prototype uses browser clipboard only. TUI requires action dispatch,
  mouse hit regions, keyboard focus and help; icon drawing alone is incomplete.
- Compact/full disclosures are design proposals; this study does not assert
  corresponding TUI commands already exist.
- Browser responsiveness demonstrates intent, not cell-accurate ratatui layout.
  Production breakpoints require checking 120×40, 200×58 and 240×70 views, small
  heights, all eight flag categories, long names, missing paths and absent metrics.

## Acceptance criteria for later implementation

- First visible selected-session line contains active flags, with positive checks
  for urgent flags and container identity. No fixture encodes disappearance of
  user identity as desired behavior.
- Pointer and keyboard copy act on the same selected session and exact full value.
  Focus/value help remains readable without mouse hover; missing values explain
  disabled state. Selection changes invalidate old hit targets.
- Clipboard feedback distinguishes a successful browser write from a TUI copy
  request sent through OSC 52; no unverified “Copied” claim in production.
- Unknown usage renders `?`; details explain provenance. Stale reports retain age
  or unknown-age marking and are never rephrased as proven live activity.
- Brief and diagnostic information remain accessible. Expanded content scrolls
  within bounded pane height; flags remain visible. Nothing actionable becomes
  unreachable on short terminals.
- Running, waiting, failed, completed and container examples each lead with the
  information needed to make the next decision.
- Global active count, attention/question indicators and project-tab access stay
  visible in list and detail views. Changing list scope or pane focus preserves
  global counter scope. Transcript scrolling preserves header and composer.
- Returning to Browse restores chosen concept. Detail controls target its session;
  per-session drafts survive switching. Existing split-pane behavior is preserved.

No production Rust, keybindings or daemon behavior changed by this study.

## Review artifacts and checks

Static previews: [Focus](session-browser-focus.png),
[Drawer](session-browser-drawer.png), [Watch](session-browser-watch.png),
[Session detail](session-browser-detail.png), [Alert center](session-browser-alerts.png).

Headless Chromium review passed: three layouts; 12 selectable rows; waiting and
failure examples; ASCII copy icons; diagnostic disclosure; exact full sandbox and
branch payloads with clipboard API stub; keyboard activation and visible focus
help; manual fallback after clipboard rejection; no page-level horizontal overflow
at 1200, 900 and 600 pixels; no JavaScript errors. Screenshots visually inspected
at 1680 pixels. Native TUI clipboard delivery and terminal glyph widths remain
implementation-time verification, outside this browser design study.

Revision checks also cover shared count across views, detail target changes,
per-session draft retention, fixed global bar during transcript scroll, seven-entry
active-session drill-down, zero-question empty state and narrow detail layout.

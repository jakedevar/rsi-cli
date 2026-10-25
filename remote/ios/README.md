# RSI Remote for iOS

A native SwiftUI client (Swift 6, no third-party runtime dependencies) for the
hub's `rsi-remote` gateway, reached over Tailscale (hub #2961, #2963). The
gateway identifies this phone by its tailnet identity; the app stores only the
host name.

Screens:

- **Projects**: the projects the gateway exposes (v1 `GET /api/v1/projects`),
  each with its session count, its PM and Global manager seat status and the
  PM ledger's pending-decision badge from v2 `GET /api/v2/projects` (the v1
  waiting-session count when v2 is not available); pull to refresh. A project
  opens on its **Manager** pane and its **Sessions** list.
- **Manager chat** (gateway API v2, `docs/rsi-remote/contracts-v2.md`): a PM /
  Global toggle, the seat's status, a compose box and the conversation newest
  first with "Load older" (`?before=<next_before>`, sent back unchanged). The
  first page is polled every 10 s while the pane is on screen and the app is
  active, backing off 20, 40, 80 to at most 120 s after consecutive errors (a
  longer `Retry-After` wins) and back to 10 s on success. A send goes to the **outbox** (below), which posts
  `…/managers/{seat}/messages` with the slice-3 write rules (exact Origin,
  `x-rsi-csrf`, one re-bootstrap on 401 resending the identical body under the
  same key); a delivered message shows "already queued" when the hub replayed
  it (deduplicated), then the state from the conversation's `operator_messages`
  (delivered, uncertain, withdrawn). `GET
  /api/v2/projects` reports `messages_enabled` (hub #2988): while it is false
  the compose box is disabled under a calm "messaging is off" notice (the
  hub's Remote settings, `m`). The gateway's 403 stays bare (no reason), so
  after a refused send the app re-lists the seats: a seat that is still listed
  (and no `messages_enabled: true`) means messaging is off, from a gateway
  that predates the flag; a seat that is gone (or a 404 or `409
  seat_not_live`) means it rotated, and the chat moves to the new seat.
- **Outbox** (#505). A message is never lost or bounced because the hub is busy,
  offline or mid-turn. Send saves it to the phone first (a JSON file in the
  app's private Application Support folder, readable after first unlock, not in
  backups) and the box is free at once; the Outbox list under the composer shows
  each message as **Queued** (with the reason it waits and a countdown),
  **Sending…**, **Delivered to the hub** or **Not sent**. The message id is made
  once, saved with the text and sent as the `idempotency_key` of every try, so a
  retry after a lost connection or a 502/504 `outcome_unknown` never queues a
  second message: the hub replays the stored row (`deduplicated: true`;
  contracts-v2 "Idempotency"). Retries wait 1 s doubling to 5 min with ±20 %
  jitter (a longer `Retry-After` wins), forever until the hub accepts it, with
  *Try now* and *Discard* on each; a fresh message waits at least 1.1 s after
  the last send (the gateway allows 1 per second). Messages to one seat go out
  in the order written. Only a text the hub refuses as written (400) or a key it
  says is used for other text (409) is **Not sent**, with *Send again* (409 takes
  a new id). A hub with messaging off holds the message (queued, "sends as soon
  as it is turned on") and sends it when the seat list says messaging is on; a
  rotated seat is followed with the same id. A message killed mid-send comes
  back queued and is resent under the same id. A message for another host waits
  for that host. Retries run while the app is active (and at launch or return
  to the foreground); iOS does not run them in the background. A file that
  cannot be read is kept as `outbox.unreadable-<time>.json` and the composer
  says so. Tests: `RSIRemoteTests/OutboxTests.swift`; the fake gateway
  (`tools/reliability`) queues messages with the hub's per-seat key dedupe and
  can drop the link or the reply: `run.sh outbox-offline outbox-busy`.
- **Conversation view** (#502): the manager chat and a session's conversation
  read like a chat app. `Model/Turns.swift` groups events into turns (pure,
  tested in `TurnTests.swift`): the operator's messages are right-aligned
  bubbles, everything the assistant did until the operator speaks again is one
  flowing turn, system notes are centred. Consecutive tool calls fold into one
  row ("Used Bash, Read · 3 calls"; a result pairs with its call by the shared
  call id, `tool_pair_key` in session history and `tool_use_id` in the manager
  chat (hub #3797, #515), and with the oldest call still waiting when the hub
  sends no id) and *Thinking* folds too; both start closed and open by tap (call
  by call for the request and result). The list is lazy (a turn is many small
  rows, newest at the bottom), opens at the newest message, follows new output
  while you are at the bottom and otherwise shows a jump-to-latest button with a
  count; *Load older* keeps your place (session history pages back with the
  served `next_cursor`, the manager chat with `next_before`). The composer is pinned at the bottom
  (field and a round send button); a message still in the outbox shows as a
  pending bubble with its state and Try now / Discard / Send again; it leaves the
  list when the conversation carries the delivered item (its `operator_message_id`
  or the echoed `idempotency_key`) or the hub's delivery record reads delivered. Message
  text is still `MarkdownText` (#159); richer Markdown and images are #503.
  Settings → Appearance picks Dark, Light or Match phone: the TUI themes are
  all dark, Light is a derived Catppuccin-Latte variant of the chosen theme
  (not a TUI port). `-rsi.remote.appearance light|dark|system` sets it from the
  command line. Evidence: `thoughts/shared/research/2026-10-23-rsi-remote-chat-evidence.md`.
- **Provider usage** (#504, `GET /api/v2/providers`, hub #3796; contract in `docs/rsi-remote/contracts-v2.md`). `Model/ProviderUsage.swift` is a tolerant wire type (every field optional, nulls are "not known", unknown fields ignored) plus the pure view model; `Views/ProviderUsageView.swift` lists every provider with its plan windows (utilisation bar, reset countdown), overage status and recent spend (shown as an "API-equivalent estimate"; the hub serves no credit balances), says "No usage data from X yet." for a provider with nothing, and reads `windows_reason` / `spend_reason` (warming, unavailable, stale, truncated) as sentences; it reads on open, on pull-to-refresh and every 60 s, and keeps the last data with its age on a failure. The Dashboard links to it. Launch argument `-providerUsageFixture full|nodata|sparse|empty|failing` shows the offline screen (no network); `tools/reliability/usage-simulator.sh` screenshots it. Evidence: `thoughts/shared/research/2026-10-23-rsi-remote-provider-usage.md`.
- **Waiting**: every session waiting on you, with its open decisions and their
  answer forms.
- **Answering**: the Waiting screen and the session view read
  `.../decision-targets` and show, per decision, option buttons (single or
  multi-select), a free-text field for a question (at most 2048 bytes, no NUL),
  Send and the delivery state. 403/404 means answers are off: the session stays
  read-only. A send posts `.../decisions/answer` (fence, row version, target
  digest) or `.../decisions/answer-pending` with the `x-rsi-csrf` token from
  `POST /auth/session`; a 401 signs in once more and resends under the same
  idempotency key. A 409 discards the draft and refreshes; 429/503 show the
  `Retry-After` wait; 502/504 `outcome_unknown` (or a connection lost after
  sending) says it is not known whether the answer was delivered and locks the
  draft so only the same answer, under the same key, can be retried. The same
  rules as `remote/web/api.js` and `state.js`.
- **Settings**: the host (`hub.<tailnet>.ts.net`), a connection test, the app
  version and the contract versions it speaks (v1 + v2).

Reliability (#350; diagnosis in
`thoughts/shared/research/2026-10-22-rsi-remote-reliability.md`):

- **One sign-in.** Requests that need a session share one sign-in; a 401 for
  an older session does not end a newer one. The gateway's boot cookie is
  single-use, so interleaved sign-ins used to refuse each other with a 403.
- **Under the gateway's permits.** At most 3 requests are in flight, 2 of them
  on the operator routes (v2, decision targets, answers): the gateway refuses
  rather than queues (4 identity checks, 4 reads, 2 operator calls).
- **Reads retry, writes do not.** A GET that meets a busy hub (503), a
  restarting rsid (502), a 504 or a link blip is tried up to 4 times with
  jittered backoff (0.5, 1, 2 s; a `Retry-After` up to 8 s wins, a longer one
  is left to the screen); a bare 403 once more. A 500 or a timeout (15 s) is
  not retried in place: the screen's backoff reads again. Answers and
  messages keep their own rules above.
- **Last good data, with its age.** A failed refresh keeps what each screen
  shows and puts a line on top: what failed ("Hub busy", "Hub restarting",
  "Refused by the hub", "Can't reach the hub", "Hub error"), the age of the
  data and the countdown to the next try. With nothing to show yet the screen
  says the same in full. A project whose sessions could not be read keeps its
  last ones, marked not refreshed.
- **Refreshing.** Projects and Waiting read every minute, the Dashboard every
  30 s, a session every minute, while shown; after a failure 5 s doubling to
  2 min. A screen that reappears within 15 s of a good read reuses it, and
  returning to the foreground reads again what is stale. Waiting reads the
  session lists again itself. A cancelled read (a tab switch, a navigation)
  is not an error.
- **Partial lists.** The gateway lists 50 sessions per project in id order;
  a project with more shows "50+ sessions" and Waiting names it as only
  partly checked.

## Layout

- `project.yml`: the [XcodeGen](https://github.com/yonaskolb/XcodeGen) spec.
  XcodeGen is a build-time tool (`brew install xcodegen`); the generated
  `RSIRemote.xcodeproj` is not committed and is reproducible from the spec.
- `RSIRemote/`: the app (`Model/Wire.swift` reads the V1 result bodies,
  `Model/Answers.swift` the targets, answer requests and status mapping,
  `App/AnswerDrafts.swift` the drafts, `Model/ManagerV2.swift` the v2 seats,
  conversation, message request, refusal mapping, the compose draft and poll
  backoff, `Model/Outbox.swift` the outbox (states, backoff, file store, retry loop),
  `App/ManagerChat.swift` the chat state,
  `Net/RemoteClient.swift` is the gateway client, `App/` and `Views/`).
- `RSIRemoteTests/`: unit tests. They decode every positive V1 response and
  safe-error fixture in `remote/contracts/v1/fixtures/corpus.json`, and
  `AnswerTests.swift` drives the answer pipeline through a `URLProtocol` stub.
  `V2Tests.swift` decodes or maps every `remote/contracts/v2/fixtures/*.json`
  (bundled as the `fixtures` folder) and replays each one through the stub,
  checking the request the client sends against the fixture's.
  `ReliabilityTests.swift` and `ScreenReliabilityTests.swift` cover the
  sign-in, in-flight limits, retries and the screens' freshness.
- `tools/reliability/`: `FakeGateway.swift` is a fault-injecting stand-in for
  the gateway (its sign-in, permits and status mapping, plus busy, restart,
  403, dropped and stalled reads); `run.sh` drives the app's own model and
  client (built for macOS) through each fault and prints what the operator
  would see, and `run.sh soak <host> <minutes>` reads a real gateway the same
  way, logging each failed request; `simulator.sh` runs the real app in a
  throwaway simulator through an rsid outage and takes screenshots. `chat-simulator.sh` does the same for the manager conversation (the fake gateway serves a scripted one: `conversation` sets its size, `append` streams new manager output) in dark, light and 3000 items.

## Test

```sh
scripts/ios/test.sh            # iOS 26.5 simulator, iPhone 17 Pro
```

It prints `ios-test: ok tests=<n> failures=0 ...`; that line is the receipt for
the hub (the hub cannot build Swift).

## Install on the phone

Operator prerequisites (hub #2961): Xcode signed in with your Apple ID (the free
Personal Team; no paid program), the iPhone paired with the Mac once by cable
with Developer Mode on, and Tailscale on the phone.

```sh
scripts/ios/dev-install.sh --dry-run   # print the exact commands
scripts/ios/dev-install.sh             # build, install and launch
```

The phone must be **unlocked** during the build and install: Xcode registers it
with the Personal Team and mounts the developer disk image on the first
install. The first launch may need Settings > General > VPN & Device Management
> trust the developer.

The script reads the team id from `$RSI_IOS_TEAM_ID`, then
`~/.config/rsi-ios/env`, then Xcode's signed-in Personal Team (open
`remote/ios/RSIRemote.xcodeproj` in Xcode once if it is not listed yet). The
device id comes from `--device`, `$RSI_IOS_DEVICE_ID` or `xcrun devicectl list
devices`. Neither is committed or printed (`--show-ids` prints them). The bundle
id is `dev.rsi.remote.<6 hex of the team id's sha256>` unless
`RSI_IOS_BUNDLE_ID` is set. Exit codes: 0 installed, 3 no paired iPhone
reachable, 4 no signing team.

The script needs `jq` (macOS ships it), no Python. To look at a screen on a
phone you cannot tap from the Mac, launch with app arguments after `--`:
`xcrun devicectl device process launch --device <id> --terminate-existing
<bundle> -- -initialTab projects|waiting|settings` (and `-gatewayHost <host>`
for this launch only, to see the unreachable and refused messages; the saved
host is not changed); `-markdownDemo 0|1..7` shows the offline Markdown fixture conversation (all of it, or one answer; `-markdownDemoZoom 1` also opens the zoom view of the inline image; no network, #503); `-outboxFaultSeconds 25` sends the outbox to an unreachable host for 25 s after launch and `-outboxProbe "text"` queues a message for the first project's manager, neither changing the saved host, to watch a message go from queued to delivered, then `xcrun devicectl device capture screenshot
--device <id> --destination x.png` (#492).

## Weekly re-sign

A free profile expires after 7 days, so a launchd user agent on the Mac checks
every 6 hours (`scripts/ios/install-weekly-agent.sh`) and
`scripts/ios/weekly-resign.sh` reinstalls from day 5 after the last success
(`~/.rsi/ios-resign.state`), retrying every run until an install succeeds. Each run
logs one `ok|skip|failed` line to `~/.rsi/logs/ios-resign.log`; exit 3 reads
`failed exit=3 needs-phone`. See `docs/rsi-remote/ios-weekly-resign.md`.

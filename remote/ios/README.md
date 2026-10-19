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
  longer `Retry-After` wins) and back to 10 s on success. A send posts
  `…/managers/{seat}/messages` with the slice-3 write rules (exact Origin,
  `x-rsi-csrf`, one re-bootstrap on 401 resending the identical body under the
  same key); the receipt shows queued or "already queued" (deduplicated), then
  the state from the conversation's `operator_messages` (delivered, uncertain,
  withdrawn). 502/504 `outcome_unknown` or a lost connection says "not known
  whether delivered" and locks the message: only Retry (same key) or Discard.
  400, 409, 429, 503 and bare 502/504 say nothing was sent. `GET
  /api/v2/projects` reports `messages_enabled` (hub #2988): while it is false
  the compose box is disabled under a calm "messaging is off" notice (the
  hub's Remote settings, `m`). The gateway's 403 stays bare (no reason), so
  after a refused send the app re-lists the seats: a seat that is still listed
  (and no `messages_enabled: true`) means messaging is off, from a gateway
  that predates the flag; a seat that is gone (or a 404 or `409
  seat_not_live`) means it rotated, and the chat moves to the new seat.
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

Errors: a 401 signs in again once; a 403 says the device is not allowed; "can't
reach" (Tailscale off, no network, timeout) is shown apart from server errors.

## Layout

- `project.yml`: the [XcodeGen](https://github.com/yonaskolb/XcodeGen) spec.
  XcodeGen is a build-time tool (`brew install xcodegen`); the generated
  `RSIRemote.xcodeproj` is not committed and is reproducible from the spec.
- `RSIRemote/`: the app (`Model/Wire.swift` reads the V1 result bodies,
  `Model/Answers.swift` the targets, answer requests and status mapping,
  `App/AnswerDrafts.swift` the drafts, `Model/ManagerV2.swift` the v2 seats,
  conversation, message request, refusal mapping, draft key rules and poll
  backoff, `App/ManagerChat.swift` the chat state,
  `Net/RemoteClient.swift` is the gateway client, `App/` and `Views/`).
- `RSIRemoteTests/`: unit tests. They decode every positive V1 response and
  safe-error fixture in `remote/contracts/v1/fixtures/corpus.json`, and
  `AnswerTests.swift` drives the answer pipeline through a `URLProtocol` stub.
  `V2Tests.swift` decodes or maps every `remote/contracts/v2/fixtures/*.json`
  (bundled as the `fixtures` folder) and replays each one through the stub,
  checking the request the client sends against the fixture's.

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

## Weekly re-sign

A free profile expires after 7 days, so a launchd user agent on the Mac checks
every 6 hours (`scripts/ios/install-weekly-agent.sh`) and
`scripts/ios/weekly-resign.sh` reinstalls from day 5 after the last success
(`~/.rsi/ios-resign.state`), retrying every run until an install succeeds. Each run
logs one `ok|skip|failed` line to `~/.rsi/logs/ios-resign.log`; exit 3 reads
`failed exit=3 needs-phone`. See `docs/rsi-remote/ios-weekly-resign.md`.

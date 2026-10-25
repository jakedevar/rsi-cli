SHELL := /usr/bin/env bash

NEXTEST_JOBS ?= 8
TEST_BENCHMARK_REPEAT ?= 3
TEST_BENCHMARK_OUT ?= target/test-suite-benchmark/local-nextest-full.json

.PHONY: help release release-install release-install-no-restart desktop desktop-install desktop-install-job desktop-e2e-graph-job release-install-tui relink-release codex-prompts codex-prompts-check claude-commands claude-commands-check \
	check-mirrors test-fast test-full test-serial test-slow-tier test-benchmark scoped-test scoped-test-tmpfs check-touched-shards \
	recursive-dag-live-dogfood-setup recursive-dag-live-dogfood-env \
	recursive-dag-live-dogfood-daemon recursive-dag-live-dogfood-gate \
	recursive-dag-live-dogfood-tui recursive-dag-live-dogfood-claude-login \
	recursive-dag-live-dogfood-status recursive-dag-live-dogfood-commit \
	recursive-dag-live-dogfood-clean dev-rsid-live-dogfood dev-rside-live-dogfood \
	dev-tui-live-dogfood dev-claude-live-dogfood-login dev-commit-live-dogfood \
	dev-status-live-dogfood disk-status clean-shared manual manual-pdf

help:
	@printf '%s\n' \
		'make release          - build release rsi + rsid binaries' \
		'make release-install  - build release rsi + rsid only (never the desktop UI; see desktop-install), refresh ~/.local/bin symlinks, and restart a running rsid at a quiet point (NOW=1 restarts immediately)' \
		'make desktop          - build the desktop UI (frontend + release Tauri binary); fails if node/webkit2gtk are missing' \
		'make desktop-install  - build the desktop UI and install rsi-desktop + app menu entry + icons (macOS: ~/Applications/RSI Desktop.app)' \
		'make desktop-install-job - desktop-install into a throwaway HOME (the desktop-install job recipe, #3239)' \
		'make desktop-e2e-graph-job - real-rsid graph-run desktop E2E with a receipt (the desktop-e2e-graph job recipe, #3279)' \
		'make release-install-no-restart - same, but do not restart rsid (still overwrites the live install; see install-release.sh --stage-dir)' \
		'make release-install-tui - build + relink only the rsi TUI binary; rsid untouched, no restart' \
		'make relink-release   - refresh ~/.local/bin symlinks (and restart rsid) without rebuilding' \
		'make codex-prompts    - sync .codex/prompts into ~/.codex/prompts' \
		'make codex-prompts-check - verify .codex/prompts matches ~/.codex/prompts' \
		'make claude-commands  - sync Codex prompts into .claude/commands' \
		'make claude-commands-check - verify Codex/Claude command parity' \
		'make test-fast        - run every bounded rsid shard, integration/bin target, and doctest (default tier: no slow-tier migration tests)' \
		'make test-slow-tier   - run only the slow tier: the released-migration history tests' \
		'make test-full        - run sharded rsid and the remaining workspace, doctests, and validators' \
		'make test-serial      - run the rsid fast lane with one test worker' \
		'make test-benchmark   - capture repeated full-lane timing evidence' \
		'make manual           - regenerate docs/manual/* and the generated regions of docs/keybindings.md' \
		'make manual-pdf       - render docs/manual/rsi-manual.md to target/rsi-manual.pdf with pandoc' \
		'make client-contract OUT=DIR - export the v1 client contract (keybindings, manual, TUI inventory, settings sources) an out-of-tree client vendors (#3453)' \
		'make disk-status      - show disk hot spots: shared cargo target, sandboxes, ~/.rsi, /tmp quota' \
		'make clean-shared     - cargo clean the shared build cache + worker scratch targets (all projects)' \
		'' \
		'Recursive DAG live dogfood ordered workflow:' \
		'  terminal 1: make dev-rsid-live-dogfood     - create fresh temp fixture and start rsid' \
		'  terminal 2: make dev-claude-live-dogfood-login - login Claude into temp fixture HOME when needed' \
		'  terminal 2: make dev-tui-live-dogfood      - enable gate, print status, and start rsi' \
		'  after session completes: make dev-commit-live-dogfood - commit output and print status' \
		'' \
		'Recursive DAG live dogfood low-level targets:' \
		'make recursive-dag-live-dogfood-setup - build tools and create a temp live dogfood fixture' \
		'make recursive-dag-live-dogfood-daemon - start rsid on the dogfood fixture' \
		'make recursive-dag-live-dogfood-gate - enable the live scheduler control gate' \
		'make recursive-dag-live-dogfood-tui - start rsi on the dogfood fixture' \
		'make recursive-dag-live-dogfood-claude-login - login Claude inside the temp fixture HOME' \
		'make recursive-dag-live-dogfood-status - print dogfood caps, graphs, and attempts' \
		'make recursive-dag-live-dogfood-commit - commit latest completed live attempt output' \
		'make recursive-dag-live-dogfood-clean - remove dogfood fixture; requires CONFIRM=1'

# Disk hot spots behind past EDQUOT / disk-pressure incidents. Note /tmp is a
# tmpfs mounted with usrquota — a per-user quota there fails ALL writes with
# EDQUOT even when the root disk has plenty of space. If writes are failing,
# run this FIRST to catch the culprit before clearing anything.
disk-status:
	@echo '== shared cargo target (~/.cargo/shared-target) =='
	@du -sh $$HOME/.cargo/shared-target 2>/dev/null || echo '  (absent)'
	@echo '== scratch cargo targets (~/.rsi/tmp/cargo-targets) =='
	@du -sh $$HOME/.rsi/tmp/cargo-targets 2>/dev/null || echo '  (absent)'
	@echo '== rsi sandboxes (~/.rsi/sandboxes) =='
	@du -sh $$HOME/.rsi/sandboxes 2>/dev/null || echo '  (absent)'
	@echo '== ~/.rsi total =='
	@du -sh $$HOME/.rsi 2>/dev/null || echo '  (absent)'
	@echo '== /tmp (tmpfs, usrquota) =='
	@df -h /tmp | tail -1
	@du -xsh /tmp/* 2>/dev/null | sort -rh | head -10
	@echo '== root disk =='
	@df -h / | tail -1

# Proper clean of the shared cargo build cache. Every workspace/sandbox routes
# builds through ~/.cargo/shared-target via ~/.cargo/config.toml [build]
# target-dir, so this clears build artifacts for ALL projects at once. Also
# removes the disk-backed scratch target dirs pipeline workers use for
# isolated verification builds (~/.rsi/tmp/cargo-targets). Safe — both are
# pure caches — but the next build of anything is from scratch.
clean-shared:
	@echo 'Before:'; du -sh $$HOME/.cargo/shared-target 2>/dev/null || echo '  (absent)'
	cargo clean
	@rm -rf $$HOME/.rsi/tmp/cargo-targets
	@echo 'After:'; du -sh $$HOME/.cargo/shared-target 2>/dev/null || echo '  (removed)'
	@echo 'Scratch targets: removed'

release:
	./scripts/check-release-seam.sh
	cargo build --release --bin rsi --bin rsid --bin rsi-build-rustc
	# #451: the guard is its own cargo call; one call would unify serde_json/preserve_order into rsid.
	cargo build --release -p rsi-repo-guards --bin rsi-repo-guard

# release-install builds and installs only rsi/rsid (#3454). The desktop UI is a
# separate, explicit path: `make desktop` (build) or `make desktop-install`.
release-install:
	./scripts/install-release.sh

# Explicit opt-in: installs the AWS orphan-sweep systemd --user timer (#3465); release-install never does.
install-cloud-watchdog:
	./scripts/install-cloud-watchdog.sh

release-install-no-restart:
	./scripts/install-release.sh --no-restart

desktop:
	./scripts/install-desktop.sh --build-only

.PHONY: desktop-npm-test desktop-vim-repeat
desktop-npm-test:
	npm --prefix desktop ci --no-audit --no-fund
	npm --prefix desktop test

.PHONY: desktop-browser-loss
desktop-browser-loss:
	npm --prefix desktop ci --no-audit --no-fund
	npm --prefix desktop run build
	cd desktop && node --test tests/smoke-lib.test.mjs tests/audit-views.test.mjs tests/field-names.test.mjs tests/smoke.test.mjs

desktop-vim-repeat: desktop-npm-test
	npm --prefix desktop run test:vim-repeat

desktop-install:
	./scripts/install-desktop.sh --required

# The `desktop-install` job recipe (.rsi/jobs.toml, #3239): the same install into a
# job-owned throwaway HOME/XDG tree that is removed afterwards.
desktop-install-job:
	./scripts/desktop-install-job.sh

# The `desktop-e2e-graph` job recipe (.rsi/jobs.toml, #3279): the graph-run scenario against
# an isolated real rsid plus its e2e-seed binary, with an executed-vs-skipped receipt.
desktop-e2e-graph-job:
	./scripts/desktop-e2e-graph-job.sh

release-install-tui:
	./scripts/install-release.sh --tui-only

relink-release:
	./scripts/install-release.sh --link-only

codex-prompts:
	./scripts/sync-codex-prompts.sh

codex-prompts-check:
	./scripts/sync-codex-prompts.sh --check

claude-commands:
	./.claude/sync-codex-prompts.sh

claude-commands-check:
	./.claude/sync-codex-prompts.sh --check

# The v1 client contract (#3453): `make client-contract OUT=/tmp/c`, then `scripts/export-client-contract.py --check`.
.PHONY: client-contract
client-contract:
	@test -n "$(OUT)" || { echo 'usage: make client-contract OUT=<dir>' >&2; exit 2; }
	python3 scripts/export-client-contract.py --out "$(OUT)"

# Operator manual (Epic M): regenerate the committed manual artifacts and the
# generated regions of docs/keybindings.md from the registries, then re-check.
.PHONY: watchdog-pressure
watchdog-pressure:
	python3 scripts/watchdog-compile-pressure.py

manual:
	RSI_BLESS_MANUAL=1 cargo test -p rsi --lib manual:: -- --quiet
	cargo test -p rsi --lib manual:: -- --quiet

manual-pdf:
	@command -v pandoc >/dev/null || { echo 'pandoc not found; open docs/manual/rsi-manual.html and print to PDF'; exit 1; }
	mkdir -p target
	pandoc docs/manual/rsi-manual.md -o target/rsi-manual.pdf

# Fails when a derived mirror (`.agents/skills/`, Codex prompts, Gemini
# commands) drifts from its canonical source (`.claude/`). Fix with
# `scripts/sync-agent-commands.sh`.
check-mirrors:
	./tools/check-command-mirrors.sh

test-fast: check-mirrors
	./scripts/run-rsid-test-shards.sh fast --jobs $(NEXTEST_JOBS)

test-full: check-mirrors
	./scripts/repo-guard rsid-test-shards --require-gates
	@status=0; \
	./scripts/run-rsid-test-shards.sh full --jobs $(NEXTEST_JOBS) || status=$$?; \
	cargo run -p rsid --bin rsi-model-control-validate --offline || status=$$?; \
	cargo run -p rsid --bin rsi-provider-capability-validate --offline || status=$$?; \
	exit $$status

# Local verification targets and recipes declared in .rsi/jobs.toml (#1558, #1560).
# Scoped verification jobs use params.scoped_test (#1954). The recipe job
# runner already wraps `make` in cargo-slot, which marks its child with
# RSI_CARGO_SLOT_HELD so these scripts do not take a second governor slot.
# Budgets stay under the 20-minute recipe cap. Only BASE (a git rev) can be
# overridden, and only a plain rev token is accepted.
VERIFY_BASE ?= origin/rolling

.PHONY: qa-resume-rsid
qa-resume-rsid:
	@test -z "$$RSI_SCOPED_TEST_FILTERS" || { echo 'qa-resume-rsid uses its local pinned plan, not filters' >&2; exit 2; }
	python3 scripts/qa-resume

.PHONY: rolling-library-qa
# Standalone workspace packages whose tests the full-library QA executes next to
# the rsid ones (#4002). Each runs in seconds; heavier crates stay check-only.
LIBRARY_QA_SMALL_PACKAGES := -p rsi-common -p rsid-core -p rsi-cloud
# The merge queue's async full run after a bounded gate (#2092): every library
# test, plus every bin and integration test target (#2094), since a bounded
# gate also omits reached targets. Runs from the pinned rolling snapshot.
rolling-library-qa:
	@test -z "$$RSI_SCOPED_TEST_FILTERS" || { echo 'rolling-library-qa requires every library test' >&2; exit 2; }
	@# #4002: the small standalone packages run their own tests too (rsi-common held
	@# the #4001 red that a cargo check passed); the measured cost is in the Issue.
	@# Then every workspace package standalone (#2444): the gate checks the
	@# selected packages as one merged invocation, which unifies features.
	@# The tests run first so a standalone red never hides a test red.
	@status=0; \
	cargo nextest run --profile rsid-qa -p rsid -p rsid-store -p rsid-remote-read $(LIBRARY_QA_SMALL_PACKAGES) --lib --bins --tests --ignore-default-filter --no-fail-fast --status-level fail --final-status-level fail -j "$${RSI_LIBRARY_QA_TEST_THREADS:-2}" || status=$$?; \
	scripts/check-standalone-packages.sh || status=$$?; \
	exit $$status

# One selected test pass, using lander tmpfs scratch when available (#1910).
scoped-test:
	@case '$(VERIFY_BASE)' in -*|*[!A-Za-z0-9._/~^-]*|'') echo 'invalid VERIFY_BASE' >&2; exit 2;; esac
	./scripts/scoped-test --base '$(VERIFY_BASE)' --runtime-max-sec 1080 --total-max-sec 1140 --cpu-quota 200

.PHONY: test-deploy-migration-backup
# Focused regression gate for the store-open hook and both deploy rollbacks.
test-deploy-migration-backup:
	./scripts/scoped-test --tmpfs --base origin/rolling --runtime-max-sec 1080 --cpu-quota 200 \
		--filter 'rsid=shard:store-01:test(store::migration_backup)' \
		--filter 'rsid=shard:store-01:test(store_open_)' \
		--filter 'rsid=shard:other-02:test(deploy)' \
		--filter 'rsid=bin:rsid'

.PHONY: test-install-release-shell
# Python-free hermetic installer shell test (HOME, build and launchers faked); #451 covers
# the rsi-repo-guard install, link and own-cargo-invocation cases. CASE="a b" runs those only.
test-install-release-shell:
	bash scripts/tests/test_install_release_start.sh $(CASE)

.PHONY: test-install-release-isolation
# #613: copy/stage fixtures must never reach host lifecycle controls.
test-install-release-isolation:
	bash scripts/tests/test_install_release_stage.sh
	bash scripts/tests/test_install_release_links.sh

# #632/#670: focused Rust installer fixtures: stage, links and provenance.
# All three suites use the shared fail-closed lifecycle/process boundary.
.PHONY: test-install-release-rust-isolation
test-install-release-rust-isolation:
	cargo test --locked -p rsi-repo-guards --test install_release_stage --test install_release_links --test install_release_provenance

# Compatibility for explicit parity-only callers; not a second default pass.
scoped-test-tmpfs:
	@case '$(VERIFY_BASE)' in -*|*[!A-Za-z0-9._/~^-]*|'') echo 'invalid VERIFY_BASE' >&2; exit 2;; esac
	./scripts/scoped-test --tmpfs --base '$(VERIFY_BASE)' --runtime-max-sec 1080 --total-max-sec 1140 --cpu-quota 200

check-touched-shards:
	@test -z "$$RSI_SCOPED_TEST_FILTERS" || { echo 'check-touched-shards takes no focused filters (#1584); use the scoped_test job' >&2; exit 2; }
	@case '$(VERIFY_BASE)' in -*|*[!A-Za-z0-9._/~^-]*|'') echo 'invalid VERIFY_BASE' >&2; exit 2;; esac
	./scripts/check-touched-shards --compile --base '$(VERIFY_BASE)'

# #3621: refresh the scoped_test receipt conformance corpus
# (crates/rsi-scoped-receipt/tests/golden/receipts.jsonl) from the receipts the
# Python runner printed in real scoped_test jobs, read-only from the hub's job
# table. Goldens are never written by hand: submit the recipe job
# `scoped-receipt-corpus`, then commit the file it wrote.
.PHONY: scoped-receipt-corpus
scoped-receipt-corpus:
	@rows="$${TMPDIR:-/tmp}/scoped-receipt-rows.$$$$" && \
	sqlite3 -readonly -separator "$$(printf '\t')" "$$HOME/.rsi/rsi.db" \
		"SELECT id, created_at, json_extract(result_json, '$$.receipt') FROM agent_jobs WHERE json_extract(params_json, '$$.scoped_test') IS NOT NULL AND json_type(result_json, '$$.receipt') = 'object' ORDER BY sequence" \
		> "$$rows" && \
	cargo run -q -p rsi-scoped-receipt --bin rsi-scoped-receipt -- corpus \
		--out crates/rsi-scoped-receipt/tests/golden/receipts.jsonl --home "$$HOME" --forbid "$$(id -un)" < "$$rows"; \
	status=$$?; rm -f "$$rows"; exit $$status

.PHONY: target-pool-build
target-pool-build:
	./scripts/repo-guard target-pool build --repo .

test-serial:
	./scripts/run-rsid-test-shards.sh fast --jobs 1

.PHONY: test-timings
# The default tier (#2270): the slow tier is left out by the profile's default-filter.
test-timings:
	cargo nextest run --profile test-timings -p rsid -p rsid-store -p rsid-remote-read --lib

.PHONY: test-slow-tier
# Only the slow tier (#2270): the released-migration tests .config/slow-tier.toml names.
# `not default()` is the complement of the profile's default-filter. Run it after a change
# to a migration, the migration runner or a slow test; the full QA runs it on every sweep.
test-slow-tier:
	@cargo nextest --version >/dev/null 2>&1 || { echo "test-slow-tier: runner_unavailable: cargo-nextest is not installed (cargo install --locked cargo-nextest); the scoped_test job runs the slow tier in full when a migration changed" >&2; exit 2; }
	cargo nextest run --profile test-timings -p rsid-store --lib --ignore-default-filter -E 'not default()'

test-benchmark:
	./scripts/test-suite-benchmark.sh capture \
		--label local-nextest-full \
		--probe nextest-full \
		--repeat $(TEST_BENCHMARK_REPEAT) \
		--threads $(NEXTEST_JOBS) \
		--out $(TEST_BENCHMARK_OUT)

.PHONY: e2e-tui
e2e-tui:
	./scripts/e2e.sh

# #2421: the editing-mode live-switch test, 20 consecutive runs.
.PHONY: e2e-editing-mode-live-switch-repeat
e2e-editing-mode-live-switch-repeat:
	E2E_REPEAT=20 E2E_FILTER=editing_mode::test_e2e_editing_mode_live_switch_without_restart ./scripts/e2e.sh

.PHONY: e2e-fresh-install-appoint-manager
e2e-fresh-install-appoint-manager:
	E2E_FILTER=fresh_install::test_e2e_fresh_install_appoint_project_manager ./scripts/e2e.sh

dev-rsid-live-dogfood:
	./scripts/recursive-dag-live-dogfood.sh daemon-fresh

dev-rside-live-dogfood: dev-rsid-live-dogfood

dev-tui-live-dogfood:
	./scripts/recursive-dag-live-dogfood.sh tui-ready

dev-claude-live-dogfood-login:
	./scripts/recursive-dag-live-dogfood.sh claude-login

dev-commit-live-dogfood:
	./scripts/recursive-dag-live-dogfood.sh commit-status

dev-status-live-dogfood:
	./scripts/recursive-dag-live-dogfood.sh status

recursive-dag-live-dogfood-setup:
	./scripts/recursive-dag-live-dogfood.sh setup

recursive-dag-live-dogfood-env:
	./scripts/recursive-dag-live-dogfood.sh env

recursive-dag-live-dogfood-daemon:
	./scripts/recursive-dag-live-dogfood.sh daemon

recursive-dag-live-dogfood-gate:
	./scripts/recursive-dag-live-dogfood.sh gate

recursive-dag-live-dogfood-tui:
	./scripts/recursive-dag-live-dogfood.sh tui

recursive-dag-live-dogfood-claude-login:
	./scripts/recursive-dag-live-dogfood.sh claude-login

recursive-dag-live-dogfood-status:
	./scripts/recursive-dag-live-dogfood.sh status

recursive-dag-live-dogfood-commit:
	./scripts/recursive-dag-live-dogfood.sh commit

recursive-dag-live-dogfood-clean:
	./scripts/recursive-dag-live-dogfood.sh clean

.PHONY: check-socket-activation-macos
check-socket-activation-macos:
	./scripts/check-socket-activation-macos.sh

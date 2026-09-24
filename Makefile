SHELL := /usr/bin/env bash

NEXTEST_JOBS ?= 8
TEST_BENCHMARK_REPEAT ?= 3
TEST_BENCHMARK_OUT ?= target/test-suite-benchmark/local-nextest-full.json

.PHONY: help release release-install release-install-no-restart release-install-tui relink-release codex-prompts codex-prompts-check claude-commands claude-commands-check \
	test-fast test-full test-serial test-benchmark \
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
		'make release-install  - build release binaries, refresh ~/.local/bin symlinks, and restart a running rsid' \
		'make release-install-no-restart - same, but leave any running rsid alone' \
		'make release-install-tui - build + relink only the rsi TUI binary; rsid untouched, no restart' \
		'make relink-release   - refresh ~/.local/bin symlinks (and restart rsid) without rebuilding' \
		'make codex-prompts    - sync .codex/prompts into ~/.codex/prompts' \
		'make codex-prompts-check - verify .codex/prompts matches ~/.codex/prompts' \
		'make claude-commands  - sync Codex prompts into .claude/commands' \
		'make claude-commands-check - verify Codex/Claude command parity' \
		'make test-fast        - run the complete rsid library suite with bounded nextest workers' \
		'make test-full        - run workspace, doctests, model-control validator, and provider-capability validator' \
		'make test-serial      - run the rsid library suite through the legacy serial path' \
		'make test-benchmark   - capture repeated full-lane timing evidence' \
		'make manual           - regenerate docs/manual/* and the generated regions of docs/keybindings.md' \
		'make manual-pdf       - render docs/manual/rsi-manual.md to target/rsi-manual.pdf with pandoc' \
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
	cargo build --release --bin rsi --bin rsid

release-install:
	./scripts/install-release.sh

release-install-no-restart:
	./scripts/install-release.sh --no-restart

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

# Operator manual (Epic M): regenerate the committed manual artifacts and the
# generated regions of docs/keybindings.md from the registries, then re-check.
manual:
	RSI_BLESS_MANUAL=1 cargo test -p rsi --lib manual:: -- --quiet
	cargo test -p rsi --lib manual:: -- --quiet

manual-pdf:
	@command -v pandoc >/dev/null || { echo 'pandoc not found; open docs/manual/rsi-manual.html and print to PDF'; exit 1; }
	mkdir -p target
	pandoc docs/manual/rsi-manual.md -o target/rsi-manual.pdf

test-fast:
	cargo nextest run --profile rsid-fast -p rsid --lib -j $(NEXTEST_JOBS)

test-full:
	@status=0; \
	cargo nextest run --profile ci-full --workspace -j $(NEXTEST_JOBS) || status=$$?; \
	cargo test --workspace --doc || status=$$?; \
	cargo run -p rsid --bin rsi-model-control-validate --offline || status=$$?; \
	cargo run -p rsid --bin rsi-provider-capability-validate --offline || status=$$?; \
	exit $$status

test-serial:
	cargo test -p rsid --lib -- --test-threads=1

test-benchmark:
	./scripts/test-suite-benchmark.sh capture \
		--label local-nextest-full \
		--probe nextest-full \
		--repeat $(TEST_BENCHMARK_REPEAT) \
		--threads $(NEXTEST_JOBS) \
		--out $(TEST_BENCHMARK_OUT)

.PHONY: e2e-tui
e2e-tui:
	RSI_E2E=1 cargo test -p rsi --test e2e_tui -- --nocapture

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

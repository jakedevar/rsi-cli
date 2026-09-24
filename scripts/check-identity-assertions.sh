#!/bin/sh
# Guard: a test must never assert that an entity's OWN name is absent from the UI.
#
# Pattern this catches (and only this):
#   1. A test assigns a string literal to an identity field
#      (`title`/`name`/`query`/`display_title`/`short_summary`), then
#   2. The same file asserts that literal does NOT appear in rendered output
#      (`!<expr>.contains("<that literal>")`).
#
# That combination pins data loss as a requirement: it encodes "this entity's
# own name must not be visible", which defeats the next agent sent to fix it.
# Precedent: a renderer test asserted a Group's own title was absent, making
# every Group unfindable in the session list. See
# thoughts/shared/plans/2026-09-01-container-top-sorted-item.md
#
# Deliberately NARROW. Asserting that chrome, key hints, leaked secrets, or
# removed affordances are absent is legitimate and is NOT flagged -- those
# needles are never assigned as an entity's identity.
#
# Usage: scripts/check-identity-assertions.sh [paths...]   (default: crates/)
# Exit 1 on any finding.

set -u
ROOT="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
cd "$ROOT" || exit 2
exec python3 tools/check_identity_assertions.py "$@"

#!/usr/bin/env bash
# RSI-006: validate the eval corpus shape.
# Asserts every ticket directory has the required files and every
# expected.json parses as JSON. Exits 0 on success, 1 on any failure.

set -euo pipefail

ROOT="${RSI_EVAL_CORPUS_ROOT:-eval/corpus}"

if [ ! -d "$ROOT" ]; then
    echo "error: corpus root $ROOT does not exist" >&2
    exit 1
fi

failed=0
count=0
for d in "$ROOT"/*/; do
    [ -d "$d" ] || continue
    count=$((count + 1))
    name=$(basename "$d")

    for f in ticket.md prompt.txt expected.json; do
        if [ ! -f "$d/$f" ]; then
            echo "error: $name is missing $f" >&2
            failed=$((failed + 1))
        fi
    done

    if [ -f "$d/expected.json" ]; then
        if ! jq . "$d/expected.json" >/dev/null 2>&1; then
            echo "error: $name/expected.json is not valid JSON" >&2
            failed=$((failed + 1))
        fi
    fi
done

if [ "$count" -ne 10 ]; then
    echo "error: expected 10 corpus directories, found $count" >&2
    failed=$((failed + 1))
fi

if [ "$failed" -ne 0 ]; then
    echo "corpus validation failed with $failed error(s)" >&2
    exit 1
fi

echo "corpus OK: $count tickets validated"

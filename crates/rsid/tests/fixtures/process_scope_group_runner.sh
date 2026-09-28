#!/bin/sh
while [ "$1" != -- ]; do shift; done
shift
"$@" <&0 &
child=$!
if [ -n "${FAKE_CHILD_PID_FILE:-}" ]; then printf '%s\n' "$child" > "$FAKE_CHILD_PID_FILE"; fi
trap 'kill -TERM "$child" 2>/dev/null; wait "$child"; exit 143' TERM
wait "$child"

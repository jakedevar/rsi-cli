The synthetic CLI cases in `cases.json`, plus the deterministic IPv4 boundary
matrix in `tests/legacy_golden.rs`, are paired with `golden.json`. There are 78 cases.
They cover both refusal and success output, exact exit codes and stderr, default
file selection, ordering, missing files, CRLF, identity string escaping, the
preceding-ASCII identity rule, private operator config, thoughts, binary/extension
skips, the egress fixture exceptions and public-IP classification boundaries.

Trusted capture: daemon AgentSubmitJob
`1e9fcd5b-2da0-43b7-8543-ba19293aac08`, exit 0, completed
2026-10-22T06:58:27.519562454Z. The job ran the one exact Rust integration test
`legacy_capture` (1 passed), extracting the three unchanged original guard blobs
from source `177af2414bfda86e7fa3dab218133c4b25ee27c9` with `git show` and
executing them with `/usr/bin/python3` (the host's CPython 3.14). Each original
and Rust CLI received the same synthetic files and arguments; output and exit
code were compared before recording the golden. No Python source was generated
or modified. The capture's original libtest output is in that job's log.

IPv4 tokens are losslessly encoded as `<ipv4:a,b,c,d>` in the golden to keep
public IP literals out of tracked fixtures. The same normalization is applied
to Rust output. Input addresses are assembled from octets. No operator personal
values are used in fixtures; synthetic account-policy tests inject fictional
names into the same matching function that production uses.

Ordinary regression runs use Rust and the recorded golden only. The capture
test is now ignored and requires an explicit daemon job and exact test filter
(plus libtest's `--ignored`); it is not an acceptance lane or a CI dependency.

Adopted from preserved source `6eb7cace8da7ea43c22c6a88d05ea0c36923114f`
for the canonical `rsi-repo-guard` binary. Only the synthetic corpus and Rust
golden replay are reused; the duplicate guard implementation is not integrated.

"""Conservative classification of interrupted compile/test phases (#516)."""

from pathlib import Path
import re


SIGNAL = re.compile(r"\bsignal:\s*(?:2|9|15)\b|^SCOPED_PROCESS_SIGNAL SIG(?:INT|KILL|TERM)$")
ANSI = re.compile(r"\x1b\[[0-9;]*m")
COMPILER_ERROR = re.compile(r"^\s*error(?:\[E\d+\])?:")
CARGO_SUMMARY = re.compile(r"^\s*error: (?:could not compile `|test failed, to rerun pass )")
TEST_FAILURE = re.compile(r"^SCOPED_(?:FAILURE |TEST_EXIT )|^\s*(?:test .* \.\.\. |test result: )FAILED\b|^\s*FAILED\s*$|^\s*(?:FAIL|TIMEOUT|ABORT)\s+\[")


class PanicEvidence:
    """Keep panic/assertion evidence until its test gets an explicit ok verdict.

    A SCOPED_END only accounts for execution; it is not a passing verdict. In
    particular, an interrupted unwind can end before libtest prints FAILED.
    """

    START = re.compile(r"^test (\S+)(?: - should panic)? \.\.\.(?: (.*))?$")
    PANIC = re.compile(r"thread '([^']+)'(?: \(\d+\))? panicked at\b")
    ASSERTION = re.compile(r"\bassertion(?:\s+[`'\"].*?[`'\"])?\s+failed\b")
    UNKNOWN = "<unattributed-panic-or-assertion>"

    def __init__(self):
        self.active = None
        self.unresolved = set()

    def line(self, raw):
        text = ANSI.sub("", raw).strip()
        if text.startswith("SCOPED_START "):
            self.active = text.split(" ", 1)[1]
        start = self.START.match(text)
        if start:
            self.active = start[1]
        panic = self.PANIC.search(text)
        if panic or self.ASSERTION.search(text):
            name = self.active
            if name is None and panic and panic[1] not in ("main", "<unnamed>"):
                name = panic[1]
            self.unresolved.add(name or self.UNKNOWN)
        verdict = start[2] if start else text
        if verdict == "ok":
            self.unresolved.discard(self.active)
            self.active = None


def signal_only(lines):
    """Require termination evidence and veto ordinary compiler/test failures anywhere."""
    interrupted = False
    panics = PanicEvidence()
    for raw in lines:
        line = ANSI.sub("", raw).rstrip()
        panics.line(line)
        if ((COMPILER_ERROR.match(line) and not CARGO_SUMMARY.match(line))
                or TEST_FAILURE.search(line)
                or re.search(r"due to \d+ previous errors?", line)
                or re.search(r"\bexit status: [1-9]\d*\b", line)):
            return False
        interrupted |= bool(SIGNAL.search(line))
    return interrupted and not panics.unresolved


def infrastructure_interrupted(receipt):
    """Read full candidate package logs, never a tail that could hide a real error.

    Missing evidence and unrelated failures fail closed. The classification describes
    observed signals, not their sender (a test can also signal itself).
    """
    if (receipt.get("dry_run") or receipt.get("failures") or receipt.get("unmatched_filters")
            or receipt.get("derive_failure") or receipt.get("ok_reason")
            or (receipt.get("warnings") or {}).get("status") in
            ("new_warnings", "baseline_raised", "unavailable")
            or any(item.get("status") not in ("ok", "skipped")
                   for item in (receipt.get("static_guards") or {}).values())
            or any(item.get("status") != "passed"
                   for item in receipt.get("python", []) + receipt.get("desktop", []))):
        return False
    failed = False
    for package in receipt.get("packages", []):
        status = package.get("status")
        if status in ("passed", "not applicable"):
            continue
        if status != "failed" or package.get("failed_tests"):
            return False
        log_dir = receipt.get("log_dir")
        if not log_dir:
            return False
        try:
            with (Path(log_dir) / (package["package"] + ".log")).open(errors="replace") as log:
                if not signal_only(log):
                    return False
        except (OSError, KeyError):
            return False
        failed = True
    return failed

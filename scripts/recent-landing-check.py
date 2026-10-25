"""Bounded, informational detection of stale-copy removals from a trial merge."""

import re
import shlex
import subprocess
import time


LINE_LIMIT = 2000
FILE_LIMIT = 64
TIME_LIMIT = 10
OPT_OUT = "Allow-Recent-Landing-Removals"
STDERR_LIMIT = 1024
ARG_LIMIT = 200


def git_diagnostic(argv, *, step=None, kind="git_exit", exit_code=None, stderr=None,
                   timeout_seconds=None):
    """Keep every argv token; explicitly abbreviate hashes/long paths, never slice a command.

    Argument count is bounded by the existing deleted-line budget. Stderr is a tail so
    Git's final fatal error survives verbose output. A null exit code means no exit was
    observed (timeout or launch failure), rather than a fabricated subprocess status.
    """
    original = [str(arg) for arg in argv]
    display = []
    for arg in original:
        arg = re.sub(r"(?<![0-9a-f])[0-9a-f]{40,64}(?![0-9a-f])",
                     lambda match: match[0][:12] + "...", arg)
        if len(arg) > ARG_LIMIT:
            arg = arg[:80] + "...<argument shortened>..." + arg[-80:]
        display.append(arg)
    if isinstance(stderr, bytes):
        stderr = stderr.decode("utf-8", "replace")
    stderr = stderr or ""
    tail = "\n".join(stderr.splitlines()[-8:])[-STDERR_LIMIT:]
    diagnostic = {"step": step or (original[3] if len(original) > 3 else "git"),
                  "kind": kind, "argv": display, "argv_shortened": display != original,
                  "exit_code": exit_code, "stderr_tail": tail,
                  "stderr_shortened": tail != stderr}
    if timeout_seconds is not None:
        diagnostic["timeout_seconds"] = timeout_seconds
    return diagnostic


class GitStepError(ValueError):
    def __init__(self, diagnostic):
        self.diagnostic = diagnostic
        super().__init__(f"{diagnostic['step']}: {diagnostic['kind']}; "
                         f"exit_code={diagnostic['exit_code']}; "
                         f"argv={shlex.join(diagnostic['argv'])}; "
                         f"{diagnostic['stderr_tail'] or 'stderr empty'}")


def recent_landing_deletions(repo, source, rolling, merged_tree, *,
                             line_limit=LINE_LIMIT, file_limit=FILE_LIMIT,
                             time_limit=TIME_LIMIT):
    """Blame deleted rolling lines in `git diff rolling merged_tree`, never a three-dot diff.

    The first own commit is the oldest source first-parent commit not reachable from rolling.
    A trailer on any source-only commit acknowledges all intentional recent removals. Findings
    are grouped by path and introducing commit; ranges use rolling's one-based line numbers.
    Partial/unavailable results remain informational, including on missing history or Git errors.
    """
    result = {"status": "complete", "warnings": [], "rolling": rolling, "lines_blamed": 0,
              "line_limit": line_limit, "file_limit": file_limit}
    deadline = time.monotonic() + time_limit
    last_argv = []
    last_exit_code = None

    def git(*args, ancestor=False):
        nonlocal last_argv, last_exit_code
        last_argv = ["git", "-C", str(repo), *args]
        last_exit_code = None
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise GitStepError(git_diagnostic(last_argv, kind="timeout_budget",
                                             timeout_seconds=time_limit))
        try:
            proc = subprocess.run(last_argv, capture_output=True, timeout=remaining)
        except subprocess.TimeoutExpired as error:
            raise GitStepError(git_diagnostic(last_argv, kind="timeout_budget",
                                             stderr=error.stderr, timeout_seconds=time_limit)) from error
        except OSError as error:
            raise GitStepError(git_diagnostic(last_argv, kind="launch_error", stderr=str(error))) from error
        last_exit_code = proc.returncode
        if proc.returncode and not (ancestor and proc.returncode == 1):
            raise GitStepError(git_diagnostic(last_argv, exit_code=proc.returncode, stderr=proc.stderr))
        return proc

    try:
        own = git("rev-list", "--first-parent", "--reverse", f"{rolling}..{source}").stdout.splitlines()
        if not own:
            return result
        first = own[0].decode("ascii")
        result["first_own_commit"] = first
        trailers = git("log", "--no-notes", "--no-show-signature",
                       "--format=%H%x00%(trailers:key=" + OPT_OUT + ",valueonly)%x00",
                       f"{rolling}..{source}").stdout.split(b"\0")
        for index in range(1, len(trailers), 2):
            if b"true" in [line.strip().lower() for line in trailers[index].splitlines()]:
                result.update(status="opted_out", opt_out_commit=trailers[index - 1].strip().decode("ascii"))
                return result
        # -z and literal pathspecs preserve tabs, newlines, non-UTF8 names and pathspec syntax.
        stats = git("diff", "--numstat", "-z", "--no-renames", "--no-ext-diff", "--no-textconv",
                    rolling, merged_tree).stdout.split(b"\0")
        ancestry = {}
        files = 0
        for entry in stats:
            if not entry:
                continue
            _added, deleted, raw_path = entry.split(b"\t", 2)
            if deleted in (b"-", b"0"):  # Binary files and pure additions have nothing to blame.
                continue
            if files >= file_limit or result["lines_blamed"] >= line_limit:
                budget = "file" if files >= file_limit else "deleted-line"
                result.update(status="partial", reason=f"{budget} limit reached",
                              diagnostic=git_diagnostic([], step=f"{budget} budget", kind="limit"))
                break
            files += 1
            path = raw_path.decode("utf-8", "surrogateescape")
            patch = git("diff", "--unified=0", "--no-color", "--no-renames", "--no-ext-diff",
                        "--no-textconv", rolling, merged_tree, "--", ":(literal)" + path).stdout
            ranges = []
            remaining = line_limit - result["lines_blamed"]
            for match in re.finditer(rb"^@@ -(\d+)(?:,(\d+))? \+", patch, re.MULTILINE):
                start, count = int(match[1]), int(match[2]) if match[2] is not None else 1
                take = min(count, remaining)
                if take:
                    ranges.append((start, start + take - 1))
                    remaining -= take
                if take < count:
                    result.update(status="partial", reason="deleted-line limit reached",
                                  diagnostic=git_diagnostic([], step="deleted-line budget", kind="limit"))
                    break
            if not ranges:
                continue
            blame_args = [arg for start, end in ranges for arg in ("-L", f"{start},{end}")]
            # Incremental output batches all selected hunks. Stop at the source's ancestry:
            # boundary lines cannot warn, so tracing their older introducers wastes the budget.
            blame = git("blame", "--incremental", "--ignore-revs-file", "", *blame_args,
                        f"{first}..{rolling}", "--", path).stdout
            result["lines_blamed"] += sum(end - start + 1 for start, end in ranges)
            findings = {}
            for match in re.finditer(rb"^([0-9a-f]{40,64}) \d+ (\d+) (\d+)$", blame, re.MULTILINE):
                commit, start, count = match[1].decode("ascii"), int(match[2]), int(match[3])
                if commit not in ancestry:
                    ancestry[commit] = git("merge-base", "--is-ancestor", commit, first,
                                           ancestor=True).returncode == 0
                if not ancestry[commit]:
                    findings.setdefault(commit, []).append([start, start + count - 1])
            for commit, lines in sorted(findings.items()):
                result["warnings"].append({"path": path, "line_count": sum(b - a + 1 for a, b in lines),
                                           "introducing_commit": commit, "line_ranges": sorted(lines)})
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        diagnostic = (error.diagnostic if isinstance(error, GitStepError) else
                      git_diagnostic(last_argv, kind="invalid_output", exit_code=last_exit_code,
                                     stderr=str(error)))
        result.update(status="partial" if result["lines_blamed"] else "unavailable",
                      reason=str(GitStepError(diagnostic)), diagnostic=diagnostic)
    return result

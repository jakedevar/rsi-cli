"""Real Git graphs for the warning-only scoped-test recent-landing check."""

import contextlib
import io
import json
from pathlib import Path
import runpy
import subprocess
import sys
import time
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from scratch_git import scratch_git, scratch_tempdir

SCRIPTS = Path(__file__).resolve().parents[1]
CHECK = runpy.run_path(str(SCRIPTS / "recent-landing-check.py"))


class RecentLandingCheckTest(unittest.TestCase):
    def setUp(self):
        self.temp = scratch_tempdir(prefix="recent-landing-")
        self.repo = Path(self.temp.__enter__())
        self.addCleanup(self.temp.__exit__, None, None, None)
        self.path = "notes [v1].txt"
        self.old = "base\nold line\nend\n"
        self.git("init", "-q", "-b", "rolling")
        self.write(self.path, self.old)
        self.write("image.bin", b"\0old")
        self.commit("root")
        self.git("checkout", "-qb", "worker")
        self.write("worker.txt", "work\n")
        self.first = self.commit("first own change")
        self.git("checkout", "-q", "rolling")
        self.write(self.path, "base\nold line\nrecent one\nrecent two\nend\n")
        self.write("image.bin", b"\0new")
        self.introducing = self.commit("recent landing")
        self.git("update-ref", "refs/remotes/origin/rolling", "HEAD")
        self.git("checkout", "-q", "worker")
        self.git("merge", "-q", "--no-edit", "rolling")

    def git(self, *args):
        return scratch_git(self.repo, "-c", "user.email=a@b", "-c", "user.name=a",
                           *args).stdout.strip()

    def write(self, path, content):
        (self.repo / path).write_bytes(content.encode() if isinstance(content, str) else content)

    def commit(self, message):
        # Explicit paths, including only this isolated fixture's files.
        paths = [self.path, "image.bin"]
        if (self.repo / "worker.txt").exists():
            paths.append("worker.txt")
        self.git("add", *paths)
        self.git("commit", "-qm", message)
        return self.git("rev-parse", "HEAD")

    def check(self, **kwargs):
        source = self.git("rev-parse", "HEAD")
        rolling = self.git("rev-parse", "origin/rolling")
        tree = self.git("merge-tree", "--write-tree", rolling, source)
        return CHECK["recent_landing_deletions"](self.repo, source, rolling, tree, **kwargs)

    def overwrite(self, message="replace with old copy"):
        self.write(self.path, self.old)
        self.write("image.bin", b"\0old")
        return self.commit(message)

    def test_stale_copy_after_merge_names_rolling_lines_and_introducer(self):
        self.overwrite()
        result = self.check()
        self.assertEqual(result["status"], "complete")
        self.assertEqual(result["first_own_commit"], self.first)
        self.assertEqual(result["lines_blamed"], 2)  # Binary removal is skipped.
        self.assertEqual(result["warnings"], [{"path": self.path, "line_count": 2,
                          "introducing_commit": self.introducing, "line_ranges": [[3, 4]]}])

    def test_ordinary_edit_of_old_lines_warns_nothing(self):
        self.write(self.path, (self.repo / self.path).read_text().replace("old line", "edited old line"))
        self.commit("ordinary edit")
        result = self.check()
        self.assertEqual(result["status"], "complete")
        self.assertEqual(result["lines_blamed"], 1)
        self.assertEqual(result["warnings"], [])

    def test_whole_file_removal_counts_only_recent_lines(self):
        self.git("rm", self.path)
        self.git("commit", "-qm", "remove whole file")
        result = self.check()
        self.assertEqual(result["lines_blamed"], 5)
        self.assertEqual(result["warnings"], [{"path": self.path, "line_count": 2,
                          "introducing_commit": self.introducing, "line_ranges": [[3, 4]]}])

    def test_own_commit_trailer_suppresses_findings(self):
        source = self.overwrite("intentional removal\n\nAllow-Recent-Landing-Removals: true")
        result = self.check()
        self.assertEqual(result["status"], "opted_out")
        self.assertEqual(result["opt_out_commit"], source)
        self.assertEqual(result["warnings"], [])
        self.assertEqual(result["lines_blamed"], 0)

    def test_trailer_in_earlier_own_commit_applies_to_whole_source(self):
        self.overwrite("intentional removal\n\nAllow-Recent-Landing-Removals: true")
        marker = self.git("rev-parse", "HEAD")
        self.write("worker.txt", "more work\n")
        self.commit("later own change")
        self.assertEqual(self.check()["opt_out_commit"], marker)

    def test_inherited_trailer_does_not_suppress_warning(self):
        self.git("checkout", "-q", "rolling")
        self.git("commit", "--allow-empty", "-qm", "unrelated intent\n\nAllow-Recent-Landing-Removals: true")
        self.git("update-ref", "refs/remotes/origin/rolling", "HEAD")
        self.git("checkout", "-q", "worker")
        self.git("merge", "-q", "--no-edit", "rolling")
        self.overwrite()
        self.assertEqual(self.check()["warnings"][0]["introducing_commit"], self.introducing)

    def test_marker_in_body_is_not_an_opt_out(self):
        self.overwrite("removal\n\nAllow-Recent-Landing-Removals: true\nthis is prose, not trailers")
        self.assertEqual(self.check()["warnings"][0]["line_count"], 2)

    def test_line_cap_reports_partial_and_blames_only_selected_lines(self):
        self.overwrite()
        result = self.check(line_limit=1)
        self.assertEqual((result["status"], result["lines_blamed"]), ("partial", 1))
        self.assertEqual(result["warnings"][0]["line_ranges"], [[3, 3]])

    def test_file_and_time_caps_report_incomplete_coverage(self):
        self.overwrite()
        limited = self.check(file_limit=0)
        self.assertEqual(limited["status"], "partial")
        self.assertEqual(limited["diagnostic"]["step"], "file budget")
        result = self.check(time_limit=0)
        self.assertEqual((result["status"], result["warnings"]), ("unavailable", []))
        self.assertEqual(result["diagnostic"]["kind"], "timeout_budget")
        self.assertEqual(result["diagnostic"]["step"], "rev-list")
        self.assertIsNone(result["diagnostic"]["exit_code"])
        self.assertEqual(result["diagnostic"]["timeout_seconds"], 0)

    def test_failing_git_steps_retain_argv_exit_code_and_stderr_tail(self):
        self.overwrite()
        run = subprocess.run
        for step in ("rev-list", "log", "diff", "blame", "merge-base"):
            with self.subTest(step=step):
                attempted = []

                def fail(argv, **kwargs):
                    if argv[3] == step:
                        attempted.append(argv)
                        return subprocess.CompletedProcess(argv, 128, b"", b"noise\n" * 1000 +
                                                           b"fatal: missing history\n")
                    return run(argv, **kwargs)

                with patch.object(subprocess, "run", side_effect=fail):
                    result = self.check()
                self.assertEqual(result["status"], "partial" if step == "merge-base" else "unavailable")
                detail = result["diagnostic"]
                self.assertEqual((detail["step"], detail["exit_code"]), (step, 128))
                self.assertEqual(detail["argv"], CHECK["git_diagnostic"](attempted[0])["argv"])
                self.assertEqual(detail["argv"][:4], ["git", "-C", str(self.repo), step])
                self.assertTrue(detail["stderr_tail"].endswith("fatal: missing history"))
                self.assertLessEqual(len(detail["stderr_tail"]), CHECK["STDERR_LIMIT"])
                self.assertTrue(detail["stderr_shortened"])
                self.assertIn("exit_code=128", result["reason"])

    def test_failed_blame_diagnosis_is_identical_in_receipt_and_log_and_warning_only(self):
        self.overwrite()
        runner = runpy.run_path(str(SCRIPTS / "scoped-test"))
        receipt = runner["RECEIPT"]
        receipt.update(head=self.git("rev-parse", "HEAD"), candidate_repo=str(self.repo),
                       dry_run=False, emit=True)
        run = subprocess.run

        def fail(argv, **kwargs):
            if argv[3] == "blame":
                return subprocess.CompletedProcess(argv, 128, b"", b"fatal: bad object\n")
            return run(argv, **kwargs)

        output = io.StringIO()
        with patch.object(subprocess, "run", side_effect=fail), contextlib.redirect_stdout(output):
            runner["record_merge_onto_rolling"]()
            runner["emit_receipt"](0)
        lines = output.getvalue().splitlines()
        logged = json.loads(lines[0].split("reverts_recent_landing ", 1)[1])
        emitted = json.loads(lines[-1].split(" ", 1)[1])
        self.assertEqual(logged, emitted["reverts_recent_landing"])
        self.assertEqual(logged["diagnostic"]["step"], "blame")
        self.assertEqual(logged["diagnostic"]["exit_code"], 128)
        self.assertEqual(logged["diagnostic"]["stderr_tail"], "fatal: bad object")
        self.assertEqual(logged["diagnostic"]["argv"][-2:], ["--", self.path])
        self.assertEqual((emitted["exit_code"], emitted["ok"]), (0, True))

    def test_blame_timeout_keeps_full_token_list_and_partial_stderr(self):
        self.overwrite()
        run = subprocess.run

        def timeout(argv, **kwargs):
            if argv[3] == "blame":
                raise subprocess.TimeoutExpired(argv, kwargs["timeout"], stderr=b"history scan stalled\n")
            return run(argv, **kwargs)

        with patch.object(subprocess, "run", side_effect=timeout):
            result = self.check()
        detail = result["diagnostic"]
        self.assertEqual((detail["step"], detail["kind"]), ("blame", "timeout_budget"))
        self.assertEqual(detail["argv"][-2:], ["--", self.path])
        self.assertIsNone(detail["exit_code"])
        self.assertEqual(detail["stderr_tail"], "history scan stalled")
        self.assertEqual(detail["timeout_seconds"], CHECK["TIME_LIMIT"])

    def test_long_arguments_are_deliberately_shortened_without_losing_later_tokens(self):
        argv = ["git", "-C", "/long/" + "repo/" * 100, "blame", "-L", "29,29", "-L", "610,610",
                "a" * 40, "--", "nested/" * 100 + "file.py"]
        detail = CHECK["git_diagnostic"](argv)
        self.assertEqual(len(detail["argv"]), len(argv))
        self.assertEqual(detail["argv"][4:8], ["-L", "29,29", "-L", "610,610"])
        self.assertEqual(detail["argv"][8], "a" * 12 + "...")
        self.assertTrue(detail["argv"][-1].endswith("file.py"))
        self.assertTrue(detail["argv_shortened"])
        self.assertTrue(all(len(arg) <= CHECK["ARG_LIMIT"] for arg in detail["argv"]))

    def test_failed_trial_merge_is_diagnosed_in_advisory_log_and_receipt(self):
        runner = runpy.run_path(str(SCRIPTS / "scoped-test"))
        receipt = runner["RECEIPT"]
        receipt.update(head="HEAD", candidate_repo=str(self.repo), dry_run=False, emit=True)
        run = subprocess.run

        def fail(argv, **kwargs):
            if argv[3] == "merge-tree":
                return subprocess.CompletedProcess(argv, 129, "", "fatal: trial merge failed\n")
            return run(argv, **kwargs)

        output = io.StringIO()
        with patch.object(subprocess, "run", side_effect=fail), contextlib.redirect_stdout(output):
            runner["record_merge_onto_rolling"]()
            runner["emit_receipt"](0)
        lines = output.getvalue().splitlines()
        logged = json.loads(lines[0].split("reverts_recent_landing ", 1)[1])
        emitted = json.loads(lines[-1].split(" ", 1)[1])
        self.assertEqual(logged, emitted["reverts_recent_landing"])
        self.assertEqual(logged["diagnostic"]["step"], "trial merge")
        self.assertEqual(logged["diagnostic"]["argv"][3], "merge-tree")
        self.assertEqual(logged["diagnostic"]["exit_code"], 129)
        self.assertEqual(logged["diagnostic"]["stderr_tail"], "fatal: trial merge failed")
        self.assertEqual((emitted["exit_code"], emitted["ok"]), (0, True))

    def test_all_ranges_for_one_file_share_one_blame(self):
        self.write(self.path, "base\nrecent one\nrecent two\n")
        self.commit("separate deletion hunks")
        function = CHECK["recent_landing_deletions"]
        run = function.__globals__["subprocess"].run
        with patch.object(function.__globals__["subprocess"], "run", wraps=run) as calls:
            self.check()
        blame = [call.args[0] for call in calls.call_args_list if "blame" in call.args[0]]
        self.assertEqual(len(blame), 1)
        self.assertEqual(blame[0].count("-L"), 2)

    def test_large_historical_file_stops_at_source_ancestry_within_budget(self):
        self.git("checkout", "-q", "rolling")
        old = "".join(f"historical line {line}\n" for line in range(1, 50001))
        for version in range(24):
            self.write(self.path, old + f"history version {version}\n")
            self.commit(f"historical edit {version}")
            if version == 0:
                obsolete_blob = self.git("rev-parse", f"HEAD:{self.path}")
        self.git("checkout", "-qb", "large-worker")
        self.write("worker.txt", "new source\n")
        self.commit("first large-file source change")
        self.git("checkout", "-q", "rolling")
        current = (self.repo / self.path).read_text()
        self.write(self.path, current + "recent addition\n")
        introducing = self.commit("recent large-file landing")
        self.git("update-ref", "refs/remotes/origin/rolling", "HEAD")
        self.git("checkout", "-q", "large-worker")
        self.git("merge", "-q", "--no-edit", "rolling")
        self.write(self.path, current.replace("historical line 3951\n", ""))
        self.commit("delete an old line and a recent line")

        # Only this scratch repo's oldest file object is removed. A bounded probe needs
        # the source boundary's blob, but never this irrelevant pre-source history.
        (self.repo / ".git" / "objects" / obsolete_blob[:2] / obsolete_blob[2:]).unlink()
        started = time.monotonic()
        result = self.check()
        self.assertLess(time.monotonic() - started, CHECK["TIME_LIMIT"])
        self.assertEqual((result["status"], result["lines_blamed"]), ("complete", 2))
        self.assertEqual(result["warnings"], [{"path": self.path, "line_count": 1,
                          "introducing_commit": introducing, "line_ranges": [[50002, 50002]]}])

    def test_receipt_and_log_include_warning_without_changing_exit_code(self):
        self.overwrite()
        runner = runpy.run_path(str(SCRIPTS / "scoped-test"))
        receipt = runner["RECEIPT"]
        receipt.update(head=self.git("rev-parse", "HEAD"), candidate_repo=str(self.repo),
                       dry_run=False, emit=True)
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            runner["record_merge_onto_rolling"]()
            runner["emit_receipt"](0)
        self.assertIs(receipt["merges_cleanly_onto_rolling"], True)
        self.assertEqual(receipt["reverts_recent_landing"]["warnings"][0]["line_count"], 2)
        self.assertIn("scoped-test: reverts_recent_landing", output.getvalue())
        self.assertIn('"exit_code":0,"ok":true', output.getvalue().splitlines()[-1])

    def test_unmerged_rolling_additions_are_not_source_deletions(self):
        # Undo only the fixture merge by naming the original own commit as the source.
        rolling = self.git("rev-parse", "origin/rolling")
        tree = self.git("merge-tree", "--write-tree", rolling, self.first)
        result = CHECK["recent_landing_deletions"](self.repo, self.first, rolling, tree)
        self.assertEqual((result["status"], result["lines_blamed"], result["warnings"]),
                         ("complete", 0, []))

    def test_multiple_merge_bases_use_the_net_trial_merge_delta(self):
        # Criss-cross merges retain two bases: rolling's recent landing and the first worker
        # commit. The source then deletes lines received through its independent merge.
        self.git("checkout", "-q", "rolling")
        self.git("merge", "-q", "--no-edit", self.first)
        self.git("update-ref", "refs/remotes/origin/rolling", "HEAD")
        self.git("checkout", "-q", "worker")
        self.overwrite()
        bases = self.git("merge-base", "--all", "origin/rolling", "HEAD").splitlines()
        self.assertEqual(set(bases), {self.introducing, self.first})
        result = self.check()
        self.assertEqual(result["status"], "complete")
        self.assertEqual(result["lines_blamed"], 2)
        # Rolling already contains the initial own change in this graph. The remaining
        # first own commit is the worker merge, which includes the introducing commit.
        self.assertEqual(result["warnings"], [])

    def test_blame_ignore_config_cannot_hide_introducer(self):
        ignore = self.repo / "ignore-revs"
        ignore.write_text(self.introducing + "\n")
        self.git("config", "blame.ignoreRevsFile", str(ignore))
        self.overwrite()
        self.assertEqual(self.check()["warnings"][0]["introducing_commit"], self.introducing)


if __name__ == "__main__":
    unittest.main()

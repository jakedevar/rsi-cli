"""Tests for the pinned Terminal-Bench 2.0 definition (#3175). Stdlib unittest, no network."""

import collections
import contextlib
import io
import json
import os
import pathlib
import sys
import tempfile
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent.parent))

import leak_audit  # noqa: E402
import pin_check  # noqa: E402
import refresh_dataset  # noqa: E402
import split  # noqa: E402
import tb2lib  # noqa: E402

# The lists of plan appendix A (thoughts/shared/plans/2026-10-19-issue-3135-m3-benchmark-adapter.md).
PLAN_SEARCH = """break-filter-js-from-html caffe-cifar-10 code-from-image constraints-scheduling
db-wal-recovery dna-assembly extract-elf extract-moves-from-video feal-differential-cryptanalysis
financial-document-processor fix-git git-leak-recovery git-multibranch headless-terminal
hf-model-inference install-windows-3.11 merge-diff-arc-agi-task multi-source-data-merger
openssl-selfsigned-cert password-recovery path-tracing-reverse polyglot-rust-c pytorch-model-recovery
raman-fitting regex-chess rstan-to-pystan sparql-university sqlite-with-gcov
torch-pipeline-parallelism winning-avg-corewars""".split()
PLAN_HELD_OUT = """adaptive-rejection-sampler bn-fit-modify build-cython-ext build-pmars build-pov-ray
cancel-async-tasks chess-best-move circuit-fibsqrt cobol-modernization compile-compcert
configure-git-webserver count-dataset-tokens crack-7z-hash custom-memory-heap-crash
distribution-search dna-insert feal-linear-cryptanalysis filter-js-from-html fix-code-vulnerability
fix-ocaml-gc gcode-to-text gpt2-codegolf kv-store-grpc large-scale-text-editing largest-eigenval
llm-inference-batching-scheduler log-summary-date-ranges mailman make-doom-for-mips
make-mips-interpreter mcmc-sampling-stan model-extraction-relu-logits modernize-scientific-stack
mteb-leaderboard mteb-retrieve nginx-request-logging overfull-hbox path-tracing polyglot-c-py
portfolio-optimization protein-assembly prove-plus-comm pypi-server pytorch-model-cli qemu-alpine-ssh
qemu-startup query-optimize regex-log reshard-c4-data sam-cell-seg sanitize-git-repo
schemelike-metacircular-eval sqlite-db-truncate torch-tensor-parallelism train-fasttext tune-mjcf
video-processing vulnerable-secret write-compressor""".split()
PLAN_SMOKE = """merge-diff-arc-agi-task openssl-selfsigned-cert sparql-university sqlite-with-gcov
torch-pipeline-parallelism""".split()
PLAN_CALIBRATION = """break-filter-js-from-html caffe-cifar-10 dna-assembly extract-elf git-multibranch
hf-model-inference password-recovery polyglot-rust-c raman-fitting rstan-to-pystan""".split()

HEAVY = {"mteb-leaderboard", "mteb-retrieve", "hf-model-inference", "pytorch-model-recovery"}


def row(name, difficulty="medium", category="c", image_bytes=100, agent=900.0, sha="0" * 64):
    return {
        "name": name, "difficulty": difficulty, "category": category, "cpus": 1, "memory_mb": 2048,
        "agent_timeout_sec": agent, "verifier_timeout_sec": 900.0, "image": f"x/{name}:1",
        "image_digest": "sha256:" + "a" * 64, "image_bytes": image_bytes, "task_toml_sha256": sha,
    }


class DatasetAndSplitTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.header, cls.tasks, cls.shards = tb2lib.load_dataset()
        cls.split = tb2lib.load_split()
        cls.by_name = {t["name"]: t for t in cls.tasks}

    def test_header_pins_repo_commit_and_harbor(self):
        self.assertEqual(self.header["commit"], "2fd12b88aafdd04a52c298e3940bcb189f9766d6")
        self.assertEqual(self.header["harbor_version"], "0.24.0")
        self.assertEqual(self.header["repo"], "https://github.com/harbor-framework/terminal-bench-2")
        self.assertEqual(self.header["task_count"], 89)

    def test_every_task_is_fully_pinned(self):
        self.assertEqual(len(self.tasks), 89)
        for t in self.tasks:
            self.assertRegex(t["image_digest"], r"^sha256:[0-9a-f]{64}$", t["name"])
            self.assertRegex(t["task_toml_sha256"], r"^[0-9a-f]{64}$", t["name"])
            self.assertTrue(t["image"].startswith("alexgshaw/"), t["name"])
            self.assertGreater(t["image_bytes"], 0)
            self.assertGreater(t["agent_timeout_sec"], 0)
            self.assertGreater(t["verifier_timeout_sec"], 0)
        self.assertEqual(collections.Counter(t["difficulty"] for t in self.tasks),
                         {"easy": 4, "medium": 55, "hard": 30})
        self.assertEqual(max(t["agent_timeout_sec"] for t in self.tasks), 12000.0)
        self.assertEqual(self.by_name["build-pov-ray"]["agent_timeout_sec"], 12000.0)
        self.assertEqual(round(sum(t["image_bytes"] for t in self.tasks) / 1e9, 1), 49.0)

    def test_split_is_deterministic_and_equals_the_plan_lists(self):
        first = split.make_split(self.tasks)
        self.assertEqual(first, split.make_split(list(reversed(self.tasks))))
        self.assertEqual(first["seed"], 3135)
        self.assertEqual(first["search"], sorted(PLAN_SEARCH))
        self.assertEqual(first["held_out"], sorted(PLAN_HELD_OUT))
        self.assertEqual(first["smoke"], sorted(PLAN_SMOKE))
        self.assertEqual(first["calibration"], sorted(PLAN_CALIBRATION))

    def test_committed_split_json_is_the_regenerated_split(self):
        self.assertEqual(self.split, split.make_split(self.tasks))
        self.assertEqual(self.split["algorithm"], split.ALGORITHM)
        self.assertEqual(split.main([]), 0)

    def test_split_shape(self):
        s = self.split
        self.assertEqual((len(s["search"]), len(s["held_out"]), len(s["smoke"]), len(s["calibration"])),
                         (30, 59, 5, 10))
        self.assertFalse(set(s["search"]) & set(s["held_out"]))
        self.assertEqual(set(s["search"]) | set(s["held_out"]), set(self.by_name))
        self.assertTrue(set(s["smoke"]) <= set(s["search"]))
        self.assertTrue(set(s["calibration"]) <= set(s["search"]))
        self.assertFalse(set(s["smoke"]) & set(s["calibration"]))
        diff = collections.Counter(self.by_name[n]["difficulty"] for n in s["search"])
        self.assertEqual((diff["easy"], diff["medium"], diff["hard"]), (1, 19, 10))

    def test_split_check_reports_drift(self):
        with tempfile.TemporaryDirectory() as d:
            bad = pathlib.Path(d) / "split.json"
            data = dict(self.split)
            data["search"] = data["search"][:-1]
            bad.write_text(json.dumps(data, indent=2) + "\n")
            with contextlib.redirect_stderr(io.StringIO()) as err:
                self.assertEqual(split.main(["--split", str(bad)]), 1)
            self.assertIn("drift", err.getvalue())

    def test_shard_plan_is_regenerated_and_bounded(self):
        self.assertEqual(self.shards, [dict(s) for s in split.plan_shards(self.tasks, self.split)])
        cap = self.header["shard_cap_bytes"]
        self.assertEqual(cap, 12 * 10**9)
        seen = []
        for s in self.shards:
            seen += s["tasks"]
            self.assertEqual(s["bytes"], sum(self.by_name[n]["image_bytes"] for n in s["tasks"]))
            self.assertIn(s["split"], ("search", "held_out"))
            self.assertTrue(set(s["tasks"]) <= set(self.split[s["split"]]), s["name"])
            if s["heavy"]:
                self.assertEqual(len(s["tasks"]), 1, s["name"])
            else:
                self.assertLessEqual(s["bytes"], cap, s["name"])
        self.assertEqual(sorted(seen), sorted(self.by_name))
        self.assertEqual({n for s in self.shards if s["heavy"] for n in s["tasks"]}, HEAVY)

    def test_shard_planner_packs_and_isolates_heavy_images(self):
        gb = 10**9
        rows = [row("a", image_bytes=7 * gb), row("big", image_bytes=9 * gb)] + [
            row(n, image_bytes=int(3.5 * gb)) for n in ("b", "c", "d", "e")]
        sp = {"search": ["a", "big", "b", "c", "d", "e"], "held_out": []}
        shards = split.plan_shards(rows, sp)
        self.assertEqual([(s["tasks"], s["heavy"]) for s in shards],
                         [(["b", "c", "d"], False), (["e"], False), (["big"], True), (["a"], True)])

    def test_refresh_render_round_trips(self):
        rows = [row("a"), row("b")]
        sp = split.make_split([row(f"t{i}", d) for i, d in enumerate(["easy"] * 2 + ["medium"] * 20 + ["hard"] * 10)])
        text = refresh_dataset.render(rows, [])
        with tempfile.TemporaryDirectory() as d:
            p = pathlib.Path(d) / "dataset.toml"
            p.write_text(text)
            header, tasks, _ = tb2lib.load_dataset(p)
        self.assertEqual(header["commit"], refresh_dataset.COMMIT)
        self.assertEqual(tasks, rows)
        self.assertEqual(len(sp["search"]), 30)


def write_checkout(root, names, body="[metadata]\ndifficulty = \"easy\"\n"):
    for n in names:
        (root / n).mkdir(parents=True)
        (root / n / "task.toml").write_text(body + f"# {n}\n")
    return root


def fixture_dataset(root, names):
    rows = []
    for n in names:
        rows.append(row(n, sha=tb2lib.task_toml_sha256(root / n / "task.toml")))
    return rows


HEADER = {"commit": "c" * 40}


class PinCheckTest(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.root = pathlib.Path(self._tmp.name) / "checkout"
        self.root.mkdir()
        write_checkout(self.root, ["alpha", "beta"])
        self.rows = fixture_dataset(self.root, ["alpha", "beta"])

    def test_clean_checkout_has_no_drift(self):
        self.assertEqual(pin_check.check_checkout(self.root, HEADER, self.rows), [])

    def test_crlf_checkout_is_not_drift(self):
        p = self.root / "alpha" / "task.toml"
        p.write_bytes(p.read_bytes().replace(b"\n", b"\r\n"))
        self.assertEqual(pin_check.check_checkout(self.root, HEADER, self.rows), [])

    def test_missing_extra_and_changed_tasks_are_named(self):
        (self.root / "alpha" / "task.toml").write_text("changed = true\n")
        (self.root / "beta" / "task.toml").unlink()
        write_checkout(self.root, ["gamma"])
        drift = pin_check.check_checkout(self.root, HEADER, self.rows)
        self.assertEqual(len(drift), 3, drift)
        self.assertTrue(any("missing" in d and "beta" in d for d in drift), drift)
        self.assertTrue(any("not in the pinned set" in d and "gamma" in d for d in drift), drift)
        self.assertTrue(any("task.toml changed" in d and "alpha" in d for d in drift), drift)

    def test_git_checkout_at_wrong_commit_is_drift(self):
        (self.root / ".git").mkdir()

        def fake_run(cmd, **kw):
            return type("R", (), {"returncode": 0, "stdout": "d" * 40 + "\n"})()

        drift = pin_check.check_checkout(self.root, HEADER, self.rows, run=fake_run)
        self.assertEqual(len(drift), 1)
        self.assertIn("pinned", drift[0])

    def test_images_match_mismatch_and_unpulled(self):
        want = "sha256:" + "a" * 64
        local = {"x/alpha:1": [f"x/alpha@{want}"],
                 "x/beta:1": ["x/beta@sha256:" + "b" * 64],
                 }
        inspect = lambda tag: local.get(tag)  # noqa: E731
        rows = self.rows + [row("gamma")]
        drift = pin_check.check_images(rows, inspect=inspect)
        self.assertEqual(len(drift), 1, drift)
        self.assertIn("image digest drift", drift[0])
        self.assertIn("beta", drift[0])
        strict = pin_check.check_images(rows, require_pulled=True, inspect=inspect)
        self.assertEqual(len(strict), 2)
        self.assertTrue(any("not pulled" in d and "gamma" in d for d in strict))
        self.assertEqual(pin_check.check_images(rows, names={"alpha"}, inspect=inspect), [])

    def test_cli_exit_codes(self):
        ds = pathlib.Path(self._tmp.name) / "dataset.toml"
        ds.write_text(refresh_dataset.render(self.rows, []))
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            self.assertEqual(pin_check.main([str(self.root), "--dataset", str(ds)]), 0)
        (self.root / "alpha" / "task.toml").write_text("changed = true\n")
        out = io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(pin_check.main([str(self.root), "--dataset", str(ds)]), 1)
        self.assertIn("DRIFT task.toml changed: alpha", out.getvalue())
        with contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(pin_check.main([str(self.root / "nope"), "--dataset", str(ds)]), 2)


EXPECTED = "The quarterly lantern total is 48213 widgets"


class LeakAuditTest(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        root = pathlib.Path(self._tmp.name)
        self.checkout = root / "checkout"
        names = ["regex-log", "path-tracing", "path-tracing-reverse", "search-only"]
        write_checkout(self.checkout, names)
        # held-out tasks carry distinctive tests; search-only carries its own
        for n, fname, text in [
            ("regex-log", "test_regexlog_unique.py", f'def test_x():\n    assert out == "{EXPECTED}"\n    """a docstring that is long enough to ignore"""\n'),
            ("path-tracing", "test_outputs.py", 'assert True\n'),
            ("path-tracing-reverse", "test_outputs.py", 'assert True\n'),
            ("search-only", "test_outputs.py", 'assert s == "search side expected output text"\n'),
        ]:
            (self.checkout / n / "tests").mkdir()
            (self.checkout / n / "tests" / fname).write_text(text)
            (self.checkout / n / "tests" / "test.sh").write_text("uv run pytest -rA /tests/shared_boiler_line\n")
        rows = fixture_dataset(self.checkout, names)
        self.ds = root / "dataset.toml"
        self.ds.write_text(refresh_dataset.render(rows, []))
        self.sp = root / "split.json"
        self.sp.write_text(json.dumps({"held_out": ["regex-log", "path-tracing"], "search": ["path-tracing-reverse", "search-only"]}))
        _, tasks, _ = tb2lib.load_dataset(self.ds)
        self.auditor = leak_audit.Auditor(leak_audit.build_terms(self.checkout, tasks, ["regex-log", "path-tracing"]))

    def hits(self, diff, **kw):
        return leak_audit.audit(self.auditor, leak_audit.added_lines(diff), **kw)

    @staticmethod
    def diff(path, *added, removed=()):
        body = "".join(f"-{r}\n" for r in removed) + "".join(f"+{a}\n" for a in added)
        return f"diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n@@ -1,1 +1,2 @@\n context\n{body}"

    def test_held_out_task_name_is_a_hit(self):
        hits = self.hits(self.diff("prompts/system.md", "Remember the regex-log trick."))
        self.assertEqual([(h[0], h[1], h[2], h[3], h[4]) for h in hits],
                         [("prompts/system.md", 2, "task-name", "regex-log", "regex-log")])

    def test_search_task_name_and_prefix_collisions_are_misses(self):
        self.assertEqual(self.hits(self.diff("p.md", "search-only and path-tracing-reverse are fair")), [])
        self.assertEqual(self.hits(self.diff("p.md", "xregex-log regex-logs my-path-tracing")), [])
        hit = self.hits(self.diff("p.md", "see (path-tracing)."))
        self.assertEqual([h[3] for h in hit], ["path-tracing"])

    def test_removed_and_context_lines_are_not_audited(self):
        self.assertEqual(self.hits(self.diff("p.md", "fine", removed=["regex-log"])), [])

    def test_expected_output_literal_and_test_file_name_are_hits(self):
        hits = self.hits(self.diff("memory/notes.txt", f"{EXPECTED} apparently", "see test_regexlog_unique.py"))
        kinds = sorted((h[2], h[4]) for h in hits)
        self.assertEqual(kinds, [("literal", "regex-log"), ("test-file", "regex-log")])

    def test_search_and_shared_strings_are_misses(self):
        text = "search side expected output text; test_outputs.py; shared_boiler_line; a docstring that is long enough"
        self.assertEqual(self.hits(self.diff("memory/notes.txt", text)), [])

    def test_pin_files_are_excluded_by_default_but_other_tb2_files_are_not(self):
        for name in ("dataset.toml", "split.json"):
            d = self.diff(f"eval/bench/tb2/{name}", "regex-log")
            self.assertEqual(self.hits(d), [])
            self.assertEqual(len(self.hits(d, excludes=[])), 1)
        self.assertEqual(len(self.hits(self.diff("eval/bench/tb2/note.md", "regex-log"))), 1)

    def test_candidates_are_audited_even_with_a_custom_exclude(self):
        d = self.diff("eval/bench/tb2/candidates/c1/overlay.toml", "regex-log")
        self.assertEqual(len(self.hits(d)), 1)
        self.assertEqual(len(self.hits(d, excludes=["eval/bench/tb2/*"])), 1)

    # -- candidate directories, path spelling and the generality mode (#3193) --

    def repo_with_candidate(self):
        repo = pathlib.Path(self._tmp.name) / "repo"
        cand = repo / "eval/bench/tb2/candidates/c1"
        cand.mkdir(parents=True)
        (cand / "overlay.toml").write_text('instructions = "custom.md"\n')
        (cand / "custom.md").write_text("be careful\n")
        (cand / "notes.md").write_text("fine\n")
        pin = repo / "eval/bench/tb2"
        (pin / "dataset.toml").write_text('name = "regex-log"\n')
        (pin / "split.json").write_text('{"held_out": ["regex-log"]}\n')
        return repo, cand

    def run_main(self, args):
        out = io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(io.StringIO()):
            rc = leak_audit.main(["--dataset", str(self.ds), "--split", str(self.sp)] + args)
        return rc, out.getvalue()

    def test_held_out_name_in_candidate_instructions_is_a_hit_for_any_path_spelling(self):
        repo, cand = self.repo_with_candidate()
        instr = cand / "instructions.md"
        instr.write_text("ok\nuse the regex-log trick\n")
        rel = "eval/bench/tb2/candidates/c1/instructions.md"
        # absolute path
        rc, out = self.run_main(["--repo-root", str(repo), "--files", str(instr)])
        self.assertEqual(rc, 1)
        self.assertIn(f"HIT {rel}:2 task-name 'regex-log'", out)
        # relative path (resolved against the working directory)
        cwd = pathlib.Path.cwd()
        try:
            os.chdir(repo)
            rc, out = self.run_main(["--repo-root", str(repo), "--files", rel])
            rc_dot, _ = self.run_main(["--repo-root", str(repo), "--files", "./" + rel])
        finally:
            os.chdir(cwd)
        self.assertEqual((rc, rc_dot), (1, 1))
        self.assertIn(f"HIT {rel}:2", out)
        # a diff names the repo-relative path
        d = pathlib.Path(self._tmp.name) / "c.diff"
        d.write_text(self.diff(rel, "regex-log"))
        self.assertEqual(self.run_main(["--repo-root", str(repo), "--diff", str(d)])[0], 1)

    def test_dataset_and_split_stay_excluded_as_absolute_paths(self):
        repo, _ = self.repo_with_candidate()
        pin = repo / "eval/bench/tb2"
        rc, out = self.run_main(["--repo-root", str(repo), "--files", str(pin / "dataset.toml"), str(pin / "split.json")])
        self.assertEqual((rc, out), (0, "leak_audit: clean\n"))

    def test_candidate_directory_audits_overlay_instructions_and_notes(self):
        repo, cand = self.repo_with_candidate()
        rc, out = self.run_main(["--repo-root", str(repo), "--files", str(cand)])
        self.assertEqual(rc, 0)
        (cand / "notes.md").write_text("fine\nregex-log was easy\n")
        (cand / "custom.md").write_text("path-tracing first\n")
        (cand / "overlay.toml").write_text('instructions = "custom.md"\n# regex-log\n')
        rc, out = self.run_main(["--repo-root", str(repo), "--files", str(cand)])
        self.assertEqual(rc, 1)
        base = "eval/bench/tb2/candidates/c1"
        for hit in (f"{base}/notes.md:2", f"{base}/custom.md:1", f"{base}/overlay.toml:2"):
            self.assertIn(hit, out)
        empty = pathlib.Path(self._tmp.name) / "empty"
        empty.mkdir()
        self.assertEqual(self.run_main(["--files", str(empty)])[0], 2)

    def test_search_task_name_is_a_generality_hit_only_in_search_or_all_mode(self):
        repo, cand = self.repo_with_candidate()
        f = cand / "instructions.md"
        f.write_text("tip from path-tracing-reverse and search-only\n")
        base = ["--repo-root", str(repo), "--checkout", str(self.checkout), "--files", str(f)]
        self.assertEqual(self.run_main(base), (0, "leak_audit: clean\n"))
        self.assertEqual(self.run_main(base + ["--terms", "held_out"])[0], 0)
        for mode in ("search", "all"):
            rc, out = self.run_main(base + ["--terms", mode])
            self.assertEqual(rc, 1, mode)
            self.assertIn("GENERALITY eval/bench/tb2/candidates/c1/instructions.md:1 task-name 'path-tracing-reverse'", out)
            self.assertIn("task-name 'search-only'", out)
            self.assertNotIn("HIT ", out)

    def test_generality_flags_search_only_test_file_names(self):
        repo, cand = self.repo_with_candidate()
        f = cand / "instructions.md"
        (self.checkout / "search-only" / "tests" / "test_searchonly_unique.py").write_text("x = 1\n")
        f.write_text("see test_searchonly_unique.py and test_outputs.py\n")
        rc, out = self.run_main(["--repo-root", str(repo), "--checkout", str(self.checkout), "--terms", "search", "--files", str(f)])
        self.assertEqual(rc, 1)
        self.assertIn("test-file 'test_searchonly_unique.py' (search-only)", out)
        self.assertNotIn("test_outputs.py'", out)

    def test_all_mode_reports_held_out_and_generality_and_near_misses_stay_clean(self):
        repo, cand = self.repo_with_candidate()
        f = cand / "instructions.md"
        f.write_text("path-tracing-reverse only\n")
        # the search name contains the held-out `path-tracing`: not a held-out hit
        rc, out = self.run_main(["--repo-root", str(repo), "--terms", "held_out", "--files", str(f)])
        self.assertEqual(rc, 0)
        f.write_text("path-tracing and path-tracing-reverse\n")
        rc, out = self.run_main(["--repo-root", str(repo), "--terms", "all", "--files", str(f)])
        self.assertEqual(rc, 1)
        self.assertEqual(sum(ln.startswith("HIT ") for ln in out.splitlines()), 1)
        self.assertEqual(sum(ln.startswith("GENERALITY ") for ln in out.splitlines()), 1)

    def test_file_list_mode_and_exit_codes(self):
        dirty = pathlib.Path(self._tmp.name) / "tool_description.md"
        dirty.write_text("fine line\nuse regex-log answers\n")
        clean = pathlib.Path(self._tmp.name) / "clean.md"
        clean.write_text("nothing to see\n")
        common = ["--dataset", str(self.ds), "--split", str(self.sp), "--checkout", str(self.checkout)]
        out = io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(leak_audit.main(common + ["--files", str(dirty), str(clean)]), 1)
        self.assertIn("tool_description.md:2 task-name 'regex-log'", out.getvalue())
        with contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(leak_audit.main(common + ["--files", str(clean)]), 0)
        with contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(leak_audit.main(common + ["--files", str(clean.parent / "missing.md")]), 2)

    def test_diff_file_mode(self):
        d = pathlib.Path(self._tmp.name) / "c.diff"
        d.write_text(self.diff("harness/prompt.md", "path-tracing is easy"))
        with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
            rc = leak_audit.main(["--dataset", str(self.ds), "--split", str(self.sp), "--diff", str(d)])
        self.assertEqual(rc, 1)

    def test_real_held_out_names_are_all_audited(self):
        _, tasks, _ = tb2lib.load_dataset()
        held = tb2lib.load_split()["held_out"]
        auditor = leak_audit.Auditor(leak_audit.build_terms(None, tasks, held))
        for name in held:
            self.assertTrue(auditor.scan_line(f"tip: {name}."), name)
        for name in tb2lib.load_split()["search"]:
            self.assertEqual([h for h in auditor.scan_line(f"tip: {name}.") if h[1] == name], [], name)


if __name__ == "__main__":
    unittest.main()

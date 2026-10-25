"""Exercise the installed pre-push hook with real local Git pushes."""

from concurrent.futures import ThreadPoolExecutor
import datetime
import json
import os
from pathlib import Path
import subprocess
import unittest
import uuid

from scripts.scratch_git import clean_environment, scratch_git, scratch_tempdir


HOOKS = Path(__file__).resolve().parent / "git-hooks"
AGENT_MARKERS = ("RSI_SESSION_ID", "RSI_SESSION_TOKEN", "CLAUDE_AGENT_ROLE")


def git(
    cwd: Path,
    *args: str,
    agent_marker: str | None = None,
    lander_marker: bool = False,
    extra_env: dict[str, str] | None = None,
) -> subprocess.CompletedProcess[str]:
    env = os.environ.copy()
    env.pop("RSI_SKIP_STATIC_GUARDS", None)
    env.update(extra_env or {})
    for marker in AGENT_MARKERS:
        env.pop(marker, None)
    if agent_marker:
        env[agent_marker] = "test-session"
    if lander_marker:
        env["RSI_ROLLING_LANDER"] = "1"
    return scratch_git(cwd, *args, env=env, check=False)


class PrePushTest(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = scratch_tempdir(prefix="rsi-pre-push-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.repo = self.root / "source"
        self.remote = self.root / "remote.git"
        self.repo.mkdir()
        self.remote.mkdir()
        self.assert_git(self.repo, "init", "-q", "-b", "scratch")
        self.assert_git(self.remote, "init", "-q", "--bare")
        self.assert_git(self.repo, "config", "user.name", "Test")
        self.assert_git(self.repo, "config", "user.email", "test@example.invalid")
        (self.repo / "file.txt").write_text("fixture\n")
        self.assert_git(self.repo, "add", "file.txt")
        self.assert_git(self.repo, "commit", "-q", "-m", "fixture")
        self.assert_git(self.repo, "config", "core.hooksPath", str(HOOKS))
        self.assert_git(self.repo, "remote", "add", "publish", str(self.remote))
        self.consent_home = self.root / "home"
        self.consent_dir = self.consent_home / ".rsi" / "git-push-consents"
        self.consent_dir.mkdir(parents=True)

    def grant_main_push(self, **overrides: object) -> dict:
        # AuthorizeMainPush's on-disk v1 contract (git_push_consent.rs).
        record = {
            "version": 1, "id": str(uuid.uuid4()),
            "remote_url": str(self.remote), "ref": "refs/heads/main",
            "target_sha": git(self.repo, "rev-parse", "HEAD").stdout.strip(),
            "created_at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
            "expires_at": (datetime.datetime.now(datetime.timezone.utc)
                           + datetime.timedelta(minutes=5)).isoformat(),
            "operator_note": "Approve this exact main push once",
        }
        record.update(overrides)
        (self.consent_dir / (record["id"] + ".json")).write_text(json.dumps(record))
        return record

    def main_hook(self, target: str | None = None,
                  previous: str = "0" * 40) -> subprocess.CompletedProcess[str]:
        # Replay a proposed update directly: Git skips hooks for a no-op push.
        target = target or git(self.repo, "rev-parse", "HEAD").stdout.strip()
        env = clean_environment(self.repo, os.environ.copy())
        env.pop("RSI_SESSION_TOKEN", None)
        env.update(HOME=str(self.consent_home), USERPROFILE=str(self.consent_home),
                   RSI_SESSION_ID="test-session")
        return subprocess.run(
            ["sh", str(HOOKS / "pre-push"), "publish", str(self.remote)],
            cwd=self.repo, env=env, text=True, capture_output=True,
            input=f"HEAD {target} refs/heads/main {previous}\n", check=False,
        )

    def test_exact_operator_consent_allows_one_push_and_records_use(self) -> None:
        grant = self.grant_main_push()
        result = git(self.repo, "push", "publish", "HEAD:refs/heads/main",
                     agent_marker="RSI_SESSION_TOKEN",
                     extra_env={"HOME": str(self.consent_home),
                                "USERPROFILE": str(self.consent_home)})
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.remote_ref("refs/heads/main"), grant["target_sha"])
        receipt = json.loads((self.consent_dir / (grant["id"] + ".used")).read_text())
        self.assertEqual(receipt["target_sha"], grant["target_sha"])
        self.assertEqual(receipt["remote_url"], str(self.remote))
        self.assertEqual(receipt["ref"], "refs/heads/main")
        datetime.datetime.fromisoformat(receipt["used_at"].replace("Z", "+00:00"))
        self.assertNotEqual(self.main_hook().returncode, 0)

    def test_consent_refuses_wrong_remote_ref_sha_and_expiry_without_spending(self) -> None:
        expired = (datetime.datetime.now(datetime.timezone.utc)
                   - datetime.timedelta(seconds=1)).isoformat()
        for override in ({"remote_url": str(self.root / "elsewhere.git")},
                         {"ref": "refs/heads/rolling"},
                         {"target_sha": "a" * 40}, {"expires_at": expired}):
            with self.subTest(override=override):
                self.grant_main_push(**override)
                result = self.main_hook()
                self.assertNotEqual(result.returncode, 0, result.stderr)
                self.assertEqual(list(self.consent_dir.glob("*.used")), [])
        self.grant_main_push()
        result = self.main_hook()
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_concurrent_hooks_can_consume_a_grant_only_once(self) -> None:
        self.grant_main_push()
        with ThreadPoolExecutor(max_workers=2) as pool:
            results = list(pool.map(lambda _: self.main_hook(), range(2)))
        self.assertEqual(sorted(result.returncode for result in results), [0, 1])
        self.assertEqual(len(list(self.consent_dir.glob("*.used"))), 1)

    def test_consent_allows_fast_forward_and_refuses_forced_rewind_and_deletion(self) -> None:
        initial = git(self.repo, "rev-parse", "HEAD").stdout.strip()
        self.assert_git(self.repo, "push", "publish", "HEAD:refs/heads/main")
        self.commit_file("next.txt", "next\n")
        grant = self.grant_main_push()
        result = self.main_hook(previous=initial)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assert_git(self.repo, "push", "publish", "HEAD:refs/heads/main")
        self.grant_main_push(target_sha=initial)
        forced = git(self.repo, "push", "--force", "publish", f"{initial}:refs/heads/main",
                     agent_marker="RSI_SESSION_ID",
                     extra_env={"HOME": str(self.consent_home),
                                "USERPROFILE": str(self.consent_home)})
        self.assertNotEqual(forced.returncode, 0)
        self.assertEqual(self.remote_ref("refs/heads/main"), grant["target_sha"])
        self.assertNotEqual(self.main_hook(target="0" * 40).returncode, 0)
        self.assertEqual(len(list(self.consent_dir.glob("*.used"))), 1)

    def assert_git(self, cwd: Path, *args: str) -> None:
        result = git(cwd, *args)
        self.assertEqual(result.returncode, 0, result.stderr)

    def remote_ref(self, ref: str) -> str:
        return git(self.remote, "rev-parse", "--verify", ref).stdout.strip()

    def test_agent_protected_refs_rejected_without_remote_update(self) -> None:
        for marker, ref in (
            ("RSI_SESSION_ID", "rolling"),
            ("RSI_SESSION_TOKEN", "main"),
        ):
            with self.subTest(marker=marker, ref=ref):
                result = git(
                    self.repo,
                    "push",
                    "publish",
                    f"HEAD:refs/heads/{ref}",
                    agent_marker=marker,
                )
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(f"refs/heads/{ref}", result.stderr)
                if ref == "rolling":
                    # #2072: agents are pointed at the queue, never the lander.
                    self.assertIn("merge queue", result.stderr)
                    self.assertIn("rsi land <session-id|branch>", result.stderr)
                else:
                    self.assertIn("main is operator-only", result.stderr)
                self.assertNotIn("rsi-rolling-land", result.stderr)
                self.assertEqual(self.remote_ref(f"refs/heads/{ref}"), "")

    def test_lander_marker_allows_rolling_but_never_main(self) -> None:
        rolling = git(
            self.repo,
            "push",
            "publish",
            "HEAD:refs/heads/rolling",
            agent_marker="RSI_SESSION_ID",
            lander_marker=True,
        )
        self.assertEqual(rolling.returncode, 0, rolling.stderr)
        source = git(self.repo, "rev-parse", "HEAD").stdout.strip()
        self.assertEqual(self.remote_ref("refs/heads/rolling"), source)

        main = git(
            self.repo,
            "push",
            "publish",
            "HEAD:refs/heads/main",
            agent_marker="RSI_SESSION_ID",
            lander_marker=True,
        )
        self.assertNotEqual(main.returncode, 0)
        self.assertEqual(self.remote_ref("refs/heads/main"), "")

    def test_agent_unprotected_ref_and_operator_protected_ref_succeed(self) -> None:
        agent = git(
            self.repo,
            "push",
            "publish",
            "HEAD:refs/heads/feature",
            agent_marker="RSI_SESSION_ID",
        )
        self.assertEqual(agent.returncode, 0, agent.stderr)
        for ref in ("main", "rolling"):
            operator = git(self.repo, "push", "publish", f"HEAD:refs/heads/{ref}")
            self.assertEqual(operator.returncode, 0, operator.stderr)
        source = git(self.repo, "rev-parse", "HEAD").stdout.strip()
        self.assertEqual(self.remote_ref("refs/heads/feature"), source)
        self.assertEqual(self.remote_ref("refs/heads/main"), source)
        self.assertEqual(self.remote_ref("refs/heads/rolling"), source)

    def commit_file(self, relative: str, text: str) -> None:
        path = self.repo / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)
        self.assert_git(self.repo, "add", relative)
        self.assert_git(self.repo, "commit", "-q", "-m", f"add {relative}")

    def test_operator_push_to_rolling_refuses_a_python_syntax_error(self) -> None:
        # #2055: the incident was an IndentationError in tools/ pushed by a
        # hand merge; the hook refuses it and points at the queue command.
        self.assert_git(self.repo, "push", "publish", "HEAD:refs/heads/rolling")
        good = self.remote_ref("refs/heads/rolling")
        self.commit_file("tools/broken.py", "def main():\n    x = 1\n      y = 2\n")
        result = git(self.repo, "push", "publish", "HEAD:refs/heads/rolling")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("tools/broken.py:3: IndentationError", result.stderr)
        self.assertIn("rsi land <session-id|branch>", result.stderr)
        self.assertEqual(self.remote_ref("refs/heads/rolling"), good)

    def test_target_writer_guard_checks_the_pushed_desktop_scripts(self) -> None:
        source = HOOKS.parents[1]
        for relative in ("scripts/check-target-reclaim.py", "scripts/target-reclaim-retained.json",
                         "crates/rsid-store/src/sandbox/target_reclaim.rs",
                         "crates/rsi-common/src/sandbox_storage.rs"):
            self.commit_file(relative, (source / relative).read_text())
        self.assert_git(self.repo, "push", "publish", "HEAD:refs/heads/rolling")
        good = self.remote_ref("refs/heads/rolling")
        self.commit_file("desktop/scripts/build.sh", "mkdir -p target/new-thing\n")
        # A dirty local repair must not conceal the writer in the pushed tree.
        (self.repo / "desktop/scripts/build.sh").write_text("mkdir -p target/.rsi-tmp/new-thing\n")
        result = git(self.repo, "push", "publish", "HEAD:refs/heads/rolling")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("desktop/scripts/build.sh:1: target/new-thing", result.stderr)
        self.assertIn("check-target-reclaim fails", result.stderr)
        self.assertEqual(self.remote_ref("refs/heads/rolling"), good)

    def test_a_file_name_with_spaces_is_still_syntax_checked(self) -> None:
        # #2071: a whitespace-split file list skipped `tools/my script.py`.
        self.assert_git(self.repo, "push", "publish", "HEAD:refs/heads/rolling")
        self.commit_file("tools/my script.py", "def main():\n    x = 1\n      y = 2\n")
        result = git(self.repo, "push", "publish", "HEAD:refs/heads/rolling")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("tools/my script.py:3: IndentationError", result.stderr)

    def test_an_unknown_remote_tip_still_checks_the_pushed_python(self) -> None:
        # #2070: the remote tip was pushed from another clone, so this clone
        # does not have it; the guard falls back to the merge base with
        # origin/rolling (or the whole tree) instead of checking nothing.
        self.assert_git(self.repo, "push", "publish", "HEAD:refs/heads/rolling")
        shared = self.remote_ref("refs/heads/rolling")
        self.assert_git(self.repo, "update-ref", "refs/remotes/origin/rolling", shared)
        other = self.root / "other"
        self.assert_git(self.root, "clone", "-q", "-b", "rolling", str(self.remote), str(other))
        self.assert_git(other, "config", "user.name", "Other")
        self.assert_git(other, "config", "user.email", "other@example.invalid")
        (other / "elsewhere.txt").write_text("moved\n")
        self.assert_git(other, "add", "elsewhere.txt")
        self.assert_git(other, "commit", "-q", "-m", "remote moves")
        self.assert_git(other, "push", "-q", "origin", "HEAD:refs/heads/rolling")
        self.commit_file("tools/broken.py", "def main():\n    x = 1\n      y = 2\n")
        result = git(self.repo, "push", "publish", "HEAD:refs/heads/rolling")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("tools/broken.py:3: IndentationError", result.stderr)

    def test_operator_override_and_lander_marker_skip_the_static_guards(self) -> None:
        self.assert_git(self.repo, "push", "publish", "HEAD:refs/heads/rolling")
        self.commit_file("scripts/broken.py", "if True:\nprint('x')\n")
        overridden = git(
            self.repo,
            "push",
            "publish",
            "HEAD:refs/heads/rolling",
            extra_env={"RSI_SKIP_STATIC_GUARDS": "1"},
        )
        self.assertEqual(overridden.returncode, 0, overridden.stderr)
        self.commit_file("scripts/broken_again.py", "if True:\nprint('y')\n")
        lander = git(
            self.repo,
            "push",
            "publish",
            "HEAD:refs/heads/rolling",
            lander_marker=True,
        )
        self.assertEqual(lander.returncode, 0, lander.stderr)

    def commit_personal_paths_guard(self) -> None:
        # The pushed commit carries the guard (#360); the hook runs its own checkout's build.
        guard = (HOOKS.parents[1] / "crates/rsi-repo-guards/src/personal_paths.rs").read_text()
        self.commit_file("crates/rsi-repo-guards/src/personal_paths.rs", guard)

    def test_rolling_push_adding_a_personal_path_is_refused(self) -> None:
        # #3269: the public export refuses a personal home path; so does the push.
        self.commit_personal_paths_guard()
        self.assert_git(self.repo, "push", "publish", "HEAD:refs/heads/rolling")
        good = self.remote_ref("refs/heads/rolling")
        account = "jake" + "devar"
        self.commit_file("docs/log.md", f"socket=/Users/{account}/.rsi/daemon.sock\n")
        result = git(self.repo, "push", "publish", "HEAD:refs/heads/rolling")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("docs/log.md:1: personal path or account name", result.stderr)
        self.assertIn("check-personal-paths fails", result.stderr)
        self.assertEqual(self.remote_ref("refs/heads/rolling"), good)

    def test_rolling_push_adding_a_public_ip_is_refused_but_multicast_passes(self) -> None:
        self.commit_personal_paths_guard()
        self.assert_git(self.repo, "push", "publish", "HEAD:refs/heads/rolling")
        self.commit_file("docs/net.md", "group=224.0.0.1 doc=203.0.113.10\n")
        passed = git(self.repo, "push", "publish", "HEAD:refs/heads/rolling")
        self.assertEqual(passed.returncode, 0, passed.stderr)
        public = ".".join(["93", "184", "216", "34"])
        self.commit_file("docs/net2.md", f"host={public}\n")
        result = git(self.repo, "push", "publish", "HEAD:refs/heads/rolling")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(f"docs/net2.md:1: public IP literal {public}", result.stderr)

    def test_rolling_push_of_an_oversized_crate_agents_md_is_refused(self) -> None:
        # #3450: a crate AGENTS.md is loaded into every session that reads the crate.
        guard = (HOOKS.parents[1] / "crates/rsi-repo-guards/src/agents_md_size.rs").read_text()
        self.commit_file("crates/rsi-repo-guards/src/agents_md_size.rs", guard)
        self.commit_file("crates/demo/AGENTS.md", "one line per fact\n")
        self.assert_git(self.repo, "push", "publish", "HEAD:refs/heads/rolling")
        good = self.remote_ref("refs/heads/rolling")
        self.commit_file("crates/demo/AGENTS.md", "x" * (12 * 1024 + 1))
        result = git(self.repo, "push", "publish", "HEAD:refs/heads/rolling")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("crates/demo/AGENTS.md", result.stderr)
        self.assertIn("check_agents_md_size fails", result.stderr)
        self.assertEqual(self.remote_ref("refs/heads/rolling"), good)

    def test_operator_push_with_valid_python_still_passes(self) -> None:
        self.assert_git(self.repo, "push", "publish", "HEAD:refs/heads/rolling")
        self.commit_file("tools/fine.py", "print('ok')\n")
        result = git(self.repo, "push", "publish", "HEAD:refs/heads/rolling")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("rsi land <session-id|branch>", result.stderr)

    def test_role_only_environment_without_rsi_session_can_push(self) -> None:
        result = git(
            self.repo,
            "push",
            "publish",
            "HEAD:refs/heads/rolling",
            agent_marker="CLAUDE_AGENT_ROLE",
        )
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_agent_cannot_delete_protected_ref_or_push_it_with_other_refs(self) -> None:
        self.assert_git(self.repo, "push", "publish", "HEAD:refs/heads/rolling")
        source = self.remote_ref("refs/heads/rolling")
        deletion = git(
            self.repo,
            "push",
            "publish",
            ":refs/heads/rolling",
            agent_marker="RSI_SESSION_ID",
        )
        self.assertNotEqual(deletion.returncode, 0)
        self.assertEqual(self.remote_ref("refs/heads/rolling"), source)
        mixed = git(
            self.repo,
            "push",
            "publish",
            "HEAD:refs/heads/feature",
            "HEAD:refs/heads/main",
            agent_marker="RSI_SESSION_ID",
        )
        self.assertNotEqual(mixed.returncode, 0)
        self.assertEqual(self.remote_ref("refs/heads/main"), "")
        self.assertEqual(self.remote_ref("refs/heads/feature"), "")

    def test_private_landing_clone_lander_marker_can_publish_with_agent_environment(self) -> None:
        private = self.root / "landing-clone"
        self.assert_git(
            self.root, "clone", "--shared", "--no-checkout", "-q", str(self.repo), str(private)
        )
        self.assert_git(private, "remote", "add", "publish", str(self.remote))
        self.assertNotEqual(git(private, "config", "--local", "--get", "core.hooksPath").returncode, 0)
        result = git(
            private,
            "push",
            "publish",
            "HEAD:refs/heads/rolling",
            agent_marker="RSI_SESSION_ID",
            lander_marker=True,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.remote_ref("refs/heads/rolling"), git(private, "rev-parse", "HEAD").stdout.strip())


    def commit_shard_checker(self, bad: bool) -> str:
        """Commit a stand-in shard checker that fails iff the exported tree holds
        crates/rsid/BAD, so the check is proven to run on the pushed commit."""
        script = self.repo / "scripts" / "check-rsid-test-shards.py"
        script.parent.mkdir(exist_ok=True)
        script.write_text(
            "import sys\n"
            "from pathlib import Path\n"
            "root = Path(__file__).resolve().parent.parent\n"
            "if (root / 'crates/rsid/BAD').exists():\n"
            "    print('strict inventory red'); sys.exit(1)\n"
            "print('ok')\n"
        )
        crate = self.repo / "crates" / "rsid"
        crate.mkdir(parents=True, exist_ok=True)
        (crate / "Cargo.toml").write_text("[package]\n")
        bad_marker = crate / "BAD"
        if bad:
            bad_marker.write_text("x\n")
        elif bad_marker.exists():
            bad_marker.unlink()
        self.assert_git(self.repo, "add", "-A", "scripts", "crates")
        self.assert_git(self.repo, "commit", "-q", "-m", "shard checker fixture")
        return git(self.repo, "rev-parse", "HEAD").stdout.strip()

    def test_rolling_push_of_a_strict_inventory_failure_is_refused(self) -> None:
        self.commit_shard_checker(bad=True)
        for kwargs in ({}, {"agent_marker": "RSI_SESSION_ID", "lander_marker": True}):
            with self.subTest(kwargs=kwargs):
                result = git(self.repo, "push", "publish", "HEAD:refs/heads/rolling", **kwargs)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("strict rsid test shard inventory", result.stderr)
                self.assertEqual(self.remote_ref("refs/heads/rolling"), "")

    def test_rolling_push_checks_the_pushed_commit_not_the_working_tree(self) -> None:
        good = self.commit_shard_checker(bad=False)
        (self.repo / "crates" / "rsid" / "BAD").write_text("uncommitted\n")
        result = git(self.repo, "push", "publish", "HEAD:refs/heads/rolling")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.remote_ref("refs/heads/rolling"), good)

    def test_strict_inventory_failure_does_not_block_other_refs(self) -> None:
        bad = self.commit_shard_checker(bad=True)
        result = git(
            self.repo,
            "push",
            "publish",
            "HEAD:refs/heads/feature",
            agent_marker="RSI_SESSION_ID",
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.remote_ref("refs/heads/feature"), bad)

    def test_rolling_push_adding_an_operator_identifier_is_refused(self) -> None:
        import shutil
        from unittest import mock

        tool = self.repo / "crates/rsi-repo-guards/src/operator_identifiers.rs"
        tool.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy(HOOKS.parents[1] / "crates/rsi-repo-guards/src/operator_identifiers.rs", tool)
        self.assert_git(self.repo, "add", "crates")
        self.assert_git(self.repo, "commit", "-q", "-m", "add guard")
        self.assert_git(self.repo, "push", "publish", "HEAD:refs/heads/rolling")
        base = self.remote_ref("refs/heads/rolling")
        ids = self.root / "operator-identifiers"
        ids.write_text("jane.doe" + "@example.invalid\n")
        (self.repo / "note.md").write_text("contact jane.doe" + "@example.invalid\n")
        self.assert_git(self.repo, "add", "note.md")
        self.assert_git(self.repo, "commit", "-q", "-m", "leak")
        with mock.patch.dict(os.environ, {"RSI_OPERATOR_IDENTIFIERS_FILE": str(ids)}):
            refused = git(self.repo, "push", "publish", "HEAD:refs/heads/rolling")
            other = git(self.repo, "push", "publish", "HEAD:refs/heads/feature")
        self.assertNotEqual(refused.returncode, 0)
        self.assertIn("note.md:1: operator identifier #1", refused.stderr)
        self.assertNotIn("jane.doe" + "@example.invalid", refused.stderr)
        self.assertEqual(self.remote_ref("refs/heads/rolling"), base)
        self.assertEqual(other.returncode, 0, other.stderr)
        # Without the list the same push goes through.
        ok = git(self.repo, "push", "publish", "HEAD:refs/heads/rolling")
        self.assertEqual(ok.returncode, 0, ok.stderr)


if __name__ == "__main__":
    unittest.main()

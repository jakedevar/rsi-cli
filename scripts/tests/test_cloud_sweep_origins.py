"""Local sweep fixtures require existing, physically contained Git origins."""

import os
from pathlib import Path
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().parents[1] / "cloud-sweep.sh"


class CloudSweepOriginsTest(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.allowed = self.root / "allowed"
        self.allowed.mkdir()
        self.origin = self.allowed / "origin.git"
        self.caller = self.root / "caller"
        self.git("init", "-q", "--bare", str(self.origin))
        self.git("init", "-q", "-b", "rolling", str(self.caller))
        self.git("-C", str(self.caller), "commit", "-q", "--allow-empty", "-m", "tip")
        self.sha = self.git("-C", str(self.caller), "rev-parse", "HEAD").stdout.strip()
        self.git("-C", str(self.caller), "push", "-q", str(self.origin), "rolling")
        self.git("-C", str(self.caller), "remote", "add", "origin", str(self.origin))

    def git(self, *args):
        return subprocess.run(
            ["git", "-c", "user.name=t", "-c", "user.email=t@example.com", *args],
            env={**os.environ, "GIT_CONFIG_GLOBAL": "/dev/null", "GIT_CONFIG_NOSYSTEM": "1"},
            check=True, capture_output=True, text=True,
        )

    def bundle(self, origin, *roots):
        self.git("-C", str(self.caller), "config", "remote.origin.url", str(origin))
        command = ["bash", str(SCRIPT), "bundle", self.sha, str(self.root / "tip.bundle"),
                   "--repo", str(self.caller), "--mirror", str(self.root / "mirror.git")]
        for root in roots:
            command.extend(["--allow-local-root", str(root)])
        return subprocess.run(command, capture_output=True, text=True,
                              env={**os.environ, "CDPATH": str(self.root)})

    def assert_bundled(self, origin, *roots):
        result = self.bundle(origin, *roots)
        self.assertEqual(result.returncode, 0, result.stderr)
        heads = self.git("bundle", "list-heads", str(self.root / "tip.bundle")).stdout
        self.assertEqual(heads.strip(), f"{self.sha} refs/remotes/origin/rolling")

    def assert_refused(self, origin, *roots):
        result = self.bundle(origin, *roots)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("cloud-sweep: refusing origin URL", result.stderr)

    def test_existing_local_path_and_file_url_bundle_the_exact_tip(self):
        self.assert_bundled(self.origin, self.allowed)
        self.assert_bundled(f"file://{self.origin}", self.allowed)
        self.assert_bundled(self.allowed / ".." / "allowed" / "origin.git", self.allowed)

    def test_symlinked_origin_and_root_are_resolved_physically(self):
        alias = self.root / "alias"
        alias.symlink_to(self.allowed, target_is_directory=True)
        link = self.allowed / "link.git"
        link.symlink_to(self.origin, target_is_directory=True)
        self.assert_bundled(link, alias)
        self.assert_bundled(alias / "origin.git", self.allowed)

    def test_local_origins_require_an_explicit_allowed_root(self):
        self.assert_refused(self.origin)
        self.assert_refused(f"file://{self.origin}")

    def test_missing_paths_roots_and_regular_files_are_refused(self):
        regular = self.allowed / "regular"
        regular.write_text("not a repository")
        dangling = self.allowed / "dangling"
        dangling.symlink_to(self.allowed / "missing", target_is_directory=True)
        for origin in [self.allowed / "missing", dangling, regular,
                       f"{self.allowed}/missing/../origin.git"]:
            with self.subTest(origin=origin):
                self.assert_refused(origin, self.allowed)
        self.assert_refused(self.origin, self.root / "missing")
        self.assert_refused(self.origin, regular)
        self.assert_bundled(self.origin, self.root / "missing", self.allowed)

    def test_sibling_prefix_parent_traversal_and_symlink_escape_are_refused(self):
        sibling = self.root / "allowed-other"
        sibling.mkdir()
        escaped = self.allowed / "escaped"
        escaped.symlink_to(sibling, target_is_directory=True)
        for origin in [sibling, escaped, self.allowed / ".." / "allowed-other", self.allowed]:
            with self.subTest(origin=origin):
                self.assert_refused(origin, self.allowed)

    def test_unsupported_and_hostile_transports_are_refused_even_with_local_permission(self):
        for origin in ["ext::sh -c true", "evil::repo", "-oProxyCommand=x",
                       "http://example.com/x.git", "git://example.com/x.git",
                       "ssh://-oProxyCommand=x/y", "https://-host/x",
                       "git@github.com:-x", "file://localhost/etc",
                       "file://relative", "https://example.com/a b", "ssh://host/a\nb"]:
            with self.subTest(origin=origin):
                self.assert_refused(origin, self.allowed)


if __name__ == "__main__":
    unittest.main()

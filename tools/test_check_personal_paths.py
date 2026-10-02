import contextlib
import io
import os
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, os.path.dirname(__file__))
import check_personal_paths as cpp  # noqa: E402

ACCOUNT = "jake" + "devar"
# Assembled at runtime so this file holds no public IP literal itself.
PUBLIC_IP = ".".join(["93", "184", "216", "34"])
OTHER_RESOLVER = ".".join(["8", "8", "4", "4"])


def run(name, text):
    with tempfile.TemporaryDirectory() as d:
        path = Path(d) / name
        path.write_text(text)
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            code = cpp.main(["check", str(path)])
        return code, out.getvalue()


class CheckPersonalPaths(unittest.TestCase):
    def test_home_path_is_flagged(self):
        code, out = run("a.md", f"see /home/{ACCOUNT}/rsi/docs\n")
        self.assertEqual(code, 1)
        self.assertIn("a.md:1: personal path or account name", out)

    def test_ssh_account_is_flagged(self):
        self.assertEqual(run("a.sh", f"ssh {ACCOUNT}@host\n")[0], 1)

    def test_public_ip_is_flagged(self):
        code, out = run("a.sh", f"host={PUBLIC_IP}\n")
        self.assertEqual(code, 1)
        self.assertIn(f"public IP literal {PUBLIC_IP}", out)

    def test_placeholders_and_private_ranges_pass(self):
        text = "\n".join(
            [
                "host=remote.example.net",
                "doc=203.0.113.10 lan=192.168.1.5 lo=127.0.0.1 any=0.0.0.0",
                "cgnat=100.101.102.103 meta=169.254.169.254 vpc=10.61.0.0",
                "home=$HOME/rsi ~/rsi /home/user/rsi",
            ]
        )
        self.assertEqual(run("a.txt", text), (0, ""))

    def test_allowlist_is_only_the_github_slug_and_public_resolver(self):
        self.assertEqual(run("Cargo.toml", f'repository = "https://github.com/{ACCOUNT}/rsi"\n')[0], 0)
        self.assertEqual(run("a.rs", 'let dns = "8.8.8.8";\n')[0], 0)
        self.assertEqual(run("a.rs", f'url = "github.com/{ACCOUNT}/rsi" /home/{ACCOUNT}\n')[0], 1)
        self.assertEqual(run("a.rs", f'let dns = "{OTHER_RESOLVER}";\n')[0], 1)

    def test_egress_fixture_files_may_name_public_ips_but_not_personal_paths(self):
        rel = "crates/rsi-common/src/egress_policy.rs"
        with tempfile.TemporaryDirectory() as d:
            path = Path(d) / rel
            path.parent.mkdir(parents=True)
            cwd = os.getcwd()
            os.chdir(d)
            try:
                path.write_text(f'assert_public("{PUBLIC_IP}");\n')
                with contextlib.redirect_stdout(io.StringIO()):
                    self.assertEqual(cpp.main(["check", rel]), 0)
                path.write_text(f'let p = "/home/{ACCOUNT}/x"; assert_public("{PUBLIC_IP}");\n')
                with contextlib.redirect_stdout(io.StringIO()):
                    self.assertEqual(cpp.main(["check", rel]), 1)
            finally:
                os.chdir(cwd)
        # The same line in any other file is still a finding.
        self.assertEqual(run("egress_policy.rs", f'assert_public("{PUBLIC_IP}");\n')[0], 1)

    def test_version_numbers_are_not_ip_literals(self):
        self.assertEqual(run("a.txt", "v1.94.1 and 1.2.3.4.5 and 12.3.4.5.6\n")[0], 0)

    def test_thoughts_are_skipped(self):
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            self.assertEqual(cpp.main(["check", "thoughts/shared/x.md"]), 0)


if __name__ == "__main__":
    unittest.main()

"""The test seam is excluded from release builds by the crates' build scripts (#1021 S4).

`test-seam` carries fake capabilities and route-validation bypasses, so every
crate that declares it refuses to build with it in the release profile. The
guard text is the one in each `build.rs` (between the SEAM-GUARD markers); the
test compiles that text and runs it under the environment Cargo gives a build
script.
"""

import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
CRATES = ("rsid-core", "rsid-store", "rsid")


def guard_text(crate: str) -> str:
    source = (ROOT / "crates" / crate / "build.rs").read_text()
    match = re.search(r"// SEAM-GUARD-BEGIN\n(.*?)// SEAM-GUARD-END", source, re.S)
    assert match, f"{crate}/build.rs has no seam guard"
    # The build script must call the guard before it does anything else.
    main = source[source.index("fn main()") :]
    assert main.split("{", 1)[1].lstrip().startswith("refuse_test_seam_in_release();"), crate
    return match.group(1)


@unittest.skipUnless(shutil.which("rustc"), "requires rustc")
class SeamReleaseGuardTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.temp = tempfile.TemporaryDirectory(prefix="seam-guard-")
        cls.binaries = {}
        for crate in CRATES:
            source = Path(cls.temp.name) / f"{crate}.rs"
            source.write_text(guard_text(crate) + "fn main() { refuse_test_seam_in_release(); }\n")
            binary = Path(cls.temp.name) / crate
            subprocess.run(
                ["rustc", "--edition", "2024", "-o", str(binary), str(source)],
                check=True, capture_output=True,
            )
            cls.binaries[crate] = binary

    @classmethod
    def tearDownClass(cls):
        cls.temp.cleanup()

    def run_guard(self, crate, *, seam, profile, allow=None):
        env = {key: value for key, value in os.environ.items() if not key.startswith(("CARGO_FEATURE", "RSID_ALLOW"))}
        env["PROFILE"] = profile
        if seam:
            env["CARGO_FEATURE_TEST_SEAM"] = "1"
        if allow is not None:
            env["RSID_ALLOW_TEST_SEAM_RELEASE"] = allow
        return subprocess.run([str(self.binaries[crate])], capture_output=True, text=True, env=env)

    def test_a_release_build_with_the_seam_is_refused_in_every_seam_crate(self):
        for crate in CRATES:
            result = self.run_guard(crate, seam=True, profile="release")
            self.assertNotEqual(result.returncode, 0, crate)
            self.assertIn("`test-seam` feature is enabled in a release build", result.stderr, crate)

    def test_debug_builds_and_seam_free_release_builds_pass(self):
        for crate in CRATES:
            self.assertEqual(self.run_guard(crate, seam=True, profile="debug").returncode, 0, crate)
            self.assertEqual(self.run_guard(crate, seam=False, profile="release").returncode, 0, crate)

    def test_a_release_mode_test_run_must_opt_in_explicitly(self):
        for crate in CRATES:
            self.assertNotEqual(self.run_guard(crate, seam=True, profile="release", allow="0").returncode, 0)
            self.assertNotEqual(self.run_guard(crate, seam=True, profile="release", allow="yes").returncode, 0)
            self.assertEqual(self.run_guard(crate, seam=True, profile="release", allow="1").returncode, 0, crate)


@unittest.skipUnless(shutil.which("rustc"), "requires rustc")
class E2eSeedReleaseGuardTest(unittest.TestCase):
    """The `e2e-seed` feature (local #122) compiles the isolated-state fixture writer; no release may carry it."""

    @classmethod
    def setUpClass(cls):
        cls.temp = tempfile.TemporaryDirectory(prefix="e2e-seed-guard-")
        source = (ROOT / "crates" / "rsid" / "build.rs").read_text()
        match = re.search(r"// E2E-SEED-GUARD-BEGIN\n(.*?)// E2E-SEED-GUARD-END", source, re.S)
        assert match, "rsid/build.rs has no e2e-seed guard"
        main = source[source.index("fn main()") :]
        assert "refuse_e2e_seed_in_release();" in main.split("}", 1)[0], "build.rs main must call the guard"
        cls.source = Path(cls.temp.name) / "guard.rs"
        cls.source.write_text(match.group(1) + "fn main() { refuse_e2e_seed_in_release(); }\n")
        cls.binary = Path(cls.temp.name) / "guard"
        subprocess.run(["rustc", "--edition", "2024", "-o", str(cls.binary), str(cls.source)], check=True, capture_output=True)

    @classmethod
    def tearDownClass(cls):
        cls.temp.cleanup()

    def run_guard(self, *, seed, profile, allow=None, allow_var="RSID_ALLOW_E2E_SEED_RELEASE"):
        env = {k: v for k, v in os.environ.items() if not k.startswith(("CARGO_FEATURE", "RSID_ALLOW"))}
        env["PROFILE"] = profile
        if seed:
            env["CARGO_FEATURE_E2E_SEED"] = "1"
        if allow is not None:
            env[allow_var] = allow
        return subprocess.run([str(self.binary)], capture_output=True, text=True, env=env)

    def test_a_release_build_with_the_seed_feature_is_refused(self):
        result = self.run_guard(seed=True, profile="release")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("`e2e-seed` feature is enabled in a release build", result.stderr)

    def test_debug_and_seed_free_release_builds_pass(self):
        self.assertEqual(self.run_guard(seed=True, profile="debug").returncode, 0)
        self.assertEqual(self.run_guard(seed=False, profile="release").returncode, 0)

    def test_no_opt_in_value_allows_a_release_build_with_the_seed_feature(self):
        # Hub #3244: the seeder writes a database, so unlike test-seam it has no release override.
        for allow in ("0", "yes", "1"):
            for name in ("RSID_ALLOW_E2E_SEED_RELEASE", "RSID_ALLOW_TEST_SEAM_RELEASE"):
                result = self.run_guard(seed=True, profile="release", allow=allow, allow_var=name)
                self.assertNotEqual(result.returncode, 0, f"{name}={allow}")
                self.assertIn("`e2e-seed` feature is enabled in a release build", result.stderr)

    def test_the_guard_reads_no_release_override(self):
        guard = self.source.read_text()
        self.assertNotIn("RSID_ALLOW", guard)

    def test_the_seeder_is_a_feature_gated_binary_and_never_a_default_feature(self):
        manifest = (ROOT / "crates" / "rsid" / "Cargo.toml").read_text()
        self.assertRegex(manifest, r'name = "rsi-e2e-seed"\npath = "src/bin/rsi-e2e-seed.rs"\nrequired-features = \["e2e-seed"\]')
        self.assertIn('default = []', manifest)
        self.assertNotRegex(manifest, r'(?m)^default = \[[^\]]*e2e-seed')


if __name__ == "__main__":
    unittest.main()

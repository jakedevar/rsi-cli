"""A shard fingerprint must not depend on version-probe stderr noise."""

import hashlib
import importlib.util
import json
from pathlib import Path
import sys
import unittest
from unittest.mock import patch


SCRIPTS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS))

SPEC = importlib.util.spec_from_file_location(
    "rolling_shard_fingerprint", SCRIPTS / "rolling-shard-fingerprint.py"
)
assert SPEC and SPEC.loader
FINGERPRINT = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(FINGERPRINT)

SHA = "b" * 40
RUSTC = "rustc 1.94.0 (000000000 2026-01-01)\nhost: x86_64-unknown-linux-gnu"
STDOUT = {
    ("rustc", "-vV"): RUSTC,
    ("cargo", "--version"): "cargo 1.94.0 (000000000 2026-01-01)",
    ("cargo", "nextest", "--version"): "cargo-nextest 0.9.100",
}
NOISE = "info: syncing channel updates for 'stable-x86_64-unknown-linux-gnu'\n"


class Completed:
    def __init__(self, stdout, stderr):
        self.stdout = stdout
        self.stderr = stderr


def fake_run(*, stderr):
    """Mimic subprocess.run for the version probes and the git blob lookups."""

    def run(command, **kwargs):
        if command in STDOUT:
            return Completed(STDOUT[command], stderr)
        if command[:2] == ("git", "rev-parse"):
            return Completed("a" * 40, stderr)
        raise AssertionError(f"unexpected command: {command}")

    return run


def old_digest(sha, shard, jobs, stderr):
    """The pre-change stdout+stderr digest, kept here as the compatibility oracle."""
    inputs = {
        "rustc": (RUSTC + stderr).strip(),
        "cargo": (STDOUT[("cargo", "--version")] + stderr).strip(),
        "nextest": (STDOUT[("cargo", "nextest", "--version")] + stderr).strip(),
        "target_triple": "x86_64-unknown-linux-gnu",
        "host_class": "linux-x86_64@desktop:a",
        "feature": f"test-shard-{shard}",
        "jobs": jobs,
        "test_threads": jobs,
        "runner_blob": "a" * 40,
        "checker_blob": "a" * 40,
        "nextest_config_blob": "a" * 40,
    }
    canonical = json.dumps(inputs, sort_keys=True, separators=(",", ":")).encode()
    return hashlib.sha256(canonical).hexdigest()


class FingerprintTests(unittest.TestCase):
    def fingerprint(self, stderr):
        with patch.object(FINGERPRINT.subprocess, "run", side_effect=fake_run(stderr=stderr)), \
             patch.object(FINGERPRINT, "host_class", return_value="linux-x86_64@desktop:a"):
            return FINGERPRINT.fingerprint(SHA, "store-01", 4)

    def test_stderr_noise_does_not_change_digest(self):
        clean = self.fingerprint("")
        noisy = self.fingerprint(NOISE)
        self.assertEqual(clean["digest"], noisy["digest"])
        self.assertEqual(clean["inputs"], noisy["inputs"])
        self.assertNotIn("info:", clean["inputs"]["rustc"])

    def test_clean_digest_matches_legacy_stdout_plus_stderr(self):
        self.assertEqual(
            self.fingerprint("")["digest"],
            old_digest(SHA, "store-01", 4, ""),
        )

    def test_output_ignores_stderr(self):
        with patch.object(FINGERPRINT.subprocess, "run", side_effect=fake_run(stderr=NOISE)):
            self.assertEqual(FINGERPRINT.output("cargo", "--version"),
                             STDOUT[("cargo", "--version")])


if __name__ == "__main__":
    unittest.main()

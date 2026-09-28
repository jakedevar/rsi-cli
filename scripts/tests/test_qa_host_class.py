"""Shard fingerprints must separate execution hosts even under one class label."""

import importlib.util
import os
from pathlib import Path
import sys
import unittest
from unittest.mock import patch


SCRIPTS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS))
import qa_host_class  # noqa: E402

SPEC = importlib.util.spec_from_file_location(
    "rolling_shard_fingerprint", SCRIPTS / "rolling-shard-fingerprint.py"
)
FINGERPRINT = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(FINGERPRINT)


class HostClassTests(unittest.TestCase):
    def test_host_identity_survives_shared_class_override(self):
        with patch.dict(os.environ, {"RSI_QA_HOST_CLASS": "linux-x86_64"}), \
             patch.object(qa_host_class.Path, "read_text", return_value="same-machine-id"):
            with patch.object(qa_host_class.socket, "gethostname", return_value="desktop"):
                desktop = qa_host_class.host_class()
                self.assertEqual(desktop, qa_host_class.host_class())
            with patch.object(qa_host_class.socket, "gethostname", return_value="cloud"):
                cloud = qa_host_class.host_class()
        self.assertNotEqual(desktop, cloud)
        self.assertTrue(desktop.startswith("linux-x86_64@desktop:"))
        self.assertTrue(cloud.startswith("linux-x86_64@cloud:"))

    def test_host_class_changes_full_shard_digest(self):
        def output(*args):
            if args == ("rustc", "-vV"):
                return "rustc 1.94\nhost: x86_64-unknown-linux-gnu"
            if args[0] == "git":
                return "a" * 40
            return "tool 1.0"

        with patch.object(FINGERPRINT, "output", side_effect=output):
            with patch.object(FINGERPRINT, "host_class", return_value="linux-x86_64@desktop:a"):
                desktop = FINGERPRINT.fingerprint("b" * 40, "store-01", 4)
            with patch.object(FINGERPRINT, "host_class", return_value="linux-x86_64@cloud:b"):
                cloud = FINGERPRINT.fingerprint("b" * 40, "store-01", 4)
        self.assertNotEqual(desktop["digest"], cloud["digest"])


if __name__ == "__main__":
    unittest.main()

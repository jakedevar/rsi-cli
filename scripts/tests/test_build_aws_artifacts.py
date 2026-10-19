"""build-aws-artifacts.sh packages the binaries cargo built, wherever cargo's target is (#2177)."""
import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys
import tarfile
import unittest

SCRIPTS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS))
from scratch_git import scratch_git, scratch_tempdir  # noqa: E402

BINARIES = ["rsi", "rsid", "rsi-rpc", "rsi-agent-mcp"]


class BuildAwsArtifactsTest(unittest.TestCase):
    def setUp(self):
        machine = platform.machine()
        if not platform.system() == "Linux" or machine not in ("x86_64", "aarch64"):
            self.skipTest("the native cargo builder is only used on a Linux x86_64/aarch64 host")
        self.architecture = "x86_64" if machine == "x86_64" else "arm64"
        self.triple = f"{machine}-unknown-linux-gnu"
        self.temp = scratch_tempdir(prefix="aws-artifacts-")
        self.addCleanup(self.temp.cleanup)
        base = Path(self.temp.name).resolve()
        self.repo = base / "repo"
        (self.repo / "scripts").mkdir(parents=True)
        for name in ("build-aws-artifacts.sh", "check-release-seam.sh"):
            shutil.copy2(SCRIPTS / name, self.repo / "scripts" / name)
        (self.repo / "Cargo.toml").write_text("")
        self.git("init", "-q")
        self.git("add", "-A")
        self.git("-c", "user.name=t", "-c", "user.email=t@example.invalid", "commit", "-q", "-m", "init")
        self.sha = self.git("rev-parse", "HEAD").strip()
        # Where cargo builds: a global `build.target-dir`, not ./target and not CARGO_TARGET_DIR.
        self.global_target = base / "global-target"
        fake_bin = base / "bin"
        fake_bin.mkdir()
        cargo = fake_bin / "cargo"
        cargo.write_text(
            "#!/bin/sh\n"
            'case "$1" in\n'
            "  metadata) printf '{\"target_directory\": \"%s\"}\\n' \"$STUB_TARGET\" ;;\n"
            "  tree) ;;\n"
            "  build)\n"
            "    while [ $# -gt 0 ]; do [ \"$1\" = --target ] && triple=$2; shift; done\n"
            "    mkdir -p \"$STUB_TARGET/$triple/release\"\n"
            "    for bin in rsi rsid rsi-rpc rsi-agent-mcp; do\n"
            "      echo built-by-cargo > \"$STUB_TARGET/$triple/release/$bin\"\n"
            "    done ;;\n"
            "esac\n")
        cargo.chmod(0o755)
        self.env = {key: value for key, value in os.environ.items() if key != "CARGO_TARGET_DIR"}
        self.env.update(PATH=f"{fake_bin}:{os.environ['PATH']}", STUB_TARGET=str(self.global_target))
        self.output = base / "out"

    def git(self, *args):
        return scratch_git(self.repo, "-c", "commit.gpgsign=false", "-c", "core.hooksPath=/dev/null",
                           *args).stdout

    def plant_decoy(self):
        release = self.repo / "target" / self.triple / "release"
        release.mkdir(parents=True)
        for name in BINARIES:
            (release / name).write_text("decoy-from-another-build\n")
        # A decoy is untracked build output; the checkout must stay clean for the script.
        (self.repo / ".gitignore").write_text("target/\n")
        self.git("add", ".gitignore")
        self.git("-c", "user.name=t", "-c", "user.email=t@example.invalid", "commit", "-q", "-m", "ignore")
        self.sha = self.git("rev-parse", "HEAD").strip()

    def test_a_global_target_dir_without_the_env_var_is_the_directory_packaged(self):
        self.plant_decoy()
        result = subprocess.run(
            ["bash", str(self.repo / "scripts/build-aws-artifacts.sh"), self.sha, str(self.output),
             self.architecture],
            cwd=self.repo, env=self.env, text=True, capture_output=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        artifact = self.output / f"rsi-{self.sha}-{self.architecture}.tar.gz"
        with tarfile.open(artifact) as archive:
            for name in BINARIES:
                member = archive.extractfile(f"./{name}")
                self.assertEqual(member.read().decode(), "built-by-cargo\n", name)

    def test_an_unresolvable_target_dir_fails_instead_of_packaging_target(self):
        self.plant_decoy()
        broken = Path(self.temp.name) / "broken-bin"
        broken.mkdir()
        (broken / "cargo").write_text("#!/bin/sh\nexit 1\n")
        (broken / "cargo").chmod(0o755)
        env = dict(self.env, PATH=f"{broken}:{self.env['PATH']}")
        result = subprocess.run(
            ["bash", str(self.repo / "scripts/build-aws-artifacts.sh"), self.sha, str(self.output),
             self.architecture],
            cwd=self.repo, env=env, text=True, capture_output=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(list(self.output.glob("*.tar.gz")), [])


if __name__ == "__main__":
    unittest.main()

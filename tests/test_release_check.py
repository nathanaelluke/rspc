#!/usr/bin/env python3
"""Exercise release checks using synthetic data, without reading user credentials."""
import importlib.util
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("release_check", ROOT / "scripts/check-release.py")
CHECK = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = CHECK
SPEC.loader.exec_module(CHECK)


class ReleaseCheckTest(unittest.TestCase):
    def test_credentials_are_flagged_without_including_the_value(self):
        secret = "sample" * 8
        source = ('api_key = "' + secret + '"').encode()
        findings = CHECK.check_file("settings.rs", source)
        self.assertEqual(len(findings), 1)
        self.assertNotIn(secret, repr(findings))

    def test_runtime_files_are_rejected_even_when_they_are_empty(self):
        for path in ("credentials.json", "config/soloist-api-key", "logs/player.log",
                     ".env", "cache/catalog-v2/page.json", "catalog-versions.json",
                     "demo-queue.json", "target/player"):
            with self.subTest(path=path):
                self.assertTrue(CHECK.check_file(path, b""))

    def test_generic_fixtures_and_public_client_identifier_are_allowed(self):
        source = b'CLIENT = "' + b"1" * 32 + b'"; path = "/home/example/music";'
        self.assertEqual(CHECK.check_file("example.rs", source), [])

    def test_unreviewed_binary_data_requires_a_review(self):
        self.assertTrue(CHECK.check_file("assets/new-image.png", b"image\0data"))
        self.assertEqual(CHECK.check_file("crates/rspc-ui/assets/default.jpg", b"image\0data"), [])

    def test_scan_includes_tracked_ignored_files_and_committed_secrets(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            def git(*args):
                return subprocess.run(["git", "-C", temp, *args], check=True, capture_output=True)
            git("init", "-q")
            git("config", "user.name", "Test")
            git("config", "user.email", "test@example.org")
            (root / ".gitignore").write_text("credentials.json\n")
            (root / "credentials.json").write_text("{}")
            git("add", "-f", "credentials.json", ".gitignore")
            _, current = CHECK.check_worktree(root)
            self.assertTrue(any(f.path == "credentials.json" for f in current))
            git("commit", "-qm", "fixture")
            git("rm", "-q", "credentials.json")
            git("commit", "-qm", "remove fixture")
            _, history = CHECK.check_history(root)
            self.assertTrue(any(f.path == "history:credentials.json" for f in history))

    def test_staged_secrets_are_found_after_the_working_copy_changes(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            subprocess.run(["git", "-C", temp, "init", "-q"], check=True)
            path = root / "settings.rs"
            secret = "fixture" * 8
            path.write_text('api_key = "' + secret + '"')
            subprocess.run(["git", "-C", temp, "add", "settings.rs"], check=True)
            for removed in (False, True):
                with self.subTest(removed=removed):
                    if removed:
                        path.unlink()
                    else:
                        path.write_text("safe working copy")
                    _, findings = CHECK.check_index(root)
                    self.assertEqual(len(findings), 1)
                    self.assertEqual(findings[0].path, "index:settings.rs")
                    self.assertNotIn(secret, repr(findings))


if __name__ == "__main__":
    unittest.main(verbosity=2)

#!/usr/bin/env python3
"""Verify temporary firewall cleanup without invoking sudo or using the network."""
import contextlib
import importlib.util
import io
from pathlib import Path
import subprocess
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location(
    "pair_on_lan", Path(__file__).resolve().parents[1] / "scripts/pair-on-lan.py")
helper = importlib.util.module_from_spec(spec)
spec.loader.exec_module(helper)


class PairingTest(unittest.TestCase):
    def simulate(self, outcome, timeout=300):
        rules = [["-p", "udp", "--dport", "5353"], ["-p", "tcp", "--dport", "56669"]]
        inserted, deleted = [], []

        def run(args, **kwargs):
            if "-I" in args:
                if outcome == "insert_failure" and inserted:
                    raise subprocess.CalledProcessError(1, args)
                inserted.append(args[args.index("1") + 1:])
            if args[-1] == "doctor":
                if outcome == "interrupt":
                    raise KeyboardInterrupt
                if outcome == "daemon_failure":
                    raise subprocess.CalledProcessError(1, args)
            return subprocess.CompletedProcess(args, 0, stdout="logged_in: 1\n")

        def delete(args):
            deleted.append(args[args.index("nixos-fw") + 1:])
            return subprocess.CompletedProcess(args, 0)

        with patch.object(helper, "run", side_effect=run), \
                patch.object(helper.subprocess, "run", side_effect=delete), \
                contextlib.redirect_stdout(io.StringIO()):
            try:
                helper.pair(Path("/fake/rspc"), rules, timeout)
            finally:
                self.assertEqual(deleted, list(reversed(inserted)))
        return inserted

    def test_pairing_removes_both_rules(self):
        self.assertEqual(len(self.simulate("paired")), 2)

    def test_cancellation_removes_both_rules(self):
        with self.assertRaises(KeyboardInterrupt):
            self.simulate("interrupt")

    def test_timeout_removes_both_rules(self):
        with self.assertRaisesRegex(RuntimeError, "timed out"):
            self.simulate("timeout", timeout=0)

    def test_partial_install_is_rolled_back(self):
        with self.assertRaises(subprocess.CalledProcessError):
            self.simulate("insert_failure")

    def test_daemon_failure_removes_both_rules(self):
        with self.assertRaises(subprocess.CalledProcessError):
            self.simulate("daemon_failure")


if __name__ == "__main__":
    unittest.main(verbosity=2)

#!/usr/bin/env python3
"""Exercise the rspc CLI/TUI against an isolated, offline rspc daemon."""
import fcntl
import json
import os
from pathlib import Path
import pty
import select
import shutil
import signal
import socket
import stat
import struct
import subprocess
import tempfile
import termios
import threading
import time
import unittest

ROOT = Path(__file__).resolve().parents[1]
BIN = Path(os.environ.get("RSPC_TEST_BINARY", ROOT / "target/release/rspc"))


class DaemonTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="rspc-test-")
        self.base = Path(self.tmp.name)
        self.sock = self.base / "engine.sock"
        self.env = dict(os.environ, TERM="xterm-256color")
        self.env.pop("TMUX", None)
        for name in ("CONFIG", "STATE", "CACHE", "RUNTIME"):
            path = self.base / name.lower()
            path.mkdir(mode=0o700)
            self.env[f"XDG_{name}_HOME" if name != "RUNTIME" else "XDG_RUNTIME_DIR"] = str(path)
        self.cmd = [str(BIN), "--demo", "--socket", str(self.sock)]
        self.addCleanup(self.cleanup_daemon)
        # Exercise automatic daemon startup, not just the foreground server.
        self.cli("queue")

    def cleanup_daemon(self):
        try:
            subprocess.run(self.cmd + ["stop"], env=self.env, timeout=10, capture_output=True)
            deadline = time.monotonic() + 5
            while self.sock.exists() and time.monotonic() < deadline:
                time.sleep(0.05)
            self.assertFalse(self.sock.exists(), "stop must clean up the daemon socket")
        finally:
            self.tmp.cleanup()

    def cli(self, *args):
        result = subprocess.run(self.cmd + ["--", *args], env=self.env,
                                capture_output=True, text=True, timeout=15)
        self.assertEqual(result.returncode, 0, result.stderr + "\n" + "\n".join(p.read_text() for p in (self.base / "state/rspc").glob("*.log")))
        return result.stdout

    def raw(self, command):
        with socket.socket(socket.AF_UNIX) as client:
            client.settimeout(3)
            client.connect(str(self.sock))
            stream = client.makefile("rwb", buffering=0)
            self.assertTrue(stream.readline().startswith(b"OK MPD "))
            stream.write(command.encode() + b"\n")
            reply = []
            while True:
                line = stream.readline()
                self.assertTrue(line, "daemon disconnected before completing a response")
                if line == b"OK\n":
                    return b"".join(reply).decode()
                self.assertFalse(line.startswith(b"ACK"), line)
                reply.append(line)

    def status(self):
        state = json.loads(self.cli("status"))
        elapsed = state["elapsed"]
        if isinstance(elapsed, dict):
            state["elapsed"] = elapsed["secs"] + elapsed["nanos"] / 1e9
        return state

    def tui(self, keys=(), hangup=False):
        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 35, 120, 0, 0))

        def terminal_session():
            os.setsid()
            fcntl.ioctl(0, termios.TIOCSCTTY, 0)

        start = time.monotonic()
        proc = subprocess.Popen(self.cmd, stdin=slave, stdout=slave, stderr=slave,
                                env=self.env, preexec_fn=terminal_session)
        os.close(slave)
        output = b""
        frame_at = None
        quit_at = None
        pending_keys = list(keys)
        next_key_at = None
        try:
            while time.monotonic() - start < 12:
                if select.select([master], [], [], 0.05)[0]:
                    try:
                        chunk = os.read(master, 65536)
                    except OSError:
                        break
                    output += chunk
                    # Reply to the frontend's terminal feature probes.
                    if b"\x1b[>q" in chunk:
                        os.write(master, b"\x1bP>|XTerm(390)\x1b\\")
                    if b"\x1b[?u" in chunk:
                        os.write(master, b"\x1b[?0u\x1b[?1;2c")
                    elif b"\x1b[c" in chunk or b"\x1b[0c" in chunk:
                        os.write(master, b"\x1b[?1;2c")
                    if b"\x1b[6n" in chunk:
                        os.write(master, b"\x1b[1;1R")
                    if frame_at is None and b"Morning Light" in output:
                        frame_at = time.monotonic() - start
                        if hangup:
                            os.killpg(proc.pid, signal.SIGHUP)
                            break
                        if pending_keys:
                            next_key_at = time.monotonic() + 0.15
                        else:
                            quit_at = time.monotonic() + 0.3
                if next_key_at is not None and time.monotonic() >= next_key_at:
                    os.write(master, pending_keys.pop(0))
                    if pending_keys:
                        next_key_at = time.monotonic() + 0.15
                    else:
                        next_key_at = None
                        quit_at = time.monotonic() + 0.3
                if quit_at is not None and time.monotonic() >= quit_at:
                    os.write(master, b"q")
                    quit_at = None
                if proc.poll() is not None:
                    break
            try:
                proc.wait(timeout=2)
            except subprocess.TimeoutExpired:
                self.fail("TUI did not close:\n" + output.decode(errors="replace")[-12000:])
            self.assertEqual(proc.returncode, -signal.SIGHUP if hangup else 0,
                             output.decode(errors="replace")[-4000:])
            self.assertIsNotNone(frame_at, output.decode(errors="replace")[-4000:])
            self.assertNotIn(b"unsupported command", output)
            print(f"\nrspc TUI first populated frame: {frame_at * 1000:.0f} ms", flush=True)
            return output
        finally:
            if proc.poll() is None:
                os.killpg(proc.pid, signal.SIGKILL)
                proc.wait()
            os.close(master)

    def test_quit_reopen_keeps_playing(self):
        self.cli("play", "0")
        before = self.status()
        self.tui()
        time.sleep(0.4)
        after = self.status()
        self.assertEqual(str(after["state"]).lower(), "play")
        self.assertEqual(before["songid"], after["songid"])
        self.assertGreater(after["elapsed"], before["elapsed"])
        self.tui()
        self.assertEqual(str(self.status()["state"]).lower(), "play")
        self.assertEqual(stat.S_IMODE(self.sock.stat().st_mode), 0o600)

    def test_terminal_hangup_keeps_engine_playing(self):
        self.cli("play", "0")
        before = self.status()
        self.tui(hangup=True)
        time.sleep(0.3)
        after = self.status()
        self.assertEqual(str(after["state"]).lower(), "play")
        self.assertEqual(before["songid"], after["songid"])
        self.assertGreater(after["elapsed"], before["elapsed"])

    def test_song_info_opens_the_full_catalog_with_browser_filtering(self):
        self.cli("play", "0")
        before = self.status()["songid"]
        # Open current-song info, select Album, open the catalog, then use
        # the ordinary browser filter before closing the popup.
        output = self.tui(keys=[b"oI", b"jjjj", b"\r", b"/", b"Morning", b"\r", b"h"])
        self.assertIn(b"Spotify catalog", output)
        self.assertEqual(self.status()["songid"], before)

    def test_people_browser_opens_a_curators_public_playlists(self):
        self.assertIn("directory: People", self.raw("lsinfo"))
        self.assertIn("Example Curator", self.raw('lsinfo "People"'))
        self.assertIn("Demo Mix", self.raw('lsinfo "People/profile/demo-user/Example Curator"'))
        self.assertIn("Morning Light", self.raw('lsinfo "People/playlist/demo/Demo Mix"'))
        output = self.tui(keys=[b"2", b"/", b"People", b"\r", b"l", b"l", b"l", b"1"])
        self.assertIn(b"Example Curator", output)
        self.assertIn(b"Demo Mix", output)
        self.assertEqual(self.status()["playlistlength"], 3)
        self.assertEqual(str(self.status()["state"]).lower(), "stop")

    def test_queue_commands_and_browsers_use_real_rmpc_parser(self):
        self.cli("play", "0")
        original = self.status()["songid"]
        self.raw("move 0 2")
        self.assertEqual(self.status()["songid"], original)
        self.assertEqual(len(json.loads(self.cli("queue"))), 3)
        self.cli("volume", "42")
        self.cli("seek", "12")
        self.cli("pause")
        state = self.status()
        self.assertEqual(state["volume"], 42)
        self.assertEqual(str(state["state"]).lower(), "pause")
        self.assertGreaterEqual(state["elapsed"], 12)
        self.cli("play")
        resumed = self.status()
        self.assertEqual(str(resumed["state"]).lower(), "play")
        self.assertGreaterEqual(resumed["elapsed"], 12, "play must resume a paused song")
        self.assertIn("Morning Light", self.raw('find "(Artist == \'The Example Ensemble\')"'))
        self.assertIn("Demo Mix", self.raw("listplaylists"))
        self.assertIn("Morning Light", self.raw('listplaylistinfo "Demo Mix" 0:1'))
        self.assertIn("directory: Liked Songs", self.raw("lsinfo"))
        self.raw("command_list_ok_begin\nrepeat 1\nrandom 1\ncommand_list_end")
        self.assertTrue(self.status()["repeat"])
        self.assertTrue(self.status()["random"])

    def test_paged_artist_browser_resolves_albums_and_keeps_whole_artist_actions(self):
        self.assertIn("command: rspc_artist_albums", self.raw("commands"))
        reply = self.raw('rspc_artist_albums "The Example Ensemble" 0')
        page = json.loads(reply.removeprefix("album_page: ").strip())
        self.assertIsNone(page["next"])
        self.assertEqual(len(page["albums"]), 1)
        uri = page["albums"][0]["uri"]
        songs = self.raw("lsinfo " + json.dumps(uri))
        self.assertEqual(songs.count("file: "), 3)
        before = self.raw("playlistinfo")
        self.raw("clear")
        self.raw("add " + json.dumps(uri))
        self.assertEqual(self.raw("playlistinfo").count("file: "), 3)
        # The TUI must use the advertised extension and leave the populated queue intact.
        output = self.tui(keys=[b"3", b"l", b"l", b"oi", b"\x1b"])
        self.assertNotIn(b"timed out", output)
        self.assertIn(b"Morning Light", output)
        self.assertEqual(self.raw("playlistinfo").count("file: "), before.count("file: "))

    def test_selection_info_for_folders_on_every_library_page(self):
        for tab in [b"2", b"3", b"4", b"5", b"6"]:
            with self.subTest(tab=tab):
                keys = [tab]
                if tab == b"2":
                    keys += [b"/", b"Liked Songs", b"\r"]
                output = self.tui(keys=keys + [b"oi", b"\x1b"])
                self.assertIn(b"Selection info", output)
                self.assertNotIn(b"timed out", output)
                self.assertNotIn(b"unsupported command", output)
        for kind, value, field in [
            ("path", "Playlists", "Playlists"),
            ("path", "Albums", "Albums"),
            ("path", "People", "People"),
            ("path", "People/profile/demo-user/Example Curator", "Public playlists"),
            ("artist", "The Example Ensemble", "Releases"),
        ]:
            reply = self.raw("rspc_selection_info " + json.dumps(kind) + " " + json.dumps(value))
            info = dict(json.loads(reply.removeprefix("selection_info: ").strip()))
            self.assertEqual(info[field], "1")
        for path in ["Liked Songs", "Albums", "Playlists", "People"]:
            self.assertEqual(self.raw("listallinfo " + json.dumps(path)).count("file: "), 3)

    def test_sigterm_cleans_up_socket_and_saves_queue(self):
        # A separately managed foreground daemon must share the same graceful path.
        self.raw("rspc_shutdown")
        deadline = time.monotonic() + 5
        while self.sock.exists() and time.monotonic() < deadline:
            time.sleep(0.05)
        proc = subprocess.Popen(self.cmd + ["daemon"], env=self.env,
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        try:
            deadline = time.monotonic() + 5
            while not self.sock.exists() and time.monotonic() < deadline:
                time.sleep(0.05)
            self.assertIn("playlistlength: 3", self.raw("status"))
            proc.terminate()
            stdout, stderr = proc.communicate(timeout=10)
            self.assertEqual(proc.returncode, 0, stderr.decode())
            self.assertFalse(self.sock.exists())
            queue = self.base / "state/rspc/demo-queue.json"
            self.assertEqual(len(json.loads(queue.read_text())["entries"]), 3)
            self.assertEqual(stat.S_IMODE(queue.stat().st_mode), 0o600)
        finally:
            if proc.poll() is None:
                proc.kill()
                proc.wait()

    def test_idle_coalesces_queue_changes_and_supports_cancellation(self):
        with socket.socket(socket.AF_UNIX) as client:
            client.settimeout(3)
            client.connect(str(self.sock))
            stream = client.makefile("rwb", buffering=0)
            stream.readline()
            # A player change must not overwrite a pending playlist change.
            self.raw("move 0 2")
            self.cli("play", "0")
            stream.write(b"idle playlist\n")
            self.assertEqual(stream.readline(), b"changed: playlist\n")
            self.assertEqual(stream.readline(), b"OK\n")
            stream.write(b"idle database\n")
            self.cli("volume", "51")
            self.assertFalse(select.select([client], [], [], 0.15)[0])
            stream.write(b"noidle\n")
            self.assertEqual(stream.readline(), b"OK\n")

    def test_repeated_seeks_and_concurrent_updates_keep_the_real_tui_connected(self):
        self.cli("play", "0")
        self.cli("pause")
        original = self.status()["songid"]
        done = threading.Event()
        errors = []

        def updates():
            try:
                while not done.wait(0.004):
                    self.raw("seekcur +0.001")
            except Exception as error:
                errors.append(error)

        worker = threading.Thread(target=updates)
        worker.start()
        try:
            output = self.tui(keys=[b"f", b"b"] * 16)
        finally:
            done.set()
            worker.join(timeout=5)
        self.assertFalse(worker.is_alive(), "update worker did not stop")
        self.assertFalse(errors, str(errors))
        self.assertFalse(any(message in output.lower() for message in (
            b"lost connection", b"reconnected to mpd", b"response from mpd timed out",
        )), "frontend disconnected during repeated seeks and player updates")
        self.assertEqual(self.status()["songid"], original)
        self.assertEqual(str(self.status()["state"]).lower(), "pause")

    def test_local_mode_reads_existing_rmpc_config_without_a_spotify_daemon(self):
        config = self.base / "config/rmpc"
        config.mkdir(parents=True)
        (config / "config.ron").write_text("(address: " + json.dumps(str(self.sock)) + ",)\n")
        shutil.rmtree(self.base / "config/rspc")
        result = subprocess.run([str(BIN), "--local", "--", "status"], env=self.env,
                                capture_output=True, text=True, timeout=5)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout)["playlistlength"], 3)
        self.assertFalse((self.base / "runtime/rspc/mpd.sock").exists())
        self.assertFalse((self.base / "config/rspc").exists(), "local mode should not create rspc config")

    def test_saved_playlist_edits_use_the_existing_rmpc_commands(self):
        original = json.loads(self.cli("queue"))
        self.cli("save", "Editable Copy")
        self.raw('playlistadd "Editable Copy" "spotify:track:demo1" 1')
        self.raw('playlistdelete "Editable Copy" 0')
        self.raw('playlistmove "Editable Copy" 0 2')
        songs = self.raw('listplaylistinfo "Editable Copy"')
        self.assertLess(songs.index("Title: Second Window"), songs.index("Title: Morning Light"))
        self.raw('playlistmove "Editable Copy" 0:2 1')
        songs = self.raw('listplaylistinfo "Editable Copy"')
        self.assertLess(songs.index("Title: Morning Light"), songs.index("Title: Second Window"))
        self.raw('rename "Editable Copy" "Renamed Copy"')
        self.raw('playlistclear "Renamed Copy"')
        self.assertEqual(self.raw('listplaylistinfo "Renamed Copy"'), "")
        self.raw('rm "Renamed Copy"')
        self.assertNotIn("Renamed Copy", self.raw("listplaylists"))
        self.assertEqual(json.loads(self.cli("queue")), original)

    def test_existing_passworded_rmpc_config_can_connect(self):
        config = self.base / "custom.ron"
        config.write_text('(address: "127.0.0.1:6600", password: Some("fixture"),)\n')
        result = subprocess.run(self.cmd + ["--config", str(config), "--", "status"],
                                env=self.env, capture_output=True, text=True, timeout=5)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout)["playlistlength"], 3)

    def test_playlist_exclusions_reload_without_restarting_or_stopping_playback(self):
        self.raw("rspc_shutdown")
        deadline = time.monotonic() + 5
        while self.sock.exists() and time.monotonic() < deadline:
            time.sleep(0.02)
        config = self.base / "custom-exclusions.ron"
        config.write_text('(spotify: (exclude_folders: [], exclude_playlists: ["Demo Mix"]),)\n')
        self.cmd += ["--config", str(config)]
        self.cli("play", "0")
        self.assertNotIn("Demo Mix", self.raw("listplaylists"))
        original = self.status()["songid"]
        inode = self.sock.stat().st_ino
        with socket.socket(socket.AF_UNIX) as client:
            client.settimeout(3)
            client.connect(str(self.sock))
            stream = client.makefile("rwb", buffering=0)
            stream.readline()
            stream.write(b"idle stored_playlist\n")
            config.write_text('(spotify: (exclude_folders: [], exclude_playlists: []),)\n')
            self.assertEqual(stream.readline(), b"changed: stored_playlist\n")
        deadline = time.monotonic() + 3
        while "Demo Mix" not in self.raw("listplaylists") and time.monotonic() < deadline:
            time.sleep(0.05)
        self.assertIn("Demo Mix", self.raw("listplaylists"))
        self.assertEqual(self.sock.stat().st_ino, inode)
        self.assertEqual(self.status()["songid"], original)
        self.assertEqual(str(self.status()["state"]).lower(), "play")

    def test_consume_once_and_natural_end(self):
        self.cli("play", "0")
        self.cli("consume", "oneshot")
        self.cli("seek", "180")
        deadline = time.monotonic() + 3
        state = self.status()
        while state["playlistlength"] != 2 and time.monotonic() < deadline:
            time.sleep(0.1)
            state = self.status()
        self.assertEqual(state["playlistlength"], 2)
        self.assertEqual(str(state["state"]).lower(), "play")
        self.assertIn(str(state["consume"]).lower(), ("off", "false", "0"))
        self.cli("single", "oneshot")
        self.cli("seek", "210")
        deadline = time.monotonic() + 3
        while str(self.status()["state"]).lower() != "stop" and time.monotonic() < deadline:
            time.sleep(0.1)
        state = self.status()
        self.assertEqual(str(state["state"]).lower(), "stop")
        self.assertIn(str(state["single"]).lower(), ("off", "false", "0"))


if __name__ == "__main__":
    if not BIN.exists() or not BIN.with_name("rspc-ui").exists():
        raise SystemExit("Build rspc and rspc-ui before running this test")
    unittest.main(verbosity=2)

#!/usr/bin/env python3
"""Temporarily permit Soloist pairing through the NixOS IPv4 firewall."""
import argparse
import ipaddress
import json
import os
from pathlib import Path
import shlex
import signal
import subprocess
import sys
import time
import urllib.request


def run(args, **kwargs):
    return subprocess.run(args, check=True, text=True, **kwargs)


def connect_port(state):
    # Inspect socket ownership, never the process command line (it has the key).
    pid = int((state / "soloist/soloist.pid").read_text().strip())
    proc = Path(f"/proc/{pid}")
    inodes = set()
    for fd in (proc / "fd").iterdir():
        try:
            inodes.add(os.readlink(fd))
        except FileNotFoundError:
            pass  # An unrelated file descriptor may close during inspection.
    ports = []
    for line in (proc / "net/tcp").read_text().splitlines()[1:]:
        fields = line.split()
        address, port = fields[1].split(":")
        if (address == "00000000" and fields[3] == "0A"
                and f"socket:[{fields[9]}]" in inodes):
            ports.append(int(port, 16))
    if len(ports) != 1:
        raise RuntimeError("Expected one Soloist Connect listener; no firewall changes made.")
    # Disable environment HTTP proxies for this strictly local probe.
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    with opener.open(f"http://127.0.0.1:{ports[0]}/zc?action=getInfo", timeout=3) as response:
        info = json.load(response)
    if info.get("remoteName") != "rspc" or info.get("status") != 101:
        raise RuntimeError("Listener is not a healthy rspc pairing endpoint.")
    return pid, ports[0]


def lan(interface):
    routes = json.loads(run(["ip", "-j", "route", "show", "default"], capture_output=True).stdout)
    if not interface:
        if not routes:
            raise RuntimeError("No default network route; use --interface.")
        interface = min(routes, key=lambda r: r.get("metric", 0))["dev"]
    addresses = json.loads(run(["ip", "-j", "-4", "addr", "show", "dev", interface],
                              capture_output=True).stdout)
    addresses = [a for link in addresses for a in link["addr_info"] if a["scope"] == "global"]
    if len(addresses) != 1:
        raise RuntimeError("Expected one IPv4 LAN address on the selected interface.")
    address = addresses[0]
    network = ipaddress.IPv4Interface(f"{address['local']}/{address['prefixlen']}")
    if not network.ip.is_private or network.ip.is_loopback or network.network.prefixlen < 16:
        raise RuntimeError("Pairing helper requires a private LAN with prefix /16 or narrower.")
    return interface, str(network.ip), str(network.network)


def pair(rspc, rules, timeout):
    run(["sudo", "-v"])
    # Restrict this helper to the NixOS iptables backend it was designed for.
    run(["sudo", "-n", "iptables", "-w", "-C", "INPUT", "-j", "nixos-fw"])
    installed = []
    try:
        for rule in rules:
            run(["sudo", "-n", "iptables", "-w", "-I", "nixos-fw", "1", *rule])
            installed.append(rule)
        print("Pairing allowed temporarily. Select rspc in Spotify's device menu.", flush=True)
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            report = run([str(rspc), "doctor"], capture_output=True, timeout=5).stdout
            if "logged_in: 1\n" in report:
                print("Soloist is paired. You can now play from rspc.")
                return
            time.sleep(1)
        raise RuntimeError("Pairing timed out. Check phone Wi-Fi and local-network permission.")
    finally:
        failed = False
        for rule in reversed(installed):
            result = subprocess.run(["sudo", "-n", "iptables", "-w", "-D", "nixos-fw", *rule])
            if result.returncode:
                failed = True
                print("Remove remaining rule with: " + shlex.join(
                    ["sudo", "iptables", "-w", "-D", "nixos-fw", *rule]), file=sys.stderr)
        if installed and not failed:
            print("Temporary firewall rules removed.", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--dry-run", action="store_true", help="print rules without requesting sudo or changing them")
    parser.add_argument("--interface", help="home LAN interface (default: default route)")
    parser.add_argument("--timeout", type=int, default=300, help="pairing window in seconds (1–600)")
    args = parser.parse_args()
    if not 1 <= args.timeout <= 600:
        parser.error("--timeout must be between 1 and 600")
    rspc = Path(__file__).resolve().parents[1] / "target/release/rspc"
    state = Path(os.environ.get("XDG_STATE_HOME", str(Path.home() / ".local/state"))) / "rspc"
    # Use the already-running player; do not restart playback or erase sessions.
    report = run([str(rspc), "doctor"], capture_output=True, timeout=5).stdout
    if "logged_in: 1\n" in report:
        print("Soloist is already paired; no firewall changes needed.")
        return
    pid, port = connect_port(state)
    interface, address, network = lan(args.interface)
    common = ["-i", interface, "-s", network]
    comment = ["-m", "comment", "--comment", f"rspc-pair-{os.getpid()}", "-j", "ACCEPT"]
    rules = [common + ["-p", "udp", "--dport", "5353"] + comment,
             common + ["-d", address, "-p", "tcp", "--dport", str(port)] + comment]
    print(f"Soloist PID {pid}: {interface}, home network {network}, pairing TCP {port}.", flush=True)
    for rule in rules:
        print(shlex.join(["sudo", "iptables", "-w", "-I", "nixos-fw", "1", *rule]), flush=True)
    if not args.dry_run:
        pair(rspc, rules, args.timeout)


if __name__ == "__main__":
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(143))
    try:
        main()
    except KeyboardInterrupt:
        sys.exit(130)
    except (OSError, ValueError, RuntimeError, subprocess.SubprocessError) as error:
        print(f"Pairing failed: {error}", file=sys.stderr)
        sys.exit(1)

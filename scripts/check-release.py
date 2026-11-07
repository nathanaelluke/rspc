#!/usr/bin/env python3
"""Check publishable files for private runtime data and common credential mistakes."""
import argparse
from dataclasses import dataclass
from pathlib import Path
import re
import subprocess
import sys


@dataclass(frozen=True)
class Finding:
    path: str
    line: int
    reason: str


RUNTIME_NAMES = {
    "credentials.json", "soloist-api-key", "queue.json", "demo-queue.json", "recent.json",
    "catalog-versions.json", ".env",
}
PRIVATE_DIRS = {
    "target", ".cargo", ".direnv", "streaming_cache", "catalog-v2",
    "soloist-install", "soloist-package",
}
PUBLIC_BINARY_FILES = {"crates/rspc-ui/assets/default.jpg"}
PATTERNS = {
    "private key": re.compile(rb"-----BEGIN (?:RSA |EC |OPENSSH )?PRIVATE KEY-----"),
    "GitHub token": re.compile(rb"\b(?:gh[pousr]_[A-Za-z0-9]{30,}|github_pat_[A-Za-z0-9_]{30,})\b"),
    "AWS access key": re.compile(rb"\b(?:AKIA|ASIA)[A-Z0-9]{16}\b"),
    "Slack token": re.compile(rb"\bxox[baprs]-[A-Za-z0-9-]{24,}\b"),
    "JWT": re.compile(rb"\beyJ[A-Za-z0-9_-]{16,}\.[A-Za-z0-9_-]{16,}\.[A-Za-z0-9_-]{16,}\b"),
    "literal credential": re.compile(
        rb"(?i)[\"']?(?:api[_-]?key|access[_-]?token|refresh[_-]?token|client[_-]?secret|password)[\"']?"
        rb"\s*[:=]\s*[\"'][A-Za-z0-9_+/=-]{24,}[\"']"
    ),
    "personal home path": re.compile(rb"/(?:home|Users)/[^\s/\"'<>]+"),
    "email address": re.compile(rb"[A-Za-z0-9_.+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}"),
}
EXAMPLE_HOMES = {b"/home/some_user", b"/home/u123", b"/home/example", b"/Users/example"}


def check_file(path, data):
    """Return locations and rule names, never matched values."""
    name = Path(path)
    if (name.name in RUNTIME_NAMES or name.name.startswith("soloist-api-key.")
            or name.suffix in {".log", ".sock", ".pid", ".pem", ".key"}
            or PRIVATE_DIRS.intersection(name.parts)
            or (name.name.startswith(".env.") and name.name != ".env.example")
            or name.name.endswith(".local.ron")):
        return [Finding(path, 0, "private config, runtime data or build output")]
    if b"\0" in data:
        if path in PUBLIC_BINARY_FILES:
            return []
        return [Finding(path, 0, "unreviewed binary file")]
    findings = []
    for reason, pattern in PATTERNS.items():
        for match in pattern.finditer(data):
            if reason == "personal home path" and match.group() in EXAMPLE_HOMES:
                continue
            if reason == "email address" and name.name.startswith("LICENSE"):
                continue  # Copyright notices must retain their original attribution.
            if reason == "email address" and match.group().rsplit(b"@", 1)[1] in {
                b"example.com", b"example.org", b"example.net",
            }:
                continue
            line = data.count(b"\n", 0, match.start()) + 1
            findings.append(Finding(path, line, reason))
    return findings


def git(root, *args):
    return subprocess.check_output(["git", "-C", str(root), *args])


def check_worktree(root):
    paths = git(root, "ls-files", "-z", "--cached", "--others", "--exclude-standard")
    findings = []
    count = 0
    for raw in sorted(set(paths.split(b"\0")) - {b""}):
        path = raw.decode()
        file = root / path
        if file.is_symlink():
            target = file.readlink()
            if target.is_absolute() or not file.resolve().is_relative_to(root.resolve()):
                findings.append(Finding(path, 0, "symlink points outside the repository"))
            continue
        if not file.is_file():
            continue  # Deleted or intent-to-add entries can remain in the index.
        count += 1
        findings.extend(check_file(path, file.read_bytes()))
    return count, findings


def check_history(root):
    findings = []
    count = 0
    for entry in git(root, "rev-list", "--objects", "--all").splitlines():
        oid, _, raw_path = entry.partition(b" ")
        if not raw_path or git(root, "cat-file", "-t", oid.decode()).strip() != b"blob":
            continue
        count += 1
        path = raw_path.decode()
        for finding in check_file(path, git(root, "cat-file", "blob", oid.decode())):
            findings.append(Finding(f"history:{path}", finding.line, finding.reason))
    return count, findings


def check_index(root):
    """Inspect staged content even if its working copy was changed or removed."""
    findings = []
    count = 0
    for entry in git(root, "ls-files", "--stage", "-z").split(b"\0"):
        if not entry:
            continue
        metadata, raw_path = entry.split(b"\t", 1)
        mode, oid, _ = metadata.split()
        path = raw_path.decode()
        if mode == b"160000":
            findings.append(Finding(f"index:{path}", 0, "unreviewed submodule"))
            continue
        data = git(root, "cat-file", "blob", oid.decode())
        count += 1
        if mode == b"120000":
            target = Path(data.decode())
            destination = (root / path).parent.joinpath(target).resolve()
            if target.is_absolute() or not destination.is_relative_to(root.resolve()):
                findings.append(Finding(f"index:{path}", 0, "symlink points outside the repository"))
        else:
            findings.extend(
                Finding(f"index:{path}", f.line, f.reason) for f in check_file(path, data)
            )
    return count, findings


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--history", action="store_true", help="also inspect reachable Git blobs")
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    count, findings = check_worktree(root)
    print(f"Checked {count} publishable working-tree files.")
    staged_count, staged = check_index(root)
    findings.extend(staged)
    print(f"Checked {staged_count} index entries.")
    if args.history:
        blobs, historical = check_history(root)
        findings.extend(historical)
        print(f"Checked {blobs} reachable historical blobs.")
    for finding in findings:
        print(f"{finding.path}:{finding.line}: {finding.reason}")
    if findings:
        print("Review these files before publishing. Matched values were withheld.")
        return 1
    print("No findings. Review new binary assets and diagnostic output separately.")
    return 0


if __name__ == "__main__":
    sys.exit(main())

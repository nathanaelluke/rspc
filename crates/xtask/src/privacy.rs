use anyhow::{Context, Result, ensure};
use regex::bytes::Regex;
use std::{
    collections::BTreeSet,
    fs,
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::LazyLock,
};

const LIMIT: usize = 16 * 1024 * 1024;
const PRIVATE_DIRS: &[&str] = &[
    "target",
    ".cargo",
    ".direnv",
    "streaming_cache",
    "catalog-v2",
    "soloist-install",
    "soloist-package",
];
const PRIVATE_NAMES: &[&str] = &[
    "credentials.json",
    "soloist-api-key",
    "queue.json",
    "demo-queue.json",
    "recent.json",
    "catalog-versions.json",
    "spotify-public-session.json",
    "soloist-refresh-recovery.json",
    ".device_id",
    "dbrts",
    ".env",
];
static PATTERNS: LazyLock<Vec<(&str, Regex)>> = LazyLock::new(|| {
    [
        ("private key", r"-----BEGIN (?:RSA |EC |OPENSSH )?PRIVATE KEY-----"),
        ("GitHub token", r"\b(?:gh[pousr]_[A-Za-z0-9]{30,}|github_pat_[A-Za-z0-9_]{30,})\b"),
        ("AWS access key", r"\b(?:AKIA|ASIA)[A-Z0-9]{16}\b"),
        ("Slack token", r"\bxox[baprs]-[A-Za-z0-9-]{24,}\b"),
        ("JWT", r"\beyJ[A-Za-z0-9_-]{16,}\.[A-Za-z0-9_-]{16,}\.[A-Za-z0-9_-]{16,}\b"),
        ("literal credential", r#"(?i)["']?(?:api[_-]?key|access[_-]?token|refresh[_-]?token|client[_-]?secret|password)["']?\s*[:=]\s*["'][A-Za-z0-9_+/=-]{24,}["']"#),
        ("personal home path", r#"/(?:home|Users)/[^\s/"'<>]+"#),
        ("email address", r"[A-Za-z0-9_.+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}"),
    ].into_iter().map(|(name, pattern)| (name, Regex::new(pattern).expect("credential pattern"))).collect()
});

#[derive(Debug, PartialEq, Eq)]
pub struct Finding {
    pub path: PathBuf,
    pub line: usize,
    pub reason: &'static str,
}

fn finding(path: &Path, reason: &'static str) -> Vec<Finding> {
    vec![Finding {
        path: path.into(),
        line: 0,
        reason,
    }]
}

pub fn check_file(path: &Path, data: &[u8]) -> Vec<Finding> {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let extension = path.extension().unwrap_or_default().to_string_lossy();
    if PRIVATE_NAMES.contains(&name.as_ref())
        || name.starts_with("soloist-api-key.")
        || ["log", "sock", "pid", "pem", "key"].contains(&extension.as_ref())
        || path.components().any(|part| {
            PRIVATE_DIRS.contains(&part.as_os_str().to_string_lossy().as_ref())
                && path != Path::new(".cargo/audit.toml")
        })
        || (name.starts_with(".env.") && name != ".env.example")
        || name.ends_with(".local.ron")
    {
        return finding(path, "private config, runtime data or build output");
    }
    if data.contains(&0) {
        return if ["crates/rspc-ui/assets/default.jpg", "assets/screenshot.png"]
            .iter()
            .any(|reviewed| path == Path::new(reviewed))
        {
            Vec::new()
        } else {
            finding(path, "unreviewed binary file")
        };
    }
    let mut findings = Vec::new();
    for (reason, pattern) in PATTERNS.iter() {
        for matched in pattern.find_iter(data) {
            let value = matched.as_bytes();
            if *reason == "personal home path"
                && [
                    b"/home/some_user".as_slice(),
                    b"/home/u123",
                    b"/home/example",
                    b"/Users/example",
                ]
                .contains(&value)
            {
                continue;
            }
            if *reason == "email address" {
                if name.starts_with("LICENSE") || value == b"aur@aur.archlinux.org" {
                    continue;
                }
                if let Some(domain) = value.rsplit(|byte| *byte == b'@').next()
                    && [b"example.com".as_slice(), b"example.org", b"example.net"].contains(&domain)
                {
                    continue;
                }
            }
            findings.push(Finding {
                path: path.into(),
                line: data[..matched.start()]
                    .iter()
                    .filter(|byte| **byte == b'\n')
                    .count()
                    + 1,
                reason,
            });
        }
    }
    findings
}

fn git(root: &Path, args: &[&str]) -> Result<Vec<u8>> {
    crate::capture(Command::new("git").arg("-C").arg(root).args(args), LIMIT)
}

fn check_link(root: &Path, path: &Path, target: &Path) -> Vec<Finding> {
    let destination = path.parent().unwrap_or(Path::new("")).join(target);
    let mut depth = 0usize;
    let mut escapes = target.is_absolute();
    for part in destination.components() {
        match part {
            std::path::Component::ParentDir => {
                if depth == 0 {
                    escapes = true;
                } else {
                    depth -= 1;
                }
            }
            std::path::Component::Normal(_) => depth += 1,
            _ => {}
        }
    }
    if root
        .join(&destination)
        .canonicalize()
        .is_ok_and(|resolved| !resolved.starts_with(root))
    {
        escapes = true;
    }
    if escapes {
        finding(path, "symlink points outside the repository")
    } else {
        Vec::new()
    }
}

pub fn worktree(root: &Path) -> Result<Vec<Finding>> {
    let paths = git(
        root,
        &[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ],
    )?;
    let paths: BTreeSet<_> = paths
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .collect();
    let mut findings = Vec::new();
    for raw in paths {
        let path = Path::new(std::str::from_utf8(raw).context("Non-UTF-8 source path")?);
        let file = root.join(path);
        let metadata = match file.symlink_metadata() {
            Ok(meta) => meta,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        };
        if metadata.is_symlink() {
            findings.extend(check_link(root, path, &file.read_link()?));
        } else if metadata.is_file() {
            if metadata.len() > LIMIT as u64 {
                findings.extend(finding(path, "source file exceeds review limit"));
            } else {
                findings.extend(check_file(path, &fs::read(file)?));
            }
        }
    }
    Ok(findings)
}

/// One Git process streams all blobs, keeping history scans bounded and cheap.
fn blobs(root: &Path, entries: &[(String, PathBuf, bool)]) -> Result<Vec<Finding>> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["cat-file", "--batch"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    let result = (|| {
        let mut input = child.stdin.take().context("Missing Git input")?;
        let mut output = BufReader::new(child.stdout.take().context("Missing Git output")?);
        let mut findings = Vec::new();
        for (oid, path, symlink) in entries {
            writeln!(input, "{oid}")?;
            input.flush()?;
            let mut header = String::new();
            output.read_line(&mut header)?;
            let fields: Vec<_> = header.split_whitespace().collect();
            ensure!(fields.len() == 3, "Invalid Git object header");
            let size: u64 = fields[2].parse()?;
            if size > LIMIT as u64 || fields[1] != "blob" {
                std::io::copy(&mut output.by_ref().take(size), &mut std::io::sink())?;
                if size > LIMIT as u64 {
                    findings.extend(finding(path, "source blob exceeds review limit"));
                }
            } else {
                let mut data = vec![0; size as usize];
                output.read_exact(&mut data)?;
                if *symlink {
                    findings.extend(check_link(
                        root,
                        path,
                        Path::new(std::str::from_utf8(&data)?),
                    ));
                } else {
                    findings.extend(check_file(path, &data));
                }
            }
            let mut newline = [0];
            output.read_exact(&mut newline)?;
            ensure!(newline == *b"\n", "Invalid Git object terminator");
        }
        Ok(findings)
    })();
    if result.is_err() {
        let _ = child.kill();
    }
    let status = child.wait()?;
    let findings = result?;
    ensure!(status.success(), "Git object scan failed");
    Ok(findings)
}

pub fn index(root: &Path) -> Result<Vec<Finding>> {
    let data = git(root, &["ls-files", "--stage", "-z"])?;
    let mut entries = Vec::new();
    let mut findings = Vec::new();
    for entry in data.split(|b| *b == 0).filter(|e| !e.is_empty()) {
        let text = std::str::from_utf8(entry)?;
        let (meta, name) = text.split_once('\t').context("Invalid index entry")?;
        let fields: Vec<_> = meta.split_whitespace().collect();
        ensure!(fields.len() == 3, "Invalid index metadata");
        if fields[0] == "160000" {
            findings.extend(finding(Path::new(name), "unreviewed submodule"));
            continue;
        }
        entries.push((
            fields[1].to_owned(),
            PathBuf::from(name),
            fields[0] == "120000",
        ));
    }
    findings.extend(blobs(root, &entries)?.into_iter().map(|mut f| {
        f.path = PathBuf::from(format!("index:{}", f.path.display()));
        f
    }));
    Ok(findings)
}

pub fn history(root: &Path) -> Result<Vec<Finding>> {
    let data = git(root, &["rev-list", "--objects", "--all"])?;
    let entries: Vec<_> = std::str::from_utf8(&data)?
        .lines()
        .filter_map(|line| line.split_once(' '))
        .map(|(oid, path)| (oid.to_owned(), PathBuf::from(path), false))
        .collect();
    Ok(blobs(root, &entries)?
        .into_iter()
        .map(|mut f| {
            f.path = PathBuf::from(format!("history:{}", f.path.display()));
            f
        })
        .collect())
}

pub fn check(root: &Path, include_history: bool) -> Result<Vec<Finding>> {
    let root = root.canonicalize()?;
    let mut findings = worktree(&root)?;
    findings.extend(index(&root)?);
    if include_history {
        findings.extend(history(&root)?);
    }
    Ok(findings)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn secrets_are_reported_by_location_without_the_value() {
        let secret = "sample".repeat(8);
        let findings = check_file(
            Path::new("settings.rs"),
            format!("api_key = \"{secret}\"").as_bytes(),
        );
        assert_eq!(findings.len(), 1);
        assert!(!format!("{findings:?}").contains(&secret));
    }
    #[test]
    fn runtime_paths_and_unreviewed_binaries_are_rejected() {
        for path in [
            "credentials.json",
            "config/soloist-api-key",
            "logs/player.log",
            ".env",
            "cache/catalog-v2/page.json",
            "catalog-versions.json",
            "spotify-public-session.json",
            "soloist-refresh-recovery.json",
            "cache/dbrts",
            ".device_id",
            ".cargo/config.toml",
            "demo-queue.json",
            "target/player",
        ] {
            assert!(!check_file(Path::new(path), b"").is_empty(), "{path}");
        }
        assert!(!check_file(Path::new("assets/new.png"), b"image\0data").is_empty());
        assert!(
            check_file(
                Path::new("crates/rspc-ui/assets/default.jpg"),
                b"image\0data"
            )
            .is_empty()
        );
    }
    #[test]
    fn examples_and_public_service_accounts_are_allowed() {
        assert!(check_file(Path::new(".cargo/audit.toml"), b"[advisories]").is_empty());
        assert!(
            check_file(
                Path::new("example.rs"),
                format!(
                    "CLIENT = \"{}\"; path = \"/home/example/music\";",
                    "1".repeat(32)
                )
                .as_bytes()
            )
            .is_empty()
        );
        assert!(
            check_file(
                Path::new("README.md"),
                b"git clone ssh://aur@aur.archlinux.org/rspc.git"
            )
            .is_empty()
        );
        let email = [b"contact someone".as_slice(), b"@", b"private.invalid"].concat();
        assert!(!check_file(Path::new("README.md"), &email).is_empty());
    }
    fn repository() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        for args in [
            vec!["init", "-q"],
            vec!["config", "user.name", "Test"],
            vec!["config", "user.email", "test@example.org"],
            vec!["config", "commit.gpgsign", "false"],
            vec!["config", "core.hooksPath", "/dev/null"],
        ] {
            git(tmp.path(), &args).unwrap();
        }
        tmp
    }
    #[test]
    fn tracked_ignored_files_and_historical_secrets_are_checked() {
        let tmp = repository();
        let root = tmp.path();
        fs::write(root.join(".gitignore"), "credentials.json\n").unwrap();
        fs::write(root.join("credentials.json"), "{}").unwrap();
        git(root, &["add", "-f", "credentials.json", ".gitignore"]).unwrap();
        assert!(
            worktree(root)
                .unwrap()
                .iter()
                .any(|f| f.path == Path::new("credentials.json"))
        );
        git(root, &["commit", "-qm", "fixture"]).unwrap();
        git(root, &["rm", "-q", "credentials.json"]).unwrap();
        git(root, &["commit", "-qm", "remove fixture"]).unwrap();
        assert!(
            history(root)
                .unwrap()
                .iter()
                .any(|f| f.path == Path::new("history:credentials.json"))
        );
    }
    #[test]
    fn staged_secrets_survive_worktree_edits_or_deletion() {
        let tmp = repository();
        let root = tmp.path();
        let path = root.join("settings.rs");
        let secret = "fixture".repeat(8);
        fs::write(&path, format!("api_key = \"{secret}\"")).unwrap();
        git(root, &["add", "settings.rs"]).unwrap();
        fs::write(&path, "safe worktree").unwrap();
        for removed in [false, true] {
            if removed {
                fs::remove_file(&path).unwrap();
            }
            let findings = index(root).unwrap();
            assert_eq!(findings.len(), 1);
            assert!(!format!("{findings:?}").contains(&secret));
        }
    }
}

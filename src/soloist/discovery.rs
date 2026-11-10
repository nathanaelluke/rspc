//! Find the ZeroConf listener owned by our child without reading its command
//! line (which contains the developer key) or opening the firewall.
use anyhow::{Context, Result, ensure};
#[cfg(any(target_os = "linux", test))]
use std::{collections::HashSet, path::Path};

#[cfg(target_os = "linux")]
pub(crate) fn port(pid: u32) -> Result<u16> {
    let proc = std::path::PathBuf::from(format!("/proc/{pid}"));
    let mut sockets = HashSet::new();
    for fd in std::fs::read_dir(proc.join("fd"))? {
        let fd = fd?;
        if let Ok(target) = std::fs::read_link(fd.path())
            && let Some(inode) = socket_inode(&target)
        {
            sockets.insert(inode);
        }
    }
    let table = std::fs::read_to_string(proc.join("net/tcp"))?;
    listener(&table, &sockets)
}

#[cfg(target_os = "macos")]
pub(crate) fn port(pid: u32) -> Result<u16> {
    // lsof reports only this child's IPv4 listening sockets. Command-line
    // inspection would expose the Soloist developer key.
    let output = std::process::Command::new("/usr/sbin/lsof")
        .args([
            "-nP",
            "-a",
            "-p",
            &pid.to_string(),
            "-i4TCP",
            "-sTCP:LISTEN",
            "-Fn",
        ])
        .output()
        .context("Cannot inspect Soloist's sockets with /usr/sbin/lsof")?;
    ensure!(
        output.status.success(),
        "Cannot identify Soloist's listening sockets"
    );
    macos_listener(std::str::from_utf8(&output.stdout)?)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(crate) fn port(_: u32) -> Result<u16> {
    anyhow::bail!(
        "Automatic Soloist authorization supports Linux and macOS; use manual pairing on this platform"
    )
}

#[cfg(any(target_os = "macos", test))]
fn macos_listener(output: &str) -> Result<u16> {
    let mut ports: Vec<u16> = output
        .lines()
        .filter_map(|line| line.strip_prefix("n*:"))
        .map(str::parse)
        .collect::<Result<_, _>>()
        .context("Invalid Soloist listener port")?;
    ports.sort_unstable();
    ports.dedup();
    ensure!(
        ports.len() == 1,
        "Cannot identify Soloist's Connect listener"
    );
    Ok(ports[0])
}

#[cfg(any(target_os = "linux", test))]
fn socket_inode(target: &Path) -> Option<u64> {
    target
        .to_str()?
        .strip_prefix("socket:[")?
        .strip_suffix(']')?
        .parse()
        .ok()
}

#[cfg(any(target_os = "linux", test))]
fn listener(table: &str, sockets: &HashSet<u64>) -> Result<u16> {
    let mut ports = Vec::new();
    for line in table.lines().skip(1) {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.len() < 10 || fields[3] != "0A" {
            continue;
        }
        let Some((address, port)) = fields[1].split_once(':') else {
            continue;
        };
        if address != "00000000"
            || !fields[9]
                .parse::<u64>()
                .ok()
                .is_some_and(|n| sockets.contains(&n))
        {
            continue;
        }
        ports.push(u16::from_str_radix(port, 16).context("Invalid Soloist listener port")?);
    }
    ensure!(
        ports.len() == 1,
        "Cannot identify Soloist's Connect listener"
    );
    Ok(ports[0])
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn macos_discovery_ignores_the_loopback_websocket_and_duplicate_descriptors() {
        assert_eq!(
            macos_listener("p123\nn127.0.0.1:1234\nn*:5000\nn*:5000\n").unwrap(),
            5000
        );
        assert!(macos_listener("p123\nn*:5000\nn*:5001\n").is_err());
        assert!(macos_listener("n*:invalid\n").is_err());
    }
    #[test]
    fn only_the_owned_discovery_listener_is_selected() {
        let sockets = HashSet::from([12, 13]);
        let table = "header\n0: 00000000:D431 remote 0A a b c d e 99\n1: 0100007F:ABCD remote 0A a b c d e 12\n2: 00000000:C001 remote 01 a b c d e 13\n3: 00000000:CAFE remote 0A a b c d e 13\n";
        assert_eq!(listener(table, &sockets).unwrap(), 0xcafe);
        assert!(listener(table, &HashSet::new()).is_err());
        assert!(
            listener(
                &format!("{table}4: 00000000:BEEF remote 0A a b c d e 12\n"),
                &sockets
            )
            .is_err()
        );
    }
    #[test]
    fn a_closed_descriptor_or_regular_file_is_not_a_socket() {
        assert_eq!(socket_inode(Path::new("socket:[123]")), Some(123));
        assert_eq!(socket_inode(Path::new("/tmp/log")), None);
        assert_eq!(socket_inode(Path::new("socket:[bad]")), None);
    }
}

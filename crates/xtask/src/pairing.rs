use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use std::{
    path::Path,
    process::Command,
    time::{Duration, Instant},
};

type Rule = Vec<String>;

fn sudo(args: &[&str]) -> Result<()> {
    crate::run(Command::new("sudo").args(args))
}

pub struct Rules<F: FnMut(&str, &Rule) -> Result<()>> {
    apply: F,
    installed: Vec<Rule>,
}
impl<F: FnMut(&str, &Rule) -> Result<()>> Rules<F> {
    pub fn new(apply: F) -> Self {
        Self {
            apply,
            installed: Vec::new(),
        }
    }
    pub fn insert(&mut self, rule: Rule) -> Result<()> {
        (self.apply)("-I", &rule)?;
        self.installed.push(rule);
        Ok(())
    }
}
impl<F: FnMut(&str, &Rule) -> Result<()>> Drop for Rules<F> {
    fn drop(&mut self) {
        for rule in self.installed.iter().rev() {
            if let Err(error) = (self.apply)("-D", rule) {
                eprintln!(
                    "Cannot remove temporary pairing rule: {error}. Remove with: sudo iptables -w -D nixos-fw {}",
                    rule.iter()
                        .map(|part| format!("{part:?}"))
                        .collect::<Vec<_>>()
                        .join(" ")
                );
            }
        }
    }
}

fn lan(interface: Option<&str>) -> Result<(String, String, String)> {
    let routes: Value = serde_json::from_slice(&crate::capture(
        Command::new("ip").args(["-j", "route", "show", "default"]),
        1024 * 1024,
    )?)?;
    let interface = interface
        .map(str::to_owned)
        .or_else(|| {
            routes
                .as_array()?
                .iter()
                .min_by_key(|route| route["metric"].as_u64().unwrap_or(0))?["dev"]
                .as_str()
                .map(str::to_owned)
        })
        .context("No default network route; use --interface")?;
    ensure!(!interface.starts_with('-'), "Invalid network interface");
    let addresses: Value = serde_json::from_slice(&crate::capture(
        Command::new("ip").args(["-j", "-4", "addr", "show", "dev", &interface]),
        1024 * 1024,
    )?)?;
    let addresses: Vec<_> = addresses
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|link| link["addr_info"].as_array().into_iter().flatten())
        .filter(|address| address["scope"] == "global")
        .collect();
    ensure!(addresses.len() == 1, "Expected one IPv4 LAN address");
    let ip: std::net::Ipv4Addr = addresses[0]["local"]
        .as_str()
        .context("Missing LAN address")?
        .parse()?;
    let prefix = addresses[0]["prefixlen"]
        .as_u64()
        .context("Missing LAN prefix")?;
    ensure!(
        ip.is_private() && !ip.is_loopback() && (16..=32).contains(&prefix),
        "Pairing requires a private LAN with prefix /16 or narrower"
    );
    let network = std::net::Ipv4Addr::from(u32::from(ip) & (u32::MAX << (32 - prefix)));
    Ok((interface, ip.to_string(), format!("{network}/{prefix}")))
}

pub async fn pair(root: &Path, dry_run: bool, interface: Option<&str>, timeout: u64) -> Result<()> {
    ensure!(
        cfg!(target_os = "linux"),
        "Temporary firewall pairing is supported only on NixOS Linux"
    );
    let rspc = root.join("target/release/rspc");
    let report = || crate::capture(Command::new(&rspc).arg("doctor"), 1024 * 1024);
    if std::str::from_utf8(&report()?)?.contains("logged_in: 1\n") {
        println!("Soloist is already paired; no firewall changes needed.");
        return Ok(());
    }
    let state = rspc::paths::state().join("soloist");
    let pid = std::fs::read_to_string(state.join("soloist.pid"))?
        .trim()
        .parse()?;
    let port = rspc::pairing_port(pid)?;
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(3))
        .build()?;
    let info: Value = client
        .get(format!("http://127.0.0.1:{port}/zc?action=getInfo"))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    ensure!(
        info["remoteName"] == "rspc" && info["status"] == 101,
        "Listener is not a healthy rspc pairing endpoint"
    );
    let (interface, address, network) = lan(interface)?;
    let comment = format!("rspc-pair-{}", std::process::id());
    let common = ["-i", &interface, "-s", &network];
    let suffix = ["-m", "comment", "--comment", &comment, "-j", "ACCEPT"];
    let rules: Vec<Rule> = [
        vec!["-p", "udp", "--dport", "5353"],
        vec!["-d", &address, "-p", "tcp", "--dport", &port.to_string()],
    ]
    .into_iter()
    .map(|rule| {
        common
            .iter()
            .chain(rule.iter())
            .chain(suffix.iter())
            .map(|part| (*part).to_owned())
            .collect()
    })
    .collect();
    if dry_run {
        for rule in &rules {
            println!(
                "sudo iptables -w -I nixos-fw 1 {}",
                rule.iter()
                    .map(|part| format!("{part:?}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            );
        }
        return Ok(());
    }
    sudo(&["-v"])?;
    sudo(&["-n", "iptables", "-w", "-C", "INPUT", "-j", "nixos-fw"])?;
    let mut installed = Rules::new(|action: &str, rule: &Rule| {
        let mut cmd = Command::new("sudo");
        cmd.args(["-n", "iptables", "-w", action, "nixos-fw"]);
        if action == "-I" {
            cmd.arg("1");
        }
        crate::run(cmd.args(rule))
    });
    // Signal futures keep Ctrl-C/SIGTERM on the normal cleanup path.
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    for rule in rules {
        installed.insert(rule)?;
    }
    println!("Pairing allowed temporarily. Select rspc in Spotify's device menu.");
    let deadline = Instant::now() + Duration::from_secs(timeout);
    loop {
        if std::str::from_utf8(&report()?)?.contains("logged_in: 1\n") {
            println!("Soloist is paired.");
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("Pairing timed out");
        }
        tokio::select! {
            result = tokio::signal::ctrl_c() => {result?;bail!("Pairing cancelled");},
            _ = terminate.recv() => bail!("Pairing terminated"),
            _ = tokio::time::sleep(Duration::from_secs(1)) => {},
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    #[test]
    fn every_exit_path_rolls_back_only_inserted_rules_in_reverse_order() {
        for outcome in [
            "paired",
            "interrupt",
            "timeout",
            "insert_failure",
            "daemon_failure",
        ] {
            let inserted = RefCell::new(Vec::new());
            let deleted = RefCell::new(Vec::new());
            let exercise = || -> Result<()> {
                let mut rules = Rules::new(|action: &str, rule: &Rule| {
                    if action == "-I" {
                        ensure!(
                            outcome != "insert_failure" || inserted.borrow().is_empty(),
                            "fixture insert failure"
                        );
                        inserted.borrow_mut().push(rule.clone());
                    } else {
                        deleted.borrow_mut().push(rule.clone());
                    }
                    Ok(())
                });
                rules.insert(vec!["udp".into()])?;
                rules.insert(vec!["tcp".into()])?;
                ensure!(outcome == "paired", "fixture exit");
                Ok(())
            };
            assert_eq!(exercise().is_ok(), outcome == "paired");
            assert_eq!(
                *deleted.borrow(),
                inserted.borrow().iter().rev().cloned().collect::<Vec<_>>()
            );
        }
    }
}

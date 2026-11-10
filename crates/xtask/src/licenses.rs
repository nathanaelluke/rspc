use anyhow::{Context, Result, ensure};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write,
    fs,
    path::Path,
    process::Command,
    time::Duration,
};

pub const BUNDLE: &str = "packaging/LICENSE-THIRD-PARTY";
const TOOL: &str = "cargo-about 0.9.0";
// Notices missing from published crates, plus the statically linked Rust library.
// Hashes pin the original text independently of changes to the remote repository.
const EXTRA_NOTICES: &[(&str, &str, &str)] = &[
    (
        "governor 0.10.4",
        "https://raw.githubusercontent.com/boinkor-net/governor/9f3a79dd47dd32acd589c562b8d4fefe99b93372/LICENSE",
        "2248fdedb215c73b6431d4c0419c6476c0aa19621fa94041aa7c5a3ce952e5cc",
    ),
    (
        "objc2 framework crates 0.3.2",
        "https://raw.githubusercontent.com/madsmtm/objc2/7b1abfd750a2cacaea71d6a56ecfb83cb7de560b/LICENSE.md",
        "7f976f7e9cb2d87df7230606feb932c3f21ac0e664045a775b600046ff850c54",
    ),
    (
        "zune-inflate 0.2.54",
        "https://raw.githubusercontent.com/etemesi254/zune-image/69502ce83fdfecdd0beefd677e2abb3781b29d98/LICENSE.md",
        "e870d7fd7819a9f0453c62798cb52ae95d148a8c91dc2de1d983f997143c72de",
    ),
    (
        "Rust standard library",
        "https://raw.githubusercontent.com/rust-lang/rust/6f1c7ee0ec0ea64802f4b7a184b63b7efb645b3b/LICENSE-MIT",
        "b71bd43a069ca0641a9ecfe585ca7b3c53b5cc1608f8b68321168698e28b5ea1",
    ),
];
const INPUTS: &[&str] = &[
    "Cargo.lock",
    "Cargo.toml",
    "crates/rspc-ui/Cargo.toml",
    "crates/xtask/Cargo.toml",
    "packaging/licenses.toml",
    "packaging/LICENSE-GPL-3.0",
];

fn fingerprint(root: &Path) -> Result<String> {
    let mut hash = Sha256::new();
    hash.update(TOOL.as_bytes());
    for &(package, url, checksum) in EXTRA_NOTICES {
        for field in [package, url, checksum] {
            hash.update(field.as_bytes());
            hash.update([0]);
        }
    }
    for path in INPUTS {
        let data = fs::read(root.join(path)).with_context(|| format!("Cannot read {path}"))?;
        hash.update(path.as_bytes());
        hash.update((data.len() as u64).to_le_bytes());
        hash.update(data);
    }
    Ok(format!("{:x}", hash.finalize()))
}

pub fn check(root: &Path) -> Result<()> {
    let bundle = fs::read_to_string(root.join(BUNDLE))
        .context("Missing third-party licenses. Run `cargo run -p xtask -- licenses`.")?;
    let (header, text) = bundle
        .split_once("\n\n")
        .context("Invalid license bundle")?;
    let expected = format!("Inputs SHA256: {}", fingerprint(root)?);
    ensure!(
        header.lines().any(|line| line == expected),
        "Third-party licenses are stale. Run `cargo run -p xtask -- licenses`."
    );
    ensure!(
        header
            .lines()
            .any(|line| line == format!("Text SHA256: {:x}", Sha256::digest(text))),
        "Third-party license text changed. Regenerate the bundle."
    );
    Ok(())
}

#[derive(Default)]
struct Notice {
    titles: BTreeSet<String>,
    packages: BTreeSet<String>,
}

fn add(notices: &mut BTreeMap<String, Notice>, title: &str, text: &str, package: &str) {
    let text = text
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n");
    let notice = notices.entry(text.trim_end().to_owned()).or_default();
    notice.titles.insert(title.to_owned());
    notice.packages.insert(package.to_owned());
}

fn field<'a>(value: &'a Value, name: &str) -> Result<&'a str> {
    value[name]
        .as_str()
        .with_context(|| format!("Missing license metadata: {name}"))
}

fn package_label(package: &Value) -> Result<String> {
    let mut label = format!("{} {}", field(package, "name")?, field(package, "version")?);
    if let Some(authors) = package["authors"].as_array() {
        let authors: Vec<_> = authors.iter().filter_map(Value::as_str).collect();
        if !authors.is_empty() {
            write!(label, " (authors: {})", authors.join(", "))?;
        }
    }
    Ok(label)
}

fn render(root: &Path, report: &Value, extra: &[(String, String)]) -> Result<String> {
    let mut notices = BTreeMap::new();
    for license in report["licenses"].as_array().context("Missing licenses")? {
        let text = field(license, "text")?;
        // Some packages supply only an SPDX identifier. Keep its permission text
        // and the declared authors without adding a placeholder copyright holder.
        let text = text.replace("Copyright (c) <year> <copyright holders>\n", "");
        for used in license["used_by"]
            .as_array()
            .context("Missing license users")?
        {
            let package = &used["crate"];
            if !package["source"].is_null() {
                add(
                    &mut notices,
                    field(license, "id")?,
                    &text,
                    &package_label(package)?,
                );
            }
        }
    }
    // Preserve supplementary notices too, including copyright files and composite
    // licenses that a classifier can mistake for a single standard license.
    for entry in report["crates"]
        .as_array()
        .context("Missing crate inventory")?
    {
        let package = &entry["package"];
        if package["source"].is_null() {
            continue;
        }
        let directory = Path::new(field(package, "manifest_path")?)
            .parent()
            .context("Missing crate directory")?;
        for file in fs::read_dir(directory)? {
            let file = file?;
            let name = file.file_name().to_string_lossy().to_ascii_uppercase();
            if file.file_type()?.is_file()
                && [
                    "LICENSE",
                    "LICENCE",
                    "COPYING",
                    "COPYRIGHT",
                    "NOTICE",
                    "UNLICENSE",
                ]
                .iter()
                .any(|prefix| name.starts_with(prefix))
            {
                let text = fs::read_to_string(file.path())?;
                add(
                    &mut notices,
                    "Original upstream notices",
                    &text,
                    &package_label(package)?,
                );
            }
        }
    }
    ensure!(!notices.is_empty(), "Empty third-party license inventory");
    let gpl = fs::read_to_string(root.join("packaging/LICENSE-GPL-3.0"))?;
    add(
        &mut notices,
        "GPL-3.0 (incorporated by LGPL-3.0)",
        &gpl,
        "ansi_colours",
    );
    for (package, text) in extra {
        add(&mut notices, "Supplementary upstream notice", text, package);
    }
    let mut text = format!(
        "These notices cover Rust dependencies for all three supported release targets,\n\
         including build tools. rspc's own licenses are distributed separately.\n\n\
         Corresponding source and rebuilding\n\
         ===================================\n\
         The complete application and vendored Cargo dependency sources are provided in\n\
         rspc-{}-source.tar.gz alongside the binaries at:\n\
         https://github.com/nathanaelluke/rspc/releases/tag/v{}\n\n\
         You may modify and relink the LGPL-covered dependency under the terms below.\n\
         To do so, extract the source archive, copy the dependency out of vendor/, and\n\
         add a [patch.crates-io] entry in Cargo.toml pointing to that modified copy.\n\
         Rebuild with current stable Rust and the native dependencies in README.md:\n\
         cargo build --offline --release -j1 -p rspc -p rspc-ui\n\n\
         Authors below come from upstream package metadata. Original notices supplied\n\
         by upstream are reproduced without changing their terms.\n",
        crate::release::VERSION,
        crate::release::VERSION
    );
    for (body, notice) in notices {
        write!(
            text,
            "\n{}\n{}\n",
            "=".repeat(78),
            notice.titles.into_iter().collect::<Vec<_>>().join(" / ")
        )?;
        for package in notice.packages {
            writeln!(text, "  {package}")?;
        }
        write!(text, "\n{body}\n")?;
    }
    Ok(text)
}

pub async fn generate(root: &Path) -> Result<()> {
    let version = crate::capture(Command::new("cargo-about").arg("--version"), 1024)
        .context("Install cargo-about 0.9.0, or use `nix develop`.")?;
    ensure!(
        String::from_utf8(version)?.trim() == TOOL,
        "Use {TOOL} to regenerate licenses"
    );
    let temp = tempfile::tempdir()?;
    let report_path = temp.path().join("licenses.json");
    crate::run(
        Command::new("cargo-about")
            .current_dir(root)
            .args([
                "generate",
                "--locked",
                "--workspace",
                "--fail",
                "--format",
                "json",
                "--config",
                "packaging/licenses.toml",
                "--output-file",
            ])
            .arg(&report_path),
    )?;
    let report: Value = serde_json::from_slice(&fs::read(report_path)?)?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;
    let mut extra = Vec::new();
    for &(package, url, hash) in EXTRA_NOTICES {
        let mut response = client.get(url).send().await?.error_for_status()?;
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            ensure!(
                bytes.len() + chunk.len() <= 1024 * 1024,
                "Oversized notice for {package}"
            );
            bytes.extend_from_slice(&chunk);
        }
        ensure!(
            format!("{:x}", Sha256::digest(&bytes)) == hash,
            "Notice changed for {package}"
        );
        extra.push((package.to_owned(), String::from_utf8(bytes)?));
    }
    let text = render(root, &report, &extra)?;
    let output = format!(
        "rspc third-party licenses\nGenerated with {TOOL}\nInputs SHA256: {}\nText SHA256: {:x}\n\n{text}",
        fingerprint(root)?,
        Sha256::digest(&text)
    );
    fs::write(root.join(BUNDLE), output)?;
    check(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundle_rejects_changed_inputs_and_truncated_notices() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = crate::root();
        for path in INPUTS.iter().copied().chain([BUNDLE]) {
            let destination = temp.path().join(path);
            fs::create_dir_all(destination.parent().unwrap())?;
            fs::copy(root.join(path), destination)?;
        }
        check(temp.path())?;
        fs::write(temp.path().join("Cargo.lock"), "changed dependencies")?;
        assert!(check(temp.path()).is_err());
        fs::copy(root.join("Cargo.lock"), temp.path().join("Cargo.lock"))?;
        let mut bundle = fs::read(temp.path().join(BUNDLE))?;
        bundle.truncate(bundle.len() / 2);
        fs::write(temp.path().join(BUNDLE), bundle)?;
        assert!(check(temp.path()).is_err());
        Ok(())
    }
}

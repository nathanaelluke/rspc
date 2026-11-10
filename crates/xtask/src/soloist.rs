use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{fs, io::Write, path::Path, process::Command, time::Duration};

pub const ARCHIVES: [(&str, &str); 3] = [
    ("x86_64-linux", "x86_64"),
    ("aarch64-linux", "arm64"),
    ("aarch64-darwin", "osx_arm64"),
];
const HOST: &str = "soloist-builds.spotifycdn.com";
const LIMIT: usize = 64 * 1024 * 1024;

pub fn changelog_version(text: &str) -> Result<&str> {
    text.lines()
        .find_map(|line| line.strip_prefix("## Release v"))
        .filter(|version| {
            version.split('.').count() == 3
                && version
                    .split('.')
                    .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
        })
        .context("Cannot identify Soloist's release")
}

pub async fn update(root: &Path, write: bool) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let tmp = tempfile::tempdir()?;
    let mut sources = serde_json::Map::new();
    let mut version = None;
    for (system, arch) in ARCHIVES {
        let url = format!("https://{HOST}/soloist_release_{arch}.tar.gz");
        let path = tmp.path().join(format!("{system}.tar.gz"));
        let mut response = client.get(&url).send().await?.error_for_status()?;
        let mut file = fs::File::create(&path)?;
        let mut total = 0;
        while let Some(chunk) = response.chunk().await? {
            total += chunk.len();
            ensure!(total <= LIMIT, "Soloist archive exceeds size limit");
            file.write_all(&chunk)?;
        }
        drop(file);
        let changelog = crate::capture(
            Command::new("tar")
                .arg("-xOzf")
                .arg(&path)
                .arg("CHANGELOG.md"),
            64 * 1024,
        )?;
        let release = changelog_version(std::str::from_utf8(&changelog)?)?;
        if let Some(previous) = &version {
            ensure!(
                previous == release,
                "Spotify's platform builds have different release versions"
            );
        }
        version = Some(release.to_owned());
        sources.insert(
            system.into(),
            json!({"url":url,"sha256":crate::release::digest(&path)?}),
        );
    }
    let result: Value = json!({"version":version,"checked":chrono::Utc::now().date_naive().to_string(),"sources":sources});
    let content = format!("{}\n", serde_json::to_string_pretty(&result)?);
    if write {
        let destination = root.join("packaging/soloist-sources.json");
        let mut file = tempfile::NamedTempFile::new_in(
            destination
                .parent()
                .context("Missing packaging directory")?,
        )?;
        file.write_all(content.as_bytes())?;
        file.as_file().sync_all()?;
        file.persist(destination)?;
    }
    print!("{content}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn release_is_taken_from_the_first_valid_heading() {
        assert_eq!(
            changelog_version("# Soloist\n## Release v1.2.3\n## Release v1.2.2").unwrap(),
            "1.2.3"
        );
        assert!(changelog_version("## Release v1.2.invalid").is_err());
    }
    #[test]
    fn pinned_sources_match_official_platform_downloads() {
        let sources: Value = serde_json::from_slice(
            &fs::read(crate::root().join("packaging/soloist-sources.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            sources["sources"].as_object().unwrap().len(),
            ARCHIVES.len()
        );
        for (system, arch) in ARCHIVES {
            assert_eq!(
                sources["sources"][system]["url"],
                format!("https://{HOST}/soloist_release_{arch}.tar.gz")
            );
            let hash = sources["sources"][system]["sha256"].as_str().unwrap();
            assert_eq!(hash.len(), 64);
            assert!(hash.bytes().all(|b| b.is_ascii_hexdigit()));
        }
    }
}

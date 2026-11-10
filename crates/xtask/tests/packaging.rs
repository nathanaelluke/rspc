use anyhow::{Result, ensure};
use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::Path,
    process::Command,
};
use xtask::release;

struct Install {
    temp: tempfile::TempDir,
}
impl Install {
    fn new() -> Result<Self> {
        let temp = tempfile::tempdir()?;
        fs::create_dir(temp.path().join("build with spaces"))?;
        for name in ["rspc", "rspc-ui"] {
            let file = temp.path().join("build with spaces").join(name);
            fs::write(&file, format!("#!/bin/sh\nprintf '{name} test\\n'\n"))?;
            fs::set_permissions(file, fs::Permissions::from_mode(0o755))?;
        }
        Ok(Self { temp })
    }
    fn prefix(&self) -> std::path::PathBuf {
        self.temp.path().join("installed prefix")
    }
    fn command(&self) -> Command {
        let mut command = Command::new("bash");
        command
            .arg(xtask::root().join("scripts/install.sh"))
            .arg("--prefix")
            .arg(self.prefix())
            .arg("--build-dir")
            .arg(self.temp.path().join("build with spaces"))
            .env_remove("DESTDIR");
        command
    }
    fn install(&self) -> Result<()> {
        xtask::run(&mut self.command())
    }
}

#[test]
fn install_copies_both_binaries_and_preserves_config() -> Result<()> {
    let install = Install::new()?;
    fs::create_dir(install.prefix())?;
    fs::write(install.prefix().join("config.ron"), "my configuration")?;
    install.install()?;
    for name in ["rspc", "rspc-ui"] {
        let file = install.prefix().join("bin").join(name);
        assert!(!file.is_symlink());
        assert_eq!(
            xtask::capture(&mut Command::new(file), 1024)?,
            format!("{name} test\n").as_bytes()
        );
    }
    assert_eq!(
        fs::read_to_string(install.prefix().join("config.ron"))?,
        "my configuration"
    );
    assert!(
        install
            .prefix()
            .join("share/rspc/config/themes/default.ron")
            .is_file()
    );
    assert!(
        install
            .prefix()
            .join("share/licenses/rspc/LICENSE-BSD-3-Clause")
            .is_file()
    );
    assert!(!install.prefix().join("bin/soloist").exists());
    Ok(())
}

#[test]
fn reinstall_replaces_symlink_without_touching_target() -> Result<()> {
    let install = Install::new()?;
    fs::create_dir_all(install.prefix().join("bin"))?;
    let old = install.temp.path().join("old build");
    fs::write(&old, "old build must remain intact")?;
    symlink(&old, install.prefix().join("bin/rspc"))?;
    install.install()?;
    install.install()?;
    assert_eq!(fs::read_to_string(old)?, "old build must remain intact");
    assert!(!install.prefix().join("bin/rspc").is_symlink());
    Ok(())
}

#[test]
fn missing_frontend_fails_before_any_installation() -> Result<()> {
    let install = Install::new()?;
    fs::remove_file(install.temp.path().join("build with spaces/rspc-ui"))?;
    assert!(!install.command().output()?.status.success());
    assert!(!install.prefix().exists());
    Ok(())
}

#[test]
fn staged_prefix_is_respected() -> Result<()> {
    let install = Install::new()?;
    let stage = install.temp.path().join("stage");
    xtask::run(
        install
            .command()
            .arg("--prefix")
            .arg("/usr")
            .env("DESTDIR", &stage),
    )?;
    assert!(stage.join("usr/bin/rspc-ui").is_file());
    assert!(!install.prefix().exists());
    Ok(())
}

#[test]
fn checkout_private_files_are_not_packaged() -> Result<()> {
    let install = Install::new()?;
    let root = xtask::root();
    let checkout = install.temp.path().join("checkout");
    fs::create_dir_all(checkout.join("scripts"))?;
    fs::create_dir_all(checkout.join("config/themes"))?;
    fs::create_dir_all(checkout.join("assets"))?;
    for name in [
        "README.md",
        "assets/screenshot.png",
        "NOTICE",
        "LICENSE-MIT",
        "LICENSE-BSD-3-Clause",
        "LICENSE-librespot",
        "scripts/install.sh",
        "config/config.ron",
        "config/themes/default.ron",
    ] {
        fs::copy(root.join(name), checkout.join(name))?;
    }
    fs::write(
        checkout.join("config/themes/private.local.ron"),
        "private fixture",
    )?;
    fs::write(checkout.join("LICENSE-private.key"), "private fixture")?;
    xtask::run(
        Command::new("bash")
            .arg(checkout.join("scripts/install.sh"))
            .arg("--prefix")
            .arg(install.prefix())
            .arg("--build-dir")
            .arg(install.temp.path().join("build with spaces")),
    )?;
    assert!(
        !install
            .prefix()
            .join("share/rspc/config/themes/private.local.ron")
            .exists()
    );
    assert!(
        !install
            .prefix()
            .join("share/licenses/rspc/LICENSE-private.key")
            .exists()
    );
    Ok(())
}

#[test]
fn release_archive_contains_only_player_executables() -> Result<()> {
    let install = Install::new()?;
    let output = install.temp.path().join("release");
    release::archive(
        &xtask::root(),
        release::TARGETS[0],
        &install.temp.path().join("build with spaces"),
        &output,
    )?;
    let file = fs::read_dir(&output)?.next().unwrap()?.path();
    let listing = xtask::capture(Command::new("tar").arg("-tzf").arg(&file), 64 * 1024)?;
    let listing = String::from_utf8(listing)?;
    assert!(listing.contains("/bin/rspc\n"));
    assert!(listing.contains("/bin/rspc-ui\n"));
    assert!(!listing.lines().any(|line| line.ends_with("/soloist")));
    assert!(listing.contains("/share/doc/rspc/NOTICE"));
    Ok(())
}

fn artifacts(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    for target in release::TARGETS.into_iter().chain(["source"]) {
        fs::write(
            path.join(format!("rspc-{}-{target}.tar.gz", release::VERSION)),
            target,
        )?;
    }
    Ok(())
}

#[test]
fn recipes_have_real_checksums_and_existing_player_dependencies() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let assets = tmp.path().join("artifacts");
    artifacts(&assets)?;
    let output = tmp.path().join("recipes");
    release::recipes(&xtask::root(), &assets, &output)?;
    let cask = fs::read_to_string(output.join("homebrew/Casks/rspc.rb"))?;
    assert!(cask.contains("depends_on cask: \"spotify-soloist\""));
    assert!(cask.contains("depends_on formula: \"openssl@3\""));
    assert!(!cask.contains("no_check"));
    for target in release::TARGETS {
        assert!(cask.contains(&release::digest(
            &assets.join(format!("rspc-{}-{target}.tar.gz", release::VERSION))
        )?));
    }
    let aur = output.join("aur/rspc/PKGBUILD");
    xtask::run(Command::new("bash").arg("-n").arg(&aur))?;
    let fields = xtask::capture(
        Command::new("bash")
            .arg("-c")
            .arg(
                r#"source "$1"
printf '%s\n' "$pkgname" "$pkgver" "${depends[@]}" "${sha256sums[@]}"
"#,
            )
            .arg("packaging-test")
            .arg(&aur),
        64 * 1024,
    )?;
    let fields = String::from_utf8(fields)?;
    assert!(fields.lines().any(|line| line == "spotify-soloist-bin"));
    assert!(fields.lines().any(|line| line == release::VERSION));
    assert!(fields.contains(&release::digest(
        &assets.join(format!("rspc-{}-source.tar.gz", release::VERSION))
    )?));
    assert!(!fields.contains("SKIP"));
    assert!(!fs::read_to_string(aur)?.contains("pkgver()"));
    Ok(())
}

#[test]
fn recipes_refuse_missing_platform_instead_of_inventing_a_hash() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let assets = tmp.path().join("artifacts");
    artifacts(&assets)?;
    fs::remove_file(assets.join(format!(
        "rspc-{}-aarch64-apple-darwin.tar.gz",
        release::VERSION
    )))?;
    let output = tmp.path().join("recipes");
    assert!(release::recipes(&xtask::root(), &assets, &output).is_err());
    assert!(!output.exists());
    Ok(())
}

#[test]
fn checksum_manifest_covers_all_archives() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    artifacts(tmp.path())?;
    fs::write(tmp.path().join("package-recipes.tar.gz"), "recipes")?;
    release::checksums(tmp.path())?;
    let manifest = fs::read_to_string(tmp.path().join("SHA256SUMS"))?;
    assert_eq!(manifest.lines().count(), 5);
    for line in manifest.lines() {
        let (hash, name) = line.split_once("  ").unwrap();
        ensure!(
            hash == release::digest(&tmp.path().join(name))?,
            "Checksum mismatch"
        );
    }
    Ok(())
}

#[test]
fn development_recipe_matches_srcinfo_and_service_path() -> Result<()> {
    let root = xtask::root();
    let directory = root.join("packaging/aur/rspc-git");
    xtask::run(
        Command::new("bash")
            .arg("-n")
            .arg(directory.join("PKGBUILD")),
    )?;
    assert!(
        fs::read_to_string(directory.join(".SRCINFO"))?.contains("depends = spotify-soloist-bin")
    );
    assert!(fs::read_to_string(directory.join("PKGBUILD"))?.contains("'spotify-soloist-bin'"));
    assert!(root.join("packaging/rspc.service").is_file());
    Ok(())
}

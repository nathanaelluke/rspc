use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use std::{fs, io::Read, path::Path, process::Command};

pub const TARGETS: [&str; 3] = [
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "aarch64-apple-darwin",
];
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
const REPOSITORY: &str = "https://github.com/nathanaelluke/rspc";

pub fn digest(path: &Path) -> Result<String> {
    let mut file =
        fs::File::open(path).with_context(|| format!("Cannot read {}", path.display()))?;
    let mut digest = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

pub fn archive(root: &Path, target: &str, build: &Path, output: &Path) -> Result<()> {
    ensure!(TARGETS.contains(&target), "Unsupported release target");
    crate::licenses::check(root)?;
    let stage = tempfile::tempdir()?;
    let prefix = format!("rspc-{VERSION}");
    crate::run(
        Command::new("bash")
            .arg(root.join("scripts/install.sh"))
            .arg("--prefix")
            .arg(stage.path().join(&prefix))
            .arg("--build-dir")
            .arg(build.canonicalize()?)
            .env_remove("DESTDIR"),
    )?;
    fs::create_dir_all(output)?;
    crate::run(
        Command::new("tar")
            .arg("-czf")
            .arg(output.join(format!("{prefix}-{target}.tar.gz")))
            .arg("-C")
            .arg(stage.path())
            .arg(prefix),
    )
}

pub fn source_archive(root: &Path, output: &Path) -> Result<()> {
    crate::licenses::check(root)?;
    ensure!(
        crate::capture(
            Command::new("git").current_dir(root).args([
                "status",
                "--porcelain",
                "--untracked-files=normal"
            ]),
            1024 * 1024
        )?
        .is_empty(),
        "Commit release sources before creating the source archive"
    );
    let stage = tempfile::tempdir()?;
    let prefix = format!("rspc-{VERSION}");
    let archive = stage.path().join("source.tar");
    crate::run(
        Command::new("git")
            .current_dir(root)
            .args([
                "archive",
                "--format=tar",
                &format!("--prefix={prefix}/"),
                "--output",
            ])
            .arg(&archive)
            .arg("HEAD"),
    )?;
    crate::run(
        Command::new("tar")
            .arg("-xf")
            .arg(&archive)
            .arg("-C")
            .arg(stage.path()),
    )?;
    let source = stage.path().join(&prefix);
    crate::run(Command::new("cargo").current_dir(&source).args([
        "vendor",
        "--locked",
        "--versioned-dirs",
        "vendor",
    ]))?;
    fs::create_dir_all(source.join(".cargo"))?;
    fs::write(
        source.join(".cargo/config.toml"),
        "[build]\njobs = 1\n[source.crates-io]\nreplace-with = \"vendored-sources\"\n[source.vendored-sources]\ndirectory = \"vendor\"\n",
    )?;
    crate::licenses::check(&source)?;
    fs::create_dir_all(output)?;
    crate::run(
        Command::new("tar")
            .arg("-czf")
            .arg(output.join(format!("{prefix}-source.tar.gz")))
            .arg("-C")
            .arg(stage.path())
            .arg(prefix),
    )
}

pub fn checksums(artifacts: &Path) -> Result<()> {
    let mut archives = Vec::new();
    for entry in fs::read_dir(artifacts)? {
        let path = entry?.path();
        if path
            .file_name()
            .is_some_and(|name| name.to_string_lossy().ends_with(".tar.gz"))
        {
            archives.push(path);
        }
    }
    ensure!(!archives.is_empty(), "No release archives found");
    archives.sort();
    let mut manifest = String::new();
    for path in archives {
        manifest.push_str(&format!(
            "{}  {}\n",
            digest(&path)?,
            path.file_name().unwrap().to_string_lossy()
        ));
    }
    fs::write(artifacts.join("SHA256SUMS"), manifest)?;
    Ok(())
}

pub fn recipes(root: &Path, artifacts: &Path, output: &Path) -> Result<()> {
    let base = format!("{REPOSITORY}/releases/download/v{VERSION}");
    let hashes = TARGETS
        .map(|target| digest(&artifacts.join(format!("rspc-{VERSION}-{target}.tar.gz"))))
        .into_iter()
        .collect::<Result<Vec<_>>>()?;
    let source_name = format!("rspc-{VERSION}-source.tar.gz");
    let source_hash = digest(&artifacts.join(&source_name))?;
    let cask = format!(
        r##"cask "rspc" do
  version "{VERSION}"

  on_macos do
    sha256 "{mac}"

    url "{base}/rspc-#{{version}}-aarch64-apple-darwin.tar.gz"

    depends_on arch: :arm64
    depends_on macos: :ventura
  end
  on_linux do
    on_arm do
      sha256 "{arm}"
      url "{base}/rspc-#{{version}}-aarch64-unknown-linux-gnu.tar.gz"
    end
    on_intel do
      sha256 "{intel}"
      url "{base}/rspc-#{{version}}-x86_64-unknown-linux-gnu.tar.gz"
    end
  end

  name "rspc"
  desc "Terminal music player for Spotify, local files and streams"
  homepage "{REPOSITORY}"

  depends_on cask: "spotify-soloist"
  depends_on formula: "openssl@3"

  binary "rspc-#{{version}}/bin/rspc"
  binary "rspc-#{{version}}/bin/rspc-ui"

  caveats <<~EOS
    Spotify playback needs your own Soloist API key and `rspc login`.
    See {REPOSITORY}/blob/main/README.md#spotify-setup
    Refresh expiring builds with: brew upgrade --cask --greedy spotify-soloist
    Stop rspc before replacing its running player.
  EOS
end
"##,
        intel = hashes[0],
        arm = hashes[1],
        mac = hashes[2]
    );
    // Keep stable and development dependencies in one recipe.
    let template = fs::read_to_string(root.join("packaging/aur/rspc-git/PKGBUILD"))?;
    let mut aur = String::new();
    let mut skip_version_function = false;
    for line in template.lines() {
        if line.starts_with("pkgver()") {
            skip_version_function = true;
            continue;
        }
        if skip_version_function {
            if line == "}" {
                skip_version_function = false;
            }
            continue;
        }
        if line.starts_with("provides=") || line.starts_with("conflicts=") {
            continue;
        }
        let line = if line.starts_with("pkgname=") {
            "pkgname=rspc".into()
        } else if line.starts_with("pkgver=") {
            format!("pkgver={VERSION}")
        } else if line.starts_with("source=") {
            format!("source=(\"rspc-${{pkgver}}.tar.gz::{base}/{source_name}\")")
        } else if line.starts_with("sha256sums=") {
            format!("sha256sums=('{source_hash}')")
        } else {
            line.replace("'pkgconf' 'git'", "'pkgconf'")
                .replace("$srcdir/rspc\"", "$srcdir/rspc-$pkgver\"")
        };
        aur.push_str(&line);
        aur.push('\n');
    }
    for (path, content) in [
        (output.join("homebrew/Casks/rspc.rb"), cask),
        (output.join("aur/rspc/PKGBUILD"), aur),
    ] {
        fs::create_dir_all(path.parent().context("Recipe has no parent directory")?)?;
        fs::write(path, content)?;
    }
    Ok(())
}

use anyhow::{Result, ensure};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(about = "rspc development tools")]
struct Args {
    #[command(subcommand)]
    command: Action,
}

#[derive(Subcommand)]
enum Action {
    /// Print the shared workspace version.
    Version,
    /// Refresh the third-party license bundle using cargo-about.
    Licenses,
    /// Check source, index and optionally history for private data.
    CheckRelease {
        #[arg(long)]
        history: bool,
    },
    /// Package an existing native build.
    Archive {
        #[arg(long)]
        target: String,
        #[arg(long, default_value = "target/release")]
        build_dir: PathBuf,
        #[arg(long)]
        output: PathBuf,
    },
    /// Package committed source and Cargo dependencies for offline rebuilding.
    SourceArchive {
        #[arg(long)]
        output: PathBuf,
    },
    /// Generate package recipes from release checksums.
    Recipes {
        #[arg(long)]
        artifacts: PathBuf,
        #[arg(long)]
        output: PathBuf,
    },
    /// Write SHA256SUMS for release archives.
    Checksums {
        #[arg(long)]
        artifacts: PathBuf,
    },
    /// Refresh Nix's checksums from Spotify's official archives.
    UpdateSoloist {
        #[arg(long)]
        write: bool,
    },
    /// Temporarily allow manual phone pairing on NixOS.
    PairOnLan {
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        interface: Option<String>,
        #[arg(long, default_value_t = 300, value_parser = clap::value_parser!(u64).range(1..=600))]
        timeout: u64,
    },
    /// Measure protocol response latency without printing library contents.
    Benchmark {
        #[arg(long)]
        socket: Option<PathBuf>,
        #[arg(long)]
        artist: Option<String>,
        #[arg(long, default_value_t = 20, value_parser = clap::value_parser!(u64).range(1..=10000))]
        samples: u64,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let root = xtask::root();
    match Args::parse().command {
        Action::Version => println!("{}", env!("CARGO_PKG_VERSION")),
        Action::Licenses => xtask::licenses::generate(&root).await?,
        Action::CheckRelease { history } => {
            xtask::licenses::check(&root)?;
            let findings = xtask::privacy::check(&root, history)?;
            for finding in &findings {
                println!(
                    "{}:{}: {}",
                    finding.path.display(),
                    finding.line,
                    finding.reason
                );
            }
            ensure!(
                findings.is_empty(),
                "Review these files before publishing; matched values were withheld"
            );
            println!("No findings. Review binary assets and diagnostic output separately.");
        }
        Action::Archive {
            target,
            build_dir,
            output,
        } => xtask::release::archive(&root, &target, &build_dir, &output)?,
        Action::SourceArchive { output } => xtask::release::source_archive(&root, &output)?,
        Action::Recipes { artifacts, output } => {
            xtask::release::recipes(&root, &artifacts, &output)?
        }
        Action::Checksums { artifacts } => xtask::release::checksums(&artifacts)?,
        Action::UpdateSoloist { write } => xtask::soloist::update(&root, write).await?,
        Action::PairOnLan {
            dry_run,
            interface,
            timeout,
        } => xtask::pairing::pair(&root, dry_run, interface.as_deref(), timeout).await?,
        Action::Benchmark {
            socket,
            artist,
            samples,
        } => xtask::benchmark::measure(socket, artist, samples as usize)?,
    }
    Ok(())
}

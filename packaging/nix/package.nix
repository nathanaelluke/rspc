{ lib, stdenv, rustPlatform, pkg-config, openssl, makeWrapper, soloist, mpd, git }:
rustPlatform.buildRustPackage {
  pname = "rspc";
  version = (builtins.fromTOML (builtins.readFile ../../Cargo.toml)).workspace.package.version;
  src = lib.fileset.toSource {
    root = ../..;
    fileset = lib.fileset.unions [
      ../../Cargo.toml ../../Cargo.lock ../../build.rs ../../src ../../protocol
      (lib.fileset.difference ../../crates (lib.fileset.maybeMissing ../../crates/rspc-ui/target))
      ../../config ../../assets ../../scripts/install.sh ../../README.md ../../NOTICE ../../LICENSE-MIT
      ../../LICENSE-BSD-3-Clause ../../LICENSE-librespot
      ../../packaging/aur ../../packaging/soloist-sources.json ../../packaging/rspc.service
    ];
  };
  cargoLock.lockFile = ../../Cargo.lock;
  nativeBuildInputs = [pkg-config makeWrapper];
  buildInputs = [openssl];
  nativeCheckInputs = [mpd git];
  cargoBuildFlags = ["-p" "rspc" "-p" "rspc-ui"];
  cargoTestFlags = ["--workspace"];
  enableParallelBuilding = false;
  env.CARGO_BUILD_JOBS = "1";
  # Nix's Rust hook passes -j explicitly, overriding CARGO_BUILD_JOBS.
  preBuild = ''export NIX_BUILD_CORES=1'';
  preCheck = ''
    export RUST_TEST_THREADS=1
    export RSPC_TEST_BINARY="$PWD/target/${stdenv.hostPlatform.rust.rustcTarget}/release/rspc"
    export RSPC_UI_TEST_BINARY="$PWD/target/${stdenv.hostPlatform.rust.rustcTarget}/release/rspc-ui"
  '';
  installPhase = ''
    runHook preInstall
    ./scripts/install.sh --prefix "$out" --build-dir "target/${stdenv.hostPlatform.rust.rustcTarget}/release"
    runHook postInstall
  '';
  postFixup = ''
    wrapProgram "$out/bin/rspc" --set-default RSPC_SOLOIST_PATH "${soloist}/bin/soloist"
  '';
  meta = {
    description = "Terminal music player for Spotify, local files and streams";
    homepage = "https://github.com/nathanaelluke/rspc";
    license = with lib.licenses; [mit bsd3];
    mainProgram = "rspc";
    platforms = soloist.meta.platforms;
    # The runtime closure includes Spotify's nonredistributable player.
    hydraPlatforms = [];
  };
}

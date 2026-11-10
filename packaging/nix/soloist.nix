{ lib, stdenv, fetchurl, autoPatchelfHook, makeWrapper, pipewire, libpulseaudio }:
let
  releases = builtins.fromJSON (builtins.readFile ../soloist-sources.json);
  source = releases.sources.${stdenv.hostPlatform.system};
in stdenv.mkDerivation {
  pname = "soloist";
  inherit (releases) version;
  # Fetch directly from Spotify. Never mirror the player or upload its closure.
  src = fetchurl { inherit (source) url sha256; };
  sourceRoot = ".";
  strictDeps = true;
  dontBuild = true;
  dontStrip = true;
  nativeBuildInputs = lib.optionals stdenv.hostPlatform.isLinux [autoPatchelfHook makeWrapper];
  buildInputs = lib.optionals stdenv.hostPlatform.isLinux [stdenv.cc.cc.lib];
  installPhase = ''
    runHook preInstall
    install -Dm755 soloist "$out/bin/soloist"
    install -Dm644 THIRD_PARTY_LICENSES.txt "$out/share/licenses/soloist/THIRD_PARTY_LICENSES.txt"
    install -Dm644 CHANGELOG.md "$out/share/doc/soloist/CHANGELOG.md"
    runHook postInstall
  '';
  postFixup = lib.optionalString stdenv.hostPlatform.isLinux ''
    wrapProgram "$out/bin/soloist" --prefix LD_LIBRARY_PATH : \
      "${lib.makeLibraryPath [pipewire libpulseaudio]}"
  '';
  # Its distribution terms prohibit binary redistribution, including Nix caches.
  allowSubstitutes = false;
  preferLocalBuild = true;
  meta = {
    description = "Spotify's headless Connect player";
    homepage = "https://developer.spotify.com/documentation/soloist";
    license = lib.licenses.unfree;
    sourceProvenance = [lib.sourceTypes.binaryNativeCode];
    platforms = builtins.attrNames releases.sources;
    mainProgram = "soloist";
    hydraPlatforms = [];
  };
}

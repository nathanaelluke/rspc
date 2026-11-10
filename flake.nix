{
  description = "rspc terminal music player";
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  outputs = {self, nixpkgs, ...}: let
    systems = ["x86_64-linux" "aarch64-linux" "aarch64-darwin"];
    forAllSystems = nixpkgs.lib.genAttrs systems;
    packagesFor = system: import nixpkgs {
      inherit system;
      # Only Spotify's player is nonfree. Other packages retain normal checks.
      config.allowUnfreePredicate = pkg: nixpkgs.lib.getName pkg == "soloist";
    };
  in {
    packages = forAllSystems (system: let
      pkgs = packagesFor system;
      soloist = pkgs.callPackage ./packaging/nix/soloist.nix {};
      rspc = pkgs.callPackage ./packaging/nix/package.nix {inherit soloist;};
      mpd = pkgs.mpd;
    in {inherit rspc soloist mpd; default = rspc;});
    apps = forAllSystems (system: {
      default = {
        type = "app";
        program = "${self.packages.${system}.rspc}/bin/rspc";
      };
    });
    devShells = forAllSystems (system: let pkgs = packagesFor system; in {
      default = pkgs.mkShell {
        packages = with pkgs; [cargo rustc clippy rustfmt pkg-config mpd git];
        buildInputs = [pkgs.openssl];
        CARGO_BUILD_JOBS = "1";
      };
    });
    formatter = forAllSystems (system: (packagesFor system).alejandra);
  };
}

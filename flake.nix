{
  description = "rspc terminal music player";
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  outputs = {nixpkgs, ...}: let
    system = "x86_64-linux";
    pkgs = import nixpkgs {inherit system;};
  in {
    devShells.${system}.default = pkgs.mkShell {
      packages = with pkgs; [cargo rustc clippy rustfmt pkg-config python3];
      buildInputs = [pkgs.openssl];
      CARGO_BUILD_JOBS = "2";
    };
    formatter.${system} = pkgs.alejandra;
  };
}

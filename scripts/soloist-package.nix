let
  rspcRoot = builtins.getEnv "RSPC_SOURCE_ROOT";
  archive = builtins.getEnv "RSPC_SOLOIST_ARCHIVE";
  flake = builtins.getFlake rspcRoot;
  pkgs = import flake.inputs.nixpkgs { system = "x86_64-linux"; config.allowUnfree = true; };
in pkgs.soloist.overrideAttrs (_: {
  version = "local-current";
  src = builtins.path { path = builtins.toPath archive; name = "soloist-official.tar.gz"; };
  doInstallCheck = false;
})

#!/usr/bin/env bash
set -euo pipefail
rspc_source_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
rspc_state="${XDG_STATE_HOME:-$HOME/.local/state}/rspc"
mkdir -p "$rspc_state/soloist-install"
chmod 700 "$rspc_state/soloist-install"
rspc_archive="$rspc_state/soloist-install/soloist.tar.gz"
curl --fail --location --silent --show-error https://soloist-builds.spotifycdn.com/soloist_release_x86_64.tar.gz --output "$rspc_archive"
RSPC_SOURCE_ROOT="$rspc_source_root" RSPC_SOLOIST_ARCHIVE="$rspc_archive" nix build --impure --file "$rspc_source_root/scripts/soloist-package.nix" --out-link "$rspc_state/soloist-package"
"$rspc_state/soloist-package/bin/soloist" --version

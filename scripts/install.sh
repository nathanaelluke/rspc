#!/usr/bin/env bash
set -euo pipefail
rspc_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
rspc_bin_dir="${1:-$HOME/.local/bin}"
mkdir -p "$rspc_bin_dir"
for rspc_binary in rspc rspc-ui; do
  rspc_source="$rspc_root/target/release/$rspc_binary"
  rspc_dest="$rspc_bin_dir/$rspc_binary"
  [[ -x "$rspc_source" ]] || { echo "Build $rspc_binary with --release first." >&2; exit 1; }
  if [[ -L "$rspc_dest" ]]; then
    rspc_old="$(readlink -- "$rspc_dest")"
    case "$rspc_old" in
      "$rspc_root/target/debug/$rspc_binary"|"$rspc_source") ln -sfn -- "$rspc_source" "$rspc_dest"; continue ;;
    esac
  fi
  # Refuse to overwrite unrelated existing commands.
  ln -s -- "$rspc_source" "$rspc_dest"
done
printf 'Installed optimized rspc launch links in %s\n' "$rspc_bin_dir"

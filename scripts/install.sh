#!/usr/bin/env bash
# Install a completed build without tying the installation to this checkout.
set -euo pipefail
rspc_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
rspc_prefix="${PREFIX:-$HOME/.local}"
rspc_build_dir="$rspc_root/target/release"
while (($#)); do
  case "$1" in
    --prefix) rspc_prefix="${2:?--prefix needs a directory}"; shift 2 ;;
    --build-dir) rspc_build_dir="${2:?--build-dir needs a directory}"; shift 2 ;;
    --help)
      printf 'Usage: %s [--prefix DIRECTORY] [--build-dir DIRECTORY]\nSupports PREFIX and DESTDIR. Installs rspc and rspc-ui; Soloist is a separate dependency.\n' "$0"
      exit 0 ;;
    *) printf 'Unknown argument: %s\n' "$1" >&2; exit 1 ;;
  esac
done
case "$rspc_prefix" in
  /*) ;;
  *) printf 'The installation prefix must be absolute.\n' >&2; exit 1 ;;
esac
for rspc_name in rspc rspc-ui; do
  [[ -x "$rspc_build_dir/$rspc_name" ]] || {
    printf 'Missing %s. Build both executables with cargo build --release --locked -j1 -p rspc -p rspc-ui.\n' "$rspc_build_dir/$rspc_name" >&2
    exit 1
  }
done
[[ -s "$rspc_root/packaging/LICENSE-THIRD-PARTY" ]] || {
  printf 'Missing third-party license bundle. Run cargo run -p xtask -- licenses.\n' >&2
  exit 1
}
rspc_dest="${DESTDIR:-}$rspc_prefix"
install -d "$rspc_dest/bin" "$rspc_dest/share/doc/rspc" \
  "$rspc_dest/share/licenses/rspc" "$rspc_dest/share/rspc"
rspc_temp=""
trap '[[ -z "$rspc_temp" ]] || rm -f -- "$rspc_temp"' EXIT
for rspc_name in rspc rspc-ui; do
  # Rename over existing development symlinks instead of writing through them.
  rspc_temp="$(mktemp "$rspc_dest/bin/.rspc-install.XXXXXX")"
  install -m755 "$rspc_build_dir/$rspc_name" "$rspc_temp"
  mv -f -- "$rspc_temp" "$rspc_dest/bin/$rspc_name"
  rspc_temp=""
done
for rspc_doc in README.md NOTICE; do
  install -m644 "$rspc_root/$rspc_doc" "$rspc_dest/share/doc/rspc/$rspc_doc"
done
install -d "$rspc_dest/share/doc/rspc/assets"
install -m644 "$rspc_root/assets/screenshot.png" "$rspc_dest/share/doc/rspc/assets/screenshot.png"
for rspc_license in LICENSE-MIT LICENSE-BSD-3-Clause LICENSE-librespot; do
  install -m644 "$rspc_root/$rspc_license" "$rspc_dest/share/licenses/rspc/$rspc_license"
done
install -m644 "$rspc_root/packaging/LICENSE-THIRD-PARTY" "$rspc_dest/share/licenses/rspc/LICENSE-THIRD-PARTY"
install -d "$rspc_dest/share/rspc/config/themes"
install -m644 "$rspc_root/config/config.ron" "$rspc_dest/share/rspc/config/config.ron"
install -m644 "$rspc_root/config/themes/default.ron" "$rspc_dest/share/rspc/config/themes/default.ron"
printf 'Installed rspc and rspc-ui in %s/bin\n' "$rspc_dest"

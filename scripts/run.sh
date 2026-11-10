#!/usr/bin/env bash
set -euo pipefail
rspc_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$rspc_root"
cargo build --release --locked -j 1 -p rspc -p rspc-ui
exec "$rspc_root/target/release/rspc" "$@"

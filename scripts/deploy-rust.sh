#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export PATH="$HOME/.cargo/bin:$HOME/.bun/bin:$PATH"

HAPI_HUB_RS_LISTEN="${HAPI_HUB_RS_LISTEN:-127.0.0.1:18787}"
HAPI_HUB_RS_DB="${HAPI_HUB_RS_DB:-$ROOT_DIR/.hapi-rust.sqlite}"

echo "[1/4] Building hapi-wasm (wasm32-unknown-unknown release)..."
cargo build --manifest-path "$ROOT_DIR/Cargo.toml" -p hapi-wasm --target wasm32-unknown-unknown --release
mkdir -p "$ROOT_DIR/web/public/wasm"
cp "$ROOT_DIR/target/wasm32-unknown-unknown/release/hapi_wasm.wasm" "$ROOT_DIR/web/public/wasm/hapi_wasm.wasm"

echo "[2/4] Building native hub, runner and session wrapper release binaries..."
cargo build --manifest-path "$ROOT_DIR/Cargo.toml" -p hapi-hub-rs -p hapi-runner-rs -p hapi-session-wrapper --release
wrapper_dest="${HAPI_RUST_WRAPPER_BIN:-}"
if [[ -z "$wrapper_dest" ]]; then
  hapi_path="$(command -v hapi)"
  hapi_path="$(readlink -f "$hapi_path")"
  wrapper_dest="$(dirname "$hapi_path")/hapi-session-wrapper"
fi
wrapper_tmp="$(mktemp -- "$wrapper_dest.tmp.XXXXXXXX")"
trap 'rm -f -- "$wrapper_tmp"' EXIT
install -m 0755 -- "$ROOT_DIR/target/release/hapi-session-wrapper" "$wrapper_tmp"
mv -fT -- "$wrapper_tmp" "$wrapper_dest"
trap - EXIT

echo "[3/4] Building web frontend shell..."
(cd "$ROOT_DIR/web" && bun run build)

echo "[4/4] Starting hapi-hub-rs on $HAPI_HUB_RS_LISTEN (db=$HAPI_HUB_RS_DB)..."
HAPI_HUB_RS_LISTEN="$HAPI_HUB_RS_LISTEN" HAPI_HUB_RS_DB="$HAPI_HUB_RS_DB" \
  exec "$ROOT_DIR/target/release/hapi-hub-rs"

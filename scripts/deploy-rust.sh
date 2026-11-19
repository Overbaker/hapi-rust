#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export PATH="$HOME/.cargo/bin:$HOME/.bun/bin:$PATH"

HAPI_HUB_RS_LISTEN="${HAPI_HUB_RS_LISTEN:-127.0.0.1:18787}"
HAPI_HUB_RS_DB="${HAPI_HUB_RS_DB:-$ROOT_DIR/.hapi-rust.sqlite}"

wrapper_dest="${HAPI_RUST_WRAPPER_BIN:-}"
if (( $# )); then
  if [[ $# != 2 || $1 != --prefix || -z $2 || -n $wrapper_dest ]]; then
    echo "Usage: deploy-rust.sh [--prefix DIR], or set HAPI_RUST_WRAPPER_BIN" >&2
    exit 2
  fi
  wrapper_dest="$(realpath -m -- "$2")/bin/hapi-session-wrapper"
fi
if [[ -n $wrapper_dest && ( $wrapper_dest != /* || ${wrapper_dest##*/} != hapi-session-wrapper ) ]]; then
  echo "HAPI_RUST_WRAPPER_BIN must be an absolute path ending in /hapi-session-wrapper" >&2
  exit 2
fi

package_managed() {
  case "$1" in
    */node_modules/*|*/Cellar/*|*/.linuxbrew/*|/nix/store/*|/gnu/store/*|/snap/*|/usr/bin/*|/usr/lib/*) return 0 ;;
  esac
  { command -v dpkg-query >/dev/null && dpkg-query -S "$1" >/dev/null 2>&1; } ||
    { command -v rpm >/dev/null && rpm -qf "$1" >/dev/null 2>&1; } ||
    { command -v pacman >/dev/null && pacman -Qo "$1" >/dev/null 2>&1; } ||
    { command -v apk >/dev/null && apk info --who-owns "$1" >/dev/null 2>&1; }
}

install_wrapper() {
  local launcher hapi_path dest_dir source
  if [[ -z $wrapper_dest ]]; then
    if ! launcher="$(command -v hapi)"; then
      echo "Skipping session wrapper install: hapi is absent from PATH." >&2
      return
    fi
    hapi_path="$(readlink -f -- "$launcher")"
    if [[ $hapi_path == *.cjs ]]; then
      if ! hapi_path="$(node -e 'const bin = require(process.argv[1]).getBinaryPath(); if (!bin) process.exit(1); console.log(bin)' "$hapi_path")"; then
        echo "Skipping session wrapper install: cannot resolve the packaged hapi binary." >&2
        return
      fi
      echo "Skipping package-managed hapi at $hapi_path. Set HAPI_RUST_WRAPPER_BIN or use --prefix DIR to install." >&2
      return
    fi
    wrapper_dest="$(dirname "$hapi_path")/hapi-session-wrapper"
    if package_managed "$hapi_path" || package_managed "$wrapper_dest"; then
      echo "Skipping package-managed hapi at $hapi_path. Set HAPI_RUST_WRAPPER_BIN or use --prefix DIR to install." >&2
      return
    fi
  fi
  dest_dir="$(dirname "$wrapper_dest")"
  mkdir -p -- "$dest_dir"
  if [[ ! -x $dest_dir/hapi ]] || [[ $(head -c 2 "$dest_dir/hapi") == '#!' ]]; then
    echo "No compiled hapi next to $wrapper_dest; configure HAPI_RUST_WRAPPER_BIN on the runner." >&2
  fi
  echo "Session wrapper destination: $wrapper_dest"
  source="$ROOT_DIR/target/release/hapi-session-wrapper"
  if [[ -x $wrapper_dest ]] && cmp -s -- "$source" "$wrapper_dest"; then return; fi
  wrapper_tmp="$(mktemp -- "$wrapper_dest.tmp.XXXXXXXX")"
  trap 'rm -f -- "$wrapper_tmp"' EXIT
  install -m 0755 -- "$source" "$wrapper_tmp"
  mv -fT -- "$wrapper_tmp" "$wrapper_dest"
  trap - EXIT
}

echo "[1/4] Building hapi-wasm (wasm32-unknown-unknown release)..."
cargo build --manifest-path "$ROOT_DIR/Cargo.toml" -p hapi-wasm --target wasm32-unknown-unknown --release
mkdir -p "$ROOT_DIR/web/public/wasm"
cp "$ROOT_DIR/target/wasm32-unknown-unknown/release/hapi_wasm.wasm" "$ROOT_DIR/web/public/wasm/hapi_wasm.wasm"

echo "[2/4] Building native hub, runner and session wrapper release binaries..."
cargo build --manifest-path "$ROOT_DIR/Cargo.toml" -p hapi-hub-rs -p hapi-runner-rs -p hapi-session-wrapper --release
echo "[3/4] Building web frontend shell..."
(cd "$ROOT_DIR/web" && bun run build)

install_wrapper

echo "[4/4] Starting hapi-hub-rs on $HAPI_HUB_RS_LISTEN (db=$HAPI_HUB_RS_DB)..."
HAPI_HUB_RS_LISTEN="$HAPI_HUB_RS_LISTEN" HAPI_HUB_RS_DB="$HAPI_HUB_RS_DB" \
  exec "$ROOT_DIR/target/release/hapi-hub-rs"

#!/usr/bin/env bash
set -euo pipefail
repo="$(cd "$(dirname "$0")/.." && pwd)"
fixture="$(mktemp -d)"
trap 'rm -rf -- "$fixture"' EXIT
mkdir -p "$fixture"/{scripts,web,bin,home,target/release,target/wasm32-unknown-unknown/release}
cp "$repo/scripts/deploy-rust.sh" "$fixture/scripts/"
cd "$fixture"
export HOME="$fixture/home"
unset HAPI_RUST_WRAPPER_BIN
for cmd in bash dirname mkdir cp mktemp chmod mv rm cmp head readlink realpath install node; do
  ln -s "$(command -v "$cmd")" "bin/$cmd"
done
export PATH="$fixture/bin"
echo wasm > target/wasm32-unknown-unknown/release/hapi_wasm.wasm
echo wrapper > target/release/hapi-session-wrapper
for cmd in bin/cargo bin/bun target/release/hapi-hub-rs; do
  echo -e '#!/bin/sh\nexit 0' > "$cmd"
  chmod +x "$cmd"
done
deploy() { bash scripts/deploy-rust.sh "$@" > log 2>&1; }
deploy
[[ $(<log) == *'absent from PATH'* ]]
mkdir -p node_modules/test
echo 'exports.getBinaryPath = () => "/pkg/platform/bin/hapi"' > node_modules/test/hapi.cjs
chmod +x node_modules/test/hapi.cjs
/bin/ln -s "$fixture/node_modules/test/hapi.cjs" bin/hapi
deploy
[[ $(<log) == *'Skipping package-managed hapi at /pkg/platform/bin/hapi'* ]]
[[ ! -e node_modules/test/hapi-session-wrapper ]]
if HAPI_RUST_WRAPPER_BIN="$fixture/other-name" deploy; then exit 1; fi
dest="$fixture/prefix/bin/hapi-session-wrapper"
deploy --prefix "$fixture/prefix"
[[ -x $dest && $(<"$dest") == wrapper ]]
/bin/ln "$dest" prior
deploy --prefix "$fixture/prefix"
[[ $dest -ef prior ]]
echo updated > target/release/hapi-session-wrapper
echo -e '#!/bin/sh\nexit 1' > bin/bun
if deploy --prefix "$fixture/prefix"; then exit 1; fi
[[ $dest -ef prior ]]
echo -e '#!/bin/sh\nexit 0' > bin/bun
HAPI_RUST_WRAPPER_BIN="$dest" deploy
[[ $(<"$dest") == updated && $(<prior) == wrapper ]]

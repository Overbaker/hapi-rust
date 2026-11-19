#!/usr/bin/env bash
set -euo pipefail
scratch=
if [[ -z ${IDLE_TOOLS:-} ]]; then scratch=$(mktemp -d); fi
toolsdir=${IDLE_TOOLS:-$scratch}
pids=()
cleanup() {
    for pid in $(jobs -pr); do kill -TERM "$pid" 2>/dev/null || true; done
    wait || true
    if [[ -n $scratch ]]; then rm -rf -- "$scratch"; fi
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
out=${1:?pass an output directory}
mkdir -p "$toolsdir" "$out"
out=$(realpath "$out")
verify_sha() { printf '%s  %s\n' "$1" "$2" | sha256sum --check --status; }
for v in 1.3.11 1.4.4; do
    if [[ $v == 1.3.11 && -n ${BUN_A:-} ]] || [[ $v == 1.4.4 && -n ${BUN_B:-} ]]; then continue; fi
    if [[ ! -x $toolsdir/$v/bun-linux-x64/bun ]]; then
        mkdir -p "$toolsdir/$v"
        curl -fsSL --retry 2 "https://github.com/oven-sh/bun/releases/download/bun-v$v/bun-linux-x64.zip" -o "$toolsdir/$v/bun.zip"
        case "$v" in
            1.3.11) digest=8611ba935af886f05a6f38740a15160326c15e5d5d07adef966130b4493607ed ;;
            1.4.4) digest=bf8ccbfb2b0b80159e0f9a6d15c55317add8077534dbc6130a3edb52fccb395c ;;
        esac
        verify_sha "$digest" "$toolsdir/$v/bun.zip"
        unzip -qo "$toolsdir/$v/bun.zip" -d "$toolsdir/$v"
    fi
done
export BUN_A=${BUN_A:-$toolsdir/1.3.11/bun-linux-x64/bun}
export BUN_B=${BUN_B:-$toolsdir/1.4.4/bun-linux-x64/bun}
export IDLE_BUILD=${IDLE_BUILD:-$(mktemp -d "$toolsdir/build.XXXXXX")}
if [[ ! -x $IDLE_BUILD/1.4.4/diagnostic/bun-linux-x64-baseline/hapi ]]; then
    export IDLE_TUNWG="$toolsdir/tunwg-x64-linux"
    if [[ ! -f $IDLE_TUNWG ]]; then
        curl -fsSL --retry 2 'https://github.com/tiann/tunwg/releases/download/v26.08.03%2B122a6d0/tunwg' -o "$IDLE_TUNWG"
    fi
    verify_sha c1a7e08d956d9ee1087b8bf3b651d5dc32aee80eee1052c11218189fd5635768 "$IDLE_TUNWG"
    bun benchmarks/idle-wrapper/build.ts "$IDLE_BUILD"
fi
if [[ -z ${IDLE_CODEX:-} ]]; then
    IDLE_CODEX="$toolsdir/package/vendor/x86_64-unknown-linux-musl/bin/codex"
    if [[ ! -x $IDLE_CODEX ]]; then
        curl -fsSL --retry 2 'https://registry.npmjs.org/@openai/codex/-/codex-0.154.0-linux-x64.tgz' -o "$toolsdir/codex.tgz"
        verify_sha e27c83a49e6031685ee7f956c12aad5f16484d3a80181dd3fea930fb96b3832b "$toolsdir/codex.tgz"
        tar -xzf "$toolsdir/codex.tgz" -C "$toolsdir"
    fi
fi
if [[ -z ${STRACE:-} ]]; then
    if command -v strace >/dev/null; then
        STRACE=$(command -v strace)
    else
        (cd "$toolsdir" && apt download strace && dpkg-deb -x strace*.deb tracer)
        STRACE="$toolsdir/tracer/usr/bin/strace"
    fi
fi
export IDLE_CODEX STRACE
versions=${IDLE_VERSIONS:-'1.3.11 1.4.4'}
groups=${IDLE_GROUPS:-'bare network claude codex agy agy-poll agy-off scan scan-off retry retry-off'}
export IDLE_GROUPS="$groups"
for v in $versions; do
    pids=()
    for g in $groups; do
        bun benchmarks/idle-wrapper/run.ts "$v" "$g" "$out" >"$out/$v-$g.log" 2>&1 &
        pids+=("$!")
    done
    result=0
    for pid in "${pids[@]}"; do wait "$pid" || result=1; done
    ((result == 0)) || exit "$result"
done

bun benchmarks/idle-wrapper/summarize.ts "$out" | tee "$out/table.md"

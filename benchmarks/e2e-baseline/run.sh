#!/usr/bin/env bash
set -euo pipefail
export PYTHONDONTWRITEBYTECODE=1
cd "$(dirname "$0")/../.."
dest=benchmarks/e2e-baseline/results
mkdir -p "$dest"
commit=${BASELINE_COMMIT:-$(git rev-parse HEAD)}
host=$(hostname -s)
raw="$dest/baseline-$commit-$host-$(date -u +%Y%m%dT%H%M%SZ)-$$.jsonl"
export BASELINE_COMMIT="$commit" BASELINE_RUN_ID="$(basename "$raw" .jsonl)"
[[ "$commit" =~ ^[0-9a-f]{40}$ ]] || { echo "Full harness Git ID required" >&2; exit 2; }
expected=${BASELINE_EXPECTED_SHA256:-$(python3 benchmarks/e2e-baseline/fingerprint.py "$commit")}
HARNESS_SHA256=$(python3 benchmarks/e2e-baseline/fingerprint.py)
[[ "$HARNESS_SHA256" == "$expected" ]] || { echo "Sources differ from committed harness $commit" >&2; exit 2; }
export HARNESS_SHA256
export BASELINE_SECTIONS="${*:-full}"
bun -e 'import os from "node:os"; import {chromium} from "playwright"; import pw from "playwright/package.json"; console.log(JSON.stringify({kind:"environment",round:3,sections:process.env.BASELINE_SECTIONS,gitCommit:process.env.BASELINE_COMMIT,harness:process.env.HARNESS_SHA256,time:new Date().toISOString(),commit:process.env.BASELINE_COMMIT,host:os.hostname(),playwright:pw.version,chromium:Bun.spawnSync([chromium.executablePath(),"--version"]).stdout.toString().trim(),cpu:os.cpus()[0].model,logicalCpus:os.cpus().length,memoryBytes:os.totalmem(),kernel:os.release(),bun:Bun.version,corpusSha256:new Bun.CryptoHasher("sha256").update(await Bun.file("benchmarks/e2e-baseline/corpus.md").arrayBuffer()).digest("hex")}))' | tee "$raw"
export BASELINE_CLAUDE_JSONL="${BASELINE_CLAUDE_JSONL:-cli/src/claude/utils/__fixtures__/1-continue-run-ls-tool.jsonl}"
status=0
run() {
    local section=$1
    shift
    echo "{\"section\":\"$section\",\"state\":\"start\"}" | tee -a "$raw"
    local code=0
    "$@" | tee -a "$raw" || code=$?
    echo "{\"section\":\"$section\",\"exit\":$code}" | tee -a "$raw"
    if (( code != 0 )); then status=1; fi
}
sections=("$@")
if (( $# == 0 )); then sections=(fixture navigation scroll mount stream flood hub hook edits diff); fi
for section in "${sections[@]}"; do
    case "$section" in
        fixture) run fixture bun benchmarks/e2e-baseline/fixture.ts ;;
        stream|stream:*)
            for repeat in 1 2 3 4 5; do
                if [[ "$section" == stream ]]; then
                    run "stream-$repeat" env BROWSER_CASES=stream STREAM_REPEAT="$repeat" bun e2e/perf/browser.ts
                else
                    run "$section-$repeat" env BROWSER_CASES=stream STREAM_REPEAT="$repeat" STREAM_SCENARIOS="${section#stream:}" bun e2e/perf/browser.ts
                fi
            done ;;
        navigation|lcp|scroll|mount) run "$section" env BROWSER_CASES="$section" bun e2e/perf/browser.ts ;;
        flood) for i in 1 2 3; do
            args=()
            if (( i % 2 == 0 )); then args+=(--reverse); fi
            run "flood-$i" bun benchmarks/e2e-baseline/terminal.ts "${args[@]}"
        done ;;
        hub) run hub bun benchmarks/e2e-baseline/hub.ts ;;
        hook)
            if ! command -v hyperfine >/dev/null; then
                pkgdir=$(mktemp -d)
                trap 'rm -rf "$pkgdir"' EXIT
                (cd "$pkgdir" && apt-get download hyperfine >&2 && dpkg-deb -x ./*.deb .)
                export PATH="$pkgdir/usr/bin:$PATH"
            fi
            run hook bun benchmarks/e2e-baseline/hook.ts ;;
        edits) run edits python3 benchmarks/e2e-baseline/edit-sample.py ;;
        diff) for i in 1 2 3 4; do run "diff-$i" bun e2e/perf/diff.ts; done ;;
        *) echo "Unknown section $section" >&2; exit 2 ;;
    esac
done
python3 benchmarks/e2e-baseline/report.py "$dest" "$commit" "$host"
exit "$status"

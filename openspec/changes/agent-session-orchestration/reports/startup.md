# Headless wrapper startup on aedas-me

The baseline is commit `cdd4184df062`. Both executables were built on aedas-me (HKGI0211551, x86-64 WSL2 Linux 6.18.33.2) with Bun 1.4.4, the frozen lockfile, and the default `bun-linux-x64-baseline` target. Each build used the same Linux tunwg asset and the build script's stub web manifest.

## Results and retention

The revised candidate misses the 10% retention threshold. Across 40 processes per flavor and build, Claude median startup improved 5.04% and wrapper PSS fell 8.53%. Both changes improved the measured workload. Neither reached 10%. The candidate remains available in the uncommitted review worktree.

| Flavor | Baseline p50 ms | Revised p50 ms | p50 change | Baseline p95 ms | Revised p95 ms | Baseline PSS MiB | Revised PSS MiB | PSS change |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Claude | 264.8 | 251.4 | -5.04% | 270.5 | 258.4 | 73.47 | 67.21 | -8.53% |
| Codex | 203.2 | 201.8 | -0.73% | 213.6 | 208.0 | 54.37 | 55.51 | +2.09% |
| Opencode | 259.9 | 257.8 | -0.80% | 268.1 | 264.7 | 75.22 | 76.75 | +2.03% |
| Pi | 236.9 | 234.8 | -0.90% | 247.3 | 240.7 | 63.93 | 65.50 | +2.45% |

The first comparison measured Claude PSS at 73.44 MiB for baseline and 67.50 MiB for the candidate. The reverse-order comparison measured 73.53 MiB and 66.91 MiB, respectively. The original pre-review comparison recorded a 12.77% PSS reduction (75.40 to 65.77 MiB). The table above uses the two new comparisons of the revised executable. These fresh-launch measurements exclude cached reopens.

## Import coverage

The guard resolves the runner's Claude command in a fresh Bun process with piped stdio. It checks that command resolution loads no other flavor directories. In the same process it then loads `runClaude` and `commands/resume`, checks that the real Claude remote launcher and its base were evaluated, and rejects React, Ink, the terminal displays, the Claude local launcher, other flavors' launchers, hub, and web. Shared API and RPC code still loads some Codex, Cursor, and Pi helpers during session initialization.

Reopens can reuse nonempty hub catalog lists for up to 24 hours. A stamp under `HAPI_HOME/claude-catalog` binds the lists to the session, working directory, model, original arguments, and Claude CLI version. The wrapper checks the installed version with `claude --version` only for an otherwise eligible reopen. Missing stamps and catalog-affecting arguments trigger a fresh SDK catalog probe. The first real Claude SDK init refreshes the published lists and native skill names. A leading `$skill` request can trigger the probe earlier, with later messages queued in arrival order during lookup.

Only the Claude and OpenCode loops have local launchers to defer. Codex uses its shared app-server runtime and Pi uses an RPC subprocess. OpenCode kept its synchronous TTY rendering contract, which its existing switch-to-local regression test exercises.

## Workload

The [harness](../../../../benchmarks/wrapper-startup/run.py) starts a loopback hub and runner from the measured executable in a temporary home. Each sample runs the real CLI with `--started-by runner` and piped output. The timer starts before process creation and stops when a native-agent stand-in writes its first marker. It accepts only Claude's fork invocation, Codex's app-server, OpenCode's ACP process, or Pi's RPC process. Claude uses `--fork-session` so it reaches its main agent spawn without waiting for a first prompt. Executable discovery, version checks, and Claude's SDK catalog probe cannot emit the marker.

Each benchmark invocation discards one warm-up per flavor and measures 20 fresh processes per flavor, rotating the flavor order between rounds. For the review comparison, the baseline and candidate were each measured twice: baseline then candidate in the first pair, candidate then baseline in the second. The pooled results use all 40 measured processes per build and flavor. The reverse-order pair ran in a single remote invocation on aedas-me. The filesystem cache stays warm. Latency is the median and nearest-rank p95 in milliseconds. PSS is the median of the wrapper PID's Linux `smaps_rollup` PSS at 1, 1.25, and 1.5 seconds after the marker, then the median across runs. The native stand-in sleeps during this window. This measures wrapper memory at the spawn boundary and excludes the agent, hub, and runner PIDs. A fully initialized provider session or an active conversation can have a different footprint.

## Reproduction

Run the following in each revision on aedas-me. Use a separate output directory for each build. The host executable build requires the repository's Linux tunwg asset under `shared/tools/tunwg/`.

```sh
bun install --frozen-lockfile
bun cli/scripts/build-executable.ts --outdir /root/work/hapi-p4-results/base
python3 benchmarks/wrapper-startup/run.py /root/work/hapi-p4-results/base/bun-linux-x64-baseline/hapi
```

For the original baseline, use the same benchmark script from this change with the baseline executable.

## Bytecode compilation

The bytecode candidate used a static production-environment preload before importing `index`. Both `--compile --bytecode` and `--compile --bytecode --format=cjs` failed on top-level `await` in Ink's reconciler, Yoga's WASM loader, and the hub passkey route. The plain fallback compiled and printed `hapi version: 0.36.4`. No bytecode executable was available to measure. The bytecode flag and bootstrap changes were dropped under the specified fallback rule.

## Final verification

On aedas-me the requested repository-wide typecheck passed. The full CLI Vitest run with `TMPDIR=/tmp/p4tmp` reported 5,896 passed, 46 skipped, and the three expected root-permission failures in `cursorNotifyRuleOverlay.test.ts`. The import-graph guard and all 14 `runClaude` catalog and deferred-delivery tests passed, as did the base and Claude launcher tests for TTY loss, render failure, and cleanup.

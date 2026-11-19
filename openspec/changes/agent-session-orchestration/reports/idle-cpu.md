# P3 idle-wrapper CPU A/B

**Keep Bun 1.4.4. agy/S5 remains open.** Among these remote wrappers only agy uses one of the modified timers. The best matched post-turn agy pair measured 6.96 fewer observed schedule-ins/s with the fast poll suppressed. Stock agy still averaged **≥5.29/s** in the first after run. The scanner and retry changes concern interactive local launchers; the original evidence for them was synthetic. The original India population and its CPU distribution have not been reproduced.

## Scope

The production observation on india-vps was a mean of 0.5–2% CPU across old wrappers, with a peak of 3.5%. The inventory included 23 wrappers using deleted executables and processes up to 12 days old. The proposal identifies HAPI 0.36.x but provides no patch tag, binary hash, or embedded Bun version. Controlled runs used aedas-me, Ubuntu WSL2, Linux 6.18.33.2-microsoft-standard-WSL2 x86_64 (24 available CPUs). The follow-up also sampled three existing india-vps wrappers read-only (16 available CPUs); current observations cannot identify every wrapper from the original inventory. [Production observation](../proposal.md)

The earlier never-prompted Claude experiment measured at most 0.150% CPU and 2.200 **main-thread** slices/s on Bun 1.4.4. CPU met S5 for that workload. The old wakeup measurement cannot establish process-wide S5, and the scanner switch in that experiment never reached a scanner. The following post-turn measurements supersede it.

## Method

- Build the before wrappers from `cdd4184df0623c43eeb394484b3657e705cd0f99` and the after wrappers from the working tree layered on that commit, with Bun 1.3.11 and 1.4.4 through `cli/scripts/build-executable.ts --target bun-linux-x64-baseline`. Verify the embedded runtime by invoking each binary with `BUN_BE_BUN=1 --version`. Stock wrapper measurements use the release builder with its default empty embedded web UI.
- Each cohort is a separate harness invocation with a private home, database, loopback ports, hub and runner for wrapper workloads. Start five headless `--started-by runner` sessions in five distinct directories. Send one prompt per session through `POST /api/sessions/:id/messages`. Require a fixture answer in hub history, an active non-thinking session and a live agent child. Check those last two conditions at warm-up and every minute boundary. Claude uses a stream-JSON child that emits init, an assistant answer and a result, then waits on stdin. agy uses an ACP child that creates a transcript directory and completes a prompt. Codex uses the real 0.154.0 app-server with an isolated CODEX_HOME and a loopback Responses endpoint. No model account is involved.
- Warm up 60 seconds, then collect ten consecutive 60-second windows per PID. CPU uses process-wide utime + stime from `/proc/PID/stat`, the host's CLK_TCK and elapsed monotonic time. A rate of 100% represents one CPU. Sum scheduling slices across **all observed TIDs** in `/proc/PID/task/*/schedstat`, matching TID and task start time. Schedule-ins include preemption and rescheduling after sched_yield. They are an imperfect proxy for wakeups; observed totals are lower bounds on schedule-ins. At CLK_TCK=100 a 60 s CPU window resolves about 0.0167 percentage points. Threads that start and exit entirely between boundaries cannot be counted. Lost TIDs in the tables are threads seen at the start of a window and missing at its end. PSS is the wrapper's own `smaps_rollup`. Agent, hub and runner resource use is excluded.
- Each mean and sample SD summarizes the five per-PID ten-minute means. Each maximum is the worst of the 50 PID-minute windows. Thread counts span all endpoints. The five PIDs share the cohort environment, and cohorts can share the host concurrently. SD describes dispersion across PIDs, not uncertainty across five independent hosts. Syscall traces run **after** sampling for 30 seconds with `strace -f -c -p` on one wrapper per stock cohort.

## Controls and attribution

| Workload | Construction and assertion |
|---|---|
| bare | One 10 s empty interval per Bun process. This is the control named in the design rule. |
| network | An established Socket.IO WebSocket and HAPI's MCP HTTP listener, with an inert metadata sink. Warm-up starts after the connection callback. |
| agy-poll / agy-off | Same diagnostic executable and real remote ACP path. Arm the 350 ms idle poll or suppress it at the allocation site. All five PIDs must record the site reached and the expected armed/disabled marker. A disabled cohort fails if any PID records an allocation. |
| scan / scan-off | Run the real Codex transcript scanner with an existing transcript and a healthy watcher. Toggle only its 2 s periodic scan at the allocation site. The watcher stays enabled. |
| retry / retry-off | Run the same scanner against a missing transcript. Toggle only the watcher's 1 s retry after the first watch error. The scanner's periodic timer stays enabled. |

Remote Codex on this commit uses `runSharedRuntime`; the Codex scanner starts in `codexLocalLauncher`. Claude's scanner likewise starts in its local launcher. The standalone scanner pairs are positive controls for the actual implementations. Only the real remote agy arm exercises a candidate timer in these three remote wrapper paths. The diagnostic probes are generated into temporary build sources and never ship in a production wrapper.

## Round 1 before the timer changes

| Workload | Bun | CPU % mean ± SD | Max CPU % | Slices/s mean ± SD | Max slices/s | PSS MiB mean ± SD | Threads min–max | Lost TIDs |
|---|---|---:|---:|---:|---:|---:|---:|---:|
| agy | 1.3.11 | 0.909 ± 0.166 | 2.033 | 426.613 ± 70.681 | 913.524 | 137.478 ± 0.383 | 13–17 | 50 |
| agy | 1.4.4 | 0.126 ± 0.003 | 0.183 | 12.072 ± 0.162 | 14.867 | 45.528 ± 0.205 | 13–18 | 433 |
| agy-off | 1.3.11 | 0.299 ± 0.016 | 0.367 | 139.341 ± 2.919 | 153.733 | 134.788 ± 0.257 | 13–17 | 46 |
| agy-off | 1.4.4 | 0.056 ± 0.003 | 0.117 | 3.514 ± 0.030 | 3.932 | 46.310 ± 0.053 | 6–9 | 52 |
| agy-poll | 1.3.11 | 1.021 ± 0.069 | 2.083 | 470.507 ± 36.841 | 952.677 | 135.379 ± 0.355 | 13–17 | 46 |
| agy-poll | 1.4.4 | 0.122 ± 0.001 | 0.167 | 12.020 ± 0.142 | 15.900 | 45.910 ± 0.302 | 13–18 | 443 |
| bare | 1.3.11 | 0.012 ± 0.003 | 0.033 | 0.705 ± 0.023 | 0.900 | 10.419 ± 0.084 | 2–2 | 50 |
| bare | 1.4.4 | 0.001 ± 0.001 | 0.017 | 0.340 ± 0.008 | 0.450 | 3.790 ± 0.013 | 2–2 | 0 |
| claude | 1.3.11 | 0.313 ± 0.030 | 0.400 | 139.630 ± 5.468 | 165.132 | 147.624 ± 0.386 | 12–15 | 5 |
| claude | 1.4.4 | 0.057 ± 0.001 | 0.117 | 3.664 ± 0.023 | 3.949 | 49.256 ± 0.295 | 5–9 | 61 |
| codex | 1.4.4 | 0.059 ± 0.001 | 0.133 | 3.791 ± 0.058 | 4.317 | 46.916 ± 0.251 | 6–9 | 44 |
| v0.36.5 deleted Claude | 1.4.4 | 0.062 ± 0.003 | 0.133 | 3.765 ± 0.030 | 4.200 | 57.240 ± 0.981 | 5–9 | 55 |
| network | 1.3.11 | 0.190 ± 0.008 | 0.283 | 43.875 ± 6.970 | 113.016 | 122.275 ± 0.244 | 2–9 | 275 |
| network | 1.4.4 | 0.050 ± 0.001 | 0.117 | 3.485 ± 0.048 | 4.134 | 42.921 ± 0.072 | 2–3 | 30 |
| retry | 1.3.11 | 0.743 ± 0.109 | 1.267 | 330.695 ± 55.063 | 637.700 | 98.640 ± 0.029 | 12–12 | 5 |
| retry | 1.4.4 | 0.128 ± 0.004 | 0.167 | 16.668 ± 1.337 | 25.886 | 21.440 ± 0.206 | 6–14 | 191 |
| retry-off | 1.3.11 | 0.501 ± 0.005 | 0.567 | 263.662 ± 4.293 | 280.497 | 98.234 ± 0.124 | 12–13 | 20 |
| retry-off | 1.4.4 | 0.053 ± 0.001 | 0.083 | 5.360 ± 0.124 | 7.370 | 21.076 ± 0.111 | 5–13 | 83 |
| scan | 1.3.11 | 0.356 ± 0.017 | 0.433 | 184.952 ± 7.615 | 212.094 | 97.985 ± 0.080 | 12–12 | 5 |
| scan | 1.4.4 | 0.037 ± 0.002 | 0.067 | 4.821 ± 0.015 | 5.150 | 20.716 ± 0.111 | 5–6 | 53 |
| scan-off | 1.3.11 | 0.021 ± 0.001 | 0.083 | 3.247 ± 0.372 | 25.717 | 97.064 ± 0.325 | 5–14 | 125 |
| scan-off | 1.4.4 | 0.010 ± 0.001 | 0.033 | 0.507 ± 0.026 | 0.717 | 20.134 ± 0.076 | 5–7 | 0 |
| codex | 1.3.11 | N/A: ws+unix rejected on all five launches | — | — | — | — | — | — |
| v0.36.0 deleted Claude | 1.4.4 | 0.073 ± 0.002 | 0.150 | 4.959 ± 0.021 | 5.450 | 57.058 ± 0.313 | 5–11 | 68 |

| Workload | Bun | Top thread names (slices/s, summed by name) | Top syscalls (calls in 30 s) |
|---|---|---|---|
| agy | 1.3.11 | HeapHelper 366.14, hapi 60.43, JITWorker 0.03, HTTP Client 0.01 | sched_yield 27607, futex 17561, clock_gettime 1199, epoll_pwait2 345, statx 170 |
| agy | 1.4.4 | mi-scavenger 5.78, hapi 4.73, HeapHelper 1.41, ollector Thread 0.07 | futex 402, statx 172, epoll_pwait2 127, rt_sigprocmask 45, madvise 34 |
| claude | 1.3.11 | HeapHelper 115.78, hapi 23.85, JITWorker 0.00, Bun Pool 0 0.00 | sched_yield 8594, futex 5841, clock_gettime 358, epoll_pwait2 92, read 45 |
| claude | 1.4.4 | hapi 1.87, mi-scavenger 1.74, JSCWarmUp 0.03, JITWorker 0.02 | futex 289, rt_sigprocmask 45, epoll_pwait2 44, madvise 29, getrandom 10 |
| codex | 1.4.4 | hapi 1.90, mi-scavenger 1.85, JSCWarmUp 0.02, JITWorker 0.02 | futex 217, epoll_pwait2 44, rt_sigprocmask 42, madvise 25, getrandom 9 |
| v0.36.5 deleted Claude | 1.4.4 | hapi 1.92, mi-scavenger 1.80, JSCWarmUp 0.03, JITWorker 0.01 | futex 350, epoll_pwait2 45, rt_sigprocmask 45, madvise 25, getrandom 10 |
| v0.36.0 deleted Claude | 1.4.4 | mi-scavenger 2.64, hapi 2.27, JSCWarmUp 0.03, JITWorker 0.02 | futex 201, epoll_pwait2 56, rt_sigprocmask 40, madvise 32, sendto 17 |

**Decision rule.** The original bare controls used the downloaded x64 Bun binaries; the wrappers used x64-baseline. That bare-process difference was 0.011 CPU percentage points and 0.365 slices/s. Neither reached the design threshold (0.2 percentage points or 3/s). The bare control is too weak to attribute the wrapper differences to Bun alone. The connected control showed a much larger version effect: 43.875 versus 3.485 slices/s. On 1.3.11 the real Claude and agy workloads failed S5, and the real Codex app-server transport failed at startup on all five launches with `Wrong url scheme for WebSocket ws+unix`. No Codex 1.3.11 idle rate exists. Keep the 1.4.4 pin; regressing it would lose both performance and Linux Codex compatibility. The repository already pins 1.4.4 in `package.json`; the CI setup action reads that field by default. [Design](../design.md) · [setup-bun v2 version selection](https://github.com/oven-sh/setup-bun/blob/0c5077e51419868618aeaa5fe8019c62421857d6/README.md)

**HAPI-side attribution.** The design's HAPI branch applies. The matched post-turn positive/negative agy pair on 1.4.4 measured 12.088 − 5.128 = **6.960 schedule-ins/s**. All five positive controls recorded construction and all five negative controls recorded suppression. The earlier 3.514/s negative control is unexplained and cannot be reproduced with the surviving base instrumentation. Compared with the later negative control, the observed main thread changed from 1.807 to 1.838/s and mi-scavenger from 1.663 to 1.658/s. HeapHelper changed from zero observed slices to 1.502/s. Distinct observed TIDs increased from 80 to 458 and lost TIDs from 52 to 428. These counters locate the extra work by thread, but cannot identify its trigger. Both arms retained a directory watcher; there was no independent watcher-off comparison. A HAPI, watcher, runtime or harness interaction remains possible. In standalone scanner processes, disabling healthy-file scans reduced observed scheduling by 4.314/s and disabling missing-file retries by 11.308/s. Neither delta measures a remote wrapper improvement. On 1.3.11, `HeapHelper` accounted for most observed slices in Claude and agy, and `sched_yield` and `futex` led the syscall counts. This supports a workload-dependent runtime contribution. The traces do not identify the allocator or GC mechanism behind the PSS difference. The earlier 2.7× PSS gap remains unexplained; these post-turn stock Claude wrappers measured 147.6 versus 49.3 MiB (3.0×), with PSS also sensitive to shared pages across cohorts.

**Production comparison.** The 1.3.11 agy workload reached a 0.909% mean, within the reported India range. This reproduces the magnitude in a different workload, not the identity or cause of the production wrappers. As historical controls, we built upstream [v0.36.0](https://github.com/tiann/hapi/tree/8ff6bd12fae6da205cb50edb9710470c0c0429e6) and [v0.36.5](https://github.com/tiann/hapi/tree/590a9533c9cd926a44e4d102c29076c7bdf82426) with Bun 1.4.4. Five prompted Claude wrappers per tag retained their live children after unlinking the executable; all ten `/proc/PID/exe` links ended in `(deleted)`. Their means were 0.073% and 0.062%; the highest minute was 0.150%. Unlinking alone did not reproduce the reported 0.5–2% mean. The v0.36.0 hub ran from that tag's source with the reflect polyfill preloaded because its bundled hub failed at startup; the measured v0.36.0 wrappers used the unmodified release builder. These historical cohorts kept a reachable hub and runner. They did not reconstruct production reconnect/auth state, 12 days of history, PID-1 adoption, user MCP servers or the exact old artifact.

## Round 1 after the timer changes

| Workload | Bun | CPU % mean ± SD | Max CPU % | Slices/s mean ± SD | Max slices/s | PSS MiB mean ± SD | Threads min–max | Lost TIDs |
|---|---|---:|---:|---:|---:|---:|---:|---:|
| agy | 1.4.4 | 0.058 ± 0.001 | 0.100 | 5.286 ± 0.066 | 6.950 | 44.803 ± 0.175 | 13–17 | 432 |
| claude | 1.4.4 | 0.062 ± 0.003 | 0.133 | 3.625 ± 0.026 | 3.982 | 50.177 ± 0.254 | 5–9 | 56 |
| codex | 1.4.4 | 0.058 ± 0.002 | 0.100 | 3.805 ± 0.032 | 4.083 | 46.944 ± 0.221 | 5–9 | 40 |
| retry | 1.4.4 | 0.055 ± 0.002 | 0.083 | 5.879 ± 0.163 | 7.533 | 21.682 ± 0.071 | 6–15 | 193 |
| retry-off | 1.4.4 | 0.050 ± 0.001 | 0.083 | 5.395 ± 0.122 | 6.900 | 21.753 ± 0.139 | 5–13 | 127 |
| scan | 1.4.4 | 0.012 ± 0.001 | 0.033 | 0.846 ± 0.035 | 1.117 | 22.431 ± 0.030 | 5–5 | 0 |
| scan-off | 1.4.4 | 0.009 ± 0.001 | 0.033 | 0.455 ± 0.010 | 0.717 | 20.985 ± 0.018 | 5–5 | 0 |
| post-turn probe agy-off | 1.4.4 | 0.054 ± 0.003 | 0.100 | 5.128 ± 0.221 | 7.583 | 48.098 ± 0.149 | 13–17 | 428 |
| post-turn probe agy-poll | 1.4.4 | 0.121 ± 0.002 | 0.183 | 12.088 ± 0.075 | 14.899 | 48.579 ± 0.102 | 13–18 | 434 |
| agy repeat | 1.4.4 | 0.055 ± 0.003 | 0.133 | 5.147 ± 0.146 | 7.316 | 45.679 ± 0.350 | 7–17 | 416 |

| Workload | Bun | Top thread names (slices/s, summed by name) | Top syscalls (calls in 30 s) |
|---|---|---|---|
| agy | 1.4.4 | hapi 1.81, mi-scavenger 1.70, HeapHelper 1.64, ollector Thread 0.06 | futex 159, rt_sigprocmask 45, epoll_pwait2 42, madvise 41, getrandom 10 |
| claude | 1.4.4 | hapi 1.87, mi-scavenger 1.71, JSCWarmUp 0.03, JITWorker 0.01 | futex 128, epoll_pwait2 41, rt_sigprocmask 40, madvise 16, getrandom 9 |
| codex | 1.4.4 | hapi 1.90, mi-scavenger 1.87, JSCWarmUp 0.02, JITWorker 0.01 | futex 168, epoll_pwait2 45, rt_sigprocmask 42, madvise 33, getrandom 9 |
| agy repeat | 1.4.4 | hapi 1.86, mi-scavenger 1.69, HeapHelper 1.47, ollector Thread 0.05 | futex 385, rt_sigprocmask 45, epoll_pwait2 42, madvise 38, getrandom 10 |

Every measured stock wrapper stayed below 0.3% CPU in every post-fix minute. Claude and Codex stayed below 5 observed slices/s. **agy still misses the scheduling target**: 5.286/s on average (6.950/s worst minute), versus 12.072/s before. The independent repeat also exceeded 5/s. The post-turn probes recorded an actual allocation in 5/5 positive controls and suppression in 5/5 negative controls. The residual includes named runtime threads: `mi-scavenger` and `HeapHelper` together accounted for 3.34/s of the 5.286/s. Thread names alone do not attribute the extra work to Bun. The lower negative-control run rules out interpreting 5.286/s as an observed floor.

Healthy scanner work fell from 4.821 to 0.846 slices/s. With a missing transcript, the capped-backoff case fell from 16.668 to 5.879/s; even the post-fix no-retry control recorded 5.395/s. Discovery polling continues in that fault scenario. The syscall aggregates and rates belong to the wrapper PID and its threads; they do not measure the agent process tree.

## Remediation and reproduction

The agy idle loop now uses the transcript-directory watcher, retaining a 30 s fallback for silently missed events. It polls at the fast cadence while the watcher is unavailable. It rechecks the transcript after attaching, handles directory renames and watcher loss, and replaces the fast timer on recovery and clears its timer on exit. The fixed-path Codex and Kimi scanners poll every 30 seconds while all selected files have healthy watchers; an unavailable watcher restores the original cadence. A healthy-period tick also compares device and inode with the watch attachment and reattaches on replacement, including when the old inode is held open. Silent events on an unchanged inode can still be delayed by up to the 30 s cadence. Claude’s local scanner continues polling for newly created subagent transcripts because watches on existing files cannot observe them. Watch failures now retry after 1, 2, 4, 8, 16 and then 30 seconds. Recovery rescans the file to catch writes during the gap. The 10-second session keepalive retains its original implementation.

From a Linux x86_64 checkout with frozen dependencies installed, run:

```sh
bun benchmarks/idle-wrapper/run.ts /tmp/hapi-idle-results
```

The suite downloads the requested runtimes, or accepts `BUN_A` and `BUN_B`. It can use `IDLE_CODEX` and `STRACE` overrides. Use `IDLE_VERSIONS` and `IDLE_GROUPS` to select cohorts. The default suite builds the current checkout and runs the fixed production wrappers; the diagnostic `agy-poll` pair can re-arm the former 350 ms cadence. To run a historical artifact, set `IDLE_HISTORIC` to the executable from that tag's baseline build, select its embedded version and set `IDLE_GROUPS=historic`. For v0.36.0 also set `IDLE_HISTORIC_HUB` to the release checkout’s `hub` directory.

On aedas-me the pre-fix outputs are in `/tmp/hapi-p3-r1-results` (including v0.36.5) and `/tmp/hapi-p3-r1-0360`. Final measurements are in `/tmp/hapi-p3-r1-after`; post-turn allocation checks and the independent final agy repetition are in `/tmp/hapi-p3-r1-postturn` and `/tmp/hapi-p3-r1-repeat-final`. For each completed workload, `samples.json` contains the PID-minute windows, per-thread deltas, child PIDs and probe evidence; `table.md` contains the summary. Stock wrapper runs also write `strace.txt`. The incompatible cohort records the error separately in `unsupported.json`.

## Round 2 evidence and provenance

The [round 1 archive](evidence/round1.tar.gz) contains the per-cohort raw JSON, generated tables and syscall summaries behind the two historical tables above. Directory names preserve the original output labels. `hapi-p3-r1-results/*-historic` is v0.36.5; `hapi-p3-r1-0360/*-historic` is v0.36.0. Unprefixed after rows come from `hapi-p3-r1-after`, “post-turn probe” from `hapi-p3-r1-postturn`, and “agy repeat” from `hapi-p3-r1-repeat-final`. The historical before diagnostics cannot be regenerated by running the current site-instrumentation script on cdd4184d. Their captured outputs can be audited directly.

| Table | Production source identity |
|---|---|
| Round 1 before / round 2 local Kimi before | cdd4184df0623c43eeb394484b3657e705cd0f99, empty production diff |
| Round 1 after | Same base plus uncommitted changes. The surviving end-of-round production diff hashes to `974969749e72fd7ffb6d5f1bb6a80d45ff75aae7cf2c58aa6aed600a9f8af533`. This is reconstructed provenance. No build-time diff hash survives for individual intermediate diagnostic binaries. |
| Round 2 after | Same base, production diff SHA-256 `a2cf7a6bfaea19ccc46883483b6930d107d52405400012e740be265229b465b8` |
| Historical Claude | Unmodified v0.36.0 and v0.36.5 release sources identified above. |

Production digests use the bytes of `git diff --no-ext-diff <base> -- cli/src/agy/agyAcpRemoteLauncher.ts cli/src/modules/common/session/BaseSessionScanner.ts cli/src/modules/watcher/startFileWatcher.ts` (use the committed P3 tree as the second revision when auditing later). The two Bun 1.4.4 executables actually used for the round 2 local comparison hashed to `d92db01eed2965820f78d9a5c1b8f6d6f44cd231d9ed8541e7233af97dfc1fb0` before and `541b68fae6f2057eba750c843e167e45dc14377cfd33f00d6df4eded7086248c` after.

### Existing India processes

Read-only sampling used the same `snapshot` and `interval` functions for ten one-minute windows and `BUN_BE_BUN=1 /proc/PID/exe --version` for the runtime. All three executables were deleted and all three parents were PID 1. The selection covers two Claude processes and one agy process; session activity and connection/auth state were not independently observed. [Raw per-thread samples](evidence/india.json)

| PID | Flavor | Embedded Bun | Mean CPU % | Worst CPU % | Mean observed slices/s | Leading threads (slices/s) |
|---:|---|---|---:|---:|---:|---|
| 594906 | Claude | 1.4.4 | 0.122 | 0.150 | 6.52 | hapi 2.28, HeapHelper 2.15, mi-scavenger 2.07 |
| 863245 | Claude | 1.4.4 | 0.118 | 0.133 | 6.73 | hapi 2.40, mi-scavenger 2.26, HeapHelper 2.07 |
| 2192888 | agy | 1.3.11 | 3.127 | 4.779 | 714.79 | HeapHelper 630.16, hapi 74.55, JITWorker 10.07 |

This directly establishes mixed embedded runtimes among **current** deleted India wrappers and one high-CPU agy process. It does not reproduce the original 23-wrapper population mean or separate version, session state and host effects. The high schedule-in rate can include repeated yields.

### Additional controlled workloads

For `kimi-local`, five real HAPI local launchers per cohort ran the published Kimi Code 2.6.3 TUI on separate pseudo-terminals. In each private workspace the PTY driver accepted the trust prompt and submitted one message to a loopback model fixture. Warm-up required the fixture answer in the actual Kimi wire transcript and an inotify watch on that transcript’s inode in the wrapper process. The samples measure the HAPI wrapper; they exclude the TUI child. Both builds used the same real child executable and the same Bun baseline target. The before build reversed the production patch against the staged CLI sources before invoking the release builder. [Observed PTY, watch and child state](evidence/kimi-local.json)

For `orphan` and `auth-rejected`, all five remote Claude wrappers first completed a turn and retained their agent child. The harness then killed its private hub and runner. The orphan arm kept the listener unavailable. The rejected-token arm bound a new Socket.IO server on the same port and required an explicit namespace authentication rejection for each of the five session IDs. A namespace refusal need not trigger the same retries as a transport failure. The harness remained the wrappers’ parent. Neither simulation reconstructs the India wrappers’ process age, exact history, or original authentication state.

To select the failure states, set `IDLE_GROUPS='orphan auth-rejected'`. To measure a local session, set `IDLE_GROUPS=kimi-local` and `IDLE_KIMI` to the native TUI launcher. Set `IDLE_VERSIONS=1.4.4` to select the retained runtime. For a before-only stock build, pass a production-source Git patch through `IDLE_BASE_PATCH` to `build.ts` and supply the resulting stock build to the cohort via `IDLE_BUILD`. Source and runtime identities are listed above.

| Round 2 workload | Bun | CPU % mean ± SD | Max CPU % | Slices/s mean ± SD | Max slices/s | PSS MiB mean ± SD | Threads min–max | Lost TIDs |
|---|---|---:|---:|---:|---:|---:|---:|---:|
| agy | 1.4.4 | 0.049 ± 0.001 | 0.083 | 3.660 ± 0.032 | 3.965 | 45.110 ± 0.089 | 6–9 | 49 |
| auth-rejected | 1.4.4 | 0.020 ± 0.001 | 0.067 | 0.749 ± 0.049 | 0.967 | 48.770 ± 0.209 | 5–6 | 0 |
| bare | 1.4.4 | 0.001 ± 0.001 | 0.017 | 0.339 ± 0.006 | 0.400 | 2.390 ± 0.004 | 2–2 | 0 |
| orphan | 1.4.4 | 0.057 ± 0.002 | 0.100 | 2.169 ± 0.058 | 2.633 | 50.615 ± 0.403 | 7–10 | 80 |
| local Kimi before | 1.4.4 | 0.156 ± 0.006 | 0.200 | 25.751 ± 0.629 | 28.629 | 55.664 ± 0.187 | 13–18 | 422 |
| local Kimi after | 1.4.4 | 0.140 ± 0.002 | 0.200 | 23.259 ± 0.135 | 25.134 | 52.669 ± 0.257 | 13–17 | 429 |
| auth-rejected | 1.3.11 | 0.346 ± 0.013 | 0.467 | 98.355 ± 8.430 | 133.415 | 147.289 ± 0.295 | 12–14 | 0 |
| bare | 1.3.11 | 0.015 ± 0.001 | 0.033 | 0.683 ± 0.018 | 0.900 | 7.763 ± 0.051 | 2–2 | 50 |
| orphan | 1.3.11 | 0.586 ± 0.011 | 0.700 | 180.729 ± 6.812 | 207.469 | 147.918 ± 0.126 | 13–14 | 25 |

The real local Kimi comparison moved from 0.156% to 0.140% mean wrapper CPU and from 25.751 to 23.259 observed slices/s. This was one paired run with a healthy wire file. It supports a modest local-mode difference but cannot isolate the watcher-error backoff or establish an effect in a remote Claude/Codex wrapper.

With the same baseline engine inside each stock executable (`BUN_BE_BUN=1`), the bare differences were 0.014 percentage points and 0.344 observed slices/s. The design’s bare-control threshold still fails. When the hub and runner were gone, Bun 1.3.11 Claude reached 0.586% mean CPU, inside the lower part of the historical India range; 1.4.4 measured 0.057%. The token-rejected cases measured 0.346% and 0.020%, respectively. Reconnect state is therefore a plausible contributor on some runtimes. The original India connection states, runtimes across the full population, host load and work histories remain unknown. No controlled cohort reproduced that population.

With the 30 s fallback restored, the new 1.4.4 agy cohort recorded 3.660 observed slices/s, below the threshold in this run. The earlier post-fix runs were above it, thread churn varied substantially, and the proxy misses some short-lived threads. **agy/S5 remains open.** The within-cohort SD does not capture these between-run effects. Group mixes and version order differed, so small differences and shared-page PSS estimates should not be generalized. [Round 2 samples, generated tables and syscall summaries](evidence/round2.tar.gz)

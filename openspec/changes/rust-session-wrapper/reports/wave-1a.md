# Wave 1a · Claude runner integration and resident cost

Measured on **aedas-me**, Ubuntu on WSL2, x86_64, kernel `6.18.33.2-microsoft-standard-WSL2`, 24 visible logical CPUs. Bun 1.4.4 and rustc 1.100.0. Measured 2026-11-19. The india-vps measurement remains open.

| Measurement | TypeScript | Rust release | Proposal target and remote result |
|---|---:|---:|---|
| S1 wrapper PSS, cohort mean (KiB) | 74705.4 | 1335.0 | ≤ 15 MB per Rust wrapper · **Pass**, maximum 1,362 KiB |
| HAPI-owned PSS per session, mean (KiB) | 74705.4 | 1335.0 | Reported including resident helpers |
| Full session PSS, mean (KiB) | 80317.7 | 6945.8 | Includes the external fake Claude |
| S2 wrapper CPU, cohort mean (% of one CPU) | 0.053302 | 0.000000 | ≤ 0.1% · **Pass**, Rust maximum 0.000000% |
| Main-thread scheduler dispatches/s (schedstat), mean | 1.830578 | 0.333143 | Linux scheduling proxy |
| S2 whole-wrapper context switches/s, mean | 6.247951 | 0.333143 | ≤ 1 wakeup/s excluding protocol timers · **Pass on the inclusive proxy**, Rust maximum 0.333152/s |
| S8 spawn → webhook p50 (ms) | 235 | 8 | Rust p50 ≤ TS p50 · **Pass** |
| Spawn → webhook p95 (ms) | 244 | 10 | Reported. S8 specifies p50. |

## Per-session PSS (KiB)

| Cohort member | TS wrapper | TS HAPI total | TS with agent | Rust wrapper | Rust HAPI total | Rust with agent |
|---:|---:|---:|---:|---:|---:|---:|
| 1 | 74754 | 74754 | 80362 | 1360 | 1360 | 6976 |
| 2 | 74318 | 74318 | 79934 | 1313 | 1313 | 6922 |
| 3 | 74158 | 74158 | 79768 | 1311 | 1311 | 6918 |
| 4 | 74714 | 74714 | 80331 | 1313 | 1313 | 6920 |
| 5 | 74690 | 74690 | 80304 | 1335 | 1335 | 6949 |
| 6 | 74962 | 74962 | 80571 | 1362 | 1362 | 6976 |
| 7 | 75461 | 75461 | 81073 | 1358 | 1358 | 6972 |
| 8 | 74974 | 74974 | 80584 | 1360 | 1360 | 6966 |
| 9 | 74350 | 74350 | 79968 | 1319 | 1319 | 6931 |
| 10 | 74673 | 74673 | 80282 | 1319 | 1319 | 6928 |

## Measurement conditions

A single isolated local HAPI hub served both cohorts. Its real runner started 20 sessions sequentially per runtime through the machine-spawn endpoint, first TS under Bun and then the optimized Rust binary. The first ten in each cohort supplied startup samples only. The other ten each completed one headless scripted Claude turn and stayed resident. All twenty were idle together for at least ten minutes before the 60-second sampling window.

PSS is the end-of-window `smaps_rollup` value. The sampler enumerated each wrapper’s process group and descendants. It observed 0 additional HAPI-owned helpers across the ten TS sessions and 0 across the ten Rust sessions at the endpoint, plus one fake Claude per session. The hub, runner, and test driver are shared infrastructure and are excluded. Peer tools, terminals, and hooks were not invoked during the idle window.

CPU uses process-wide user plus system tick deltas at 100 Hz, divided by the measured monotonic interval for each wrapper. The table gives arithmetic means across the ten wrappers. The elapsed intervals ranged from 60.032655 to 60.036705 seconds. All ten Rust wrappers accumulated zero whole CPU ticks at that resolution.

The leader's `schedstat` count and the whole-wrapper context-switch count are proxies for wakeups. Each Rust wrapper recorded 20 dispatches and 20 context switches. These counts include keepalives, Engine.IO pings and preemptions, so the inclusive rate also clears S2. A pilot rejected changes in TS process/thread membership. In the recorded run 91 TS threads present at the start were gone at the end. Endpoint thread snapshots alone yielded 4.740513 observed dispatches/s for TS. Inherited perf counters retained the retired workers' activity and measured 6.247951 switches/s across all TS threads.

Latency spans the runner log timestamp immediately before wrapper spawn through entry to its session-started webhook handler, with 1 ms resolution. It excludes machine-spawn preflight and subsequent RPC registration. Percentiles use nearest rank across 20 observations per runtime. The measured wrapper sources were at `8e53e8d9`. The measured runner included the live-generation fix (`cli/src/runner/run.ts` SHA-256 `b6f4220439a5851b30ba214252909386a030768914b5cc79c7ddb2d1946e7dc2`). The later guard for a late webhook following an unverifiable timeout was outside these successful startup cases.

[Raw per-wrapper KiB, ticks, scheduler counts, elapsed times and 40 latency observations](wave-1a.samples.json) · [Reproduction instructions](../../../../benchmarks/wrapper-compare/README.md)

## Tier-B outcomes and coverage

- A real hub spawn produced an active Rust Claude row. After restarting the runner, the inventory recognized the same live PID, with no drift report. StopSession returned `stopped` and the wrapper group had zero live members.
- TS → Rust → TS wakes retained the original HAPI and native Claude IDs. Previous hub messages kept their IDs and content, the native transcript was append-only, and no session redirect was recorded. The fixture advanced the idle-sweep clock. Eligibility, machine RPC, tree stop, hub hibernation mark and wake-on-send used production code.
- A source layout without a Rust binary selected TS. In a separate test, the actual Rust wrapper acquired the row and exited 86 before the webhook when its hook directory could not be created. The runner retried TS using the existing row. Both fallback tests finished a turn with exactly one row.
- Generation-mismatch tests also verified that a pre-webhook Rust timeout cannot signal a reused PID and an unverifiable timeout cannot promote a late webhook.
- The remote runner integration suite passed with Rust enabled (31 tests). The stress test was skipped and the opt-in benchmark ran separately. Both legs of all 18 parity scenarios passed (62 tests including harness checks), as did 202 Rust tests, Clippy, formatting and CLI typecheck. The browser live E2E portion of S8, india-vps thresholds, packaged TS executable performance, real-provider inference, and non-Claude flavors were not measured here.

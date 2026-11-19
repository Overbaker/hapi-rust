# Wave 1a · Claude wrapper residency and startup

**Remote fixture results on aedas-me, Ubuntu under WSL2, 2026-11-19.** Each row describes a particular local-hub experiment. The singleton and ten-session rows use the same executable bytes. In the **cohort of 10, shared text amortised**, PSS assigns each process a fraction of shared executable pages. The compiled runner was also resident in the ten-session windows. RSS, PSS and USS are shown together. The india-vps acceptance measurement remains open.

## Binaries measured

| Executable | `--version` | Bytes | SHA-256 |
|---|---|---:|---|
| Compiled HAPI CLI | `hapi version: 0.36.4` | 164,804,784 | `294af8d61b4f4ae3e9c379a0a0b9b67f8fe999d80307aff391b5cd906bf7d8b2` |
| Rust release | `0.36.4 (rust ce73f85e79f9)` | 5,801,880 | `80fa4b0162f38dd266cd76e65d7d13822ad2d16085b4ad0dc718e38685fcc4ac` |

The source checkout was `ce73f85e79f9`. The TS runner and TS wrapper used the compiled `bun-linux-x64` CLI from `cli/scripts/build-executable.ts` without embedded web assets. An independent compiled build from the same checkout matched the measured executable byte-for-byte. The Rust build stamp matches the checkout revision. The harness matched each running wrapper's `/proc/PID/exe` device and inode to the identified artifact. The raw samples include both hashes, versions, sizes and the compiled build receipt.

## Reading the tables

- **Light** means one completed headless fixture turn. **Loaded** means 30 completed turns and 30 relayed tool results of 34,500 bytes apiece. The native transcript was 1,050,063 bytes per loaded session. Each loaded wrapper held a real `/bin/sh` PTY. A terminal client checked shell output before and after sampling.
- The ten-session groups had at least ten minutes of idle time. Singleton windows retained one already-warm wrapper, stopped the other sessions and the runner, and verified that this was the only process executing the measured binary before and after the window. Shared system libraries can still contribute fractional PSS. The TS-only scaling baseline supplied the TS singleton. Each paired row compares both runtimes on the same hub.
- Memory is end-of-window guest `smaps_rollup` in KiB. USS is Private_Clean + Private_Dirty. HAPI-owned PSS includes helper descendants and process-group members, including terminal shells. Loaded sessions had 1 additional HAPI-owned processes per session at the endpoint (PTY shells). The last residency column also includes the external Python Claude fixture. Shared hub, runner and measurement clients are outside the session totals.
- S1 uses **maximum Rust PSS ≤ 15,000,000 bytes** within each named configuration. The same numerical comparison for RSS and USS is diagnostic. S2 uses the maximum per-wrapper 60-second mean against **0.1% of one CPU** and **1 wakeup/s**. Scheduling counts are proxies including preemptions, keepalives and Engine.IO traffic. Passing the inclusive proxy clears the budget in that fixture. An exceeded inclusive budget leaves the protocol-excluded rate unresolved.
- The high-resolution whole-wrapper CPU column uses inherited task-clock nanoseconds, including retired threads. The schedstat CPU column sums field 1 across observable threads and can lose CPU from threads that exited. Leader dispatches use field 3. Whole-thread switches use inherited software counters. The tick column shows `< 0.017` for zero observed ticks at 100 Hz. Window lengths were 60.007164–60.104096 seconds. Every sampled session was active after its window, with a matching live wrapper PID and a hub heartbeat timestamp inside the window.

### Wrapper and session memory (mean KiB)

| Condition | Wrapper | N | RSS | PSS | USS | HAPI-owned PSS/session | PSS/session with fixture |
|---|---|---|---|---|---|---|---|
| HTTP light, TS scaling baseline | TS | 10 | 98,286.0 | 52,660.7 | 48,189.6 | 52,660.7 | 59,672.5 |
| HTTP light, sole TS session | TS | 1 | 98,556.0 | 95,896.0 | 95,724.0 | 95,896.0 | 103,932.0 |
| HTTP light, paired | TS | 10 | 97,810.0 | 52,546.3 | 48,088.8 | 52,546.3 | 59,445.7 |
| HTTP light, paired | RUST | 10 | 7,186.0 | 1,379.5 | 893.6 | 1,379.5 | 8,276.3 |
| HTTP light, sole Rust session | RUST | 1 | 7,284.0 | 5,269.0 | 5,164.0 | 5,269.0 | 13,441.0 |
| HTTPS light, paired | TS | 10 | 98,874.8 | 52,470.9 | 47,939.2 | 52,470.9 | 59,349.8 |
| HTTPS light, paired | RUST | 10 | 8,210.0 | 1,733.8 | 1,186.0 | 1,733.8 | 8,615.5 |
| HTTPS loaded, paired | TS | 10 | 101,652.4 | 55,137.8 | 50,531.6 | 55,277.4 | 62,376.1 |
| HTTPS loaded, paired | RUST | 10 | 8,598.0 | 2,070.0 | 1,526.8 | 2,210.4 | 9,312.6 |

### Wrapper memory components (mean KiB)

| Condition | Wrapper | N | PSS anon | PSS file | Private clean | Private dirty | Swap | SwapPss |
|---|---|---|---|---|---|---|---|---|
| HTTP light, TS scaling baseline | TS | 10 | 48,188.4 | 4,472.3 | 1.2 | 48,188.4 | 0.0 | 0.0 |
| HTTP light, sole TS session | TS | 1 | 48,388.0 | 47,508.0 | 47,336.0 | 48,388.0 | 0.0 | 0.0 |
| HTTP light, paired | TS | 10 | 48,088.8 | 4,457.5 | 0.0 | 48,088.8 | 0.0 | 0.0 |
| HTTP light, paired | RUST | 10 | 891.2 | 488.3 | 2.4 | 891.2 | 0.0 | 0.0 |
| HTTP light, sole Rust session | RUST | 1 | 888.0 | 4,381.0 | 4,276.0 | 888.0 | 0.0 | 0.0 |
| HTTPS light, paired | TS | 10 | 47,939.2 | 4,531.7 | 0.0 | 47,939.2 | 0.0 | 0.0 |
| HTTPS light, paired | RUST | 10 | 1,184.4 | 549.4 | 1.6 | 1,184.4 | 0.0 | 0.0 |
| HTTPS loaded, paired | TS | 10 | 50,530.0 | 4,607.8 | 1.6 | 50,530.0 | 0.0 | 0.0 |
| HTTPS loaded, paired | RUST | 10 | 1,524.4 | 545.6 | 2.4 | 1,524.4 | 0.0 | 0.0 |

### Idle CPU and scheduling (cohort means)

| Condition | Wrapper | N | Whole CPU % | Tick CPU % | Observed schedstat CPU % | Leader dispatches/s | Whole switches/s |
|---|---|---|---|---|---|---|---|
| HTTP light, TS scaling baseline | TS | 10 | 0.053923 | 0.059967 | 0.050633 | 1.767371 | 5.955091 |
| HTTP light, sole TS session | TS | 1 | 0.046601 | 0.066651 | 0.045091 | 1.882884 | 5.998568 |
| HTTP light, paired | TS | 10 | 0.052006 | 0.049951 | 0.047389 | 1.794912 | 6.252220 |
| HTTP light, paired | RUST | 10 | 0.001966 | 0.004995 | 0.002315 | 0.333009 | 0.333009 |
| HTTP light, sole Rust session | RUST | 1 | 0.002026 | < 0.017 | 0.002389 | 0.333294 | 0.333294 |
| HTTPS light, paired | TS | 10 | 0.051962 | 0.056593 | 0.044854 | 1.807632 | 6.258467 |
| HTTPS light, paired | RUST | 10 | 0.002240 | 0.001664 | 0.002601 | 0.334560 | 0.334560 |
| HTTPS loaded, paired | TS | 10 | 0.047277 | 0.053246 | 0.043566 | 1.798716 | 5.953562 |
| HTTPS loaded, paired | RUST | 10 | 0.002197 | < 0.017 | 0.002549 | 0.341153 | 0.341153 |

### S1 and S2 checks for the sampled Rust wrappers

| Rust condition | S1 max PSS (KiB) | Max RSS vs 15 MB | Max USS vs 15 MB | Max CPU % | Max switches/s | S2 inclusive proxy |
|---|---|---|---|---|---|---|
| HTTP light, paired | 1,394 · pass | 7,288 · pass | 908 · pass | 0.002176 | 0.333012 | Pass |
| HTTP light, sole Rust session | 5,269 · pass | 7,284 · pass | 5,164 · pass | 0.002026 | 0.333294 | Pass |
| HTTPS light, paired | 1,761 · pass | 8,336 · pass | 1,200 · pass | 0.002530 | 0.349540 | Pass |
| HTTPS loaded, paired | 2,089 · pass | 8,704 · pass | 1,552 · pass | 0.002499 | 0.349478 | Pass |

### S8 fresh spawn to webhook (ms)

| Fresh-start cohort | TS p50 | Rust p50 | TS p95 | Rust p95 | S8 latency only |
|---|---|---|---|---|---|
| HTTP light, TS scaling baseline | 237 | — | 243 | — | TS-only baseline |
| HTTP light, paired | 238 | 8 | 246 | 11 | Pass |
| HTTPS light, paired | 280 | 11 | 290 | 14 | Pass |
| HTTPS loaded, paired | 277 | 12 | 291 | 15 | Pass |

Startup percentiles use nearest rank over 20 sequential new sessions per runtime, from the runner's millisecond timestamp before spawn to receipt of the session-started webhook. Rust and TS report that webhook at different points in bootstrap. History and terminals are created **after** this timing. The S8 latency target is Rust p50 ≤ compiled TS p50 for the fresh starts on the indicated loopback transport. The CLI integration portion passed separately. The browser live E2E portion was not run, so S8 as a whole is open. Singletons reuse a session and have no separate startup distribution.

## Tier-B and verification

The real hub and runner exercised tracked-child stop, restart with persisted tracking, and restart with the fixture’s tracking record removed. The last case adopted exactly the tested PID and emitted no drift report for it. The agent created one child inside the wrapper group and one in a new session via `setsid`. Immediately after StopSession returned `stopped`, the group and all snapshotted live descendant generations were gone, including the outside-group child. This covers descendants still attributable to the wrapper before stop.

Both hibernation directions retained the HAPI row, previous hub message IDs, the native transcript prefix and zero merge redirects. A second variant rotated the native ID and invoked the generated SessionStart hook, withholding SDK init until the hub reflected the new ID. The fixture advances eligibility time only for the named row. Missing Rust and a confirmed pre-webhook exit 86 both fell back to compiled TS with one working row. Old-agent versions, non-Linux selection and empty-flavor rollback were outside these tier-B fallback cases.

The integration CI job builds the locked release wrapper and compiled CLI, runs runner integration, then runs the runner and tier-B tests again with Rust Claude selected. Missing expected binaries and a Rust build from another revision fail setup. Remote verification passed 216 Rust tests, Clippy with warnings denied, formatting and CLI typecheck. The runner integration suite passed 34 tests and the Rust-selected rerun passed 32. Both legs of all 18 parity scenarios passed (62 tests including the harness checks).

## Limits

The HTTPS/WSS cohorts used a loopback TLS listener in front of the same real hub, a self-signed CA supplied through `NODE_EXTRA_CA_CERTS`, and normal certificate validation. Rust initialised its TLS client and native roots. This environment does not measure internet RTT or a public reverse proxy. Guest Swap and SwapPss cannot establish Windows host paging.

Outstanding coverage includes india-vps, real Claude inference, user MCP servers, image uploads/base64 tool results, peer tools under load, days-old or much larger sessions, other flavors, and the web live E2E portion of S8. The loaded fixture exercises post-history idle and one open terminal per session. Results do not establish a production-wide S1, S2 or S8 pass.

[Raw per-wrapper samples and all startup observations](wave-1a.samples.json) · [Reproduction commands](../../../../benchmarks/wrapper-compare/README.md)

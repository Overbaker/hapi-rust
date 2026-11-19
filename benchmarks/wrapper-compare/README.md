# Claude wrapper comparison

Build from a Linux checkout with Bun, Rust, OpenSSL, Python 3, a C compiler and permission to open per-thread software perf counters. These commands produce the release Rust wrapper and the compiled CLI-only HAPI executable used by the tests and benchmarks.

```sh
bun install --frozen-lockfile
bun run download:tunwg
cargo build --locked --release -p hapi-session-wrapper
bun cli/scripts/build-executable.ts --target bun-linux-x64 --record-build
export HAPI_WRAPPER_BENCH_DIR=/tmp/hapi-wrapper-results
benchmarks/wrapper-compare/run.sh
python3 benchmarks/wrapper-compare/summarize.py "$HAPI_WRAPPER_BENCH_DIR" --json /tmp/wave-1a.samples.json
```

The tier-B suite checks the Rust build revision against the checkout, verifies the compiled CLI's SHA-256 and revision against its build receipt, and matches the live wrapper's executable device and inode. The build and the tests require Git metadata for the checkout being measured. A source-sync mirror also needs the matching shallow Git metadata so the test can resolve HEAD independently of the build stamp. The tests fail on missing expected binaries in CI. On a developer machine, set `HAPI_EXPECT_RUST_WRAPPER=1` to require the Rust leg.

## Profiles

Pass a subset of the following profile names as arguments to rerun them. Run serially on a quiet host. The script gives each profile a new temporary HAPI home and local hub. Both wrapper runtimes are launched by the compiled runner.

| Profile | Workload |
|---|---|
| `http-ts` | Ten one-turn compiled TS sessions, then a single retained TS session |
| `http-rust` | Ten one-turn sessions per runtime on the same HTTP hub, then a single retained Rust session |
| `https-light` | Ten one-turn sessions per runtime on the same HTTPS hub |
| `https-loaded` | Ten sessions per runtime on HTTPS, each with 30 turns, a tool result per turn, roughly 1 MB of native JSONL and one live PTY |

The TLS listener fronts the real test hub, forwarding decrypted HTTP and upgraded WebSocket traffic over loopback. The driver creates a temporary self-signed CA and an IP-SAN certificate. It starts the clients with `NODE_EXTRA_CA_CERTS` and normal certificate verification. Loaded terminals run `/bin/sh` with an isolated home through the authenticated hub terminal namespace. A shell round trip before and after the window verifies that each PTY remains usable.

Each cohort uses 20 sequential cold session starts. The first ten sessions stop after startup; the other ten complete their fixture turns. Startup latency uses the runner's millisecond log timestamps immediately before wrapper spawn and on entry to the session-started webhook handler. Use nearest-rank p50 and p95. Even in the loaded profile these are fresh starts **before** the history and terminal exist.

The ten-session cohorts idle at least ten minutes before a roughly 60-second window. Singleton measurements retain one of those already-idle wrappers, stop the other sessions and the runner, and sample another window. They require exactly one process executing the measured binary. Singleton latency is not separately sampled.

## Counters

The sampler reads `smaps_rollup` RSS, PSS, Pss_Anon, Pss_File, Pss_Shmem (when available), Private_Clean, Private_Dirty, Swap and SwapPss. USS is Private_Clean + Private_Dirty. It totals HAPI-owned PSS and full-session PSS over the process group and ancestry, identifying the external fixture agent separately. Shared hub, runner and test clients are excluded.

CPU uses inherited task-clock nanoseconds across all wrapper threads. The JSON also contains process user+system ticks and the sum of endpoint thread `schedstat` on-CPU nanoseconds. Endpoint sums can miss retired workers. Leader dispatch counts come directly from `schedstat`; inherited software context-switch counters include retired threads, preemptions and protocol timers. These are proxies for wakeups. The sampler records thread turnover, elapsed nanoseconds, the wall-clock window, an in-window hub heartbeat, and post-window active state for each PID. Raw sample files include binary sizes, versions, SHA-256, individual startup times and transcript sizes. Keep the JSON with the report.

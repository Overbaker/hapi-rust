# Claude wrapper comparison

On Linux with procfs, Python 3, a C compiler, and permission to open software perf events, build the release wrapper and run the opt-in benchmark from the repository root. The integration harness starts its own hub and runner, uses a temporary HAPI home, and reaps its processes at teardown. Set the output path to a location outside the temporary home.

```sh
bun install --frozen-lockfile
cargo build --release -p hapi-session-wrapper
cd cli
export TMPDIR="$(mktemp -d)"
HAPI_WRAPPER_BENCH_OUT=/tmp/wrapper-samples.json \
    bun x --no-install vitest run --config vitest.integration.config.ts \
    src/runner/rustWrapper.integration.test.ts -t 'measures wrapper cohorts'
```

The driver starts 20 sessions sequentially per runtime through the hub's machine-spawn API. It stops the first ten and leaves the next ten idle after one completed fake-Claude turn. The runner restarts between runtimes while the same hub and the ten TS sessions stay up. The sampler waits ten minutes after both cohorts are ready, then observes a 60-second interval.

Latency comes from the runner's millisecond log timestamps immediately before calling `spawnSessionWrapper` and on entry to the `/session-started` handler. The samples exclude HTTP spawn-request preflight and post-webhook processing. Report nearest-rank percentiles over all 20 attempts.

The sampler reads Linux `smaps_rollup` PSS for the wrapper, its group and its descendants. It reports HAPI-owned PSS separately from the inclusive total with the fake model process. It verifies the wrapper process generation and the presence of the fake agent at both endpoints. CPU uses the wrapper's process-wide user and system clock ticks from `/proc/PID/stat`, including retired worker threads. Divide the delta by `CLK_TCK` and the observed monotonic interval (100% is one logical CPU).

The third field of `/proc/PID/schedstat` gives the main thread's scheduling count. The sampler also opens a software context-switch counter on every live wrapper thread before the interval. Those counters inherit into new threads (`inherit_thread`), include retired threads, and exclude forked processes. This yields a whole-wrapper scheduling rate even when the worker pool changes. Both rates include preemptions and protocol timers. The JSON includes the underlying counts, elapsed nanoseconds, KiB and all individual latency samples.

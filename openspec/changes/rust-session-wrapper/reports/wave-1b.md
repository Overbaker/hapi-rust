# Wave 1b · Codex shared runtime on aedas-me

A compiled TypeScript wrapper and a Rust release wrapper each hosted one and five roots against a scripted Codex app-server on a Unix socket. Each hosted root had its own running MCP bridge. Rust reduced median spawn-to-webhook latency by 82.9%. With five roots, the wrapper and its helpers used 2,814 KiB PSS per hosted session for Rust and 55,283 KiB for TypeScript.

## Startup latency

Elapsed milliseconds, nearest-rank percentiles. There were 20 fresh wrappers of each flavor. The harness alternated the order of each pair. It started four additional roots sequentially with `/new` in each wrapper. Spawn time starts immediately before process creation and ends when the fixture runner receives the webhook. The fifth-webhook column includes the harness polling and issuing the four commands. The filesystem cache was warm.

| Wrapper | Spawn → first webhook p50 / p95 | Spawn → fifth webhook p50 / p95 | Additional root request → webhook p50 / p95 (80 requests) |
| --- | --- | --- | --- |
| TS | 854.6 / 1,020.5 | 1,750.0 / 2,054.2 | 219.2 / 253.5 |
| Rust | 146.4 / 152.3 | 211.2 / 215.3 | 10.1 / 13.3 |

## Idle residency and CPU

Each cohort ran for ten minutes after its final scripted turn, followed by one 60-second sample. Memory is end-of-window KiB from `smaps_rollup`. USS sums private clean and dirty pages. CPU is percent of one logical CPU. Switches/s is the whole-thread scheduling proxy for wakeups, including preemptions, keepalives and Engine.IO pings. Counts come from inherited Linux perf software context-switch and task-clock events on every wrapper and helper thread.

| Runtime | Roots | Process accounting | RSS (KiB) | PSS (KiB) | USS (KiB) | CPU % | Switches/s |
| --- | --- | --- | --- | --- | --- | --- | --- |
| TS | 1 | Wrapper | 92,144.0 | 74,266.0 | 58,956.0 | 0.0475 | 5.633 |
| TS | 1 | MCP helpers, sum | 70,432.0 | 52,554.0 | 37,244.0 | 0.0482 | 6.133 |
| TS | 1 | Wrapper + helpers | 162,576.0 | 126,820.0 | 96,200.0 | 0.0957 | 11.767 |
| TS | 1 | Per hosted session | 162,576.0 | 126,820.0 | 96,200.0 | 0.0957 | 11.767 |
| TS | 5 | Wrapper | 94,884.0 | 66,890.0 | 61,624.0 | 0.0703 | 7.500 |
| TS | 5 | MCP helpers, sum | 350,972.0 | 209,523.0 | 182,856.0 | 0.2178 | 26.217 |
| TS | 5 | Wrapper + helpers | 445,856.0 | 276,413.0 | 244,480.0 | 0.2881 | 33.717 |
| TS | 5 | Per hosted session | 89,171.2 | 55,282.6 | 48,896.0 | 0.0576 | 6.743 |
| Rust | 1 | Wrapper | 10,208.0 | 6,173.0 | 4,148.0 | 0.0022 | 0.333 |
| Rust | 1 | MCP helpers, sum | 6,772.0 | 2,730.0 | 704.0 | 0.0000 | 0.000 |
| Rust | 1 | Wrapper + helpers | 16,980.0 | 8,903.0 | 4,852.0 | 0.0022 | 0.333 |
| Rust | 1 | Per hosted session | 16,980.0 | 8,903.0 | 4,852.0 | 0.0022 | 0.333 |
| Rust | 5 | Wrapper | 12,284.0 | 6,317.0 | 5,180.0 | 0.0079 | 1.067 |
| Rust | 5 | MCP helpers, sum | 35,080.0 | 7,755.0 | 3,388.0 | 0.0000 | 0.000 |
| Rust | 5 | Wrapper + helpers | 47,364.0 | 14,072.0 | 8,568.0 | 0.0079 | 1.067 |
| Rust | 5 | Per hosted session | 9,472.8 | 2,814.4 | 1,713.6 | 0.0016 | 0.213 |

The per-session row divides the wrapper and all its helpers by the number of hosted roots. Summed RSS counts shared pages in every mapping. PSS apportions shared pages. The two cohort sizes used distinct, byte-identical executable copies on separate inodes so they could run concurrently without sharing their executable text across cohorts. System libraries could still be shared.

| Runtime | Roots | Scripted engine RSS / PSS / USS (KiB) | Total tree PSS / root including engine (KiB) |
| --- | --- | --- | --- |
| TS | 1 | 33,508 / 12,844 / 6,700 | 139,664.0 |
| TS | 5 | 36,364 / 20,670 / 7,516 | 59,416.6 |
| Rust | 1 | 33,528 / 9,823 / 6,724 | 18,726.0 |
| Rust | 5 | 36,160 / 11,891 / 7,364 | 5,192.6 |

The hub, runner webhook receiver and sampler ran outside the wrapper tree. Each measured tree contained exactly one wrapper, one external scripted engine and one MCP helper per root. The harness checked wrapper and helper executable identities, initialized each helper, listed its tools and called `change_title` through the real HTTP bridge before returning the native thread. It checked the process census across each sample and observed a heartbeat from every root during sampling.

## Fixture verdicts

- Rust used 8,903 KiB of wrapper-plus-helper PSS for one root and 2,814.4 KiB per root at five. The corresponding compiled TS measurements were 126,820 and 55,282.6 KiB. The spec’s 15 MB PSS acceptance requirement names Claude and Agy.
- Spawn-to-first-webhook p50 was 146.4 ms for Rust and 854.6 ms for TS. Rust reduced median latency by 82.9% on this scripted startup path.
- One Rust root and its bridge used 0.0022% of one CPU and recorded 0.333 switches/s. Both inclusive measurements were within the 0.1% CPU and 1 wakeup/s budgets.
- The five-root Rust runtime used 0.0079% of one CPU and recorded 1.067 switches/s. CPU stayed within budget. The inclusive scheduling count exceeded 1/s, and the rate after excluding protocol traffic remains unresolved.

## Reproduction

Source revision `86bef73302fa`. Measurement started 2026-11-19T17:54:32.501046+00:00 on aedas-me, Linux 6.18.33.2-microsoft-standard-WSL2 (x86_64).

| Artifact | Version | Bytes | SHA-256 |
| --- | --- | --- | --- |
| TS | hapi version: 0.36.4 | 164,804,784 | `5d9a12b2c44190cd1e356012508a3220a1d288697caef7b161e04ce5570e4661` |
| Rust | 0.36.4 (rust 86bef73302fa) | 9,210,664 | `5fd6d7b6c10f6c27f8232cd0f34f6dba29ca9c65341772566ea0a88dc0cf5bb6` |

Run from the repository root with the frozen Bun dependencies, CLI build assets, a C compiler and access to guest `/proc` and perf software counters.

```sh
bun cli/scripts/build-executable.ts --target bun-linux-x64
HAPI_WRAPPER_GIT_SHA="$(git rev-parse --short=12 HEAD)" cargo build --release -p hapi-session-wrapper
python3 benchmarks/wrapper-compare/codex_measure.py \
    cli/dist-exe/bun-linux-x64/hapi target/release/hapi-session-wrapper \
    /tmp/wave-1b.samples.json --revision "$(git rev-parse --short=12 HEAD)"
```

Per-process counters, executable hashes and all latency observations are in the [raw samples](wave-1b.samples.json).

## Not measured

- Real Codex startup, inference or external MCP servers. Task 2.22 remains open.
- india-vps, production hub or runner load, non-loopback networking and TLS. The empirical verdicts apply to the aedas-me fixture.
- Session counts other than one and five, long histories, active-turn CPU, peak memory and multi-day residency.

# Partial aedas-me ledger · Phase 0 final review

Executed in WSL on **aedas-me** (HKGI0211551), Intel(R) Core(TM) Ultra 9 285K, 24 logical CPUs, 33,363,247,104 bytes RAM, 6.18.33.2-microsoft-standard-WSL2. Source checkout HEAD supplied by the caller: `cdd4184df0623c43eeb394484b3657e705cd0f99`. Each section identifies its benchmark source hash. Absolute thresholds below apply provisionally to this host. Confirmation on india-vps is outstanding for every gate.

One remote payload generates the [measurement JSON](../../../../benchmarks/e2e-baseline/results/baseline-cdd4184df0623c43eeb394484b3657e705cd0f99-HKGI0211551.json), [combined JSONL](baseline-cdd4184df0623c43eeb394484b3657e705cd0f99-HKGI0211551.jsonl), and Markdown. Timestamped raw ledgers and compressed traces/profiles are in `benchmarks/e2e-baseline/results/`. The combined archive preserves earlier review rounds; only round-2 attempts supply current measurements.

```bash
/root/hapi-ci/aedas-me-verify.sh /root/hapi-e2e hapi-e2e "BASELINE_COMMIT=$(git rev-parse HEAD) bun run bench:e2e-baseline"
```

## Provenance and coverage

Report/attribution source digest: `28480ce8f962d2bdc88e9eb6b23eddedc9d271eb248581e5155fb6c1d7c3bb01`. Measurement hashes below cover source files in both benchmark directories, excluding results. Earlier unversioned round-1 rows are archived, never selected as current evidence.

| Remote raw file | Harness SHA-256 | Playwright / Chromium / Bun | Sections (exit code) |
| --- | --- | --- | --- |
| baseline-cdd4184df0623c43eeb394484b3657e705cd0f99-HKGI0211551-20261119T102150Z-136967.jsonl | b7b957801a35471cfe4e6906d3c9e82a1176b9d1ef4cc3191488b7eb4be23ef2 | 1.61.0 / Google Chrome for Testing 149.0.7827.55 / 1.4.4 | navigation (0), mount (0), scroll (1) |
| baseline-cdd4184df0623c43eeb394484b3657e705cd0f99-HKGI0211551-20261119T103029Z-146496.jsonl | d1493bea67eefb44dc5fd834afc1d0e4f09d492fb95e61794d83974f7d7def78 | 1.61.0 / Google Chrome for Testing 149.0.7827.55 / 1.4.4 | fixture (0), edits (0), flood-1 (0), flood-2 (0), flood-3 (0), hub (0), hook (0), diff-1 (0), diff-2 (0), diff-3 (0), diff-4 (0), stream (0), navigation (0), scroll (1) |
| baseline-cdd4184df0623c43eeb394484b3657e705cd0f99-HKGI0211551-20261119T105601Z-201003.jsonl | 5fb2ab61c414400eaf2a16284eb63d22bafb27fa2ddd95343d2eca374553ae14 | 1.61.0 / Google Chrome for Testing 149.0.7827.55 / 1.4.4 | stream:observer (0), navigation (0) |

| Requirement | Coverage |
| --- | --- |
| Three checkouts on india-vps | Not measured; one source tree on aedas-me |
| 500-message LCP, matched completed window | 1× Measured, 4× Not measurable here |
| 2,000-message scroll smoothness | Not measurable here · frame production or wheel cadence stalled |
| 100 cumulative updates; frame work and long tasks | 14 accepted captures; invalid window/paint measurements are flagged below |
| Cold 200-row parse and normalize, two seeds | Measured |
| Hub page CPU, TTFB; SSE full/projected bytes; gzip load | Measured |
| Terminal append and hub lag | Measured; probe validity reported per trial |
| Diff mount, Worker, hook cold start | Measured |
| 10-session PSS including children; prompt → first model token | Not measured; no live model or ten-agent stack |
| Rust database safety | No Rust component accesses the disposable TS database |

## Browser end-to-end

The final LCP probe warms a same-origin blank page, then opens the authentic cold route in a fresh browser context for each navigation. Six consecutive animation callbacks at exactly 51 mounted rows verify the common end state. Samples at other counts or without a verified paint are discarded. At least 20 accepted opens per CPU rate are required.

| CPU × | Accepted | Discarded | LCP median ms | Range ms |
| --- | --- | --- | --- | --- |
| 1 | 20 | 0 | 320 | 300–332 |
| 4 | 0 | 4 | not measurable here | — |

The hydration stress probe forces a fresh 200-row latest response and verifies every ID reached the real normalizer. This differs from normal navigation's initial 20-row tail and subsequent context loading. The heavy-tail model alternates Read/Bash tool records with 5/16/32/50 Ki-character outputs plus the existing truncation cases. These synthetic distributions have no production-frequency calibration.

| Seed | CPU × | Cold N | UTF-8 bytes / response row | Parse + normalize p50/p95/p99 ms |
| --- | --- | --- | --- | --- |
| standard | 1 | 20 | 2,080.47 | 3/4.1/4.3 |
| heavy | 1 | 20 | 4,706.68 | 3.7/5.5/5.8 |
| standard | 4 | 20 | 2,080.47 | 10.8/13.5/13.8 |
| heavy | 4 | 20 | 4,706.68 | 13.5/17.8/18.4 |

**Scroll smoothness: not measurable here.** Acceptance requires a warmed compositor, ≥30 observed fps, adequate travel, an older-page response, and elapsed time within 0.8–1.5× the wheel schedule including settlement. Rejected trace counts and timings are diagnostics only.

### Streaming and attribution

Each comparison starts in a pristine seeded session. Baseline and controls require 51 mounted rows immediately before delivery. The size sweep uses homogeneous alternating user/assistant records. Reasoning snapshots render in the thinking card; the assistant case sends message + streamSnapshot through the assistant-text renderer. All use 100 cumulative socket updates at 250 ms spacing. Idle uses a 25-second window. Default-flags omits both software compositor overrides.

| Case | KiB | Mounted before → after | Observed frames | fps | Main busy % | Inter-delivery busy p95 ms | Peak occupied work / update p50/p95/p99 ms | Tasks >50 ms |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| baseline | 6 | 51 → 51 | 1183 | 46.7 | 44.5 | 147.5 | 78.61/122.01/135.32 | 100 |
| baseline | 30 | 51 → 51 | 1159 | 45.72 | 43.75 | 129.97 | 85.6/107.2/111.81 | 100 |
| idle | 0 | 51 → 51 | 1500 | 60 | 5.37 | 15.87 | 1.27/1.64/2.09 | 0 |
| clean-tail | 6 | 51 → 51 | 1158 | 45.65 | 43.62 | 149.34 | 79.72/116.58/177.76 | 100 |
| clean-tail | 30 | 51 → 51 | 1124 | 44.29 | 43.74 | 142.66 | 92.84/109.76/120.44 | 100 |
| default-flags | 6 | 51 → 51 | 1207 | 47.62 | 42.2 | 131.01 | 75/98.29/102.52 | 100 |
| default-flags | 30 | 51 → 51 | 536 | 21.01 | 82.94 | 266.26 | 198.63/263.71/277.8 | 101 |
| cpu4 | 6 | 51 → 51 | 68 | 2.62 | 99.35 | 475.47 | not measurable here | 61 |
| cpu4 | 30 | 51 → 51 | 72 | 2.79 | 98.31 | 564.16 | not measurable here | 53 |
| assistant | 6 | 51 → 51 | 796 | 31.31 | 81.77 | 269.72 | 92.44/145.17/150.67 | 97 |
| assistant | 30 | 51 → 51 | 559 | 22.02 | 84.48 | 257.21 | 111.37/141.41/185.52 | 90 |
| rows5 | 6 | 5 → 5 | 1505 | 59.45 | 14.58 | 55.13 | 15.34/22.03/67.8 | 3 |
| rows51 | 6 | 51 → 51 | 1361 | 53.7 | 31.84 | 122.58 | 48.28/78.97/104.18 | 31 |
| rows100 | 6 | 100 → 100 | 1096 | 43.22 | 46.1 | 149.95 | 94.58/115.62/124.47 | 103 |

Main-thread work sums occupied RunTask spans between animation callbacks within each SSE update window. At least 80% of windows must contain a frame boundary. A stalled headless compositor invalidates frame timing. The earlier unmatched 4× run had five mounted rows against 51; its apparent acceleration does not estimate throttling.

Idle control: longest traced main-thread task 2.34 ms and median observed frame gap 16.67 ms.

With identical 6 KiB reasoning input and the homogeneous fixture, delivery-window busy p95 was 55.13 ms at 5 rows, 122.58 ms at 51 rows, 149.95 ms at 100 rows. Compare row counts within this sweep; the mixed 51-row seed has a different component mix.

| Path | KiB | Updates with exactly one long task | Median onset after SSE ms | Median long task ms | Traced long-task ms | Sampled long-task ms | Render ms/update | Commit | Style/layout | Markdown | Other |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| baseline | 6 | 100/100 | 7.29 | 75.88 | 7,915.34 | 7,995.17 | 44.13 | 15.23 | 11.99 | 1.89 | 6.72 |
| baseline | 30 | 100/100 | 6.06 | 83.08 | 8,602.65 | 8,696.29 | 43.11 | 14.06 | 13.88 | 8.86 | 7.06 |
| assistant | 6 | 86/100 | 0.53 | 69.89 | 6,874.3 | 6,874.82 | 39.82 | 14.2 | 9.44 | 1.66 | 3.63 |
| assistant | 30 | 78/100 | 1.52 | 86.9 | 7,985.19 | 7,995.74 | 39.93 | 14.39 | 11.57 | 8.37 | 5.7 |

| Path | KiB | Sync React root ms/update | Render stack | Commit stack | useSizeHandle/clientHeight | Timestamp formatting | setItem in long tasks | setItem in tasks >5 ms | Markdown % of sampled occupied time | Markdown ms/update p95, all occupied tasks |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| baseline | 6 | 73.24 | 46.02 | 27.22 | 12.16 | 13.63 | 0.14 | 4.97 | 1.46 | 3.28 |
| baseline | 30 | 79.89 | 51.97 | 27.94 | 14.08 | 12.59 | 0.13 | 4.72 | 6.96 | 19.77 |
| assistant | 6 | 65.12 | 41.48 | 23.64 | 9.59 | 11.77 | 0.05 | 4.9 | 0.84 | 3.3 |
| assistant | 30 | 74.27 | 48.28 | 25.96 | 11.75 | 11.34 | 0.07 | 7.62 | 3.94 | 26.05 |

The first table uses exclusive buckets; the second uses inclusive stacks and can overlap render, commit and forced layout. Render/commit attribution uses samples in traced long tasks. The storage comparison also includes tasks above 5 ms; the parser-only estimate includes all occupied tasks. Background (program) samples outside those tasks are excluded. Sync-root samples contain processRootScheduleInMicrotask or performSyncWorkOnRoot. The phase stacks locate a synchronous React pass through the mounted thread after delivery. Sampling cannot count invocations of every row. Markdown-only improvements cannot remove other rendering, storage, and layout costs.

For the 6 / 30 KiB reasoning turns, 100 / 100 of 100 updates had exactly one long task. Median task duration was 75.88 / 83.08 ms. Compare these with the observer-only durations below. The next TS experiment should bound the rows processed by this pass and measure the resulting timestamp formatting and synchronous viewport geometry reads. A parser change cannot eliminate either cost.

**baseline 6 KiB** · highest inclusive frames inside long tasks: `react-dom/cjs/react-dom-client.production.js ` 73.25 ms/update, `react-dom/cjs/react-dom-client.production.js performSyncWorkOnRoot` 73.24 ms/update, `react-dom/cjs/react-dom-client.production.js processRootScheduleInMicrotask` 73.23 ms/update, `react-dom/cjs/react-dom-client.production.js performWorkOnRoot` 73.23 ms/update, `react-dom/cjs/react-dom-client.production.js flushSyncWorkAcrossRoots_impl` 73.22 ms/update.

**baseline 30 KiB** · highest inclusive frames inside long tasks: `react-dom/cjs/react-dom-client.production.js performSyncWorkOnRoot` 79.89 ms/update, `react-dom/cjs/react-dom-client.production.js ` 79.88 ms/update, `react-dom/cjs/react-dom-client.production.js flushSyncWorkAcrossRoots_impl` 79.87 ms/update, `react-dom/cjs/react-dom-client.production.js processRootScheduleInMicrotask` 79.87 ms/update, `react-dom/cjs/react-dom-client.production.js performWorkOnRoot` 79.86 ms/update.

**assistant 6 KiB** · highest inclusive frames inside long tasks: `react-dom/cjs/react-dom-client.production.js ` 65.12 ms/update, `react-dom/cjs/react-dom-client.production.js processRootScheduleInMicrotask` 65.12 ms/update, `react-dom/cjs/react-dom-client.production.js performSyncWorkOnRoot` 65.12 ms/update, `react-dom/cjs/react-dom-client.production.js flushSyncWorkAcrossRoots_impl` 65.1 ms/update, `react-dom/cjs/react-dom-client.production.js performWorkOnRoot` 65.1 ms/update.

**assistant 30 KiB** · highest inclusive frames inside long tasks: `react-dom/cjs/react-dom-client.production.js processRootScheduleInMicrotask` 74.25 ms/update, `react-dom/cjs/react-dom-client.production.js performSyncWorkOnRoot` 74.25 ms/update, `react-dom/cjs/react-dom-client.production.js performWorkOnRoot` 74.25 ms/update, `react-dom/cjs/react-dom-client.production.js ` 74.25 ms/update, `react-dom/cjs/react-dom-client.production.js flushSyncWorkAcrossRoots_impl` 74.24 ms/update.

| Observer-only reasoning | Mounted | Count >50 ms | Median duration ms | p95 ms |
| --- | --- | --- | --- | --- |
| 6 | 51 | 97 | 70 | 91 |
| 30 | 51 | 100 | 93 | 130 |

6 KiB traced minus observer-only median long-task duration: 5.88 ms (100 vs 97 tasks).

30 KiB traced minus observer-only median long-task duration: -9.92 ms (100 vs 100 tasks).

The observer-only control runs the same replay without CDP tracing, profiling or per-update performance marks. Compare its duration and count to the traced reasoning case. This is an empirical overhead comparison on separate turns.

## Hub end-to-end

BASELINE_CLAUDE_JSONL replayed 2 recorded visible Claude messages (36 text bytes) through the CLI handler and SSE route: 1041 frame bytes, 498 gzip bytes. The default source is the repository's recorded Claude JSONL fixture. It contains settled messages and cannot establish a 6/30 KiB cumulative-turn ratio.

The long-turn replay uses the checked-in prose/code corpus unless an authorized transcript supplies ≥30 KiB. Its 100 slices have synthetic boundaries. Actual SSE snapshots and projected deltas pass through the same production gzip transformer with per-frame flushing. Projected deltas have no implemented hub CPU path.

| KiB | Full raw/gzip B | Projected delta + full settle raw/gzip B | Projected delta-only raw/gzip B | Full-settle / delta-only savings |
| --- | --- | --- | --- | --- |
| 6 | 357913/16205 | 32374/6626 | 25974/6217 | 2.45× / 2.61× |
| 30 | 1625646/47243 | 82514/29155 | 51254/18154 | 1.62× / 2.6× |

| Seed | Rows stored/returned | Encoding | TTFB p95 ms | Page p95 ms | Encoded bytes | Hub CPU ms/page |
| --- | --- | --- | --- | --- | --- | --- |
| standard | 500/200 | identity | 5.49 | 5.62 | 416093 | 4.33 |
| standard | 2000/200 | identity | 10.03 | 10.66 | 354310 | 5.57 |
| heavy | 500/200 | identity | 8.38 | 8.81 | 941336 | 6.87 |
| standard | 500/200 | gzip | 6.12 | 9.18 | 108158 | 7.83 |
| standard | 2000/200 | gzip | 5.41 | 7.93 | 94498 | 6.77 |
| heavy | 500/200 | gzip | 8.46 | 15.05 | 229974 | 12.97 |

| Encoding | Decode | Parse | Stringify | Compression stack | Other route |
| --- | --- | --- | --- | --- | --- |
| identity | 1539 (44.33%) | 450 (12.96%) | 727 (20.94%) | 0 (0%) | 756 (21.77%) |
| gzip | 1512 (44.02%) | 437 (12.72%) | 697 (20.29%) | 26 (0.76%) | 763 (22.21%) |

Page latency ends after reading the encoded body. The client then decompresses and validates it. CPU buckets count samples with identifiable page-handler or compression stacks. Async native compressor work can lack a JS stack; a zero compression bucket cannot establish zero cost.

| 50 readers, ten streams | Deliveries | Emit → reader p99 ms | Hub CPU, % one core |
| --- | --- | --- | --- |
| identity | 5000 | 7.38 | 8.59 |
| gzip | 5000 | 9.22 | 11.95 |

Gzip subscribers negotiate compression and decompress each continuous stream in the reader process. The 83 ms producer interval is a synthetic 3× ACP rate.

## Isolated measurements

Terminal trials send 3,200 independent 64 KiB CLI terminal:output events spaced by at least 8 ms (target ceiling 7.81 MiB/s). A real authenticated terminal verifies all 200 MiB. The disposable hub times the production append call or a matched no-op at the same handler location. A worker independently pings the hub main loop every 5 ms and times the round trip. The in-hub monitorEventLoopDelay histogram and worker are checked against a deliberate 45 ms stall before each process's trials; an unresponsive probe is reported as not measurable here.

| Run | Mode | MiB/s | Append calls | Call p95/p99/max ms | Hub ticks | Hub lag p99 ms | Worker ticks | Worker RTT p99 ms |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 1 | idle | 0 | 0 | — | 1384 | 0.22 | 295 | 0.21 |
| 1 | append | 7.54 | 3200 | 0.3/0.53/2.69 | 24191 | 0.54 | 5207 | 0.63 |
| 1 | noop | 7.57 | 3200 | 0/0/0.01 | 24279 | 0.31 | 5194 | 0.34 |
| 2 | idle | 0 | 0 | — | 1385 | 0.2 | 295 | 0.44 |
| 2 | noop | 7.51 | 3200 | 0/0/0.07 | 24288 | 0.47 | 5224 | 0.56 |
| 2 | append | 7.54 | 3200 | 0.22/0.42/0.8 | 24135 | 0.53 | 5205 | 0.59 |
| 3 | idle | 0 | 0 | — | 1393 | 0.15 | 295 | 0.33 |
| 3 | append | 7.53 | 3200 | 0.3/0.53/1.72 | 24285 | 0.56 | 5217 | 0.57 |
| 3 | noop | 7.54 | 3200 | 0/0/0.02 | 24330 | 0.34 | 5207 | 0.41 |

| Real DiffView CPU × | Fresh N | Mount median ms | Mount range | Layout median ms | Layout range | Ideal Worker upper bound % | DOM nodes |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 1 | 10 | 98.55 | 95.7–202.5 | 99.06 | 92.74–179.28 | 29.83 | 16,520 |
| 4 | 10 | 417.9 | 408.1–440 | 433.55 | 424.77–448.58 | 29.88 | 16,520 |

The upper bound divides both diffLines calls by synchronous mount plus forced layout. It assumes zero transfer or scheduling cost. One of those calls duplicates the other.

| Worker workload | Process | Lines | Replacement spacing | Preflight / trial guest idle % | p95 compute ms | Quiet |
| --- | --- | --- | --- | --- | --- | --- |
| distributed | 1 | 2000 | 10 | 99.75/98.22 | 6.6 | True |
| high-edit | 1 | 2000 | 2 | 99.75/98.22 | 128.4 | True |
| large | 1 | 10000 | 13 | 99.75/98.22 | 70.8 | True |
| distributed | 2 | 2000 | 10 | 99.75/95.14 | 5.8 | True |
| high-edit | 2 | 2000 | 2 | 99.75/95.14 | 106.5 | True |
| large | 2 | 10000 | 13 | 99.75/95.14 | 68 | True |
| distributed | 3 | 2000 | 10 | 93.48/89.47 | 5.3 | False |
| high-edit | 3 | 2000 | 2 | 93.48/89.47 | 121.2 | False |
| large | 3 | 10000 | 13 | 93.48/89.46 | 75.9 | False |
| distributed | 4 | 2000 | 10 | 94/94.99 | 5.6 | True |
| high-edit | 4 | 2000 | 2 | 94/94.99 | 108.6 | True |
| large | 4 | 10000 | 13 | 94/94.98 | 67.9 | True |

Four fresh sequential Worker processes include a repeat of the initial process. All are archived. Only ≥90% guest idle before and during the trial qualifies for the per-class verdict. No passing run is removed for having an inconvenient timing.

| Recorded edit source | Old → new bytes | Old → new lines |
| --- | --- | --- |
| gemini-3-flash-preview-edit-file.json | 40 → 33 | 2 → 2 |
| gemini-3.1-pro-preview-edit-file.json | 17 → 20 | 1 → 1 |

These captured Edit examples bound the observed sample only; they cannot estimate production workload prevalence.

| Hook command | Fresh processes | p50/p95/p99 ms |
| --- | --- | --- |
| ts | 200 | 61.49/64.7/66.86 |
| shell | 200 | 3.31/3.83/4.29 |

## Decisions

| Gate (aedas-me only) | Disposition | Evidence and criterion |
| --- | --- | --- |
| D-A | Undecided | Projected full-settle/delta-only savings: 6 KiB 2.45×/2.61×; 30 KiB 1.62×/2.6×. Kill requires <2× AND <30% hub CPU reduction. A ≥2× projection does not meet the byte conjunct; a <2× result needs the missing matched CPU measurement. The short real capture cannot resolve the long-turn gate. |
| D-B | NO-GO | Gzip page p95 7.93–15.05 ms. Kill if p95 <50 ms OR parse + stringify <30%. Host-local disposition, pending india-vps confirmation. |
| D-C | Undecided | Kill if buffer-attributable p99 event-loop delay <20 ms. The earlier pipe-starvation result is withdrawn. Paced socket trials isolate direct append time and compare main-loop probes; three synthetic trials at one offered rate cannot establish production peak attribution. Maximum individual append 2.69 ms. Append-minus-noop hub p99 differences in ms: 0.22, 0.06, 0.22. |
| D-D layout | GO | Target zero >50 ms main-thread tasks. Remove duplicate diffLines and slice/collapse DOM first. Both computations combined account for at most a median 29.83% of blocking at 1×. |
| D-D WASM distributed | NO-GO | Quiet Worker p95 5.6–6.6 ms (3 runs). JS p95 <12 ms kills a WASM comparison for this class. GO qualifies a comparison only. |
| D-D WASM high-edit | GO | Quiet Worker p95 106.5–128.4 ms (3 runs). JS p95 <12 ms kills a WASM comparison for this class. GO qualifies a comparison only. |
| D-D WASM large | GO | Quiet Worker p95 67.9–70.8 ms (3 runs). JS p95 <12 ms kills a WASM comparison for this class. GO qualifies a comparison only. |
| D-E | Undecided | The reasoning-card premise for Markdown WASM is contradicted: the earlier profile put Markdown at only about 2–8% of busy work, even assuming a free parser. Round-2 attribution and the assistant-text path are above. Require post-memo Markdown-attributed p95 >12 ms/update for a parser comparison; total frame time alone cannot qualify WASM. No post-memo path exists. |
| D-F | NO-GO | 200-row cold hydration 4× CPU p95 (standard / heavy): 13.5 / 17.8 ms. Kill below 25 ms; limited to these modeled response sizes. |
| D-G | Undecided | Shell p50/p95 3.31/3.83 ms. Kill at ≤10 ms; the design does not specify the statistic. Source TS command, no shipped binary or independent resident-alternative comparison. |

G2: NO-GO (provisional 不实施 on synthetic 3×). Production peak and server-only broadcast p99 are uncalibrated. Gzip delivery p99 9.22 ms and CPU 11.95% versus 20 ms / 25% of one core. G1/G3 remain undecided without ten-session PSS. The report assigns no Rust savings to agent memory/startup, model first token, RTT or DOM layout.

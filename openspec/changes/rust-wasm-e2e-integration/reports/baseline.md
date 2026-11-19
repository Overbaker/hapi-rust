# Provisional Phase 0 ledger · aedas-me

Executed in WSL on **aedas-me** (HKGI0211551), Intel(R) Core(TM) Ultra 9 285K, 24 logical CPUs, 33,363,247,104 bytes RAM, 6.18.33.2-microsoft-standard-WSL2. Measured Git commit (remote source digest verified against its Git blobs): `41b1fb3d44ab6e6173013d8243b8dd3951604d39`. Each section identifies its benchmark source hash. Absolute thresholds below apply provisionally to this host. Confirmation on india-vps is outstanding for every gate.

The full-suite invocation runs each acquisition section and generates the [measurement JSON](../../../../benchmarks/e2e-baseline/results/baseline-41b1fb3d44ab6e6173013d8243b8dd3951604d39-HKGI0211551.json), [combined JSONL](baseline-41b1fb3d44ab6e6173013d8243b8dd3951604d39-HKGI0211551.jsonl), and Markdown. Timestamped raw ledgers and compressed traces/profiles are in `benchmarks/e2e-baseline/results/`. Only results for this Git commit and its single verified harness digest supply these measurements.

```bash
revision=$(git rev-parse HEAD)
digest=$(python3 benchmarks/e2e-baseline/fingerprint.py "$revision")
/root/hapi-ci/aedas-me-verify.sh /root/hapi-e2e hapi-e2e "BASELINE_COMMIT=$revision BASELINE_EXPECTED_SHA256=$digest bun run bench:e2e-baseline"
```

## Provenance and coverage

Report/attribution source digest: `131d49233e92ae4f5578a5701f9be95ce71c06ebb3be5085811918ff48dd510a`. Measurement hashes below cover source files in both benchmark directories, excluding results. The launcher checks this digest before acquisition; history from other commits is excluded.

| Remote raw file | Harness SHA-256 | Playwright / Chromium / Bun | Sections (exit code) |
| --- | --- | --- | --- |
| baseline-41b1fb3d44ab6e6173013d8243b8dd3951604d39-HKGI0211551-20261119T145907Z-6274.jsonl | 1a0651e1ee68f7b783cf7bdd998848bde0fcd75a45f01f1c5f29bb44345d8bea | 1.61.0 / Google Chrome for Testing 149.0.7827.55 / 1.4.4 | fixture (0), navigation (0), scroll (0), mount (0), stream-1 (0), stream-2 (0), stream-3 (0), stream-4 (0), stream-5 (0), flood-1 (0), flood-2 (0), flood-3 (0), hub (0), hook (0), edits (0), diff-1 (0), diff-2 (0), diff-3 (0), diff-4 (0) |

| Requirement | Coverage |
| --- | --- |
| Three checkouts on india-vps | Not measured; one source tree on aedas-me |
| 500-message LCP, matched completed window | 1× Measured, 4× Not measurable here |
| 2,000-message scroll smoothness | Measured |
| 100 cumulative updates; frame work and long tasks | 70 accepted captures; invalid window/paint measurements are flagged below |
| Cold 200-row parse and normalize, two seeds | Measured |
| Hub page CPU, TTFB; SSE full/projected bytes; gzip load | Measured |
| Terminal append and hub lag | Measured; probe validity reported per trial |
| Diff mount, Worker, hook cold start | Measured |
| 10-session PSS including children; prompt → first model token | Not measured; no live model or ten-agent stack |
| Rust database safety | No Rust component accesses the disposable TS database |

## Browser end-to-end

The final LCP probe warms a same-origin blank page, then opens the authentic cold route in a fresh browser context for each navigation. Six consecutive animation callbacks at exactly 51 mounted rows verify the common end state. Samples at other counts or without a verified paint are discarded. At least 20 accepted opens per CPU rate are required.

| CPU × | Accepted | Discarded | LCP element | Area px² | Rows at observer delivery | LCP median ms | Range ms |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 1 | 20 | 0 | P in thread | 189,176–189,176 | 6–51 | 300 | 300–336 |
| 4 | 0 | 4 | — | — | — | not measurable here | — |

The hydration stress probe forces a fresh 200-row latest response and verifies every ID reached the real normalizer. This differs from normal navigation's initial 20-row tail and subsequent context loading. The heavy-tail model alternates Read/Bash tool records with 5/16/32/50 Ki-character outputs plus the existing truncation cases. These synthetic distributions have no production-frequency calibration.

| Seed | CPU × | Cold N | UTF-8 bytes / response row | Parse + normalize p50/p95/p99 ms |
| --- | --- | --- | --- | --- |
| standard | 1 | 20 | 2,080.47 | 2.6/3.9/4 |
| heavy | 1 | 20 | 4,706.68 | 3.5/4.2/4.2 |
| standard | 4 | 20 | 2,080.47 | 9.6/13.2/13.4 |
| heavy | 4 | 20 | 4,706.68 | 13.5/17.5/17.6 |

Validated scroll: 34 wheel iterations, 2,001.61 ms elapsed vs 1,350 ms scheduled, 50.46 observed fps, 78,579 px travelled, 1 older responses. Main-thread frame work p95 18.45 ms; 1 long tasks and 19 compositor drop events.

### Streaming and attribution

Each comparison starts in a pristine seeded session. Baseline and controls require 51 mounted rows immediately before delivery. The size sweep uses homogeneous alternating user/assistant records. Reasoning snapshots render in the thinking card; the assistant case sends message + streamSnapshot through the assistant-text renderer. All use 100 cumulative socket updates at 250 ms spacing. Idle uses a 25-second window. Default-flags omits both software compositor overrides.

Each table shows the median and min–max across independent turns. Percentiles are computed within each turn first. Five processes rotate size order; each turn uses a fresh context and its own seeded session. Frame summaries exclude captures without adequate frame coverage.

| Case | KiB | Turns | Mounted | fps | Busy % | Inter-update busy p50 ms | Busy p95 ms | Valid frame N | Peak frame work p95 ms | Long tasks |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| baseline | 6 | 5 | 51 | 48.92 (46.77–49.66) | 40.17 (39.61–43.74) | 103.04 (98.6–113.84) | 133.67 (126.36–146.04) | 5 | 91.68 (82.42–106.06) | 99 (97–100) |
| baseline | 30 | 5 | 51 | 46.55 (44.64–46.82) | 41.6 (40.2–45.23) | 105.86 (100.83–115.13) | 127.53 (124.93–144.64) | 5 | 97.94 (94.18–110.29) | 100 (100–100) |
| idle | 0 | 5 | 51 | 59.99 (59.99–60.03) | 4.86 (4.7–5.17) | 11.81 (11.6–12.41) | 14.66 (13.74–15.53) | 5 | 1.49 (1.46–1.62) | 0 (0–0) |
| clean-tail | 6 | 5 | 51 | 48.64 (48.16–49.34) | 39.35 (38.38–39.91) | 101.46 (99.49–104.48) | 128.66 (119.33–129.05) | 5 | 92.13 (81.19–95.71) | 100 (100–100) |
| clean-tail | 30 | 5 | 51 | 46.57 (44.45–46.83) | 39.84 (39.55–43.43) | 99.74 (99.66–110.26) | 124.25 (122.84–129.16) | 5 | 99.92 (96.59–108) | 100 (100–100) |
| default-flags | 6 | 5 | 51 | 49.12 (47.09–49.63) | 39.97 (39.54–43.35) | 102.48 (101.33–111) | 127.31 (126.56–149.55) | 5 | 85.53 (83.45–99.67) | 100 (98–100) |
| default-flags | 30 | 5 | 51 | 46.41 (44.06–46.76) | 41.52 (40.51–45.21) | 102.98 (101.7–111.67) | 128.63 (123.86–154.06) | 5 | 100.54 (95.17–132.01) | 100 (100–100) |
| cpu4 | 6 | 5 | 51 | 2.92 (2.85–3.04) | 99.33 (99.26–99.37) | 359.28 (353.17–363.76) | 440.89 (419.42–447.02) | 0 | not measurable here | 67 (64–68) |
| cpu4 | 30 | 5 | 51 | 2.94 (2.92–2.98) | 98.31 (98.23–98.43) | 389.86 (384.85–403.22) | 508.89 (501.71–518.57) | 0 | not measurable here | 59 (57–60) |
| assistant | 6 | 5 | 51 | 37.92 (34.84–38.94) | 80.7 (79.7–81.16) | 204.66 (198.21–210.81) | 239.72 (237.19–246.71) | 5 | 128.09 (117.25–131.5) | 96 (89–99) |
| assistant | 30 | 5 | 51 | 33.45 (32.41–36.34) | 83.27 (81.24–83.66) | 213.34 (208.58–216.29) | 248.09 (239.61–258.44) | 5 | 140.83 (118.76–147.19) | 94 (93–99) |
| rows5 | 6 | 5 | 5 | 59.43 (59.41–59.52) | 14.08 (13.86–14.61) | 31.09 (30.5–33.24) | 56.76 (51.05–58.88) | 5 | 20.09 (19.69–23.79) | 3 (3–3) |
| rows51 | 6 | 5 | 51 | 55.03 (54.96–55.12) | 28.29 (27.84–28.59) | 68.31 (67.23–70.5) | 101.96 (93–103.73) | 5 | 57.41 (56.71–58.49) | 11 (7–12) |
| rows100 | 6 | 5 | 100 | 45.49 (45.19–46.99) | 43.71 (42.78–43.81) | 107.51 (103.61–109.4) | 138.29 (130.99–145.92) | 5 | 102.5 (94.93–107.17) | 103 (103–103) |

The earlier single default-flags 30 KiB capture was an unexplained outlier. Use the repeated flags and software ranges above for comparison. Work depends on mounted rows: a freshly opened 20-message tail with six mounted rows is closer to rows5 than to the mixed 51-row baseline; its exact component mix differs.

Main-thread work sums occupied RunTask spans between animation callbacks within each SSE window. A frame statistic requires ≥100 observed frames and a frame boundary in ≥80% of update windows. CPU sampling is on the renderer main thread.

| Path | KiB | Window | Turns | Render ms/update | Commit | Layout | Parser | Other |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| baseline | 6 | long tasks | 5 | 38.06 (36.82–43) | 11.59 (11.23–15.32) | 11 (9.93–11.76) | 1.75 (1.74–1.83) | 6.46 (5.88–6.85) |
| baseline | 6 | all occupied tasks | 5 | 38.88 (37.92–43.08) | 11.76 (11.34–15.34) | 17.25 (16.86–17.61) | 1.77 (1.77–1.83) | 50.26 (49.69–51.54) |
| baseline | 30 | long tasks | 5 | 39.75 (37.68–45.17) | 12.26 (11.48–15.08) | 13.24 (13.1–13.87) | 8.72 (8.56–8.89) | 7.02 (6.79–7.33) |
| baseline | 30 | all occupied tasks | 5 | 39.76 (37.69–45.17) | 12.27 (11.48–15.1) | 15.26 (15.1–16.01) | 8.72 (8.56–8.89) | 46.72 (45.64–46.99) |
| assistant | 6 | long tasks | 5 | 36.2 (34.22–38.38) | 11.09 (10.23–12.74) | 10.01 (9.16–10.47) | 1.79 (1.59–1.85) | 3.13 (2.67–4.36) |
| assistant | 6 | all occupied tasks | 5 | 104.41 (103.68–106.97) | 30.59 (29.36–33.53) | 15.06 (13.78–15.25) | 1.93 (1.73–1.99) | 58.22 (54.36–58.51) |
| assistant | 30 | long tasks | 5 | 36.94 (35.95–37.8) | 11.8 (11.13–12.61) | 11.52 (11.38–12.32) | 8.88 (7.88–9.45) | 4.76 (3.31–4.99) |
| assistant | 30 | all occupied tasks | 5 | 104.36 (101.66–105.97) | 31.9 (30.11–32.76) | 13.42 (12.8–13.61) | 9.06 (8.07–9.6) | 56.56 (55.49–58.07) |

These phase buckets are exclusive. The all-occupied rows include assistant rendering outside long tasks. Long-task sampling and trace durations can be compared in the JSON.

| Path | KiB | Updates with one long task | Onset ms | Sync root ms/update | Geometry | Timestamps | Parser % | Inclusive subtree % | Parser p95 ms/update | Subtree p95 ms/update |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| baseline | 6 | 99 (97–100) | 6.09 (4.68–6.42) | 61.96 (60.62–71.71) | 11.22 (10.15–11.95) | 11.91 (11.18–13.43) | 1.47 (1.41–1.51) | 10.41 (10.15–10.66) | 3.28 (3.28–3.29) | 25.93 (24.1–28.14) |
| baseline | 30 | 100 (100–100) | 6.36 (6.08–7.31) | 73.78 (71.09–83.03) | 13.47 (13.23–14.11) | 12.14 (11.26–12.8) | 7.11 (6.8–7.24) | 13.98 (13.2–14.2) | 20.82 (19.79–21.86) | 28.97 (28.3–32.58) |
| assistant | 6 | 89 (87–93) | 0.37 (0.35–0.41) | 58.69 (56.56–62.61) | 10.2 (9.33–10.65) | 10.9 (10.7–11.81) | 0.91 (0.83–0.94) | 10.62 (10.17–10.89) | 3.32 (3.29–3.38) | 38.32 (35.24–40) |
| assistant | 30 | 91 (84–94) | 0.56 (0.54–0.59) | 69.26 (66.55–72.17) | 11.67 (11.5–12.53) | 11.49 (11.12–11.54) | 4.18 (3.82–4.43) | 12.52 (11.96–13) | 25.32 (23.05–25.65) | 44.76 (43.71–47.08) |

Parser and subtree columns count all occupied samples with a matching ancestor. Subtree includes the parser, mapped React Markdown components, react-markdown, hast-to-JSX, and Shiki. Inclusive values overlap React phase costs. Recognizable stacks are a lower bound on Markdown-related work; the entire subtree is not necessarily removable by WASM. Idle and unattributed program samples outside occupied tasks are excluded.

| Observer-only reasoning KiB | Turns | Tasks >50 ms | Median duration ms | p95 ms |
| --- | --- | --- | --- | --- |
| 6 | 5 | 98 (97–100) | 66 (65–70) | 81 (75–92) |
| 30 | 5 | 100 (100–100) | 77 (76–80.5) | 96 (92–110) |

Observer-only turns use PerformanceObserver without tracing, CPU profiling or per-update marks. The comparison across runs bounds instrumentation sensitivity empirically.

## Hub end-to-end

BASELINE_CLAUDE_JSONL replayed 2 recorded visible Claude messages (36 text bytes) through the CLI handler and SSE route: 1041 frame bytes, 499 gzip bytes. The default source is the repository's recorded Claude JSONL fixture. It contains settled messages and cannot establish a 6/30 KiB cumulative-turn ratio.

The long-turn replay uses the checked-in prose/code corpus unless an authorized transcript supplies ≥30 KiB. Its 100 slices have synthetic boundaries. Actual SSE snapshots and projected deltas pass through the same production gzip transformer with per-frame flushing. Projected deltas have no implemented hub CPU path.

| KiB | Full raw/gzip B | Projected delta + full settle raw/gzip B | Projected delta-only raw/gzip B | Full-settle / delta-only savings |
| --- | --- | --- | --- | --- |
| 6 | 357913/15896 | 32374/6632 | 25974/6220 | 2.4× / 2.56× |
| 30 | 1625646/47093 | 82514/29153 | 51254/18153 | 1.62× / 2.59× |

| Seed | Rows stored/returned | Encoding | TTFB p95 ms | Page p95 ms | Encoded bytes | Hub CPU ms/page |
| --- | --- | --- | --- | --- | --- | --- |
| standard | 500/200 | identity | 4.9 | 5.03 | 416093 | 3.87 |
| standard | 2000/200 | identity | 4.36 | 4.49 | 354310 | 3.57 |
| heavy | 500/200 | identity | 7.35 | 7.72 | 941336 | 6.03 |
| standard | 500/200 | gzip | 5.35 | 8.42 | 108137 | 7.27 |
| standard | 2000/200 | gzip | 5.11 | 7.66 | 94511 | 6.5 |
| heavy | 500/200 | gzip | 8.05 | 14.2 | 229358 | 12.57 |

| Encoding | Decode | Parse | Stringify | Compression stack | Other route |
| --- | --- | --- | --- | --- | --- |
| identity | 1247 (43.66%) | 351 (12.29%) | 671 (23.49%) | 0 (0%) | 587 (20.55%) |
| gzip | 1442 (45.52%) | 405 (12.78%) | 648 (20.45%) | 22 (0.69%) | 651 (20.55%) |

Page latency ends after reading the encoded body. The client then decompresses and validates it. CPU buckets count samples with identifiable page-handler or compression stacks. Async native compressor work can lack a JS stack; a zero compression bucket cannot establish zero cost.

| 50 readers, ten streams | KiB | Deliveries | Emit → reader p99 ms | Hub CPU, % one core |
| --- | --- | --- | --- | --- |
| identity | 6 | 5000 | 7.01 | 8.01 |
| gzip | 6 | 5000 | 9.42 | 11.49 |
| identity | 30 | 5000 | 8.09 | 12.49 |
| gzip | 30 | 5000 | 15.02 | 18.29 |

Gzip subscribers negotiate compression and decompress each continuous stream in the reader process. The 83 ms producer interval is a synthetic 3× ACP rate.

## Isolated measurements

Terminal trials send 3,200 independent 64 KiB CLI terminal:output events spaced by at least 8 ms (target ceiling 7.81 MiB/s). A real authenticated terminal verifies all 200 MiB. The disposable hub times the production append call or a matched no-op at the same handler location. A worker independently pings the hub main loop every 5 ms and times the round trip. The in-hub monitorEventLoopDelay histogram and worker are checked against a deliberate 45 ms stall before each process's trials; an unresponsive probe is reported as not measurable here.

| Run | Mode | MiB/s | Append calls | Call p95/p99/max ms | Hub ticks | Hub lag p99 ms | Worker ticks | Worker RTT p99 ms |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 1 | idle | 0 | 0 | — | 1395 | 0.15 | 295 | 0.35 |
| 1 | append | 7.53 | 3200 | 0.25/0.53/1.71 | 24280 | 0.54 | 5221 | 0.48 |
| 1 | noop | 7.58 | 3200 | 0/0/0.02 | 24362 | 0.28 | 5193 | 0.33 |
| 2 | idle | 0 | 0 | — | 1387 | 0.24 | 294 | 0.3 |
| 2 | noop | 7.53 | 3200 | 0/0/0 | 24409 | 0.34 | 5219 | 0.4 |
| 2 | append | 7.56 | 3200 | 0.28/0.46/2.11 | 24176 | 0.47 | 5200 | 0.51 |
| 3 | idle | 0 | 0 | — | 1382 | 0.28 | 294 | 0.3 |
| 3 | append | 7.55 | 3200 | 0.32/0.46/1.49 | 24149 | 0.6 | 5207 | 0.58 |
| 3 | noop | 7.56 | 3200 | 0/0/0.01 | 24314 | 0.33 | 5202 | 0.41 |

| Real DiffView CPU × | Fresh N | Mount median ms | Mount range | Layout median ms | Layout range | Ideal Worker upper bound % | DOM nodes |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 1 | 10 | 99.8 | 93.1–103.3 | 96.96 | 92.1–100.18 | 30.64 | 16,520 |
| 4 | 10 | 409.75 | 404.6–426.8 | 422.06 | 396.38–429.62 | 30.51 | 16,520 |

The upper bound divides both diffLines calls by synchronous mount plus forced layout. It assumes zero transfer or scheduling cost. One of those calls duplicates the other.

| Worker workload | Process | Lines | Replacement spacing | Preflight / trial guest idle % | p95 compute ms | Quiet |
| --- | --- | --- | --- | --- | --- | --- |
| distributed | 1 | 2000 | 10 | 100/98.51 | 6.1 | True |
| high-edit | 1 | 2000 | 2 | 100/98.51 | 118.1 | True |
| large | 1 | 10000 | 13 | 100/98.51 | 68.2 | True |
| distributed | 2 | 2000 | 10 | 99.92/98.5 | 6.5 | True |
| high-edit | 2 | 2000 | 2 | 99.92/98.5 | 113.5 | True |
| large | 2 | 10000 | 13 | 99.92/98.5 | 69.8 | True |
| distributed | 3 | 2000 | 10 | 99.83/98.19 | 6.1 | True |
| high-edit | 3 | 2000 | 2 | 99.83/98.19 | 108 | True |
| large | 3 | 10000 | 13 | 99.83/98.19 | 69.5 | True |
| distributed | 4 | 2000 | 10 | 99.88/98.57 | 5.5 | True |
| high-edit | 4 | 2000 | 2 | 99.88/98.57 | 113.5 | True |
| large | 4 | 10000 | 13 | 99.88/98.57 | 67.5 | True |

Four fresh sequential Worker processes include a repeat of the initial process. All are archived. Only ≥90% guest idle before and during the trial qualifies for the per-class verdict. No passing run is removed for having an inconvenient timing.

| Recorded edit source | Old → new bytes | Old → new lines |
| --- | --- | --- |
| gemini-3-flash-preview-edit-file.json | 40 → 33 | 2 → 2 |
| gemini-3.1-pro-preview-edit-file.json | 17 → 20 | 1 → 1 |

These captured Edit examples bound the observed sample only; they cannot estimate production workload prevalence.

| Hook command | Fresh processes | p50/p95/p99 ms |
| --- | --- | --- |
| ts | 200 | 61.33/64.59/67.19 |
| shell | 200 | 3.19/3.68/4.58 |

## Decisions

| Gate (aedas-me only) | Disposition | Evidence and criterion |
| --- | --- | --- |
| D-A | Undecided | Projected full-settle/delta-only savings: 6 KiB 2.4×/2.56×; 30 KiB 1.62×/2.59×. Kill requires <2× AND <30% hub CPU reduction. A ≥2× projection does not meet the byte conjunct; a <2× result needs the missing matched CPU measurement. The short real capture cannot resolve the long-turn gate. |
| D-B | NO-GO | Sequential gzip page p95 7.66–14.2 ms (kill <50 ms: True); sampled parse + stringify share 33.24% (kill <30%: False). Either disjunct suffices. Host-local, pending india-vps confirmation. |
| D-C | Undecided | Kill if buffer-attributable p99 event-loop delay <20 ms. The earlier pipe-starvation result is withdrawn. Paced socket trials isolate direct append time and compare main-loop probes; three synthetic trials at one offered rate cannot establish production peak attribution. Maximum individual append 2.11 ms. Append-minus-noop hub p99 differences in ms: 0.26, 0.13, 0.27. |
| D-D layout | GO | Target zero >50 ms main-thread tasks. Remove duplicate diffLines and slice/collapse DOM first. Both computations combined account for at most a median 30.64% of blocking at 1×. |
| D-D WASM distributed | NO-GO | Quiet JS Worker p95 5.5–6.5 ms (4 runs); <12 ms kills WASM. These are synthetic inputs. Even when ≥12 ms, a comparison is conditional on observing representative ≥500-line edits with comparable edit distance in authorized sessions. The recorded examples do not establish prevalence. |
| D-D WASM high-edit | Undecided | Quiet JS Worker p95 108–118.1 ms (4 runs); <12 ms kills WASM. These are synthetic inputs. Even when ≥12 ms, a comparison is conditional on observing representative ≥500-line edits with comparable edit distance in authorized sessions. The recorded examples do not establish prevalence. |
| D-D WASM large | Undecided | Quiet JS Worker p95 67.5–69.8 ms (4 runs); <12 ms kills WASM. These are synthetic inputs. Even when ≥12 ms, a comparison is conditional on observing representative ≥500-line edits with comparable edit distance in authorized sessions. The recorded examples do not establish prevalence. |
| D-E | Undecided | baseline 6 KiB (N=5): median parser/subtree share 1.47%/10.41%; parser/subtree p95 3.28 (3.28–3.29) / 25.93 (24.1–28.14) ms; pre-memo subtree p95 exceeds 12 ms in every turn. baseline 30 KiB (N=5): median parser/subtree share 7.11%/13.98%; parser/subtree p95 20.82 (19.79–21.86) / 28.97 (28.3–32.58) ms; pre-memo subtree p95 exceeds 12 ms in every turn. assistant 6 KiB (N=5): median parser/subtree share 0.91%/10.62%; parser/subtree p95 3.32 (3.29–3.38) / 38.32 (35.24–40) ms; pre-memo subtree p95 exceeds 12 ms in every turn. assistant 30 KiB (N=5): median parser/subtree share 4.18%/12.52%; parser/subtree p95 25.32 (23.05–25.65) / 44.76 (43.71–47.08) ms; pre-memo subtree p95 exceeds 12 ms in every turn. Design kill: post-memo frame-work p95 ≤12 ms. Proposed amendment: also require post-memo Markdown-attributed p95 >12 ms/update before a WASM comparison. These are pre-memo observations. |
| D-F | NO-GO | 200-row cold hydration 4× CPU p95 (standard / heavy): 13.2 / 17.5 ms. Kill below 25 ms; limited to these modeled response sizes. |
| D-G | Undecided | Shell maximum 12.04 ms; 1/200 runs above 10 ms. A maximum ≤10 ms meets the design kill for a Rust hook on this host. TS uses the source command; the shipped binary and a distinct resident alternative were not benchmarked. |

G2: NO-GO (synthetic load only). Worst of 6/30 KiB, identity/gzip: delivery p99 15.02 ms and 18.29% of one core. Kill requires both p99 <20 ms and CPU <25%. Production peak and server-only broadcast p99 remain uncalibrated. G1/G3 remain undecided without ten-session PSS. Agent startup, model first token, network RTT and DOM layout are outside the claimed savings.

---
title: "Rust Session Wrapper: Architecture & Benchmarks"
description: "How HAPI-Rust cuts per-session AI coding agent wrapper memory from 83.7 MiB to 4.4 MiB PSS and cold-start latency from 280 ms to 11 ms while preserving 100% protocol compatibility."
---

# Rust Session Wrapper: Architecture & Benchmarks

**HAPI-Rust** ([Overbaker/hapi-rust](https://github.com/Overbaker/hapi-rust)) replaces HAPI's per-session Bun/TypeScript wrapper process with a native Rust binary (`hapi-session-wrapper`), reducing median wrapper memory from **83.7 MiB to 4.4 MiB PSS** (~19× lower) and cold startup from **238–290 ms to 8–14 ms** so a single host can keep 100+ Claude Code, Codex, Antigravity, and Pi sessions open.

## Why Rewrite Only the Session Wrapper?

In HAPI's architecture, the **hub** and **runner** are single shared processes, while the **session wrapper** scales linearly with $N$ active coding agents. On a 23-session Linux workstation running upstream TypeScript/Bun wrappers, the wrapper processes alone consumed **3.38 GB** (~147 MiB each) before counting the agents themselves.

Upstream [tiann/hapi](https://github.com/tiann/hapi) stays TypeScript-only for maintenance simplicity ([tiann/hapi#2534](https://github.com/tiann/hapi/issues/2534)). Rather than rewriting the entire hub or web client, HAPI-Rust swaps out only the $O(N)$ bottleneck — preserving full compatibility with HAPI's hub, web/PWA UI, SQLite schema, and Socket.IO/REST wire protocol.

## Measured Performance

![HAPI-Rust vs TypeScript/Bun wrapper memory and startup latency benchmarks](/images/benchmark-chart.svg)

### Production Linux Hosts (29 Live Sessions, 3 Hosts, 2026-11-21)

Measured as wrapper-process **PSS** (Proportional Set Size) only:

| Wrapper Runtime | Live Sessions | Median PSS | Range |
|---|---:|---:|---:|
| **Rust** (`claude`, `codex`, `agy`) | 10 | **4.4 MiB** | 2.2–6.5 MiB |
| **TypeScript / Bun** (`claude`, `codex`, `agy`, `pi`) | 19 | **83.7 MiB** | 45.8–242.0 MiB |

### Controlled 10-Session Comparison (Linux / WSL2)

Each cohort launches sequential cold sessions against a real hub and runner over HTTP or TLS, measuring per-wrapper means (**RSS / PSS / USS**) from `/proc/<pid>/smaps_rollup` and spawn-to-webhook startup latency (**p50 / p95**):

| Cohort | Sessions | Compiled TS (RSS / PSS / USS) | Rust (RSS / PSS / USS) | Startup p50/p95 (TS → Rust) |
|---|---:|---:|---:|---:|
| HTTP light, single | 1 | 98.6 / 95.9 / 95.7 MiB | **7.3 / 5.3 / 5.2 MiB** | — |
| HTTP light | 10 | 97.8 / 52.5 / 48.1 MiB | **7.2 / 1.38 / 0.89 MiB** | 238/246 ms → **8/11 ms** |
| HTTPS light | 10 | 98.9 / 52.5 / 47.9 MiB | **8.2 / 1.73 / 1.19 MiB** | 280/290 ms → **11/14 ms** |
| HTTPS loaded (30 turns, ~1 MB JSONL, 1 live PTY) | 10 | 101.7 / 55.1 / 50.5 MiB | **8.6 / 2.07 / 1.53 MiB** | 277/291 ms → **12/15 ms** |

Notice how cleanly shared pages scale in Rust: across 10 loaded sessions with 1 MB transcripts and live PTYs, per-wrapper PSS is just **2.07 MiB** (~26× lower than compiled TS's 55.1 MiB).

## Crate Architecture

![HAPI-Rust architecture diagram](/images/architecture.svg)

The Rust workspace consists of two focused crates:

1. **`crates/hapi-wrapper-core`**:
   - **Transport**: `tokio` + `hyper` + `tokio-tungstenite` + `rustls` (native system root certificates, no OpenSSL runtime dependency on Linux).
   - **Protocol**: Engine.IO / Socket.IO v4 client, REST & runner webhook client, versioned compare-and-swap (CAS) metadata & state sync, backfill, and `MessageQueue2`.
   - **Extensions**: Local MCP HTTP server, hook-forwarder subcommands, RPC handlers (files, git, uploads, discovery, models), and PTY terminal bridge.
   - **Process-tree lifecycle**: Linux child-subreaper cleanup (`nix`) and Windows Job Objects (`windows-sys`) so stopping a session reaps detached child trees cleanly.
2. **`crates/hapi-session-wrapper`**:
   - **Agent flavors**: `claude` (Claude Code), `codex` (OpenAI Codex, including Windows x64), `agy` (Antigravity), and `pi` (experimental).
   - **Exit-86 fallback contract**: If a mode or flavor is unsupported, the wrapper exits with code `86` and the runner retries transparently with the TypeScript wrapper.

## Parity & Safety Guarantees

- **Cross-language parity harness**: 18 recorded Claude Code scenarios verify **62/62** transcript assertions match between TypeScript and Rust (`bun run test:parity`).
- **Offline testability**: `cargo test --workspace` runs locally without requiring paid LLM API keys.
- **Runtime override**: Set `HAPI_RUST_WRAPPER_FLAVORS=claude,codex` to restrict flavors, or `HAPI_RUST_WRAPPER_FLAVORS=none` to revert all new sessions to TypeScript immediately.

## Automatic Idle Session Hibernation

Alongside the Rust wrapper, HAPI-Rust ships automatic idle session hibernation on the hub:

- **Default**: Sessions idle for **24 hours** (`HAPI_SESSION_HIBERNATE_MS=86400000`) hibernate automatically, releasing both wrapper and agent memory.
- **Transparent wake**: Sending the next message from the web/PWA client wakes the session automatically with full transcript continuity.
- **Manual control**: Run `hapi hibernate --help` to inspect or hibernate sessions manually, or set `HAPI_SESSION_HIBERNATE_MS=0` to disable auto-hibernation.

## Known Gaps & Contributing

Every known divergence or missing feature is tracked publicly under [`known-gap`](https://github.com/Overbaker/hapi-rust/issues?q=is%3Aissue+is%3Aopen+label%3Aknown-gap). High-impact areas open for contributors and co-maintainers include:

- **Good first issues**: [#15](https://github.com/Overbaker/hapi-rust/issues/15) (JSON nesting depth), [#16](https://github.com/Overbaker/hapi-rust/issues/16) (`rpc/discovery` TOCTOU open), [#17](https://github.com/Overbaker/hapi-rust/issues/17) (MCP HTTP task lifetime), [#27](https://github.com/Overbaker/hapi-rust/issues/27) (CI PTY timeout).
- **Platform & feature tracks**: [#21](https://github.com/Overbaker/hapi-rust/issues/21) (macOS Apple Silicon/Intel CI & Windows expansion), [#25](https://github.com/Overbaker/hapi-rust/issues/25) (Claude Code rewind & fork-at-message), [#26](https://github.com/Overbaker/hapi-rust/issues/26) (dynamic MCP `link_pr`).

See [CONTRIBUTING.md](https://github.com/Overbaker/hapi-rust/blob/deploy/next/CONTRIBUTING.md) to get started.

# HAPI-Rust — Local-First Remote Control & Rust Session Wrapper for AI Coding Agents

**HAPI-Rust** ([`Overbaker/hapi-rust`](https://github.com/Overbaker/hapi-rust)) is an open-source, local-first platform for running and remote-controlling **Claude Code, OpenAI Codex, Antigravity, Pi, OpenCode, and Cursor Agent** sessions from any desktop or mobile browser/PWA — powered by a **4.4 MiB Rust session wrapper** (~19× less memory than Bun/TypeScript) and **automatic idle session hibernation** so a single Linux or Windows host can keep 100+ coding agent sessions open.

[![License: AGPL-3.0](https://img.shields.io/github/license/Overbaker/hapi-rust)](LICENSE)
[![Releases](https://img.shields.io/github/v/release/Overbaker/hapi-rust?include_prereleases)](https://github.com/Overbaker/hapi-rust/releases)
[![Good First Issues](https://img.shields.io/github/issues/Overbaker/hapi-rust/good%20first%20issue)](https://github.com/Overbaker/hapi-rust/issues?q=is%3Aissue+is%3Aopen+label%3A%22good+first+issue%22)
[![Help Wanted](https://img.shields.io/github/issues/Overbaker/hapi-rust/help%20wanted)](https://github.com/Overbaker/hapi-rust/issues?q=is%3Aissue+is%3Aopen+label%3A%22help+wanted%22)

> **Relationship to upstream**: Fork of [tiann/hapi](https://github.com/tiann/hapi) keeping HAPI's hub, web/PWA client, and wire protocol intact while replacing the per-session TypeScript wrapper with a native Rust binary (`crates/hapi-session-wrapper`). Upstream stays TypeScript-only ([tiann/hapi#2534](https://github.com/tiann/hapi/issues/2534)); general TypeScript fixes continue to be sent back upstream. *(Note: this project is an AI coding agent controller, unrelated to the Houdini Engine `hapi-rs` crate.)*

---

## Why HAPI-Rust?

Running dozens of local AI coding agents with a Bun/TypeScript wrapper per session quickly consumes gigabytes of RAM (on a 23-session Linux host, TypeScript wrappers alone took **3.38 GB**, ~147 MiB per session, before counting the agents themselves). HAPI-Rust solves multi-session scaling without giving up HAPI's local-first workflow:

- **19× lower wrapper memory in production**: Median **4.4 MiB PSS** per session vs **83.7 MiB** on upstream Bun/TypeScript.
- **~25× faster cold session startup**: Spawn-to-webhook latency drops from **238–290 ms → 8–14 ms**.
- **Automatic idle hibernation**: Sessions idle for 24 h (`HAPI_SESSION_HIBERNATE_MS`) automatically hibernate and transparently wake on the next message.
- **Drop-in safety**: Transparent automatic fallback to the TypeScript wrapper whenever a platform, flavor, or mode isn't covered by Rust yet (62/62 transcript parity checks across 18 recorded Claude Code scenarios).
- **Local-first & self-hosted**: Keep your local filesystem, toolchains, and credentials on your own machine; connect remotely via end-to-end encrypted relay (`hapi hub --relay`), Tailscale, or Cloudflare Tunnel from any browser or installable PWA.

---

## Benchmarks

### 1. Real-World Production Memory (29 Live Sessions, 3 Linux Hosts, 2026-11-21)

Measured as wrapper-process **PSS** (Proportional Set Size) only:

| Wrapper Runtime | Live Sessions | Median PSS | Range |
|---|---:|---:|---:|
| **Rust** (`claude`, `codex`, `agy`) | 10 | **4.4 MiB** | 2.2–6.5 MiB |
| **TypeScript / Bun** (`claude`, `codex`, `agy`, `pi`) | 19 | **83.7 MiB** | 45.8–242.0 MiB |

### 2. Controlled 10-Session Comparison (Linux/WSL2)

Per-wrapper means (**RSS / PSS / USS**) and cold spawn-to-webhook latency (**p50 / p95**):

| Workload Cohort | Sessions | Compiled TS (RSS / PSS / USS) | Rust (RSS / PSS / USS) | Startup p50/p95 (TS → Rust) |
|---|---:|---:|---:|---:|
| HTTP light, single | 1 | 98.6 / 95.9 / 95.7 MiB | **7.3 / 5.3 / 5.2 MiB** | — |
| HTTP light | 10 | 97.8 / 52.5 / 48.1 MiB | **7.2 / 1.38 / 0.89 MiB** | 238/246 ms → **8/11 ms** |
| HTTPS light | 10 | 98.9 / 52.5 / 47.9 MiB | **8.2 / 1.73 / 1.19 MiB** | 280/290 ms → **11/14 ms** |
| HTTPS loaded (30 turns, ~1 MB JSONL, 1 live PTY) | 10 | 101.7 / 55.1 / 50.5 MiB | **8.6 / 2.07 / 1.53 MiB** | 277/291 ms → **12/15 ms** |

See the [Rust Session Wrapper Architecture & Benchmarks guide](docs/guide/rust-wrapper.md) for methodology and crate design.

---

## What Is Different from Upstream

| Feature | **HAPI-Rust** (`Overbaker/hapi-rust`) | Upstream HAPI (`tiann/hapi`) | `claude remote-control` |
|---|---|---|---|
| **Session wrapper** | Rust (`claude`, `codex`, `agy`, `pi` exp.) + TS fallback | TypeScript (Bun), one process per session | Anthropic CLI/cloud |
| **Wrapper memory / session** | **3–7 MiB** (4.4 MiB median) | 80–240 MiB (83.7 MiB median) | Single-vendor |
| **Idle sessions** | **Hibernate after 24 h** and wake on next message (`claude`, `codex`, `opencode`, `pi`, `agy acp`) | Stay running | — |
| **Supported agents** | Claude Code, Codex, Antigravity, Pi, OpenCode, Cursor, Grok, Kimi, Copilot, DeepSeek Harness | Same multi-agent set | Claude Code only |
| **Clients** | Free Web / PWA only (native app source, docs, and promotion removed) | Web / PWA + paid iOS / Android apps | Claude web / mobile |
| **Settings → About** | This fork's issues, upstream version + fork version (`next.<date>.<seq>.<sha>`) | Website, app version, app store links | — |

---

## Architecture

```text
Browser / Mobile PWA  ◀── REST + SSE ──▶  HAPI Hub (SQLite + optional WireGuard/TLS Relay)
                                                ▲
                                     Socket.IO /cli + REST
                                                │
                                          HAPI Runner
                             ┌──────────────────┴──────────────────┐
                             ▼                                     ▼
               hapi-session-wrapper (Rust, ~4.4 MiB)      TS Wrapper (fallback)
               [hapi-wrapper-core: Tokio, rustls, MCP,    [Other flavors / OS]
                hooks, PTY, process-tree subreaper]
                             │
                Claude Code / Codex / Antigravity / Pi
```

- **`crates/hapi-wrapper-core`**: Shared async runtime — Engine.IO/Socket.IO client, REST & runner webhook client, versioned CAS session client, RPC handlers, local MCP HTTP & hook-forwarder servers, PTY bridge, and subreaper / Job Object process-tree cleanup.
- **`crates/hapi-session-wrapper`**: Per-agent flavors (`claude`, `codex`, `agy`, `pi`) with exit-86 automatic fallback contract to TypeScript.

---

## Platform Support

| Platform | Rust Wrapper Flavors | Fallback |
|---|---|---|
| **Linux x64, Linux arm64** | Claude Code, Codex, Antigravity, Pi *(experimental)* | Automatic TS wrapper |
| **Windows x64** | Codex | Automatic TS wrapper |
| **macOS & others** | TypeScript wrapper *(macOS Rust support tracked in [#21](https://github.com/Overbaker/hapi-rust/issues/21))* | Automatic TS wrapper |

The runner falls back to the TypeScript wrapper when the Rust binary is missing, the platform or mode is not covered, or the Rust wrapper exits before the session starts.

---

## Install & Quick Start

### Option 1: Prebuilt Binaries (Recommended)

Download a build from the [releases page](https://github.com/Overbaker/hapi-rust/releases) (`linux-x64`, `linux-arm64`, or `windows-x64`). Each archive holds `hapi` and `hapi-session-wrapper` in the same `bin` directory; keep them together, because the runner looks for the wrapper next to `hapi`.

> Note: The npm package `@twsxtd/hapi` is upstream's TypeScript-only build and does **not** include the Rust wrapper.

```bash
# Start the local hub (add --relay for E2E encrypted WireGuard + TLS remote access)
hapi hub --relay

# Start a session (uses the Rust wrapper automatically on supported platforms)
hapi claude
# or: hapi codex
```

### Option 2: Build from Source

Requires **Bun 1.4.4** and **Rust stable**:

```bash
git clone https://github.com/Overbaker/hapi-rust.git
cd hapi-rust
bun install
bun run build:single-exe
cargo build --release -p hapi-session-wrapper
```

### Runtime Switches

| Variable | Where | Default | Meaning |
|---|---|---|---|
| `HAPI_RUST_WRAPPER_FLAVORS` | runner | every supported agent | Comma-separated list such as `claude,codex`, or `none` for TypeScript everywhere |
| `HAPI_SESSION_HIBERNATE_MS` | hub | `86400000` (24 h) | Idle time before a session hibernates; `0` turns it off |

`hapi hibernate --help` lists the commands that put sessions to sleep by hand. For using HAPI itself (hub, relay, agents, PWA), see the [guides](docs/guide/quick-start.md) or the [online documentation](https://overbaker.github.io/hapi-rust/).

---

## Contributors & Co-Maintainers Wanted

Open gaps are tracked transparently as [`known-gap` issues](https://github.com/Overbaker/hapi-rust/issues?q=is%3Aissue+is%3Aopen+label%3Aknown-gap). You can run the full Rust test suite (`cargo test --workspace`) and parity harness locally **without** paid LLM API keys.

| Track | Open Issues & Scope | Best Fit For |
|---|---|---|
| **Good First Issues** | [#15](https://github.com/Overbaker/hapi-rust/issues/15) (JSON nesting depth), [#16](https://github.com/Overbaker/hapi-rust/issues/16) (`rpc/discovery` TOCTOU open), [#17](https://github.com/Overbaker/hapi-rust/issues/17) (MCP HTTP task lifetime), [#27](https://github.com/Overbaker/hapi-rust/issues/27) (flaky CI PTY timeout) | First-time Rust contributors |
| **macOS & Windows** | [#21](https://github.com/Overbaker/hapi-rust/issues/21) (macOS Apple Silicon/Intel CI & release builds; Windows Claude/Pi) | Cross-platform Rust co-maintainers |
| **Claude Code Parity** | [#25](https://github.com/Overbaker/hapi-rust/issues/25) (Rewind & fork-from-message), [#26](https://github.com/Overbaker/hapi-rust/issues/26) (dynamic MCP `link_pr`) | Async Rust / protocol engineers |
| **Process Lifecycle** | [#23](https://github.com/Overbaker/hapi-rust/issues/23), [#24](https://github.com/Overbaker/hapi-rust/issues/24) (subreaper/cgroup & Job Object cleanup) | Linux / Windows systems programmers |
| **New Agent Flavors** | Verify Pi real turns; add DeepSeek Harness, OpenCode, or Cursor Agent in Rust | Power users of those agents |

See [CONTRIBUTING.md](CONTRIBUTING.md) for Rust + TypeScript dev commands and co-maintainer guidelines, or join [GitHub Discussions](https://github.com/Overbaker/hapi-rust/discussions).

---

## FAQ

**How is HAPI-Rust different from `tiann/hapi`?**
HAPI-Rust keeps `tiann/hapi`'s hub, web/PWA client, and protocol, and adds a native Rust session wrapper (4.4 MiB vs 83.7 MiB median PSS) plus 24-hour automatic idle session hibernation. It removes native mobile app code and promotion to focus on Web/PWA and high-density server/workstation hosting.

**Can I run 50–100+ Claude Code or Codex sessions on one machine?**
Yes. At ~4.4 MiB PSS per live Rust wrapper (and 1.38–2.07 MiB PSS in 10-session shared-page workloads), wrapper overhead for 100 sessions stays under a few hundred megabytes instead of 8–20+ GiB, and idle sessions hibernate automatically after 24 hours.

**What happens if a feature isn't implemented in the Rust wrapper yet?**
The runner checks flavor/platform support and falls back transparently to the TypeScript wrapper if `hapi-session-wrapper` is absent or exits with fallback code `86`. You can also set `HAPI_RUST_WRAPPER_FLAVORS=none` at any time.

**Does HAPI-Rust work on macOS?**
Yes — on macOS, HAPI-Rust runs seamlessly using the TypeScript wrapper fallback today. Native macOS (`darwin-arm64` / `darwin-x64`) Rust wrapper builds are tracked in [#21](https://github.com/Overbaker/hapi-rust/issues/21), and macOS co-maintainers are very welcome.

---

## Credits

Built on [tiann/hapi](https://github.com/tiann/hapi) under its AGPL-3.0 license. Thanks to its maintainers and contributors. HAPI ("哈皮") is a transliteration of [Happy](https://github.com/slopus/happy).

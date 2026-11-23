# HAPI (Rust session wrapper fork)

A fork of [tiann/hapi](https://github.com/tiann/hapi) that keeps HAPI's hub, web app and protocol, and replaces the per-session wrapper with a small Rust binary, so one machine can keep hundreds of sessions open.

Upstream stays TypeScript-only ([tiann/hapi#2534](https://github.com/tiann/hapi/issues/2534)), so the Rust work lives here. The fork follows upstream for everything else and sends its TypeScript fixes back upstream. Bugs and questions: [issues](https://github.com/Overbaker/hapi-rust/issues).

## What is different from upstream

| | Upstream HAPI | This fork |
|---|---|---|
| Session wrapper | TypeScript (Bun), one process per session | Rust for Claude Code, Codex, Antigravity and Pi (experimental); TypeScript for every other agent |
| Wrapper memory per session | 80–240 MiB for Claude Code | 3–7 MiB |
| Idle sessions | Stay running | Hibernate after 24 h idle and wake on the next message (Claude Code, Codex, OpenCode, Pi, Antigravity on `agy acp`) |
| Clients | Web / PWA plus native iOS / Android apps | Web / PWA only; native app source, docs and promotion removed |
| Settings → About | Website, app version, app store links | This fork's issues, upstream version plus fork version (`next.<date>.<seq>.<sha>`) |

Wrapper memory measured on 2026-11-21 (29 live sessions on 3 Linux hosts, PSS of the wrapper process only):

| Wrapper | Sessions | Median | Range |
|---|---|---|---|
| Rust (claude, codex, agy) | 10 | 4.4 MiB | 2.2–6.5 MiB |
| TypeScript (claude, codex, agy, pi) | 19 | 83.7 MiB | 45.8–242.0 MiB |

## Platforms

| Platform | Rust wrapper |
|---|---|
| Linux x64, Linux arm64 | Claude Code, Codex, Antigravity, Pi (experimental) |
| Windows x64 | Codex |
| Anything else | TypeScript wrapper, as upstream |

The runner falls back to the TypeScript wrapper when the Rust binary is missing, the platform or mode is not covered, or the Rust wrapper exits before the session starts.

## Switches

| Variable | Where | Default | Meaning |
|---|---|---|---|
| `HAPI_RUST_WRAPPER_FLAVORS` | runner | every supported agent | Comma-separated list such as `claude,codex`, or `none` for TypeScript everywhere |
| `HAPI_SESSION_HIBERNATE_MS` | hub | `86400000` (24 h) | Idle time before a session hibernates; `0` turns it off |

`hapi hibernate --help` lists the commands that put sessions to sleep by hand.

## Install

Download a build from the [releases page](https://github.com/Overbaker/hapi-rust/releases). Each archive holds `hapi` and `hapi-session-wrapper` in the same `bin` directory; keep them together, because the runner looks for the wrapper next to `hapi`. The npm package `@twsxtd/hapi` is upstream's build and has no Rust wrapper.

Build from source (Bun 1.4.4, Rust stable):

```bash
bun install
bun run build:single-exe
cargo build --release -p hapi-session-wrapper
```

For using HAPI itself (hub, relay, agents, PWA), see the [guides](docs/guide/quick-start.md).

## Not done yet

Open gaps are tracked as [`known-gap` issues](https://github.com/Overbaker/hapi-rust/issues?q=is%3Aissue+is%3Aopen+label%3Aknown-gap). The larger ones: Pi has not completed a verified real turn on the Rust wrapper, Claude Code rewind / fork-at-message is not implemented in Rust, and DeepSeek Harness has no Rust wrapper.

## Credits

Built on [tiann/hapi](https://github.com/tiann/hapi) under its AGPL-3.0 license. Thanks to its maintainers and contributors. HAPI ("哈皮") is a transliteration of [Happy](https://github.com/slopus/happy).

# HAPI (Rust session wrapper fork)

This is a fork of [tiann/hapi](https://github.com/tiann/hapi). It keeps HAPI's hub, web app and protocol, and replaces the per-session wrapper process with a small Rust binary so that one machine can keep hundreds of sessions open.

Upstream keeps HAPI in TypeScript only ([tiann/hapi#2534](https://github.com/tiann/hapi/issues/2534)), so the Rust work lives here. This fork follows upstream for everything else, does not aim to match it feature for feature, and sends its TypeScript fixes back as upstream pull requests.

## What is different from upstream

| | Upstream HAPI | This fork |
|---|---|---|
| Session wrapper | TypeScript (Bun), one process per session | Rust for Claude Code, Codex, Antigravity and Pi (experimental); TypeScript for every other agent |
| Wrapper memory per session | 80–240 MiB for Claude Code | 3–7 MiB |
| Idle sessions | Stay running (hibernation is proposed in [tiann/hapi#2512](https://github.com/tiann/hapi/pull/2512)) | Hibernate after 24 hours idle and wake on the next message: Claude Code, Codex, OpenCode, Pi, and Antigravity on `agy acp`. Shown as sleeping in the web app, separate from archived |
| Native iOS / Android source | Included | Removed; the web app / PWA is the client this fork tests |

Measured on our own machines on 2026-11-21 (29 live sessions on 3 Linux hosts, PSS of the wrapper process only, not the agent):

| Wrapper | Sessions | Median | Range |
|---|---|---|---|
| Rust (claude, codex, agy) | 10 | 4.4 MiB | 2.2–6.5 MiB |
| TypeScript (claude, codex, agy, pi) | 19 | 83.7 MiB | 45.8–242.0 MiB |

### Platforms

| Platform | Rust wrapper |
|---|---|
| Linux x64, Linux arm64 | Claude Code, Codex, Antigravity, Pi (experimental) |
| Windows x64 | Codex |
| Anything else | TypeScript wrapper, as upstream |

The runner falls back to the TypeScript wrapper by itself when the Rust binary is missing, the platform or mode is not covered, or the Rust wrapper exits before the session starts.

### Switches

| Variable | Where | Default | Meaning |
|---|---|---|---|
| `HAPI_RUST_WRAPPER_FLAVORS` | runner | every supported agent | A comma-separated list such as `claude,codex`, or `none` to use the TypeScript wrapper everywhere |
| `HAPI_SESSION_HIBERNATE_MS` | hub | `86400000` (24 h) | Idle time before a session hibernates; `0` turns hibernation off |

`hapi hibernate --help` lists the commands that put sessions to sleep by hand.

### Install

Builds are published on the [releases page](https://github.com/Overbaker/hapi-rust/releases). Each archive holds `hapi` and `hapi-session-wrapper` in the same `bin` directory; keep them together, because the runner looks for the wrapper next to `hapi`. The npm package `@twsxtd/hapi` is upstream's build and does not contain the Rust wrapper.

### Not done yet

Review on this fork reports problems without blocking our own pull requests; what is not fixed at once stays open. Open gaps are tracked as [`known-gap` issues](https://github.com/Overbaker/hapi-rust/issues?q=is%3Aissue+is%3Aopen+label%3Aknown-gap). The larger ones: Pi has not completed a verified real turn on the Rust wrapper, Claude Code rewind / fork-at-message is not implemented in Rust, and DeepSeek Harness has no Rust wrapper.

The rest of this page is upstream's introduction to HAPI.

## Features

- **Seamless Handoff** - Work locally, switch to remote when needed, switch back anytime. No context loss, no session restart.
- **Shared Codex Sessions** - Use Codex from your terminal and phone at the same time. Requires Codex 0.154.0+. [Usage and limits](docs/guide/codex-shared-sessions.md).
- **Native First** - HAPI wraps your AI agent instead of replacing it. Same terminal, same experience, same muscle memory.
- **AFK Without Stopping** - Step away from your desk? Approve AI requests from your phone with one tap.
- **Your AI, Your Choice** - Claude Code, Codex, Cursor Agent, Grok Build, OpenCode, Kimi, Copilot, Antigravity, Pi, DeepSeek Harness—different agents, one unified workflow.
- **Terminal Anywhere** - Run commands from your phone's browser or desktop web app, directly connected to the working machine.
- **Voice Control** - Use dictation in native apps, or talk to your AI agent hands-free with the web voice assistant.
- **Workspace Browser** - Opt-in via one or more `hapi runner start --workspace-root <path>` flags: browse scoped file trees from the web and start sessions in allowed subdirectories.

## Demo

https://github.com/user-attachments/assets/38230353-94c6-4dbe-9c29-b2a2cc457546

## Getting Started

The commands below install upstream's npm build. For this fork's build, see [Install](#install) above.

```bash
npx @twsxtd/hapi hub --relay     # start hub with E2E encrypted relay
npx @twsxtd/hapi                 # choose an agent and start a session
```

`hapi server` remains supported as an alias.

Use `hapi <agent> [options]` to start an agent directly, for example `hapi claude`
or `hapi codex`. Scripts must specify the agent explicitly. `hapi --help` shows
HAPI's commands and supported agents.

The hub displays a URL and two QR codes. Then connect a client:

- **Web / PWA (free):** open the web URL in a browser, or scan the web QR code. [Install it as a PWA](docs/guide/pwa.md) for a home-screen app and push notifications.
- **Native apps (paid):** install HAPI from the [App Store](https://apps.apple.com/app/id6805699104) or [Google Play](https://play.google.com/store/apps/details?id=run.hapi.companion), then scan the companion QR. See [Native apps](docs/guide/native-apps.md) for pairing.

> The relay uses WireGuard + TLS for end-to-end encryption. Your data is encrypted from your device to your machine.

For self-hosted options (Cloudflare Tunnel, Tailscale), see [Installation](docs/guide/installation.md)

## Docs

- [Native apps (iOS / Android)](docs/guide/native-apps.md)
- [Web / PWA](docs/guide/pwa.md)
- [How it Works](docs/guide/how-it-works.md)
- [Supported Agents](docs/guide/agents.md)
- [Voice Assistant](docs/guide/voice-assistant.md)
- [Why HAPI](docs/guide/why-hapi.md)
- [FAQ](docs/guide/faq.md)

## Clients

- **Web / PWA (free)** - The full-featured web app, including the remote terminal and voice assistant. Works in any browser and installs as a PWA. See [Web / PWA](docs/guide/pwa.md).
- **Native iOS / Android apps (paid)** - SwiftUI/UIKit and Kotlin Compose clients with chat, approvals, session creation, files, dictation, and native push notifications. See the [native app guide](docs/guide/native-apps.md) for capabilities and pairing.

<a href="https://apps.apple.com/app/id6805699104"><img src="https://developer.apple.com/assets/elements/badges/download-on-the-app-store.svg" alt="Download on the App Store" height="48"></a>
<a href="https://play.google.com/store/apps/details?id=run.hapi.companion"><img src="https://play.google.com/intl/en_us/badges/static/images/badges/en_badge_web_generic.png" alt="Get it on Google Play" height="72"></a>

Native apps are not part of this fork. See the [client contract](docs/api/client-contract/index.md) for the shared protocol.

## Build from source

Requires Bun 1.4.4.

```bash
bun install
bun run build:single-exe
```

## Credits

This fork is built on [tiann/hapi](https://github.com/tiann/hapi) and keeps its AGPL-3.0 license. Thanks to its maintainers and contributors.

HAPI means "哈皮" a Chinese transliteration of [Happy](https://github.com/slopus/happy). Great credit to the original project.

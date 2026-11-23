---
layout: home
title: HAPI-Rust — 4.4 MiB Rust Session Wrapper for AI Coding Agents
titleTemplate: false
description: Run 100+ local Claude Code, OpenAI Codex, Antigravity, and Pi sessions from any browser/PWA with a 4.4 MiB Rust session wrapper and automatic 24h idle hibernation.

hero:
  name: HAPI-Rust
  text: 100+ AI Coding Sessions on One Host
  tagline: Local-first remote control for Claude Code, Codex, Antigravity & Pi — powered by a 4.4 MiB Rust session wrapper (19× less RAM than Bun/TS) and 24h auto-hibernation.
  image:
    src: /logo.svg
    alt: HAPI-Rust
  actions:
    - theme: brand
      text: Quick Start
      link: /guide/quick-start
    - theme: alt
      text: Architecture & Benchmarks
      link: /guide/rust-wrapper
    - theme: alt
      text: Download Latest Release
      link: https://github.com/Overbaker/hapi-rust/releases/latest

features:
  - title: 4.4 MiB Median PSS (19× Smaller)
    details: Measured across 29 live Linux sessions vs 83.7 MiB on upstream Bun/TypeScript — run dozens to hundreds of sessions without exhausting RAM.
  - title: 11 ms Cold Start (25× Faster)
    details: Native Tokio + rustls binary cuts spawn-to-webhook startup latency from 238–290 ms down to 8–14 ms.
  - title: 24h Auto-Hibernation
    details: Idle sessions sleep automatically to free memory and wake transparently on the next message from your phone or browser.
  - title: Drop-In Safety & Free PWA
    details: 62/62 TS↔Rust parity checks passing with automatic exit-86 TypeScript fallback. Local-first SQLite hub + E2EE relay + free Web/PWA client.
---

<div style="margin-top: 2.5rem; display: flex; flex-direction: column; gap: 2rem;">

![HAPI-Rust vs TypeScript/Bun wrapper memory and startup latency benchmarks](/images/benchmark-chart.svg)

![HAPI-Rust architecture diagram](/images/architecture.svg)

</div>

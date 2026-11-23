# Contributing to HAPI

Thank you for your interest in contributing to HAPI! We welcome bug fixes, feature suggestions, and thoughtful contributions from the community.

## Code of Conduct

- **Be friendly and respectful** - Treat everyone with kindness and professionalism
- **Help others** - Share knowledge and assist fellow contributors
- **Give constructive feedback** - Focus on the code, not the person
- **Be patient** - Remember that everyone has different experience levels

## AI-Generated Code Policy (Vibe Coding)

If you used AI tools to generate code ("Vibe Coding"), please disclose the model in your PR description. Code is judged on merit — focused scope, tests, and the other expectations below apply equally regardless of how the code was authored.

## Pull Request Guidelines

### No Mega PRs

We do not accept oversized pull requests that introduce large features all at once. Large PRs are:

- Difficult to review thoroughly
- More likely to introduce bugs
- Harder to revert if issues arise

**If you want to add a significant feature, please open an issue first to discuss the approach.** We can help break it down into smaller, reviewable chunks.

### PR Best Practices

- Keep PRs focused on a single concern
- Write clear commit messages
- Include relevant tests if applicable
- Update documentation if needed
- Reference related issues in your PR description

## Bug Reports

Bug reports are always welcome! When reporting a bug, please include:

- A clear description of the issue
- Steps to reproduce
- Expected vs actual behavior
- Your environment (OS, Node/Bun version, etc.)
- Any relevant logs or screenshots

## Feature Requests

Have an idea to improve HAPI? Open an issue with:

- A clear description of the feature
- The problem it solves
- Any implementation ideas you have

## Getting Started

1. Fork and clone the repository
2. Install Bun 1.4.4 and Rust stable, then install JS dependencies from the repo root:
   ```bash
   bun install
   ```
3. Start TypeScript hub/web development:
   ```bash
   bun run dev
   ```

See the [README](README.md) for prebuilt downloads and single-executable builds.

## Working on the Rust Session Wrapper

The Rust workspace lives in `Cargo.toml` with two crates:

- `crates/hapi-wrapper-core` — shared Engine.IO/Socket.IO client, REST/webhook client, CAS session state, RPC handlers, local MCP HTTP server, hook forwarder, PTY bridge, and process-tree lifecycle.
- `crates/hapi-session-wrapper` — agent flavor implementations (`claude`, `codex`, `agy`, `pi`) and the exit-86 TypeScript fallback contract.

### Quick Rust Verification (No Paid LLM API Keys Required)

```bash
# Run all Rust unit & integration tests
cargo test --workspace

# Strict lint & formatting checks (matches CI)
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check

# Build release wrapper binary
cargo build --release -p hapi-session-wrapper

# Run TypeScript ↔ Rust transcript parity suite
bun run test:parity
```

### Good First Issues & Co-Maintainer Tracks

- **First-time contributors**: Check issues labeled [`good first issue`](https://github.com/Overbaker/hapi-rust/issues?q=is%3Aissue+is%3Aopen+label%3A%22good+first+issue%22) (`#15` JSON nesting depth, `#16` `rpc/discovery` TOCTOU open, `#17` MCP HTTP task lifetime, `#27` flaky CI PTY timeout).
- **Platform & flavor co-maintainers**: We actively welcome co-maintainers for:
  1. **macOS (`darwin-arm64` / `darwin-x64`)** CI & release builds (`#21`)
  2. **Windows** Rust wrapper expansion beyond Codex (`#21`)
  3. **Claude Code parity** (`#25` Rewind/Fork, `#26` dynamic MCP `link_pr`)
  4. **New agent flavors** (Pi production verification, DeepSeek Harness, OpenCode, Cursor Agent)

Contributors with 2–3 merged PRs in a track are welcome to join as area triage/co-maintainers — mention your interest in [GitHub Discussions](https://github.com/Overbaker/hapi-rust/discussions) or on your PR.

### Upstream vs Fork Changes

- **General TypeScript bug fixes** affecting upstream HAPI are kept cleanly scoped so they can also be contributed back to [tiann/hapi](https://github.com/tiann/hapi).
- **Rust wrapper, hibernation, docs, and fork packaging PRs** target `deploy/next` in `Overbaker/hapi-rust`.

From the repo root, `bun typecheck` checks CLI, Hub, Web, and Relay;
`bun run test` runs CLI/Web Vitest and Hub/Shared/Relay Bun tests.

For documentation changes, run `bun run --cwd docs docs:build` and check
affected links and anchors in the generated site.

## Questions?

If you have questions, feel free to open an issue. We're here to help!

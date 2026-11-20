# hapi-wrapper-core

This library provides the foundations for an optional native HAPI session wrapper.
It implements JavaScript-compatible JSON helpers, resolves CLI configuration from
the environment and settings file, and connects to the hub’s `/cli` namespace
over WebSocket using Engine.IO 4 and Socket.IO 5. The TypeScript product does
not use this crate yet.

A session socket (`sio::client::Client::start_session`) reports its connection
state through a watch channel, starts stopped for an archived session, and after
a server disconnect or a rejected handshake reconnects only on request.

On top of the socket it provides the session layer of `cli/src/api/apiSession.ts`:

- `rest`: the hub REST client (machine registration, session create, fetch and
  message backfill) and the best-effort runner `session-started` webhook.
- `lifecycle::start_marker`: the `ps lstart` marker that identifies a live
  process across PID reuse.
- `rpc::manager`: session-scoped RPC handler registration and dispatch.
- `session::bootstrap`: create or reopen a session, report a runner-started
  session through the webhook, and start its client.
- `session::metadata`: machine and session metadata, worktree discovery, and the
  fields kept when a session is reopened, including the Claude rewind intent.
- `session::incoming_filter`: incoming message dedup by id and sequence cursor,
  backfill and rewind bookkeeping, and the typed user message check.
- `session::echo`: bookkeeping for prompts that the agent may echo back.
- `session::versioned_ack`: queued compare-and-swap updates for metadata and
  agent state, retried with jittered backoff.
- `session::client`: RPC registration and `session-alive` on every connect,
  `update` handling, the hub-archived latch, the stoppable 10 s keepalive,
  incoming dedup with backfill after reconnect, the emit API, prompt-echo
  bookkeeping, and flush and close. A caller can pass `SessionExtension`s
  through `bootstrap::Options.extensions`: each registers its RPC handlers (given the
  resolved session id) before the socket starts, is told about every accepted connect (with its socket
  generation) and its disconnect, and once more when the client is closed or
  dropped, may consume socket events of an accepted connection that the client
  does not handle itself, and is awaited by `shutdown_extensions()`. The final
  `disconnected()` waits for a callback already in progress (or runs after it,
  when that callback closes the client), and no callback starts after it.
- `rpc::handlers`: workspace path checks and the file RPCs of
  `cli/src/modules/common/handlers/files.ts` and `directories.ts`
  (`readFile`, `readGeneratedImage`, `writeFile`, `listDirectory`, `statFiles`,
  `getDirectoryTree`), plus the in-memory generated media store.
- `rpc::handlers::commands`: the shell, search and git RPCs of
  `cli/src/modules/common/handlers/bash.ts`, `ripgrep.ts`, `difftastic.ts` and
  `git.ts` (`bash`, `ripgrep`, `difftastic`, `git-status`, `git-diff-numstat`,
  `git-diff-file`). `rg` and `difft` are taken from
  `$HAPI_WRAPPER_RUNTIME_PATH/tools/unpacked`.
- `rpc::handlers::discovery`: registers `listSlashCommands` and `listSkills`
  and resolves a named skill from the same discovery roots.
- `schemas`: the session metadata checks of `shared/src/schemas.ts`, including
  the full OpenCode rewind record.

The standalone `queue::MessageQueue2` groups consecutive messages by mode and
supports isolated turns, reservations, and deferred batch acknowledgements.
Reset permits late confirmation of a discarded reservation, as in the CLI.

Some tests compare results with the TypeScript implementation. They run `bun`
and the CLI's `vitest` from `cli/`, and start a Socket.IO hub fixture from
`hub/node_modules`. Run `bun install` first.

The PEM files in `tests/fixtures/` contain a self-signed test CA and a localhost
server certificate and private key. They are used only by the TLS tests.

The file RPCs check both the lexical path and its canonical target against the
working directory, so `listDirectory` and `writeFile` refuse a symlink that
leads outside it (TypeScript checks only the lexical path for these two).
Tree and listing traverse the canonical path, so `getDirectoryTree` reports
canonical paths. The handlers are built on Unix only.

Directory entries follow English `localeCompare` order for printable ASCII;
other characters sort after ASCII by code point.

The shell, search and git RPCs differ from TypeScript in these ways:

- `bash`, `difftastic` and the git RPCs check the canonical working directory
  as well as the lexical one, and a relative `cwd` (or none) is resolved against
  the working directory rather than the process directory.
- `bash` and the git RPCs run the command in its own process group and kill the
  whole group with `SIGKILL` on timeout or output overflow (TypeScript sends
  `SIGTERM` to the direct child only). Each output stream is limited to 1 MiB,
  as in `exec`.
- All six RPCs give the command `/dev/null` as stdin instead of an open pipe,
  so a `bash` command that reads stdin sees end of file instead of waiting
  for the timeout.
- A `ripgrep` file search replaces each path after `--` with the canonical path
  it was checked against, so the spawned process sees the path that was
  validated; results under an in-workspace symlink are reported under its
  target.
- `git-diff-file` resolves a relative `filePath` against the canonical `cwd`,
  which is how git reads it, and also checks the canonical target of the nearest existing directory on the
  way to it. Git still receives `filePath` unchanged.
- `ripgrep` and `difftastic` reply `Invalid arguments` when `args` is not an
  array of strings.

Socket.IO JSON decoding replaces each unpaired UTF-16 surrogate with U+FFFD.
Valid surrogate pairs retain their character, and re-encoding preserves the
normalized string.

The `permission` module ports the CLI's shared permission bookkeeping
(`cli/src/modules/common/permission/BasePermissionHandler.ts`): mode- and
name-based auto-approval, pending requests mirrored into `agentState`, and
cancellation of orphaned or superseded requests. It does not depend on a
transport; `response_handler` returns the closure that an RPC layer registers
for the `permission` method.

The `hooks` module ports Claude's hook bridge: the token-protected loopback
server (`cli/src/claude/utils/startHookServer.ts`), the `hook-forwarder`
command that relays hook stdin to it (`sessionHookForwarder.ts`), and the
generated hook settings file (`cli/src/modules/common/hooks/generateHookSettings.ts`).
Unlike the TypeScript writer, the settings file is created with mode 0600 and
renamed into place, because its command line carries the hook token.

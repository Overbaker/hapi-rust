# hapi-wrapper-core

This library provides the foundations for an optional native HAPI session wrapper.
It implements JavaScript-compatible JSON helpers, resolves CLI configuration from
the environment and settings file, and connects to the hub’s `/cli` namespace
over WebSocket using Engine.IO 4 and Socket.IO 5. The TypeScript product does
not use this crate yet.

It also provides the pieces the session layer of `cli/src/api/apiSession.ts`
builds on:

- `rest`: the hub REST client (machine registration, session create, fetch and
  message backfill) and the best-effort runner `session-started` webhook.
- `lifecycle::start_marker`: the `ps lstart` marker that identifies a live
  process across PID reuse.
- `rpc::manager`: session-scoped RPC handler registration and dispatch.
- `rpc::handlers`: workspace path checks and the file RPCs of
  `cli/src/modules/common/handlers/files.ts` and `directories.ts`
  (`readFile`, `readGeneratedImage`, `writeFile`, `listDirectory`, `statFiles`,
  `getDirectoryTree`), plus the in-memory generated media store.
- `rpc::handlers::commands`: the shell, search and git RPCs of
  `cli/src/modules/common/handlers/bash.ts`, `ripgrep.ts`, `difftastic.ts` and
  `git.ts` (`bash`, `ripgrep`, `difftastic`, `git-status`, `git-diff-numstat`,
  `git-diff-file`). `rg` and `difft` are taken from
  `$HAPI_WRAPPER_RUNTIME_PATH/tools/unpacked`.
- `schemas`: the session metadata checks of `shared/src/schemas.ts`, including
  the full OpenCode rewind record.

Some tests compare results with the TypeScript implementation. They run `bun`
from `cli/`, and start a Socket.IO hub fixture from `hub/node_modules`. Run
`bun install` first.

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
- `ripgrep` and `difftastic` get `/dev/null` as stdin instead of an open pipe.
- A `ripgrep` file search replaces each path after `--` with the canonical path
  it was checked against, so the spawned process sees the path that was
  validated; results under an in-workspace symlink are reported under its
  target.
- `git-diff-file` resolves `filePath` against `cwd`, which is how git reads it,
  checks the nearest existing ancestor of its directory canonically, and passes
  git that directory joined with the original last component, so a tracked
  symlink is still diffed as a link.
- `ripgrep` and `difftastic` reply `Invalid arguments` when `args` is not an
  array of strings.

Socket.IO JSON decoding replaces each unpaired UTF-16 surrogate with U+FFFD.
Valid surrogate pairs retain their character, and re-encoding preserves the
normalized string.

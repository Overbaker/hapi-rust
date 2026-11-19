## cdd4184df062 fix(runner): harden wrapper inventory ownership, parsing and publish

- adopt only wrappers parented by this runner or orphaned (ppid 1/subreaper),
  with the handoff parent treated as owned; skip sessions with a stop in flight
- re-verify the start marker before adoption and clear stale exit tombstones
- quote-aware argv, last hapi fragment before flags, POSIX comm fallback,
  Windows parent PIDs kept
- drift: hostPid match on a fresh scan (incl. shared codex roots)
- publish inventory on connect, dedupe unchanged scans; tests for ps parsing,
  schema slice/catch and the connect path

## 1cb8d8169f3a feat(runner): inventory orphan wrappers and report binary drift

The runner now scans runner-started HAPI wrappers (own UID only) at start and
every 10 minutes and puts each one in exactly one bucket: tracked, adopted
(session id + same-snapshot start marker + signalable, flavor recorded, no
duplicate session) or reported (no_session_id, no_start_marker,
duplicate_session, not_signalable, adopt_failed, drift). Nothing is killed.

Linux drift detection compares /proc/<pid>/exe (dev+ino) with the binary
spawnHappyCLI runs; deleted or mismatched wrappers are reported. The hub uses
a 10 minute hibernation window for drifted sessions whose hostPid matches a
fresh scan, so they upgrade on the next resume.

## 95f833b5d5d0 feat(session): hibernate idle sessions and resume them on demand

After HAPI_SESSION_HIBERNATE_MS (default 2h, 0 disables) of keepalive-only
idleness, the hub stops an eligible session's wrapper, agent and process
group (incl. per-session MCP children) and marks it `hibernated`. The next
web send, POST /messages, matured scheduled message or ping_peer resumes it
through the existing resume path under the same session id.

- shared: SESSION_LIFECYCLE_HIBERNATED, metadata.hibernation, runner
  capability sessionHibernation, stop option requireProcessGroupCleanup
- hub: shouldHibernate (claude/codex/opencode/pi only; live work = thinking,
  background tasks, requests, attached jobs, team work, terminals,
  undelivered messages, in-flight actions), confirmed-stop only, 10 min
  backoff, never merge on resume, dedup skips hibernated rows, startup grace
- cli: verified process-group sweep after stop; ping_peer wakes hibernated
- web: Hibernated badge, kept in Active view/ordering, Waking status, i18n

## ff807091681b docs(openspec): add agent-session-orchestration and rust-wasm-e2e-integration changes

- agent-session-orchestration: idle session hibernation, orphan/binary-drift
  hygiene, idle-CPU A/B, lazy imports + bytecode (measured on india-vps)
- rust-wasm-e2e-integration: measurement-first ROI plan with kill criteria


```typescript
import { zstdCompressSync, zstdDecompressSync } from 'node:zlib'

/** JSON payloads at or above this many chars are stored zstd-compressed (BLOB).
 *  Below it the plaintext JSON is kept: zstd gains little on tiny strings and
 *  plaintext rows stay grep-able when inspecting the DB by hand. */
export const COMPRESS_MIN_CHARS = 256

/** Per-string cap inside agent message content. Tool results above this are
 *  head+tail truncated at ingest — a single file read or build log can otherwise
 *  persist megabytes that no client ever renders in full. */
export const TRUNCATE_STRING_LIMIT = 64 * 1024
const TRUNCATE_HEAD = 48 * 1024
const TRUNCATE_TAIL = 12 * 1024

function truncateString(value: string): string {
    const removed = value.length - TRUNCATE_HEAD - TRUNCATE_TAIL
    // head + tail + marker stays well under TRUNCATE_STRING_LIMIT so the
    // function is idempotent — codexDesktop compares stored rows against
    // freshly-normalized transcripts and must get identical strings on both sides.
    return `${value.slice(0, TRUNCATE_HEAD)}\n…[hapi: truncated ${removed} chars]…\n${value.slice(value.length - TRUNCATE_TAIL)}`
}

/** Returns the same reference when nothing was truncated so callers can use
 *  identity to detect change. */
function truncateDeep(value: unknown): unknown {
    if (typeof value === 'string') {
        return value.length > TRUNCATE_STRING_LIMIT ? truncateString(value) : value
    }
    if (Array.isArray(value)) {
        let copy: unknown[] | null = null
        for (let i = 0; i < value.length; i++) {
            const item = truncateDeep(value[i])
            if (item !== value[i]) {
                copy ??= value.slice()
                copy[i] = item
            }
        }
        return copy ?? value
    }
    if (value !== null && typeof value === 'object') {
        const record = value as Record<string, unknown>
        let copy: Record<string, unknown> | null = null
        for (const key of Object.keys(record)) {
            const item = truncateDeep(record[key])
            if (item !== record[key]) {
                copy ??= { ...record }
                copy[key] = item
            }
        }
        return copy ?? value
    }
    return value
}

/** Truncate oversized strings inside agent-role message content.
 *
 *  Only role === 'agent' envelopes are touched: user messages are queued in the
 *  DB and later *delivered* to the CLI verbatim (getMatureScheduledMessages /
 *  getDeliverableMessagesAfter), so truncating them would corrupt real prompts,
 *  not just display history. Idempotent: re-applying to already-truncated
 *  content returns the same reference. */
export function truncateOversizedMessageContent(content: unknown): unknown {
    if (content === null || typeof content !== 'object') return content
    if ((content as Record<string, unknown>).role !== 'agent') return content
    return truncateDeep(content)
}

/** Compress a JSON string for the messages.content column.
 *  Storage contract: TEXT value = plaintext JSON, BLOB value = zstd(JSON). */
export function compressContentJson(json: string): string | Buffer {
    if (json.length < COMPRESS_MIN_CHARS) return json
    const compressed = zstdCompressSync(Buffer.from(json, 'utf8'))
    // Guard against incompressible payloads (already-compressed base64 etc.)
    return compressed.length < Buffer.byteLength(json, 'utf8') ? compressed : json
}

export function encodeMessageContent(content: unknown): string | Buffer {
    return compressContentJson(JSON.stringify(content))
}

function safeParse(json: string): unknown | null {
    try {
        return JSON.parse(json) as unknown
    } catch {
        return null
    }
}

/** Inverse of encodeMessageContent; tolerant like safeJsonParse (null on any
 *  malformed row) because readers treat content as best-effort display data. */
export function decodeMessageContent(raw: string | Uint8Array | null): unknown | null {
    if (raw === null) return null
    if (typeof raw === 'string') return safeParse(raw)
    try {
        return safeParse(zstdDecompressSync(raw).toString('utf8'))
    } catch {
        return null
    }
}

```

## 1dd71e2f3063 fix(web): keep attached-job progress visible when the label is long (#2392)

The job label, stale marker and progress/elapsed text shared one truncating
span, so a long label hid the parts that change. The label now grows only
into the space the status leaves, down to a small minimum, and the status
truncates only after that.

Closes #2391

## 7680abb0940d feat(web): give every session state its own presence badge (#2296)

The presence badge showed a stopped or archived session as no badge at all,
and told connected-and-waiting from working only by a pulse. Every state now
has its own mark: solid green (pulsing) working, solid blue (pulsing)
background work only, green ring connected and waiting, and a power/standby
mark for stopped or archived. Colours, pulse, layout and settings are
unchanged; shape now carries working vs waiting, and the stopped state is a
mark rather than an absence.

## e262694b0149 fix(web): show cancellation status for dismissed async questions (#2341)

* fix(web): show cancellation status for dismissed async questions

* refactor(web): share permission footer visibility; focus async cancel tests

ToolCard and PermissionFooter each had their own rule for when the footer
renders, which is how the missing Canceled status needed two matching fixes.
Both now use permissionFooterContent(). Tests now cover only this regression
and stop asserting that blocking questions show no cancellation status.
Drop the README note.

## f34ff13fcef5 feat(peer): forward peer replies back to the caller with waitForReply (#2167)

* feat(peer): spawn_peer and reply forwarding for peer sessions

Peer sessions could only fire messages at each other: the caller had to
poll inspect_peer to learn that a peer finished, and nothing woke it once
it ended its turn. There was also no way to start a peer for new work.

- spawn_peer starts a session on the caller's machine (directory and
  agent default to the caller's) and sends it a task.
- ping_peer gains waitForReply.
- In both cases the caller's CLI watches the peer and, when its turn
  ends, posts the peer's latest reply into the caller session, which
  starts the caller's next turn.

spawn_peer needs manual approval like ping_peer. No hub changes.

* refactor(peer): drop own spawn_peer; add waitForReply to upstream spawn_peer

Upstream #1922 ships spawn_peer (spawn, rename, remit delivery, fresh-child
wait, manual approval). Remove this PR's duplicate spawnPeer and keep only
reply forwarding: spawnPeer now returns the remit's ping-peer localId and
resolved flavor, and MCP spawn_peer/ping_peer share one waitForReply hook.
Add a steered-task attribution test.

## eb84dfa0d900 fix(hub): don't crash when an SSE client disconnects while gzip output is buffered (#2314)

compressSseResponse pauses zlib when the client falls behind, so a large
event can leave compressed output parked in zlib's readable buffer. pull()
calls gzip.resume(), which flows that buffer on a later tick. If the client
disconnects in between, cancel() closes the stream and destroys zlib, but
the pending flow still emits 'data' and controller.enqueue() throws
"Invalid state: Controller is already closed". The throw happens inside a
zlib event handler, so nothing catches it and the hub process exits.

Track when the stream has finished (closed, errored or cancelled) and stop
touching the controller after that.

## 3caa3f9e84e2 feat(web): add setting to keep unread sessions visible (#2292)

Since 7b20218c unread sessions are always visible: collapsed projects peek
their unread rows and the preview limit never hides them. Some users prefer
a fully folded list, so add a display setting (on by default, behaviour
unchanged). When off, collapsed projects fold completely and the preview
limit applies to unread rows like any other.

## e360c88e68d4 feat(hub): cache model-list RPCs with single-flight + stale-while-revalidate (#2301)

Model-list endpoints (claude/pi/agy/dsh, machine- and session-scoped)
round-trip an RPC to the runner on every picker open. Even healthy
engines answer in ~1s; a busy one serializes them for 3-13s and a
stalled one stretches past the web server's 30s request cap, surfacing
as opaque gateway timeouts. Catalogs change rarely, so route these
fetches through the same SessionEnumerationCache treatment the
enumeration RPCs already use (#2032):

- concurrent callers share one in-flight fetch; TTL hits answer with
  no RPC; stale values serve immediately while revalidating
- an 8s cold cap returns a typed error long before the request cap
- error bodies and RPC failures propagate — never pinned as last-good
- agy refresh=true bypasses the cache; the machine-agy-models-updated
  realtime event invalidates the entry so event-driven refetches see
  the new catalog cold
- entries are released when a session leaves the active set

Also aligns the dsh/agy/agy-session routes with the existing
rpc_target_missing contract (503 + stable code instead of 500) so the
web can stop hammering dead targets, and moves the pi-models session
route onto the engine wrapper.

## 4e0e1788aa2d feat(web): add an Unread-only option to the session list filter (#2294)

Unread sessions are always visible since 0.34.1, but there was no way to
narrow the list down to just them. Add an off-by-default "Unread only"
checkbox to the sort/filter menu, persisted with the other view prefs.
The open session stays visible (same rule as the status filter), and
groups are not collapsed while the filter is on so no unread row is
hidden. Default behaviour is unchanged.

## b81a043fd2b8 fix(web): drop empty reasoning blocks so tool calls group together

Claude Code stores thinking with an empty body when the content is not
displayed. Each one became an agent-reasoning block that rendered
nothing but still sat between tool calls, so consecutive tools no
longer counted as adjacent and stayed ungrouped.

Skip creating a reasoning block for blank text. A streamed reasoning
block is still created once its first non-empty snapshot arrives.

## 3f54ea53bd59 feat(web): add a concise chat view

Long tasks bury the conversation under hours of tool calls, reasoning,
and progress narration. The concise view hides that work and keeps
only what the user said and each round's final reply.

- A round runs from one user input to the next; agent text after the
  round's last tool call or reasoning is its reply. While a round runs,
  its latest narration reads as live progress until the next tool call.
- Blocks that need the user stay visible: pending permissions
  (including inside subagents), questions, plan proposals, user shell
  commands, errors, terminal API failures, and recaps.
- Toggle from the session header or Settings > Chat; the preference is
  per device and syncs across tabs. Off by default.
- A search jump can target hidden messages, so the full thread shows
  while a search target is active.

## 7b20218c4035 feat(web): keep unread sessions visible and drop the date and unread filters

With sort and filter in place, two toolbar filters no longer earned
their space. The date range filtered on updatedAt while the list orders
by the user's last message, and sort plus search already cover finding
older sessions. The unread toggle was a transient lens that compensated
for unread rows hiding in auto-collapsed projects.

- Remove the date range picker from the session list search and the
  share-target picker.
- Remove the Unread only toggle. Instead, a collapsed project still
  lists its unread sessions under its header, and the preview limit
  never hides an unread row.
- Reorder the toolbar by purpose: list controls next to search, then
  New, then browse and settings. Mark all as read only appears with
  unread sessions, so it moves last and no longer shifts the other
  controls when it comes and goes.

## db7ace3dc379 feat(web): sort and filter the session list (#2263)

* feat(web): sort and filter the session list

The session list had one fixed order, the user's last message, and
active and archived sessions were interleaved inside each project. With
in-progress pinning off by default, a connected session could sit below
a stack of archived ones, and archived sessions could not be listed on
their own.

A sort and filter button in the list toolbar now sets:
- Sort by: last message (the existing default), date created, or recent
  activity including agent output.
- Active first (on by default): connected sessions lead each project,
  and projects with one lead the list. Manual pins still come first.
- Show: all sessions, active only, or archived only. The open session
  stays visible, and archived-only keeps projects expanded instead of
  auto-collapsing them.

Choices persist and stay in sync across consumers and tabs. The Display
setting "Show active sessions only" is now the Active status, and an
existing true value carries over.

## 631e38faaf28 feat(web): archive a session from its row on hover (#2264)

* feat(web): archive a session from its row on hover

Archiving a session took a right-click or long-press, a menu item and a
confirmation. On pointer devices, hovering a connected session's row
now swaps its time/progress cluster for an archive button, the way chat
apps tidy their history. Keyboard focus reveals it too; touch devices
keep the long-press menu.

Archiving stops the session's agent, so the button only skips the
confirmation for an idle session: it archives at once and shows a toast
with Reopen. A session that is replying, waiting on approval or running
background tasks still opens the existing confirmation. Failures are
reported in a toast.

Toasts gain an optional inline action for this, and the archive icon
moves into the shared icon set.

## b4fcddee2fbe feat(android): offer web-parity color theme presets in appearance settings (#2228)

* feat(android): offer web-parity color theme presets in appearance settings

Bring the web client's color-theme catalog (Default plus 16 named
palettes) to Android as an appearance choice. Each preset carries five
seed colors per scheme and derives every other tone with the web
client's exact math — per-channel elevation mixes, the WCAG-luminance
dark/light call, and tie-up rounding — so a preset renders the same on
both clients. A selected preset overrides the dynamic and OLED schemes
(OLED renders the preset's dark seeds, like the web layering), while
Default keeps Material You and the built-in light/dark/OLED schemes.

The picker reuses the expressive choice dialog: every card shows the
preset's light/dark canvas split plus its accent strip, and longer
catalogs scroll inside the dialog so they stay usable on small screens.
Material You switches off with an explanatory note while a preset is
active, since presets pin their own palettes.

Persistence is one DataStore key over the existing theme prefs, with
unknown ids degrading to Default. Strings land in en/ru/zh in matching
order; the proper-noun preset names stay untranslated, matching the web
locales.

## 0de50e6abcc9 feat(android): composer scheduled send with preset and specific picks (#2156)

* feat(android): composer @ mentions with session chips and file hits

Mirror the web composer @-mention autocomplete: an active `@word` at the
composer tail opens a dropdown that merges two sources like the web's
`[...sessionHits, ...fileHits]`:

- Session rows from the cached session list via `matchSessionsForMention`
  (web `sessionReference.ts` port): sidebar dedupe by
  `flavor:agentSessionId`, `hasConversationContent == true` gate so
  empty stubs are never reference targets, current session excluded,
  score tiers (title exact/prefix/contains, id prefix/contains,
  other-field) with active +50 / archived -25, Long-scored with
  updatedAt as tiebreak, limit 20. Insert text is Copy-reference prose
  (`See session "<title>" (/sessions/<id>) for context.` + steer
  suffix) with JSON-quote escaping, whitespace normalization, and
  120-grapheme truncation via BreakIterator.
- File rows via `GET /api/sessions/:id/files` (limit 50) for codex /
  copilot flavors only: copilot inserts `@<fullPath>`, codex inserts
  `@"<fullPath>"` with quote/backslash escaping; RPC failure degrades
  silently to session-only rows (web parity), CancellationException
  rethrown.

Trigger detection ports `findActiveWord.ts` byte-for-byte (hard-stop
scan, `@` boundary rule, `/`+`.` path continuation forward-only, bare
`@` popup, no code-fence exemption), evaluated at the end of the text
(the Android composer is a cursor-less String field - full cursor-aware
mirror kept as `Mentions.findActiveWord`). Selection swaps the whole
word and appends a trailing space (`applySuggestion.ts`).

The `hasConversationContent` gate field already exists on this lineage
as a non-nullable Boolean (absent / JSON null decodes to `false` via
HapiJson coercion, asserted in a new SessionDecodingTest case); the
`searchSessionFiles` seam moves onto `ChatSessionApi` with a defaulted
`limit` parameter. MentionDropdown reuses the SlashCommandDropdown
chrome; new string `chat_mention_suggestions` lands at identical key
positions in en/zh-rCN/ru, and three zh keys a prior merge had parked
at the file tail move back to their en-mirrored positions so all three
locales key-sync exactly.

## 218d99cddffc feat(android): render mermaid fences as offline diagrams (#2161)

* feat(android): render chat markdown math with offline KaTeX

Port the web math pipeline to Android chat markdown. A new
MarkdownMath pre-pass in :core:protocol extracts math with web
semantics (remark-math singleDollarTextMath:false + remark-latex-
bracket-math): $$ display flows, $$ text math only when the closer
run equals the opener and stays inside the paragraph, single $
never triggers math (currency stays prose), \(..\)/\[..\] bracket
pairs, backslash-escaped delimiters stay literal, and code spans,
fences, links and link reference definitions are fully protected.
Matches are lifted behind private-use placeholder tokens and
re-inserted as MathInlineNode/MathBlockNode custom nodes, hoisting
display math out of paragraphs while blockquote/list containers
survive the reparse. 37 JVM tests mirror the web suite's cases.

Rendering is a shared offline WebView (ui/markdown/MathWebView.kt)
fed a self-built HTML document (MathDocument.kt) against bundled
assets/katex/ (KaTeX 0.16.45, same version as web). The view is
network-blocked (blockNetworkLoads + shouldInterceptRequest
refuses http/https/ws/wss) with file/content access disabled;
asset URLs still resolve through the file:///android_asset/katex/
base URL. Theme color and font size are injected per render and
the document reports its height through a JS bridge
(ResizeObserver + onPageFinished re-measure). KaTeX failures
follow the web's three-tier semantics, degrading to monospace
raw TeX.

Inline \(..\) and single-paragraph $$ math degrades to monospace
TeX for v1: AnnotatedString cannot host a WebView and one WebView
per fragment is too costly in chat lists. The parser still emits
MathInlineNode so a future renderer can upgrade inline rendering
without touching the extraction layer.

## fb035051c29c feat(android): context-remaining indicator in composer (#2201)

* Add per-message metadata details popup to chat

Messages that carry usage signal (model, token counters, round cost or
duration) now expose an info entry under the message card; tapping it
opens a dialog listing model/tier, token totals, cached-input share,
USD cost, and round duration/turn count.

- protocol: decode result summaries into turn-summary events; native
  turn durations and token counts gate on top-level scope and merge
  per turn id onto the trailing block group
- protocol: response-group aggregation sums per-turn usage across a
  run of assistant blocks (adjacent-turn dedup, first-seen model
  order, synthesized round durations from the activity window)
- protocol: label builders render the popup lines with pinned number
  formatting (grouped totals, compact k/m counts, USD tiers,
  sub-minute durations); tests mirror the full label contract
- app: entry shows only when the message has labels, dialog follows
  the existing alert idiom, strings in en/ru/zh with aligned key order

## be5f37b9bf7d feat(android): generalize generated media blocks to video/audio/file (#2162)

* feat(android): render chat markdown math with offline KaTeX

Port the web math pipeline to Android chat markdown. A new
MarkdownMath pre-pass in :core:protocol extracts math with web
semantics (remark-math singleDollarTextMath:false + remark-latex-
bracket-math): $$ display flows, $$ text math only when the closer
run equals the opener and stays inside the paragraph, single $
never triggers math (currency stays prose), \(..\)/\[..\] bracket
pairs, backslash-escaped delimiters stay literal, and code spans,
fences, links and link reference definitions are fully protected.
Matches are lifted behind private-use placeholder tokens and
re-inserted as MathInlineNode/MathBlockNode custom nodes, hoisting
display math out of paragraphs while blockquote/list containers
survive the reparse. 37 JVM tests mirror the web suite's cases.

Rendering is a shared offline WebView (ui/markdown/MathWebView.kt)
fed a self-built HTML document (MathDocument.kt) against bundled
assets/katex/ (KaTeX 0.16.45, same version as web). The view is
network-blocked (blockNetworkLoads + shouldInterceptRequest
refuses http/https/ws/wss) with file/content access disabled;
asset URLs still resolve through the file:///android_asset/katex/
base URL. Theme color and font size are injected per render and
the document reports its height through a JS bridge
(ResizeObserver + onPageFinished re-measure). KaTeX failures
follow the web's three-tier semantics, degrading to monospace
raw TeX.

Inline \(..\) and single-paragraph $$ math degrades to monospace
TeX for v1: AnnotatedString cannot host a WebView and one WebView
per fragment is too costly in chat lists. The parser still emits
MathInlineNode so a future renderer can upgrade inline rendering
without touching the extraction layer.

## 480a476e9b90 feat(web): give every harness the same effort control (#2261)

Effort looked and behaved differently per harness. Codex and OpenCode
had no toolbar button and called it "Reasoning effort", Claude called the
unset choice "Auto" while others said "Default", Chinese used both 思考强度
and 推理强度, and the status bar and header showed effort only for
Codex, OpenCode and Pi.

- Codex/OpenCode get the same toolbar effort button as Claude; it opens
  the same anchored Effort section.
- Every effort picker is titled Effort (推理强度), and the unset choice is
  Default everywhere. An unset toolbar button reads "Default effort".
- The status bar, header, shared-turn metadata and Codex agent tool cards
  use one "effort <level>" label. Claude and Grok now show their effort,
  with "default" when unset; Pi and AGY show it only when set, since
  their models may not take one.

## bb4b136f6000 feat(cli): switch Claude effort without restarting the process (#2262)

* feat(cli): switch Claude effort without restarting the process

Changing effort in a remote Claude session made the next prompt restart
Claude with --resume and a new --effort flag, so the switch cost a
process start and a slower first reply. Claude Code's apply_flag_settings
control request changes the session effort in place (null restores the
model default), the same way set_permission_mode already changes modes.

The launcher now applies an effort-only change to the running process
before delivering the prompt. Any other process-level change, a refused
request, or no reply within 5 seconds keeps the existing restart path,
so Claude Code builds without the request still work.

## bd3fb24811fe feat(android): global connection and runner version-skew banners (extracted onto main) (#2205)

* feat(android): global connection and runner version-skew banners

Two Home-screen chrome banners mirroring the web reference banner stack:

- Global connection banner: folds the SSE engine's attributed failure
  notice (reconnectNoticeFailure, 4s grace) with the resync lifecycle
  into one notice - Syncing (300ms debounce, 10s auto-clear cap,
  depth-counted episodes) suppresses Reconnecting, which labels the
  outage reason (heartbeat timeout / visibility recovery / connect
  timeout / transport error with HTTP code / closed). The engine now
  attributes every attempt end into ConnectionState.lastFailure,
  cleared on the next successful handshake.
- Runner version-skew banner: capability-set comparison
  (RunnerSkew.REQUIRED_MACHINE_CAPABILITIES, not semver) flags runners
  missing agent-availability/cursor-chat-store-status; expanded card +
  minimized chip, snooze 1h, manual restart via new
  POST /api/machines/:id/restart-runner (restart gated on
  newer-binary-on-disk mtime drift AND supervised restart).

StoreSyncTargets grows an onResyncEvent lifecycle hook (default no-op)
feeding the syncing half; HomeScreen grows optional chrome params
(null hides each banner).

Declared divergences from web/src:
- Banners mount on Home only, not above every route: an open chat
  already surfaces its own pipe state ("Reconnecting... Tap to retry"),
  so the two never double up on one screen (web mounts App-wide).
- The web OfflineBanner is not ported: a native app has no browser
  online/offline event pair; hub reachability is expressed through the
  reconnecting banner.
- Minimize/snooze state is process-lifetime memory (web: sessionStorage
  - same session-scoped lifetime per platform); the snooze timer does
  not survive process death.
- Runner rows filter only on capability skew; the active filter lives
  upstream in MachineStore (web filters machine.active on its side).

## 9c6639c3c3f9 feat(hub): title untitled sessions with their first prompt (#2252)

New sessions showed the working directory name until the first turn
finished and a generated or native title arrived. The Hub now records
the first non-command user prompt as metadata.promptTitle, with
whitespace collapsed and capped at 80 characters, and the web shows it
after name and summary but before the path fallback.

The prompt title is Hub-owned and kept outside summary, so the existing
"only while untitled" title sources (native ai-title, change_title, the
remote prompt fallback and automatic suggestions) still apply and
replace it. It is published as a versioned metadata patch, so it does
not race the thinking patch sent while the prompt is accepted.

## 3f8686e623d7 feat(claude): offer the four current Claude models, drop the 1M presets (#2153)

* fix(claude): drop the obsolete 1M model presets

The opus, sonnet and fable aliases now resolve to models with a native 1M
window (Opus 5.5, Sonnet 5.5, Fable 5.1), so Claude Code documents the
"[1m]" suffix as having no effect on them and its own /model picker no
longer lists 1M rows. Remove Sonnet 1M / Opus 1M / Fable 1M from the
shared, Android and iOS catalogs.

Sessions stored with a "[1m]" value keep working: the composer keeps
the current model selectable, and the context-budget fallback treats any
"[1m]" model and the remaining aliases as 1M, so local-mode sessions on
plain opus/sonnet no longer show a 200k budget.

## 6ccde085483b feat(android): render settings pickers as expressive choice dialogs (#2227)

* feat(android): anchor the Material type scale on the shared reading tokens

MaterialTheme was built with only a colorScheme, leaving HapiTypography's
body/code/caption tokens an island that never influenced Material
typography. Derive the app-wide Material type scale from those tokens so
Material widgets and direct token consumers share one type system: the
body token drives bodyLarge (the chat reading size) and the caption token
drives bodySmall (the metadata size), while every other slot keeps its
Material platform default so existing MaterialTheme.typography call sites
stay pixel-identical. Only size and leading come from the tokens; weight,
tracking and family stay Material's. HapiTheme now accepts a typography
argument (defaulting to the shared scale) and passes it through, and the
monospace code token stays a direct-consumer token because Material has
no code slot.

## b4be7c2d9307 fix(cursor): stop acknowledging soft steers that an abort cancels (#2229)

Abort sends session/cancel, and ACP answers the pending soft-steer
session/prompt with stopReason "cancelled". The completion handler
ignored the stopReason and acknowledged the row as steered, so a
message Cursor may never have run was shown as delivered.

Surface the stopReason from beginSoftSteerPrompt. On "cancelled" the
row is held as indeterminate (the same path a transport failure takes),
because Cursor may or may not have folded it into the cancelled turn:
the user resolves it with Retry or Cancel instead of a silent claim or
an automatic re-run after Stop.

## 243466da040e fix(ios): keep transcript rows in sync with environment changes on iOS 27

On iOS 27, SwiftUI re-runs a representable's update only when an
environment value read during it changes; even @Environment(\.self) is
tracked per key. AnchoredTranscriptList copied context.environment
without reading keys, and compared metrics later in a drain that is
often deferred. A theme switch or a system text size change therefore
never reached the hosted rows: text kept its old size and colors.

updateUIViewController now reads every runtime-changing value the rows
follow (Dynamic Type, bold text, typography, locale, direction, color
scheme, theme, link action, enabled state). Rows still receive the
complete environment snapshot.

The tests drive Dynamic Type through UIKit traits, as the system does.
On iOS 27 a SwiftUI dynamicTypeSize override does not reach a UIKit
representable's environment, and the app never sets one above the
list. A new test checks that theme-only and text-size-only changes
reach visible rows without reconfiguring cells; it fails without the
fix.

## 4b7a1cff90bb ci(ios): build and test with Xcode 27

The iPhone Duo layout uses UIHingeInteraction, which only exists in
the iOS 27.1 SDK. CI ran Xcode 16.4 (iOS 18.5 SDK) on macos-15, so the
app build failed: #available is a runtime check and cannot hide a
symbol the SDK lacks. ChatView's iOS 26/27 modifiers would have failed
next.

Both jobs now run on the xcode-27 runner image (Xcode 27.1 by default),
the toolchain releases are built with. The READMEs state Xcode 27.1 as
the minimum.

Swift 6.4 also timed out type-checking a long `+` chain in
MultipartEncoderTests, so `swift test` failed to build; the expected
body is now joined from an array.

## 132d25617132 feat(ios): fork, rewind and export conversations

The hub's fork, rewind and export routes had no iOS UI. The chat's
menu now offers:

- Fork Conversation: when the agent advertises forkCurrent, the
  session is live and no Codex turn is running (the web gate). The new
  session opens in place of this chat.
- Rewind or Fork…: a sheet listing loaded user messages that have a
  native point in metadata.conversationHistoryPoints. Each row offers a
  confirmed Fork from Here or Rewind to Here, gated like web: forks
  need an idle turn except on Codex, and rewinds always need one. The
  actions live in a sheet, not on the transcript bubbles, because those
  keep text selection and cached row heights. After a rewind the tail
  resyncs with a history reset. An ambiguous native boundary (409
  ambiguous_native_boundary_fork_safe) offers a fork from the same
  point instead.
- Export Conversation…: saves the hub's lossless JSON payload through
  the Files save panel, using the web download's filename. A large
  export asks before retrying with force; a 413 explains the limit.
  Markdown export stays web-only.

## 423c5f51df99 feat(ios): schedule sends from the composer

iOS showed scheduled messages but could not create them. A clock
button now sits next to the composer's attachment button. It offers
the web presets (in 5 min, 30 min, 1 h, 4 h) or a specific time up to 7
days ahead. A chip above the input shows the pending time; its × sends
normally again.

Presets resolve when the user taps Send, so "in 5 minutes" counts from
the send. Validation follows the web: a 30 s grace period for past
times and a 7-day maximum.

A scheduled send is text-only and queued. The hub rejects scheduledAt
with attachments or steer, so the clock is disabled while attachments
are present and the steer menu is hidden. The schedule clears once the
send dispatches. Sending reuses the existing optimistic path, and a
retry keeps the row's scheduledAt.

## f7722fcd65b1 feat(ios): suggest slash commands in the composer

Typing / in the iOS composer offered nothing; web and Android show the
session's commands. While the input is a lone /token, a list above the
card shows matching commands. A tap inserts "/name " so the user can
add arguments.

As on Android, the sources are metadata.slashCommands plus the
/slash-commands RPC. The RPC is fetched once, when the input first
enters slash mode, and its entries override bare names. A failed fetch
retries on the next /. There is no copy of the shared built-in table,
so the CLI stays the source of truth. Ranking follows web: exact,
prefix, contains, then near-miss typos. The list is hidden in
Scratchlist mode.

## 64e062fb84dd feat(ios): search sessions, unread-only lens and mark all read

The iOS session list could only filter by machine. Web also has text
search, an unread-only filter and Mark All as Read.

- Search: a .searchable field ranks matches with the web relevance
  rules, ported from sessionListSearch.ts via Android as
  SessionListSearch. It searches title, worktree, summary, agent,
  machine, path and id; every word must match, with */? wildcards.
  Results keep the global/project pin sections, so the Pinned header
  still applies.
- Filters menu:
  - It now holds an Unread Only toggle that keeps the iPad's selected
    chat listed.
  - The machine section appears only with two or more machines, as
    before; the menu itself shows whenever the hub has sessions.
  - Mark All as Read advances the local last-seen watermarks after a
    confirmation, as on web.
- Empty list: an empty search or filter result gets its own empty state
  instead of "No sessions yet".

getSessionTitle moves into HapiKit so search and the rows share it.

## 1efcc3183d3f feat(ios): switch models for every agent in session settings

The settings sheet only had model pickers for Claude and Codex; other
agents could not change models after creation. Live sessions now load
/sessions/:id/<flavor>-models. Agy uses its machine catalog, since it
has no session route. As on web, grok and copilot skip
terminal-controlled sessions.

Rows follow web getModelOptionsForFlavor:
- Copilot, dsh, grok and kimi offer a Default or Auto row that clears
  the model.
- Cursor's Auto is the explicit 'auto' wire id.
- Opencode, agy and pi only switch between listed models.
- An unlisted current model stays visible.

Pi resolves provider/modelId from metadata.piSelectedModel and switches
with a {provider, modelId} reference.

Efforts:
- Grok and OpenCode options come from their *-reasoning-effort-options
  routes and reload after a model switch. OpenCode options that still
  describe the previous model stay hidden.
- Pi shows the current model's thinking levels.

The sheet's load-failure flag now covers every catalog, not just Codex.

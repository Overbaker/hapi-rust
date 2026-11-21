/**
 * Keepalive-idle reconciliation (tiann/hapi#1820).
 *
 * `sessionCache.expireInactive` only expires a session when its `activeAt`
 * goes stale, and `session-alive` refreshes `activeAt` every couple of
 * seconds for as long as the CLI socket lives. A session whose agent has done
 * nothing for days therefore stays `active: true` / `lifecycleState: running`
 * forever: socket pinging is being read as agent health.
 *
 * This module supplies the missing second signal. `active` keeps its existing
 * meaning (the CLI is reachable — flipping it would arm resume-respawn, dedup
 * merge and delete paths against a session whose process is very much alive);
 * `lifecycleState` moves `running` → `idle` once we have seen nothing but
 * keepalives for the configured window.
 */

import { SESSION_LIFECYCLE_IDLE, SESSION_LIFECYCLE_RUNNING, isLiveLifecycleState } from '@hapi/protocol'
import type { Session } from '@hapi/protocol/types'

/** Keepalive-only for this long ⇒ reconcile `running` → `idle`. */
export const DEFAULT_SESSION_IDLE_TIMEOUT_MS = 12 * 60 * 60 * 1000

/**
 * `HAPI_SESSION_IDLE_TIMEOUT_MS` window, in ms. `0` disables reconciliation
 * entirely; an unset or unparseable value falls back to the default.
 */
export function resolveSessionIdleTimeoutMs(
    env: Record<string, string | undefined> = process.env
): number {
    const raw = env.HAPI_SESSION_IDLE_TIMEOUT_MS
    if (raw === undefined || raw.trim() === '') {
        return DEFAULT_SESSION_IDLE_TIMEOUT_MS
    }
    const parsed = Number.parseInt(raw.trim(), 10)
    if (!Number.isFinite(parsed) || parsed < 0) {
        return DEFAULT_SESSION_IDLE_TIMEOUT_MS
    }
    return parsed
}

/**
 * Hub-side work signals that do not live on the Session row itself. The
 * keepalive-idle reconciler passes none; hibernation passes all of them.
 */
export type LiveWorkSignals = {
    peerReplyWatch?: boolean
    /** Queued-prompt thinking grace (`markMessageQueued`) still running. */
    pendingThinking?: boolean
    /** Attached session job currently running. */
    runningJob?: boolean
    /** Active team member or in-progress team task. */
    activeTeamWork?: boolean
    /** Open web terminals on this session. */
    openTerminals?: number
    /** Queued or scheduled messages not yet delivered. */
    undeliveredMessages?: number
    /** resume / archive / history action / pty or pi resume in flight. */
    operationInFlight?: boolean
}

/** Why a session still has live work, or null when it has none. */
export function liveWorkReason(session: Session, signals?: LiveWorkSignals): string | null {
    if (session.thinking) return 'mid_turn'
    if (session.derivedToolWork === true) return 'tool_work_running'
    if ((session.backgroundTaskCount ?? 0) > 0) return 'background_tasks_running'
    // A permission / input request waiting on the operator is real state the
    // agent is blocked on, not an abandoned socket.
    const requests = session.agentState?.requests
    if (requests && Object.keys(requests).length > 0) return 'pending_request'
    if (!signals) return null
    if (signals.pendingThinking) return 'mid_turn'
    if (signals.peerReplyWatch) return 'peer_reply_watch'
    if (signals.runningJob) return 'job_running'
    if (signals.activeTeamWork) return 'team_work_active'
    if ((signals.openTerminals ?? 0) > 0) return 'terminal_open'
    if ((signals.undeliveredMessages ?? 0) > 0) return 'undelivered_messages'
    if (signals.operationInFlight) return 'operation_in_flight'
    return null
}

/** Work the hub can see that must never be reconciled away as a zombie. */
export function hasLiveWork(session: Session, signals?: LiveWorkSignals): boolean {
    return liveWorkReason(session, signals) !== null
}

/** Active sub-agent member or a team task still in progress. */
export function hasActiveTeamWork(session: Session): boolean {
    const team = session.teamState
    if (!team) return false
    if (team.members?.some((member) => member.status === 'active')) return true
    if (team.tasks?.some((task) => task.status === 'in_progress')) return true
    return false
}

/**
 * Should this session be reconciled `running` → `idle` right now?
 *
 * `agentProgressAt` is the last time the hub observed actual agent progress
 * (a message in either direction, a queued prompt, a background task), NOT
 * the last keepalive.
 */
export function shouldMarkKeepaliveIdle(
    session: Session,
    agentProgressAt: number,
    now: number,
    timeoutMs: number
): boolean {
    if (timeoutMs <= 0) return false
    // Only sessions the hub still believes are live. An inactive row is #842's
    // problem, not this one.
    if (!session.active) return false
    if (session.metadata?.lifecycleState !== SESSION_LIFECYCLE_RUNNING) return false
    if (session.metadata.idleReconcileExempt === true) return false
    if (hasLiveWork(session)) return false
    return now - agentProgressAt > timeoutMs
}

/**
 * Should an already-`idle` session be reconciled back to `running`?
 *
 * Only real progress wakes a session, never `thinking`. Cursor ACP emits
 * ambient idle/running churn on sessions that are doing nothing
 * (tiann/hapi#1553); waking on it would flap the lifecycle — and a DB write
 * plus a full-session broadcast — on every 5 s tick. A session that is
 * genuinely working emits messages, and those do wake it.
 */
export function shouldClearKeepaliveIdle(
    session: Session,
    agentProgressAt: number,
    now: number,
    timeoutMs: number
): boolean {
    if (session.metadata?.lifecycleState !== SESSION_LIFECYCLE_IDLE) return false
    // Opting out after the fact still lifts an existing mark.
    if (session.metadata.idleReconcileExempt === true) return true
    return timeoutMs > 0 && now - agentProgressAt <= timeoutMs
}

/** Keepalive-only with no agent progress for this long ⇒ hibernate. */
export const DEFAULT_SESSION_HIBERNATE_MS = 24 * 60 * 60 * 1000
export const MIN_SESSION_HIBERNATE_MS = 60_000

/**
 * `HAPI_SESSION_HIBERNATE_MS` window, in ms. `0` disables hibernation; unset
 * or unparseable falls back to the default; 1..59999 is raised to 60000.
 */
export function resolveSessionHibernateMs(
    env: Record<string, string | undefined> = process.env
): number {
    const raw = env.HAPI_SESSION_HIBERNATE_MS
    if (raw === undefined) return DEFAULT_SESSION_HIBERNATE_MS
    const trimmed = raw.trim()
    if (!/^\d+$/.test(trimmed)) return DEFAULT_SESSION_HIBERNATE_MS
    const parsed = Number(trimmed)
    if (!Number.isSafeInteger(parsed)) return DEFAULT_SESSION_HIBERNATE_MS
    if (parsed === 0) return 0
    return Math.max(parsed, MIN_SESSION_HIBERNATE_MS)
}

export const HIBERNATE_ELIGIBLE_FLAVORS = ['claude', 'codex', 'opencode', 'pi', 'agy'] as const

export type HibernateEligibleFlavor = typeof HIBERNATE_ELIGIBLE_FLAVORS[number]

export function isHibernateEligibleFlavor(flavor: unknown): flavor is HibernateEligibleFlavor {
    return typeof flavor === 'string' && (HIBERNATE_ELIGIBLE_FLAVORS as readonly string[]).includes(flavor)
}

/** Live-work signals plus the resolved native resume token (rule 9). */
export type HibernationSignals = LiveWorkSignals & {
    resumeId: string | null
}

/** Subset of `RunnerState.capabilities` hibernation reads. */
export type HibernationRunnerCapabilities = {
    sessionHibernation?: boolean
    piExistingSessionResume?: boolean
} | null | undefined

function isPtySession(session: Session): boolean {
    const agentStartingMode = (session.agentState as { startingMode?: unknown } | null)?.startingMode
    return agentStartingMode === 'pty' || session.metadata?.startingMode === 'pty'
}

function hasFlavorBlocker(
    session: Session,
    flavor: HibernateEligibleFlavor,
    runnerCapable: HibernationRunnerCapabilities
): boolean {
    const metadata = session.metadata
    if (!metadata) return true
    switch (flavor) {
        case 'codex':
            return metadata.codexRuntime?.kind === 'attached'
                || metadata.codexArchivedFork !== undefined
                || metadata.codexResumePending !== undefined
        case 'opencode':
            return ['reserved', 'prepared', 'unknown'].includes(metadata.opencodeRewind?.phase ?? '')
                || metadata.opencodeForkPending === true
                || (metadata.opencodeClearOperation !== undefined && metadata.opencodeClearOperation.state !== 'aborted')
        case 'pi':
            return runnerCapable?.piExistingSessionResume !== true
                || metadata.piResumeAttempt !== undefined
        case 'agy':
            // Only `agy acp` resumes a stored conversation in place (`session/resume`).
            return metadata.agyProtocol !== 'acp'
        case 'claude':
            return false
    }
}

/**
 * Why this session's wrapper and agent must not be stopped and the row marked
 * `hibernated` right now, or null when it may. `ignoreIdleWindow` is the
 * operator "hibernate now" path: same checks, minus the switch and the window.
 *
 * `agentProgressAt` must already be `max(getAgentProgressAt, lifecycleStateSince)`.
 */
export function hibernationRefusal(
    session: Session,
    signals: HibernationSignals | (() => HibernationSignals),
    agentProgressAt: number,
    now: number,
    hibernateMs: number,
    runnerCapable: HibernationRunnerCapabilities,
    opts: { ignoreIdleWindow?: boolean } = {}
): string | null {
    if (!opts.ignoreIdleWindow && !(hibernateMs > 0)) return 'hibernation_disabled'
    if (!session.active) return 'not_active'
    const metadata = session.metadata
    if (!metadata || !isLiveLifecycleState(metadata.lifecycleState)) return 'not_live'
    if (metadata.replayableRunnerLaunch !== true) return 'launch_not_replayable'
    if (metadata.capabilities?.concurrentClients === true) return 'concurrent_clients'
    if (metadata.idleReconcileExempt === true) return 'idle_reconcile_exempt'
    if (metadata.startedBy !== 'runner' && metadata.startedFromRunner !== true) return 'not_runner_started'
    if (session.agentState?.controlledByUser === true) return 'controlled_by_user'
    if (isPtySession(session)) return 'pty_session'
    const flavor = metadata.flavor ?? 'claude'
    if (!isHibernateEligibleFlavor(flavor)) return 'flavor_unsupported'
    if (hasFlavorBlocker(session, flavor, runnerCapable)) return 'flavor_operation_pending'
    if (runnerCapable?.sessionHibernation !== true) return 'runner_unsupported'
    if (!opts.ignoreIdleWindow && !(now - agentProgressAt > hibernateMs)) return 'not_idle_long_enough'
    const sessionWork = liveWorkReason(session)
    if (sessionWork) return sessionWork
    const liveSignals = typeof signals === 'function' ? signals() : signals
    if (!liveSignals.resumeId) return 'no_resume_id'
    return liveWorkReason(session, liveSignals)
}

/**
 * Should this session's wrapper and agent be stopped and the row marked
 * `hibernated` right now?
 *
 * `agentProgressAt` must already be `max(getAgentProgressAt, lifecycleStateSince)`.
 */
export function shouldHibernate(
    session: Session,
    signals: HibernationSignals | (() => HibernationSignals),
    agentProgressAt: number,
    now: number,
    hibernateMs: number,
    runnerCapable: HibernationRunnerCapabilities
): boolean {
    return hibernationRefusal(session, signals, agentProgressAt, now, hibernateMs, runnerCapable) === null
}

const HIBERNATION_REFUSAL_MESSAGES: Record<string, string> = {
    access_denied: 'Session access denied',
    session_not_found: 'Session not found',
    already_hibernated: 'Session is already hibernated',
    hibernation_in_progress: 'A hibernation stop is already in progress',
    hibernation_disabled: 'Automatic hibernation is disabled',
    backoff: 'A recent hibernation attempt failed; retry later',
    machine_offline: 'The session runner machine is offline',
    startup_grace: 'The hub has just started',
    no_worker_report: 'No recent heartbeat from the session wrapper',
    upload_active: 'A file upload is in progress',
    worker_mismatch: 'The heartbeating wrapper does not match the session owner',
    not_active: 'Session is not active',
    not_live: 'Session is not running or idle',
    launch_not_replayable: 'No replayable runner launch is recorded for this session',
    concurrent_clients: 'Session accepts concurrent clients',
    idle_reconcile_exempt: 'Session is exempt from idle reconciliation',
    not_runner_started: 'Session was not started by a runner',
    controlled_by_user: 'Session is controlled from a local terminal',
    pty_session: 'PTY sessions cannot be hibernated',
    flavor_unsupported: 'This agent flavor cannot be hibernated',
    flavor_operation_pending: 'An agent-specific operation (fork, rewind, clear, resume) is pending',
    runner_unsupported: 'The runner does not support session hibernation',
    not_idle_long_enough: 'Session has not been idle long enough',
    mid_turn: 'The agent is mid-turn',
    tool_work_running: 'A tool call is running',
    background_tasks_running: 'Background tasks are running',
    pending_request: 'A permission or input request is pending',
    peer_reply_watch: 'A peer-reply watch is pending',
    job_running: 'A session job is running',
    team_work_active: 'Team members or tasks are active',
    terminal_open: 'A web terminal is open',
    undelivered_messages: 'Messages are queued or scheduled',
    operation_in_flight: 'Another session operation is in flight',
    no_resume_id: 'No native resume id is known, so the session could not be woken',
    no_machine: 'Session has no runner machine',
    no_directory: 'No working directory is recorded for this session',
    resume_unavailable: 'The ordinary resume path cannot resume this session',
    stop_target_unavailable: 'The runner is not connected for stop requests',
    stop_not_confirmed: 'The runner did not confirm the stop',
    version_mismatch: 'Session metadata changed concurrently; re-read and retry',
    eligibility_check_failed: 'Eligibility check failed',
    no_metadata: 'Session has no metadata',
    session_active: 'Session is active; use hibernate instead',
    not_archived: 'Session is not archived',
    archived_by_user: 'Session was archived by a user or the web, not by a wrapper signal',
    superseded: 'Session was superseded by another session',
    machine_unknown: 'The session runner machine is unknown',
    update_failed: 'Metadata update failed',
    not_eligible: 'Session is not eligible for hibernation',
}

export function describeHibernationRefusal(code: string): string {
    return HIBERNATION_REFUSAL_MESSAGES[code] ?? code
}

export function hibernationRefusalStatus(code: string): 403 | 404 | 409 {
    return code === 'access_denied' ? 403 : code === 'session_not_found' ? 404 : 409
}

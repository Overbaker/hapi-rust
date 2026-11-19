/**
 * Periodic inventory of live runner-spawned HAPI driver wrappers
 * (agent-session-orchestration P2).
 *
 * Every wrapper that claims `--started-by runner` lands in exactly one bucket:
 * tracked (the runner already owns its PID), adopted (untracked but carries a
 * HAPI session id and a same-snapshot start marker, so the runner can stop it
 * by id), or reported. Nothing here ever kills a process.
 */

import { readlinkSync, realpathSync, statSync } from 'node:fs'
import { resolveRustExecutable } from './wrapperRuntime'
import type { RunnerState } from '@/api/types'
import { logger } from '@/ui/logger'
import { isBunCompiled } from '@/projectPath'
import { isProcessAlive } from '@/utils/process'
import { resolveHappyCliExecutable } from '@/utils/spawnHappyCLI'
import {
    extractHapiSessionIdFromCommand,
    listProcessesForOrphanScan,
    RUNNER_STARTED_BY_PATTERN,
    type ProcessSnapshot,
} from './orphanReap'

export type WrapperInventory = NonNullable<RunnerState['wrapperInventory']>
export type WrapperInventoryReportedItem = WrapperInventory['reported'][number]

export const WRAPPER_INVENTORY_INTERVAL_MS = 10 * 60_000
export const WRAPPER_INVENTORY_REPORTED_MAX = 200

export type WrapperDrift = 'deleted' | 'mismatch'

export type WrapperCandidate = {
    pid: number
    cmd: string
    sessionId: string | null
    startMarker: string | null
    wrapperRuntime: 'rust' | 'ts'
    flavor: string | null
    /** Parent is another live HAPI process (not this runner, not init / a non-HAPI subreaper). */
    foreignParent: boolean
}

export type AdoptableWrapper = WrapperCandidate & { sessionId: string; startMarker: string }

export type WrapperClassification = {
    tracked: WrapperCandidate[]
    adopted: WrapperCandidate[]
    reported: WrapperInventoryReportedItem[]
    /** Untracked wrappers with id + marker to adopt (adopted plus drifted ones). */
    adopt: WrapperCandidate[]
}

function isMcpBridgeCommand(cmd: string): boolean {
    return /(?:^|\s)mcp(?:\s|$)/.test(cmd) && /(?:^|\s)--url(?:\s+|=)/.test(cmd)
}

const HAPI_BINARY_BASENAMES = new Set(['hapi', 'hapi.exe', 'hapi-session-wrapper'])

/** Quote-aware argv split: a quoted span (with spaces) stays one token, quotes removed. */
export function splitCommandLine(cmd: string): string[] {
    return (cmd.match(/(?:"[^"]*"|'[^']*'|[^\s"'])+/g) ?? []).map((token) => token.replace(/["']/g, ''))
}

function argvBasename(token: string): string {
    const unquoted = token.replace(/^["']|["']$/g, '')
    return (unquoted.split(/[\\/]/).pop() ?? '').toLowerCase()
}

const RUNTIME_BASENAMES = new Set(['bun', 'bun.exe', 'node', 'node.exe'])
const DEV_ENTRYPOINT = /(?:^|[\\/])src[\\/]index\.ts["']?$/

/**
 * Index of the argv token that is the HAPI driver entry: argv[0] for the
 * compiled `hapi` binary, or the `src/index.ts` entry when argv[0] is bun/node.
 * Null for anything else, including the npm `bin/hapi.cjs` node launcher.
 */
export function hapiDriverEntryIndex(argv: string[], name = ''): number | null {
    const namedDriver = name === 'hapi-session-wr' || HAPI_BINARY_BASENAMES.has(argvBasename(name))
    const head = argvBasename(argv[0] ?? '')
    if (!namedDriver && RUNTIME_BASENAMES.has(head)) {
        const index = argv.findIndex((token, i) => i > 0 && DEV_ENTRYPOINT.test(token))
        return index > 0 ? index : null
    }
    if (!namedDriver && name && !HAPI_BINARY_BASENAMES.has(head)) return null
    let entryIndex = namedDriver ? 0 : null
    for (const [index, token] of argv.entries()) {
        if (token.startsWith('-')) break
        if ((namedDriver || index === 0 || /[\\/]/.test(token)) && HAPI_BINARY_BASENAMES.has(argvBasename(token))) {
            entryIndex = index
        }
    }
    return entryIndex
}

/** First positional token after the driver entry (`hapi claude …` → `claude`). */
export function extractFlavorFromArgv(argv: string[], entryIndex: number): string | null {
    const token = argv[entryIndex + 1]
    if (!token || !/^[a-z][a-z0-9-]*$/.test(token)) return null
    return token
}

function isHapiProcessCommand(cmd: string, name = ''): boolean {
    const argv = splitCommandLine(cmd)
    if (hapiDriverEntryIndex(argv, name) !== null) return true
    const head = argv[0] ? argvBasename(argv[0]) : ''
    return RUNTIME_BASENAMES.has(head) && argv.some((token) => /(?:^|[\\/])bin[\\/]hapi\.cjs$/.test(token))
}

/**
 * True when the wrapper's parent is a live HAPI process other than this runner
 * (e.g. a second runner under another HAPI_HOME). PPID 0/1, a parent missing
 * from the snapshot, or a non-HAPI subreaper means the wrapper was orphaned.
 */
function hasForeignParent(
    proc: ProcessSnapshot,
    byPid: Map<number, ProcessSnapshot>,
    self: number,
    handoffFromPid?: number
): boolean {
    const ppid = proc.ppid
    if (typeof ppid !== 'number' || !Number.isFinite(ppid) || ppid <= 1 || ppid === self || ppid === handoffFromPid) return false
    const parent = byPid.get(ppid)
    if (!parent) return false
    if (parent.startMarker && proc.startMarker && Date.parse(parent.startMarker) > Date.parse(proc.startMarker)) return false
    return isHapiProcessCommand(parent.cmd || '', parent.name || '')
}

/** Runner-started HAPI driver wrappers in one process snapshot, owned by selfUid. */
export function enumerateRunnerWrappers(
    processes: ProcessSnapshot[],
    selfPid: number = process.pid,
    selfUid: number | null = typeof process.getuid === 'function' ? process.getuid() : null,
    handoffFromPid?: number
): WrapperCandidate[] {
    const self = Number(selfPid)
    const wrappers: WrapperCandidate[] = []
    const seen = new Set<number>()
    const byPid = new Map<number, ProcessSnapshot>()
    for (const proc of processes) {
        const pid = typeof proc.pid === 'number' ? proc.pid : Number(proc.pid)
        if (Number.isFinite(pid) && !byPid.has(pid)) byPid.set(pid, proc)
    }
    for (const proc of processes) {
        const pid = typeof proc.pid === 'number' ? proc.pid : Number(proc.pid)
        if (!Number.isFinite(pid) || pid <= 0 || pid === self || seen.has(pid)) continue
        if (selfUid !== null && typeof proc.uid === 'number' && proc.uid !== selfUid) continue
        const cmd = proc.cmd || ''
        if (!RUNNER_STARTED_BY_PATTERN.test(cmd)) continue
        if (isMcpBridgeCommand(cmd)) continue
        const argv = splitCommandLine(cmd)
        const entryIndex = hapiDriverEntryIndex(argv, proc.name || '')
        if (entryIndex === null) continue
        const flavor = extractFlavorFromArgv(argv, entryIndex)
        if (flavor === 'hook-forwarder') continue
        seen.add(pid)
        wrappers.push({
            pid,
            cmd,
            sessionId: extractHapiSessionIdFromCommand(cmd),
            startMarker: proc.startMarker ?? null,
            flavor,
            wrapperRuntime: proc.name === 'hapi-session-wr' || proc.name === 'hapi-session-wrapper' || argvBasename(argv[entryIndex] ?? '') === 'hapi-session-wrapper' ? 'rust' : 'ts',
            foreignParent: hasForeignParent(proc, byPid, self, handoffFromPid),
        })
    }
    return wrappers
}

export type FileIdentity = { dev: number; ino: number }

export type DriftProbeDeps = {
    platform?: NodeJS.Platform
    compiled?: boolean
    readExeLink?: (pid: number) => string | null
    statExe?: (pid: number) => FileIdentity | null
    /** Identity of the binary spawnHappyCLI would run now; null when unresolvable. */
    currentRustExecutable?: () => FileIdentity | null
    currentExecutable?: () => FileIdentity | null
}

function defaultReadExeLink(pid: number): string | null {
    try {
        return readlinkSync(`/proc/${pid}/exe`)
    } catch {
        return null
    }
}

function defaultStatExe(pid: number): FileIdentity | null {
    try {
        const stat = statSync(`/proc/${pid}/exe`)
        return { dev: stat.dev, ino: stat.ino }
    } catch {
        return null
    }
}

function currentIdentity(path: string | null): FileIdentity | null {
    if (!path) return null
    try {
        const stat = statSync(realpathSync(path))
        return { dev: stat.dev, ino: stat.ino }
    } catch {
        return null
    }
}

/** Linux-only binary drift of a wrapper versus the binary spawnHappyCLI runs now. */
export function detectBinaryDrift(wrapper: { pid: number; cmd: string; wrapperRuntime?: 'rust' | 'ts' }, deps: DriftProbeDeps = {}): WrapperDrift | null {
    if ((deps.platform ?? process.platform) !== 'linux') return null
    const exe = (deps.readExeLink ?? defaultReadExeLink)(wrapper.pid)
    if (!exe) return null
    const rust = wrapper.wrapperRuntime === 'rust' || argvBasename(exe.replace(/ \(deleted\)$/, '')) === 'hapi-session-wrapper'
    if (!rust && (wrapper.cmd.includes('src/index.ts') || !(deps.compiled ?? isBunCompiled()))) return null
    const current = rust
        ? (deps.currentRustExecutable ?? (() => currentIdentity(resolveRustExecutable())))()
        : (deps.currentExecutable ?? (() => currentIdentity(resolveHappyCliExecutable())))()
    if (!current) return null
    if (exe.endsWith(' (deleted)')) return 'deleted'
    const identity = (deps.statExe ?? defaultStatExe)(wrapper.pid)
    if (!identity) return null
    if (identity.dev !== current.dev || identity.ino !== current.ino) return 'mismatch'
    return null
}

export function classifyWrappers(
    wrappers: WrapperCandidate[],
    deps: { isTracked: (pid: number) => boolean; detectDrift: (wrapper: WrapperCandidate) => WrapperDrift | null }
): WrapperClassification {
    const result: WrapperClassification = { tracked: [], adopted: [], reported: [], adopt: [] }
    for (const wrapper of wrappers) {
        const tracked = deps.isTracked(wrapper.pid)
        const blocker = tracked ? null
            : wrapper.sessionId === null ? 'no_session_id'
                : wrapper.startMarker === null ? 'no_start_marker'
                    : wrapper.foreignParent ? 'foreign_runner'
                        : null
        const adoptable = !tracked && blocker === null
        const drift = deps.detectDrift(wrapper)
        if (drift) {
            result.reported.push({
                pid: wrapper.pid,
                ...(wrapper.sessionId !== null ? { sessionId: wrapper.sessionId } : {}),
                drift,
                reason: 'drift',
                ...(blocker ? { adoptOutcome: blocker } : {}),
            })
            if (adoptable) result.adopt.push(wrapper)
        } else if (tracked) {
            result.tracked.push(wrapper)
        } else if (wrapper.sessionId === null) {
            result.reported.push({ pid: wrapper.pid, reason: 'no_session_id' })
        } else if (wrapper.startMarker === null) {
            result.reported.push({ pid: wrapper.pid, sessionId: wrapper.sessionId, reason: 'no_start_marker' })
        } else if (wrapper.foreignParent) {
            result.reported.push({ pid: wrapper.pid, sessionId: wrapper.sessionId, reason: 'foreign_runner' })
        } else {
            result.adopted.push(wrapper)
            result.adopt.push(wrapper)
        }
    }
    return result
}

export type WrapperInventoryDeps = {
    listProcesses?: () => Promise<ProcessSnapshot[]>
    handoffFromPid?: number
    selfPid?: number
    selfUid?: number | null
    isTracked: (pid: number) => boolean
    /** True when another tracked/persisted/requested entry maps sessionId to a different live PID. */
    hasOtherLiveOwner?: (sessionId: string, pid: number) => boolean
    /** process.kill(pid, 0) succeeds (POSIX); defaults to isProcessAlive. */
    canSignal?: (pid: number) => boolean
    /** Returns true only when the wrapper was actually adopted. */
    adopt: (wrapper: AdoptableWrapper) => boolean
    detectDrift?: (wrapper: WrapperCandidate) => WrapperDrift | null
    publish?: (inventory: WrapperInventory) => void
    now?: () => number
    isStopped?: () => boolean
}

type AdoptOutcome = 'adopted' | 'duplicate_session' | 'not_signalable' | 'adopt_failed'

function tryAdopt(wrapper: AdoptableWrapper, deps: WrapperInventoryDeps): AdoptOutcome {
    if (deps.hasOtherLiveOwner?.(wrapper.sessionId, wrapper.pid)) return 'duplicate_session'
    if (!(deps.canSignal ?? isProcessAlive)(wrapper.pid)) return 'not_signalable'
    try {
        return deps.adopt(wrapper) ? 'adopted' : 'adopt_failed'
    } catch (error) {
        logger.debug(`[WRAPPER INVENTORY] Failed to adopt PID ${wrapper.pid}`, error)
        return 'adopt_failed'
    }
}

/** One scan. Returns null when the process listing failed or the inventory was stopped. */
export async function runWrapperInventory(deps: WrapperInventoryDeps): Promise<WrapperInventory | null> {
    let processes: ProcessSnapshot[]
    try {
        processes = await (deps.listProcesses ?? listProcessesForOrphanScan)()
    } catch (error) {
        logger.debug('[WRAPPER INVENTORY] Process scan failed', error)
        return null
    }
    if (deps.isStopped?.()) return null
    const wrappers = enumerateRunnerWrappers(processes, deps.selfPid, deps.selfUid, deps.handoffFromPid)
    const classification = classifyWrappers(wrappers, {
        isTracked: deps.isTracked,
        detectDrift: deps.detectDrift ?? ((wrapper) => detectBinaryDrift(wrapper)),
    })
    const pendingAdoption = new Set(classification.adopted.map((wrapper) => wrapper.pid))
    const reported = [...classification.reported]
    let adopted = 0
    for (const wrapper of classification.adopt) {
        if (wrapper.sessionId === null || wrapper.startMarker === null) continue
        const outcome = tryAdopt({ ...wrapper, sessionId: wrapper.sessionId, startMarker: wrapper.startMarker }, deps)
        if (!pendingAdoption.has(wrapper.pid)) {
            const driftItem = reported.find((item) => item.pid === wrapper.pid && item.drift !== undefined)
            if (driftItem && outcome !== 'adopted') driftItem.adoptOutcome = outcome
            continue
        }
        if (outcome === 'adopted') {
            adopted += 1
        } else {
            reported.push({ pid: wrapper.pid, sessionId: wrapper.sessionId, reason: outcome })
        }
    }
    const inventory: WrapperInventory = {
        scannedAt: (deps.now ?? Date.now)(),
        enumerated: wrappers.length,
        tracked: classification.tracked.length,
        adopted,
        reportedTotal: reported.length,
        reported: reported
            .sort((a, b) => Number(b.drift !== undefined) - Number(a.drift !== undefined))
            .slice(0, WRAPPER_INVENTORY_REPORTED_MAX),
    }
    if (reported.length > 0 || adopted > 0) {
        logger.debug('[WRAPPER INVENTORY] Scan result', {
            enumerated: inventory.enumerated,
            tracked: inventory.tracked,
            adopted: inventory.adopted,
            reported,
        })
    }
    try {
        deps.publish?.(inventory)
    } catch (error) {
        logger.debug('[WRAPPER INVENTORY] Failed to publish inventory', error)
    }
    return inventory
}

/** Run once now, then every intervalMs (unref'd). Overlapping runs are skipped. */
export function startWrapperInventory(
    deps: WrapperInventoryDeps,
    intervalMs: number = WRAPPER_INVENTORY_INTERVAL_MS
): { firstRun: Promise<WrapperInventory | null>; runNow: () => Promise<WrapperInventory | null>; stop: () => void } {
    let stopped = false
    let inFlight: Promise<WrapperInventory | null> | null = null
    const scanDeps: WrapperInventoryDeps = {
        ...deps,
        isStopped: () => stopped || Boolean(deps.isStopped?.()),
    }
    const runNow = () => {
        if (stopped) return Promise.resolve(null)
        if (inFlight) return inFlight
        inFlight = runWrapperInventory(scanDeps).finally(() => { inFlight = null })
        return inFlight
    }
    const firstRun = runNow()
    const timer = setInterval(() => { void runNow() }, intervalMs)
    timer.unref?.()
    return {
        firstRun,
        runNow,
        stop: () => {
            stopped = true
            clearInterval(timer)
        },
    }
}

import { afterEach, beforeEach, describe, expect, it } from 'bun:test'
import { Store } from '../store'
import { RpcRegistry } from '../socket/rpcRegistry'
import { SyncEngine } from './syncEngine'
import { RpcTargetMissingError } from './rpcGateway'
import type { SessionCache } from './sessionCache'

const NAMESPACE = 'default'
const MACHINE = 'machine-1'

describe('SyncEngine operator hibernation', () => {
    let store: Store
    let engine: SyncEngine
    let stopCalls: string[]
    let nextPid = 5000
    const savedEnv = process.env.HAPI_SESSION_HIBERNATE_MS

    function cache(): SessionCache {
        return (engine as unknown as { sessionCache: SessionCache }).sessionCache
    }

    function gateway(): Record<string, unknown> {
        return (engine as unknown as { rpcGateway: Record<string, unknown> }).rpcGateway
    }

    function build(capabilities: Record<string, unknown> = { sessionHibernation: true }): void {
        engine = new SyncEngine(store, {} as never, new RpcRegistry(), { broadcast() {} } as never)
        stopCalls = []
        gateway().stopRunnerSession = async (machineId: string, pidId: string) => {
            stopCalls.push(pidId)
            const owner = engine.getSessions().find(row => row.metadata?.hibernation?.state === 'stopping'
                && row.metadata.machineId === machineId && `PID-${row.metadata.hostPid}` === pidId)
            if (owner) engine.handleSessionEnd({ sid: owner.id, time: Date.now() })
            return 'stopped'
        }
        gateway().killSession = async () => {
            throw new RpcTargetMissingError('KillSession', 'handler-not-registered')
        }
        engine.getOrCreateMachine(MACHINE, { host: 'localhost', platform: 'linux', happyCliVersion: '0.1.0' },
            { status: 'running', capabilities }, NAMESPACE)
        engine.handleMachineAlive({ machineId: MACHINE, time: Date.now() })
        gateway().hasConnectedStopTarget = () => true
    }

    function insertSession(tag: string, metadata: Record<string, unknown> = {}, alive: Record<string, unknown> = {}): string {
        const created = cache().getOrCreateSession(tag, {
            path: '/tmp/project',
            host: 'localhost',
            flavor: 'claude',
            replayableRunnerLaunch: true,
            startedBy: 'runner',
            startedFromRunner: true,
            hostPid: ++nextPid,
            hostStartMarker: `gen-${nextPid}`,
            machineId: MACHINE,
            claudeSessionId: `native-${tag}`,
            lifecycleState: 'running',
            ...metadata,
        }, null, NAMESPACE)
        engine.handleSessionAlive({ hasActiveUpload: false, hasPeerReplyWatch: false, sid: created.id, time: Date.now(), ...alive })
        return created.id
    }

    function signalArchive(id: string, archivedBy = 'cli'): void {
        const stored = store.sessions.getSession(id)!
        store.sessions.updateSessionMetadata(id, {
            ...(stored.metadata as object), lifecycleState: 'archived', lifecycleStateSince: 1_700_000_000_000,
            archivedBy, archiveReason: 'SIGTERM'
        }, stored.metadataVersion, NAMESPACE)
        cache().refreshSession(id)
        engine.handleSessionEnd({ sid: id, time: Date.now() })
        expect(cache().getSession(id)!.active).toBe(false)
    }

    beforeEach(() => {
        delete process.env.HAPI_SESSION_HIBERNATE_MS
        store = new Store(':memory:')
        build()
    })

    afterEach(() => {
        engine.stop()
        if (savedEnv === undefined) delete process.env.HAPI_SESSION_HIBERNATE_MS
        else process.env.HAPI_SESSION_HIBERNATE_MS = savedEnv
    })

    describe('hibernateSessionNow', () => {
        it('hibernates a just-active eligible session without waiting for the idle window or startup grace', async () => {
            const id = insertSession('eligible')
            const updatedAt = cache().getSession(id)!.updatedAt
            expect(await engine.hibernateSessionNow(id, NAMESPACE)).toEqual({ type: 'hibernated', sessionId: id })
            const session = cache().getSession(id)!
            expect(session.active).toBe(false)
            expect(session.updatedAt).toBe(updatedAt)
            expect(session.metadata?.lifecycleState).toBe('hibernated')
            expect(session.metadata?.hibernation?.state).toBe('hibernated')
            expect(session.metadata?.hibernation?.reason).toBe('manual')
            expect(session.metadata?.archivedBy).toBeUndefined()
            expect(stopCalls).toHaveLength(1)
        })

        it('works when automatic hibernation is switched off', async () => {
            engine.stop()
            process.env.HAPI_SESSION_HIBERNATE_MS = '0'
            build()
            const id = insertSession('switched-off')
            expect(await engine.hibernateIdleSessions(Date.now() + 48 * 3600_000)).toEqual([])
            expect(await engine.hibernateSessionNow(id, NAMESPACE)).toEqual({ type: 'hibernated', sessionId: id })
            expect(cache().getSession(id)!.metadata?.lifecycleState).toBe('hibernated')
        })

        it('refuses a session in another namespace', async () => {
            const id = insertSession('ns')
            expect(await engine.hibernateSessionNow(id, 'other')).toMatchObject({ type: 'refused', code: 'access_denied' })
            expect(await engine.hibernateSessionNow('missing', NAMESPACE)).toMatchObject({ type: 'refused', code: 'session_not_found' })
            expect(stopCalls).toEqual([])
            expect(cache().getSession(id)!.active).toBe(true)
        })

        const refusals: Array<[string, Record<string, unknown>, Record<string, unknown>]> = [
            ['mid_turn', {}, { thinking: true }],
            ['upload_active', {}, { hasActiveUpload: true }],
            ['peer_reply_watch', {}, { hasPeerReplyWatch: true }],
            ['launch_not_replayable', { replayableRunnerLaunch: false }, {}],
            ['not_runner_started', { startedBy: 'terminal', startedFromRunner: false }, {}],
            ['pty_session', { startingMode: 'pty' }, {}],
            ['flavor_unsupported', { flavor: 'cursor' }, {}],
            ['idle_reconcile_exempt', { idleReconcileExempt: true }, {}],
            ['no_resume_id', { claudeSessionId: undefined }, {}],
        ]
        for (const [code, metadata, alive] of refusals) {
            it(`refuses with ${code}`, async () => {
                const id = insertSession(`refuse-${code}`, metadata, alive)
                const result = await engine.hibernateSessionNow(id, NAMESPACE)
                expect(result).toEqual({ type: 'refused', sessionId: id, code })
                expect(stopCalls).toEqual([])
                expect(cache().getSession(id)!.metadata?.hibernation).toBeUndefined()
            })
        }

        it('refuses when the runner lacks the capability or is not connected', async () => {
            engine.stop()
            store = new Store(':memory:')
            build({})
            const id = insertSession('no-cap')
            expect(await engine.hibernateSessionNow(id, NAMESPACE)).toMatchObject({ code: 'runner_unsupported' })
            engine.stop()
            store = new Store(':memory:')
            build()
            const other = insertSession('no-target')
            gateway().hasConnectedStopTarget = () => false
            expect(await engine.hibernateSessionNow(other, NAMESPACE)).toMatchObject({ code: 'stop_target_unavailable' })
        })

        it('refuses an already-hibernated session', async () => {
            const id = insertSession('again')
            expect((await engine.hibernateSessionNow(id, NAMESPACE)).type).toBe('hibernated')
            expect(await engine.hibernateSessionNow(id, NAMESPACE)).toMatchObject({ code: 'already_hibernated' })
        })

        it('lists candidates with dry-run refusals, scoped to namespace and machine', async () => {
            const ok = insertSession('cand-ok')
            const busy = insertSession('cand-busy', {}, { thinking: true })
            const elsewhere = insertSession('cand-elsewhere', { machineId: 'machine-2' })
            const rows = engine.listHibernateCandidates(NAMESPACE, { machineId: MACHINE })
            expect(rows.map(row => [row.sessionId, row.refusal]).sort()).toEqual([[busy, 'mid_turn'], [ok, null]].sort())
            expect(rows.some(row => row.sessionId === elsewhere)).toBe(false)
            expect(engine.listHibernateCandidates('other')).toEqual([])
            expect(engine.listHibernateCandidates(NAMESPACE, { minIdleMs: 3600_000 })).toEqual([])
        })
    })

    describe('markArchivedSessionHibernated', () => {
        it('turns a signal-archived runner session into a hibernated one', async () => {
            const id = insertSession('signalled')
            signalArchive(id)
            const updatedAt = cache().getSession(id)!.updatedAt
            expect(engine.listSignalArchivedSessions(NAMESPACE).map(row => row.sessionId)).toEqual([id])
            expect(engine.markArchivedSessionHibernated(id, NAMESPACE)).toEqual({ type: 'hibernated', sessionId: id })
            const session = cache().getSession(id)!
            expect(session.active).toBe(false)
            expect(session.updatedAt).toBe(updatedAt)
            expect(session.metadata?.lifecycleState).toBe('hibernated')
            expect(session.metadata?.hibernation).toMatchObject({ state: 'hibernated', reason: 'manual', since: 1_700_000_000_000 })
            expect(session.metadata?.archivedBy).toBeUndefined()
            expect(session.metadata?.archiveReason).toBeUndefined()
            expect(engine.listSignalArchivedSessions(NAMESPACE)).toEqual([])
        })

        it('works when automatic hibernation is switched off', () => {
            engine.stop()
            process.env.HAPI_SESSION_HIBERNATE_MS = '0'
            build()
            const id = insertSession('signalled-off')
            signalArchive(id)
            expect(engine.markArchivedSessionHibernated(id, NAMESPACE).type).toBe('hibernated')
        })

        it('refuses user/web archives, live sessions, other namespaces and unwakeable rows', () => {
            const byHub = insertSession('by-hub')
            signalArchive(byHub, 'hub')
            expect(engine.markArchivedSessionHibernated(byHub, NAMESPACE)).toMatchObject({ code: 'archived_by_user' })

            const live = insertSession('live')
            expect(engine.markArchivedSessionHibernated(live, NAMESPACE)).toMatchObject({ code: 'session_active' })

            const ns = insertSession('ns-mark')
            signalArchive(ns)
            expect(engine.markArchivedSessionHibernated(ns, 'other')).toMatchObject({ code: 'access_denied' })

            const noResume = insertSession('no-resume', { claudeSessionId: undefined })
            signalArchive(noResume)
            expect(engine.markArchivedSessionHibernated(noResume, NAMESPACE)).toMatchObject({ code: 'no_resume_id' })

            const notReplayable = insertSession('not-replayable', { replayableRunnerLaunch: false })
            signalArchive(notReplayable)
            expect(engine.markArchivedSessionHibernated(notReplayable, NAMESPACE)).toMatchObject({ code: 'launch_not_replayable' })

            const terminal = insertSession('terminal', { startedBy: 'terminal', startedFromRunner: false })
            signalArchive(terminal)
            expect(engine.markArchivedSessionHibernated(terminal, NAMESPACE)).toMatchObject({ code: 'not_runner_started' })

            for (const id of [byHub, ns, noResume, notReplayable, terminal]) {
                expect(cache().getSession(id)!.metadata?.lifecycleState).toBe('archived')
            }
        })

        it('refuses when the runner cannot wake hibernated sessions', () => {
            engine.stop()
            store = new Store(':memory:')
            build({})
            const id = insertSession('no-cap-mark')
            signalArchive(id)
            expect(engine.markArchivedSessionHibernated(id, NAMESPACE)).toMatchObject({ code: 'runner_unsupported' })
        })

        it('rejects a stale expected version and a stale cache snapshot', () => {
            const id = insertSession('stale')
            signalArchive(id)
            const version = cache().getSession(id)!.metadataVersion
            expect(engine.markArchivedSessionHibernated(id, NAMESPACE, version - 1)).toMatchObject({ code: 'version_mismatch' })

            const stored = store.sessions.getSession(id)!
            store.sessions.updateSessionMetadata(id, { ...(stored.metadata as object), name: 'renamed elsewhere' },
                stored.metadataVersion, NAMESPACE)
            expect(engine.markArchivedSessionHibernated(id, NAMESPACE, version)).toMatchObject({ code: 'version_mismatch' })
            expect(store.sessions.getSession(id)!.metadata).toMatchObject({ lifecycleState: 'archived', archivedBy: 'cli' })

            cache().refreshSession(id)
            expect(engine.markArchivedSessionHibernated(id, NAMESPACE, cache().getSession(id)!.metadataVersion).type).toBe('hibernated')
        })

        it('filters the signal-archived list by --since', () => {
            const id = insertSession('since')
            signalArchive(id)
            expect(engine.listSignalArchivedSessions(NAMESPACE, { since: 1_700_000_000_001 })).toEqual([])
            expect(engine.listSignalArchivedSessions(NAMESPACE, { since: 1_699_999_999_999 }).map(row => row.sessionId)).toEqual([id])
            expect(engine.listSignalArchivedSessions('other')).toEqual([])
        })
    })
})

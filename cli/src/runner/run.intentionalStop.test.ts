import { mkdtempSync, symlinkSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { PassThrough } from 'node:stream'
import { EventEmitter } from 'node:events'
import { afterAll, beforeAll, beforeEach, describe, expect, it, vi } from 'vitest'
import type { CodexRuntimeRecord } from '@/codex/shared/registry'
import type { startRunnerControlServer } from './controlServer'

// Exercise the real runner closure end to end so the test can observe what
// reportSessionChildExited receives after a stop and a later child exit.

const mocks = vi.hoisted(() => ({
    controlServer: vi.fn(),
    readRuntimes: vi.fn(),
    findRuntime: vi.fn(),
    reapOrphans: vi.fn<typeof import('./orphanReap')['reapRunnerSpawnedOrphans']>(),
    kill: vi.fn(),
    marker: vi.fn(() => 'generation'),
    getSession: vi.fn(),
    runnerState: vi.fn(async (_update: (state: null) => any) => {}),
    sweep: vi.fn(async () => true),
    isProcessAlive: vi.fn<(pid: number) => boolean>(() => true),
    spawnHappyCLI: vi.fn(),
    reportChildExit: vi.fn(),
}))

vi.mock('./controlServer', () => ({ startRunnerControlServer: mocks.controlServer }))
vi.mock('./controlClient', () => ({
    isRunnerRunningCurrentlyInstalledHappyVersion: async () => false,
    stopRunner: async () => {},
    getInstalledCliMtimeMs: () => undefined,
    waitForRunnerHandoff: async () => false,
    cleanupRunnerState: async () => {},
}))
vi.mock('@/persistence', async importOriginal => ({
    ...await importOriginal<typeof import('@/persistence')>(),
    acquireRunnerLock: async () => ({}),
    releaseRunnerLock: async () => {},
}))
vi.mock('@/ui/auth', () => ({ authAndSetupMachineIfNeeded: async () => ({ machineId: 'machine' }) }))
vi.mock('@/ui/logger', () => ({
    logger: {
        debug: (...args: unknown[]) => { if (process.env.HAPI_TEST_VERBOSE) console.log('[runner]', ...args) },
        debugLargeJson: (...args: unknown[]) => { if (process.env.HAPI_TEST_VERBOSE) console.log('[runner:json]', ...args) },
        warn: (...args: unknown[]) => { if (process.env.HAPI_TEST_VERBOSE) console.log('[runner:warn]', ...args) },
    },
}))
vi.mock('@/ui/doctor', () => ({ getEnvironmentInfo: () => ({}) }))
vi.mock('@/agent/agentAvailability', () => ({ getAgentAvailability: () => ({ available: true }), agentUnavailableMessage: () => '' }))
vi.mock('@/modules/common/cursorModelsPrewarm', () => ({ scheduleCursorModelsPrewarm: () => {} }))
vi.mock('@/codex/shared/registry', () => ({
    readRuntimes: mocks.readRuntimes,
    findRuntime: mocks.findRuntime,
    runtimeAuthHash: () => 'test-auth',
    runtimeMayBeAlive: () => true,
}))
vi.mock('@/runner/orphanReap', () => ({
    reapRunnerSpawnedOrphans: mocks.reapOrphans,
    findStopSessionOrphanTargets: vi.fn(),
    listWindowsProcessesWithCommandLine: vi.fn(),
}))
vi.mock('@/utils/process', async importOriginal => ({
    ...await importOriginal<typeof import('@/utils/process')>(),
    isWindows: () => false,
    isProcessAlive: mocks.isProcessAlive,
    getProcessStartMarker: mocks.marker,
    // The probe reads /proc/<pid>/status on Linux: a busy shared host can have
    // an unrelated non-leader task owning PID_GONE's TID, which would classify
    // the durable record as exited (dropping it without a tombstone) before
    // the stop even runs. Keep generation probing deterministic here.
    getProcessThreadGroupId: () => null,
    killProcess: mocks.kill,
    killProcessTreeByPid: mocks.kill,
    killProcessByChildProcess: mocks.kill,
    sweepProcessGroup: mocks.sweep,
}))
vi.mock('@/utils/spawnHappyCLI', () => ({ spawnSessionWrapper: mocks.spawnHappyCLI, spawnHappyCLI: mocks.spawnHappyCLI }))
vi.mock('@/api/api', () => ({
    ApiClient: {
        create: async () => ({
            getSession: mocks.getSession,
            getOrCreateMachine: async () => ({ id: 'machine-1' }),
            machineSyncClient: () => ({
                setRPCHandlers: vi.fn(),
                connect: vi.fn(),
                shutdown: vi.fn(),
                updateRunnerState: mocks.runnerState,
                reportSessionChildExited: mocks.reportChildExit,
            }),
        }),
    },
}))

import { configuration } from '@/configuration'
import { startRunner } from './run'

type RunnerHandlers = Parameters<typeof startRunnerControlServer>[0]

const SESSION = '98c01763-8292-4422-91ef-19de101b2ad8'
const OTHER = '3914e0a7-caf3-48d9-bed2-2e124c126847'
const PID_GONE = 5000

function makeFakeChild(pid: number): EventEmitter & { pid: number } {
    const child = new EventEmitter() as EventEmitter & { pid: number }
    child.pid = pid
    return child
}

describe('runner intentional-stop marker cleanup', () => {
    let handlers: RunnerHandlers
    let runnerPromise: Promise<void>
    let home: string
    let shutdownStarted = false
    let captured: Error

    beforeAll(async () => {
        home = mkdtempSync(join(tmpdir(), 'hapi-intentional-stop-'))
        symlinkSync(process.execPath, join(home, 'hapi-session-wrapper'))
        captured = new Error('runner stopped after control handlers captured')
        handlers = {} as RunnerHandlers
        mocks.controlServer.mockImplementation(async (capturedHandlers: RunnerHandlers) => {
            handlers = capturedHandlers
            return { port: 0, stop: async () => {} }
        })
        vi.spyOn(process, 'on').mockReturnValue(process)
        vi.spyOn(process, 'exit').mockImplementation(() => { throw captured })
        vi.stubEnv('HAPI_RUNNER_HANDOFF_FROM_PID', '')
        vi.stubEnv('HAPI_RUNNER_HEARTBEAT_INTERVAL', '3600000')
        vi.stubEnv('HAPI_DISABLE_VERSION_HANDOFF', '1')
        Object.defineProperty(configuration, 'runnerStateFile', { value: join(home, 'runner.json'), configurable: true })
        // A runner restart recovers this durable record: PID_GONE once hosted
        // 'sess-gone' and the generation probe verifies it as alive.
        writeFileSync(`${configuration.runnerStateFile}.resume-processes.json`, JSON.stringify([
            { requestedSessionId: 'sess-gone', pid: PID_GONE, processStartMarker: 'generation' },
        ]))
        runnerPromise = startRunner()
        await vi.waitFor(() => expect(handlers.stopSession).toBeTypeOf('function'))
    })

    afterAll(async () => {
        if (!shutdownStarted) handlers.requestShutdown()
        await expect(runnerPromise).rejects.toBe(captured)
        vi.restoreAllMocks()
        vi.unstubAllEnvs()
        rmSync(home, { recursive: true, force: true })
    })

    beforeEach(() => {
        vi.clearAllMocks()
        mocks.marker.mockReturnValue('generation')
        mocks.getSession.mockResolvedValue({ metadata: { machineId: 'machine', hostPid: 6900, hostStartMarker: 'generation', wrapperRuntime: 'rust' } })
        mocks.readRuntimes.mockResolvedValue([])
        mocks.findRuntime.mockResolvedValue(undefined)
        mocks.reapOrphans.mockResolvedValue(null)
        mocks.kill.mockResolvedValue(true)
        mocks.sweep.mockResolvedValue(true)
        mocks.isProcessAlive.mockImplementation(() => true)
        mocks.spawnHappyCLI.mockReset().mockImplementation(() => { throw new Error('no child queued for spawn') })
    })

    it.each([['codex', 86], ['claude', 0], ['agy', 1]] as const)('retries %s after exit %d and reuses the stderr session id', async (agent, code) => {
        vi.stubEnv('HAPI_RUST_WRAPPER_FLAVORS', agent)
        vi.stubEnv('HAPI_RUST_WRAPPER_BIN', join(home, 'hapi-session-wrapper'))
        const rust = Object.assign(makeFakeChild(6900), { stderr: new PassThrough() })
        const ts = makeFakeChild(6901)
        mocks.spawnHappyCLI.mockReturnValueOnce(rust).mockReturnValueOnce(ts)
        try {
            const startup = handlers.spawnSession({ directory: home, agent })
            await vi.waitFor(() => expect(rust.listenerCount('exit')).toBeGreaterThan(0))
            mocks.isProcessAlive.mockImplementation(pid => pid !== rust.pid)
            const prefix = 'HAPI_RUST_WRAPPER_SESSION_ID='
            rust.stderr.write(prefix + '--yolo\n' + prefix + 'x'.repeat(200_000) + '\n')
            rust.stderr.write('x'.repeat(10_000))
            rust.stderr.write(prefix + OTHER + '\nHAPI_RUST_WRAPPER_SESS')
            rust.emit('exit', code, null)
            rust.stderr.end('ION_ID=' + SESSION + '\r\n' + prefix + OTHER + '\n')
            await vi.waitFor(() => expect(mocks.spawnHappyCLI).toHaveBeenCalledTimes(2))
            const [runtime, args] = mocks.spawnHappyCLI.mock.calls[1]!
            expect(runtime.runtime).toBe('ts')
            expect(args.slice(args.indexOf('--existing-session-id'), args.indexOf('--existing-session-id') + 2)).toEqual(['--existing-session-id', SESSION])
            await confirmSpawn(startup, SESSION, ts.pid)
            await expect(startup).resolves.toEqual({ type: 'success', sessionId: SESSION })
            expect(mocks.runnerState.mock.lastCall![0](null).lastSpawnError).toBeNull()
            ts.emit('exit', 0, null)
        } finally {
            vi.stubEnv('HAPI_RUST_WRAPPER_FLAVORS', '')
        }
    })

    it.each([1, 0, null])('does not retry a Codex PID without an explicit safe fallback (exit %s)', async code => {
        vi.stubEnv('HAPI_RUST_WRAPPER_FLAVORS', 'codex')
        vi.stubEnv('HAPI_RUST_WRAPPER_BIN', join(home, 'hapi-session-wrapper'))
        const child = makeFakeChild(6955)
        mocks.spawnHappyCLI.mockReturnValueOnce(child)
        try {
            const startup = handlers.spawnSession({ directory: home, agent: 'codex', reservedSessionId: SESSION })
            await vi.waitFor(() => expect(child.listenerCount('exit')).toBeGreaterThan(0))
            child.emit('exit', code, code === null ? 'SIGTERM' : null)
            await expect(startup).resolves.toMatchObject({ type: 'error' })
            expect(mocks.spawnHappyCLI).toHaveBeenCalledTimes(1)
        } finally {
            vi.stubEnv('HAPI_RUST_WRAPPER_FLAVORS', '')
        }
    })

    it.each([true, false])('retries a pre-webhook process error only after confirming death (dead=%s)', async dead => {
        vi.stubEnv('HAPI_RUST_WRAPPER_FLAVORS', 'claude')
        vi.stubEnv('HAPI_RUST_WRAPPER_BIN', join(home, 'hapi-session-wrapper'))
        const rust = makeFakeChild(6910)
        const ts = makeFakeChild(6911)
        mocks.spawnHappyCLI.mockReturnValueOnce(rust).mockReturnValueOnce(ts)
        mocks.kill.mockImplementation(async () => {
            mocks.isProcessAlive.mockImplementation(pid => pid !== rust.pid || !dead)
            return true
        })
        try {
            const startup = handlers.spawnSession({ directory: home, agent: 'claude' })
            await vi.waitFor(() => expect(rust.listenerCount('exit')).toBeGreaterThan(0))
            rust.emit('error', new Error('pre-webhook failure'))
            expect(mocks.kill).toHaveBeenCalledWith(rust)
            if (dead) {
                await vi.waitFor(() => expect(mocks.spawnHappyCLI).toHaveBeenCalledTimes(2))
                await confirmSpawn(startup, 'recovered', ts.pid)
                await expect(startup).resolves.toEqual({ type: 'success', sessionId: 'recovered' })
                ts.emit('exit', 0, null)
            } else {
                await expect(startup).resolves.toMatchObject({ type: 'error', errorMessage: expect.stringContaining('Cannot verify') })
                expect(mocks.spawnHappyCLI).toHaveBeenCalledTimes(1)
                rust.emit('exit', 1, null)
            }
        } finally {
            vi.stubEnv('HAPI_RUST_WRAPPER_FLAVORS', '')
        }
    })

    it('retries a no-pid Rust spawn once and returns the TS no-pid error', async () => {
        vi.stubEnv('HAPI_RUST_WRAPPER_FLAVORS', 'claude')
        vi.stubEnv('HAPI_RUST_WRAPPER_BIN', join(home, 'hapi-session-wrapper'))
        mocks.spawnHappyCLI.mockImplementation(() => new EventEmitter())
        try {
            await expect(handlers.spawnSession({ directory: home, agent: 'claude' })).resolves.toMatchObject({
                type: 'error', errorMessage: expect.stringContaining('no PID'),
            })
            expect(mocks.spawnHappyCLI.mock.calls.map(([runtime]) => runtime.runtime)).toEqual(['rust', 'ts'])
        } finally {
            vi.stubEnv('HAPI_RUST_WRAPPER_FLAVORS', '')
        }
    })

    it('keeps a timed-out Codex startup recoverable without retrying on its late exit', async () => {
        vi.stubEnv('HAPI_RUST_WRAPPER_FLAVORS', 'codex')
        vi.stubEnv('HAPI_RUST_WRAPPER_BIN', join(home, 'hapi-session-wrapper'))
        vi.stubEnv('HAPI_RUNNER_WEBHOOK_TIMEOUT_MS', '20')
        const rust = makeFakeChild(6920)
        mocks.spawnHappyCLI.mockReturnValueOnce(rust)
        try {
            await expect(handlers.spawnSession({ directory: home, agent: 'codex' })).resolves.toMatchObject({
                type: 'error', errorMessage: expect.stringContaining('still unconfirmed'),
            })
            mocks.isProcessAlive.mockImplementation(pid => pid !== rust.pid)
            rust.emit('exit', 86, null)
            expect(mocks.spawnHappyCLI).toHaveBeenCalledTimes(1)
            expect(mocks.kill).not.toHaveBeenCalled()
        } finally {
            vi.stubEnv('HAPI_RUST_WRAPPER_FLAVORS', '')
            vi.stubEnv('HAPI_RUNNER_WEBHOOK_TIMEOUT_MS', '')
        }
    })

    it.each(['tree', 'group', 'drain', 'stop', 'foreign-marker'] as const)('blocks fallback on %s failure or cancellation', async scenario => {
        vi.stubEnv('HAPI_RUST_WRAPPER_FLAVORS', 'claude')
        vi.stubEnv('HAPI_RUST_WRAPPER_BIN', join(home, 'hapi-session-wrapper'))
        vi.stubEnv('HAPI_RUNNER_WEBHOOK_TIMEOUT_MS', '200')
        const rust = Object.assign(makeFakeChild(6940), { stderr: new PassThrough() })
        mocks.spawnHappyCLI.mockReturnValueOnce(rust)
        const existingSessionId = scenario === 'foreign-marker' ? undefined : SESSION
        try {
            const startup = handlers.spawnSession({ directory: home, agent: 'claude', existingSessionId })
            await vi.waitFor(() => expect(rust.listenerCount('exit')).toBeGreaterThan(0))
            mocks.kill.mockImplementationOnce(async () => {
                mocks.isProcessAlive.mockReturnValue(false)
                return scenario !== 'tree'
            })
            mocks.sweep.mockResolvedValueOnce(scenario !== 'group')
            rust.emit('error', new Error('startup failed'))
            await vi.waitFor(() => expect(mocks.sweep).toHaveBeenCalled())
            if (scenario === 'stop') await handlers.stopSession(SESSION)
            if (scenario !== 'drain') rust.stderr.end('HAPI_RUST_WRAPPER_SESSION_ID=' + OTHER + '\n')
            const result = await startup
            expect(result).toMatchObject({ type: 'error', errorMessage: expect.stringContaining({
                tree: 'process tree', group: 'process tree', drain: 'stderr did not close',
                stop: 'cancelled', 'foreign-marker': 'ownership',
            }[scenario]) })
            expect(mocks.spawnHappyCLI).toHaveBeenCalledTimes(1)
        } finally {
            rust.stderr.destroy()
            vi.stubEnv('HAPI_RUST_WRAPPER_FLAVORS', '')
            vi.stubEnv('HAPI_RUNNER_WEBHOOK_TIMEOUT_MS', '')
        }
    })

    it.each(['existing', 'reserved', 'late-webhook'] as const)('reuses the %s id, protects a recycled pid, and reports a failed TS retry', async source => {
        vi.stubEnv('HAPI_RUST_WRAPPER_FLAVORS', 'codex')
        vi.stubEnv('HAPI_RUST_WRAPPER_BIN', join(home, 'hapi-session-wrapper'))
        const rust = Object.assign(makeFakeChild(6950), { stderr: new PassThrough() })
        const ts = makeFakeChild(6951)
        const options = { directory: home, agent: 'codex' as const,
            existingSessionId: source === 'existing' ? SESSION : undefined,
            reservedSessionId: source === 'reserved' ? SESSION : undefined }
        mocks.spawnHappyCLI.mockReturnValueOnce(rust).mockReturnValueOnce(ts)
        try {
            const startup = handlers.spawnSession(options)
            await vi.waitFor(() => expect(rust.listenerCount('exit')).toBeGreaterThan(0))
            mocks.marker.mockReturnValue('recycled')
            rust.emit('exit', 86, null)
            if (source === 'late-webhook') {
                handlers.onHappySessionWebhook(SESSION, { path: home, host: 'host', hostPid: rust.pid, startedBy: 'runner' })
            }
            rust.stderr.end('HAPI_RUST_WRAPPER_SESSION_ID=' + OTHER + '\n')
            await vi.waitFor(() => expect(ts.listenerCount('exit')).toBeGreaterThan(0))
            const args = mocks.spawnHappyCLI.mock.calls[1]![1]
            expect(args).toContain(SESSION)
            expect(args).not.toContain(OTHER)
            expect(mocks.kill).not.toHaveBeenCalled()
            expect(mocks.sweep).toHaveBeenCalledWith(rust.pid, { expectedStartMarker: 'generation' })
            ts.emit('exit', 3, null)
            await expect(startup).resolves.toMatchObject({ type: 'error' })
            expect(mocks.spawnHappyCLI).toHaveBeenCalledTimes(2)
            expect(mocks.runnerState.mock.lastCall![0](null).lastSpawnError).toMatchObject({
                exitCode: 3, wrapperFallback: { reason: 'exit-before-webhook', exitCode: 86 },
            })
            if (source !== 'late-webhook') {
                vi.stubEnv('HAPI_RUST_WRAPPER_FLAVORS', '')
                await handlers.spawnSession(options)
                expect(mocks.spawnHappyCLI).toHaveBeenCalledTimes(3)
            }
        } finally {
            vi.stubEnv('HAPI_RUST_WRAPPER_FLAVORS', '')
        }
    })

    it('does not retry a Claude webhook timeout followed by a late exit', async () => {
        vi.stubEnv('HAPI_RUST_WRAPPER_FLAVORS', 'claude')
        vi.stubEnv('HAPI_RUST_WRAPPER_BIN', join(home, 'hapi-session-wrapper'))
        vi.stubEnv('HAPI_RUNNER_WEBHOOK_TIMEOUT_MS', '20')
        const child = makeFakeChild(6960)
        mocks.spawnHappyCLI.mockReturnValueOnce(child)
        try {
            const result = await handlers.spawnSession({ directory: home, agent: 'claude' })
            expect(result).toMatchObject({ type: 'error', errorMessage: expect.stringContaining('timeout') })
            child.emit('exit', 86, null)
            expect(mocks.spawnHappyCLI).toHaveBeenCalledTimes(1)
        } finally {
            vi.stubEnv('HAPI_RUST_WRAPPER_FLAVORS', '')
            vi.stubEnv('HAPI_RUNNER_WEBHOOK_TIMEOUT_MS', '')
        }
    })

    // The spawn registers its child asynchronously (workspace validation runs
    // first), so the confirming webhook is retried until the awaiter resolves.
    async function confirmSpawn(spawnPromise: Promise<unknown>, sessionId: string, pid: number): Promise<void> {
        const metadata = {
            path: home,
            host: 'localhost',
            machineId: 'machine',
            hostPid: pid,
            startedBy: 'runner',
            lifecycleState: 'running',
        }
        let settled = false
        void spawnPromise.then(() => { settled = true }, () => { settled = true })
        const deadline = Date.now() + 4_000
        while (!settled && Date.now() < deadline) {
            handlers.onHappySessionWebhook(sessionId, metadata as never)
            await new Promise(resolve => setTimeout(resolve, 10))
        }
        if (!settled) throw new Error(`Spawn ${sessionId} was not confirmed by webhook`)
    }

    it('reports a reused pid unexpected exit after a stop that returned already_gone', async () => {
        // The process is already dead when the stop runs: the runner reports
        // already_gone without any exit event for it.
        mocks.isProcessAlive.mockImplementation((pid: number) => pid !== PID_GONE)
        await expect(handlers.stopSession('sess-gone')).resolves.toBe('already_gone')

        // A later spawn reuses the OS pid for a new session and dies on its own.
        const reusedChild = makeFakeChild(PID_GONE)
        mocks.spawnHappyCLI.mockImplementation(() => reusedChild)
        const spawnPromise = handlers.spawnSession({ directory: home, agent: 'claude', existingSessionId: 'sess-new' })
        await confirmSpawn(spawnPromise, 'sess-new', PID_GONE)
        await expect(spawnPromise).resolves.toEqual({ type: 'success', sessionId: 'sess-new' })

        reusedChild.emit('exit', 1, null)
        await vi.waitFor(() => expect(mocks.reportChildExit).toHaveBeenCalledTimes(1))
        expect(mocks.reportChildExit).toHaveBeenCalledWith(
            expect.objectContaining({ sid: 'sess-new', pid: PID_GONE })
        )
    })

    it('sweeps the recorded process group of a wrapper that exited on its own', async () => {
        const child = makeFakeChild(6300)
        mocks.spawnHappyCLI.mockImplementation(() => child)
        const spawnPromise = handlers.spawnSession({ directory: home, agent: 'claude', existingSessionId: 'sess-exited' })
        await confirmSpawn(spawnPromise, 'sess-exited', 6300)
        await expect(spawnPromise).resolves.toEqual({ type: 'success', sessionId: 'sess-exited' })

        mocks.isProcessAlive.mockImplementation((pid: number) => pid !== 6300)
        child.emit('exit', 1, null)
        await vi.waitFor(() => expect(mocks.reportChildExit).toHaveBeenCalled())

        mocks.sweep.mockResolvedValueOnce(false)
        await expect(handlers.stopSession('sess-exited', { requireProcessGroupCleanup: true })).resolves.toBe('still_alive')
        expect(mocks.sweep).toHaveBeenCalledWith(6300, expect.objectContaining({ expectedStartMarker: 'generation' }))

        await expect(handlers.stopSession('sess-exited', { requireProcessGroupCleanup: true })).resolves.toBe('already_gone')
        expect(mocks.sweep).toHaveBeenCalledTimes(2)

        // Swept once: the record is dropped, so a later archive does not resweep.
        await expect(handlers.stopSession('sess-exited')).resolves.toBe('already_gone')
        expect(mocks.sweep).toHaveBeenCalledTimes(2)
        expect(mocks.kill).not.toHaveBeenCalled()
    })

    it('archive of an exited wrapper keeps already_gone even when its group sweep fails', async () => {
        const child = makeFakeChild(6400)
        mocks.spawnHappyCLI.mockImplementation(() => child)
        const spawnPromise = handlers.spawnSession({ directory: home, agent: 'claude', existingSessionId: 'sess-archive' })
        await confirmSpawn(spawnPromise, 'sess-archive', 6400)
        await expect(spawnPromise).resolves.toEqual({ type: 'success', sessionId: 'sess-archive' })

        mocks.isProcessAlive.mockImplementation((pid: number) => pid !== 6400)
        child.emit('exit', 1, null)
        await vi.waitFor(() => expect(mocks.reportChildExit).toHaveBeenCalled())

        mocks.sweep.mockResolvedValue(false)
        await expect(handlers.stopSession('sess-archive')).resolves.toBe('already_gone')
        expect(mocks.sweep).toHaveBeenCalledWith(6400, expect.objectContaining({ expectedStartMarker: 'generation' }))
    })

    it('does not record a process group for a child the stop killed intentionally', async () => {
        const child = makeFakeChild(6500)
        mocks.spawnHappyCLI.mockImplementation(() => child)
        const spawnPromise = handlers.spawnSession({ directory: home, agent: 'claude', existingSessionId: 'sess-killed' })
        await confirmSpawn(spawnPromise, 'sess-killed', 6500)
        await expect(spawnPromise).resolves.toEqual({ type: 'success', sessionId: 'sess-killed' })

        await expect(handlers.stopSession('sess-killed')).resolves.toBe('stopped')
        expect(mocks.sweep).toHaveBeenCalledTimes(1)
        mocks.isProcessAlive.mockImplementation((pid: number) => pid !== 6500)
        child.emit('exit', 0, null)
        await Promise.resolve()

        await expect(handlers.stopSession('sess-killed')).resolves.toBe('already_gone')
        expect(mocks.sweep).toHaveBeenCalledTimes(1)
    })

    it('still suppresses the exit of a child the stop killed intentionally', async () => {
        const child = makeFakeChild(6100)
        mocks.spawnHappyCLI.mockImplementation(() => child)
        const spawnPromise = handlers.spawnSession({ directory: home, agent: 'claude', existingSessionId: 'sess-live' })
        await confirmSpawn(spawnPromise, 'sess-live', 6100)
        await expect(spawnPromise).resolves.toEqual({ type: 'success', sessionId: 'sess-live' })

        await expect(handlers.stopSession('sess-live')).resolves.toBe('stopped')
        expect(mocks.kill).toHaveBeenCalled()

        // The killed child's own exit event must not reach the hub as an
        // unexpected death: the stop flow already ends the session.
        child.emit('exit', 0, null)
        await Promise.resolve()
        expect(mocks.reportChildExit).not.toHaveBeenCalled()
    })

    it('releases the intentional marker when a stop preserves the shared wrapper for siblings', async () => {
        const wrapper = makeFakeChild(6200)
        mocks.spawnHappyCLI.mockImplementation(() => wrapper)
        const spawnPromise = handlers.spawnSession({ directory: home, agent: 'claude', existingSessionId: 'wrapper-main' })
        await confirmSpawn(spawnPromise, 'wrapper-main', 6200)
        await expect(spawnPromise).resolves.toEqual({ type: 'success', sessionId: 'wrapper-main' })
        for (const root of ['root-a', 'sibling-b']) {
            handlers.onHappySessionWebhook(root, {
                path: home,
                host: 'localhost',
                machineId: 'machine',
                hostPid: 6200,
                startedBy: 'runner',
                lifecycleState: 'running',
                capabilities: { concurrentClients: true },
            } as never)
        }

        // Archiving the primary root keeps the wrapper alive for the sibling.
        mocks.readRuntimes.mockResolvedValue([{
            id: 'runtime', pid: 6200, marker: 'generation', endpoint: 'unused',
            command: 'codex', args: [], codexHome: home,
            hub: configuration.apiUrl, authHash: 'test-auth',
            sessions: {
                'root-a': { threadId: 'a', namespace: 'test', active: false },
                'sibling-b': { threadId: 'b', namespace: 'test', active: true },
            },
        }])
        await expect(handlers.stopSession('root-a')).resolves.toBe('stopped')
        expect(mocks.kill).not.toHaveBeenCalled()

        // The surviving wrapper later crashes on its own: the sibling's death
        // must still be reported, not suppressed by the root's stop marker.
        wrapper.emit('exit', 1, null)
        await vi.waitFor(() => expect(mocks.reportChildExit).toHaveBeenCalledWith(
            expect.objectContaining({ sid: 'sibling-b', pid: 6200 })
        ))
    })

    it('cancels a pending retry when the runner shuts down during stderr drain', async () => {
        vi.stubEnv('HAPI_RUST_WRAPPER_FLAVORS', 'codex')
        vi.stubEnv('HAPI_RUST_WRAPPER_BIN', join(home, 'hapi-session-wrapper'))
        const child = Object.assign(makeFakeChild(6970), { stderr: new PassThrough() })
        mocks.spawnHappyCLI.mockReturnValueOnce(child)
        const startup = handlers.spawnSession({ directory: home, agent: 'codex' })
        await vi.waitFor(() => expect(child.listenerCount('exit')).toBeGreaterThan(0))
        mocks.isProcessAlive.mockReturnValue(false)
        child.emit('exit', 86, null)
        await vi.waitFor(() => expect(mocks.sweep).toHaveBeenCalled())
        shutdownStarted = true
        const shutdown = expect(runnerPromise).rejects.toBe(captured)
        handlers.requestShutdown()
        child.stderr.end()
        await expect(startup).resolves.toMatchObject({ type: 'error', errorMessage: 'Wrapper startup cancelled' })
        expect(mocks.spawnHappyCLI).toHaveBeenCalledTimes(1)
        await shutdown
    })

})
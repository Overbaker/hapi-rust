import { EventEmitter } from 'node:events'
import { mkdtempSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { ApiMachineClient } from '@/api/apiMachine'
import type { Metadata, Session } from '@/api/types'
import { configuration } from '@/configuration'
import type { SpawnSessionOptions, SpawnSessionResult } from '@/modules/common/rpcTypes'
import type { startRunnerControlServer } from './controlServer'
import { startRunner } from './run'

type ControlHandlers = Parameters<typeof startRunnerControlServer>[0]
type MachineHandlers = Parameters<ApiMachineClient['setRPCHandlers']>[0]
type SessionLookup = Pick<Session, 'id' | 'metadata'>

const mocks = vi.hoisted(() => ({
    controlServer: vi.fn<typeof startRunnerControlServer>(),
    setRPCHandlers: vi.fn<ApiMachineClient['setRPCHandlers']>(),
    getSession: vi.fn<(id: string) => Promise<SessionLookup>>(),
    spawnRust: vi.fn<(...args: Parameters<typeof import('@/utils/spawnHappyCLI')['spawnSessionWrapper']>) => FakeChild>(),
    spawnTs: vi.fn<(...args: Parameters<typeof import('@/utils/spawnHappyCLI')['spawnHappyCLI']>) => FakeChild>(),
    readRuntimes: vi.fn<typeof import('@/codex/shared/registry')['readRuntimes']>(),
    isProcessAlive: vi.fn<(pid: number) => boolean>(),
    killProcess: vi.fn(async () => true),
    reportChildExit: vi.fn<ApiMachineClient['reportSessionChildExited']>(),
}))

vi.mock('./controlServer', () => ({ startRunnerControlServer: mocks.controlServer }))
vi.mock('./controlClient', () => ({
    cleanupRunnerState: async () => {},
    getInstalledCliMtimeMs: () => undefined,
    isRunnerRunningCurrentlyInstalledHappyVersion: async () => false,
    stopRunner: async () => {},
    waitForRunnerHandoff: async () => false,
}))
vi.mock('@/persistence', async importOriginal => ({
    ...await importOriginal<typeof import('@/persistence')>(),
    acquireRunnerLock: async () => ({}),
    readRunnerState: async () => null,
    releaseRunnerLock: async () => {},
    writeRunnerState: () => {},
}))
vi.mock('@/ui/auth', () => ({ authAndSetupMachineIfNeeded: async () => ({ machineId: MACHINE_ID }) }))
vi.mock('@/ui/logger', () => ({ logger: { debug: vi.fn(), debugLargeJson: vi.fn(), warn: vi.fn() } }))
vi.mock('@/ui/doctor', () => ({ getEnvironmentInfo: () => ({}) }))
vi.mock('@/agent/agentAvailability', () => ({
    getAgentAvailability: () => ({ available: true }),
    agentUnavailableMessage: () => 'Unexpected agent availability failure',
}))
vi.mock('@/modules/common/cursorModelsPrewarm', () => ({ scheduleCursorModelsPrewarm: () => {} }))
vi.mock('@/codex/shared/registry', async importOriginal => ({
    ...await importOriginal<typeof import('@/codex/shared/registry')>(),
    readRuntimes: mocks.readRuntimes,
}))
vi.mock('./wrapperRuntime', () => ({ resolveWrapperRuntime: () => ({ runtime: 'rust', bin: 'fake-rust-wrapper' }) }))
vi.mock('@/utils/spawnHappyCLI', async importOriginal => ({
    ...await importOriginal<typeof import('@/utils/spawnHappyCLI')>(),
    spawnSessionWrapper: mocks.spawnRust,
    spawnHappyCLI: mocks.spawnTs,
}))
vi.mock('@/utils/process', async importOriginal => ({
    ...await importOriginal<typeof import('@/utils/process')>(),
    getProcessStartMarker: (pid: number) => `start-${pid}`,
    getProcessThreadGroupId: () => null,
    getSystemBootId: () => null,
    isProcessAlive: mocks.isProcessAlive,
    killProcess: mocks.killProcess,
    killProcessByChildProcess: mocks.killProcess,
    killProcessTreeByPid: mocks.killProcess,
}))
vi.mock('@/api/api', () => ({
    ApiClient: {
        create: async () => ({
            getSession: mocks.getSession,
            getOrCreateMachine: async () => ({ id: MACHINE_ID }),
            machineSyncClient: () => ({
                setRPCHandlers: mocks.setRPCHandlers,
                connect: () => {},
                shutdown: () => {},
                updateRunnerState: async () => {},
                reportSessionChildExited: mocks.reportChildExit,
            }),
        }),
    },
}))

const MACHINE_ID = 'wrapper-fallback-machine'
const RUST_PID = 6100
const TS_PID = 6101
const FALLBACK_EXIT = 86
const STARTUP_TIMEOUT_MS = 1_000
const ALIAS_ID = '10000000-0000-4000-8000-000000000001'
const CANONICAL_ID = '10000000-0000-4000-8000-000000000002'
const noSuchProcess = Object.assign(new Error('No such process'), { code: 'ESRCH' })

class FakeChild extends EventEmitter {
    readonly stderr = new EventEmitter()
    exitCode: number | null = null
    readonly signalCode = null

    constructor(readonly pid: number) {
        super()
    }

    exit(code: number) {
        this.exitCode = code
        this.emit('exit', code, null)
    }

    closeStderr() {
        this.stderr.emit('close')
    }

    reportSessionId(id: string) {
        this.stderr.emit('data', Buffer.from(`HAPI_RUST_WRAPPER_SESSION_ID=${id}\n`))
    }
}

describe('runner Rust wrapper fallback', () => {
    const runnerExited = new Error('test runner exited')
    const originalRunnerStateFile = configuration.runnerStateFile
    let directory: string
    let control: ControlHandlers
    let machine: MachineHandlers
    let runner: Promise<void>
    let rust: FakeChild
    let ts: FakeChild

    beforeEach(async () => {
        vi.useFakeTimers()
        vi.clearAllMocks()
        vi.stubEnv('HAPI_RUNNER_HANDOFF_FROM_PID', '')
        vi.stubEnv('HAPI_DISABLE_VERSION_HANDOFF', '1')
        vi.stubEnv('HAPI_RUNNER_HEARTBEAT_INTERVAL', '60000')
        vi.stubEnv('HAPI_RUNNER_WEBHOOK_TIMEOUT_MS', String(STARTUP_TIMEOUT_MS))
        directory = mkdtempSync(join(tmpdir(), 'hapi-wrapper-fallback-'))
        Object.defineProperty(configuration, 'runnerStateFile', { value: join(directory, 'runner.json') })
        vi.spyOn(process, 'on').mockReturnValue(process)
        vi.spyOn(process, 'exit').mockImplementation(() => { throw runnerExited })
        vi.spyOn(process, 'kill').mockImplementation(() => {
            throw noSuchProcess
        })
        rust = new FakeChild(RUST_PID)
        ts = new FakeChild(TS_PID)
        const children = new Map([[rust.pid, rust], [ts.pid, ts]])
        mocks.isProcessAlive.mockImplementation(pid => children.get(pid)?.exitCode === null)
        mocks.spawnRust.mockReset().mockImplementation(() => { throw new Error('Unexpected Rust spawn') })
        mocks.spawnTs.mockReset().mockImplementation(() => { throw new Error('Unexpected TypeScript spawn') })
        mocks.getSession.mockReset().mockImplementation(async () => { throw new Error('Unexpected session lookup') })
        mocks.readRuntimes.mockReset().mockResolvedValue([])
        const controlReady = Promise.withResolvers<ControlHandlers>()
        const machineReady = Promise.withResolvers<MachineHandlers>()
        mocks.controlServer.mockImplementation(async handlers => {
            controlReady.resolve(handlers)
            return { port: 0, stop: async () => {} }
        })
        mocks.setRPCHandlers.mockImplementation(machineReady.resolve)
        runner = startRunner()
        const ready = await Promise.race([
            Promise.all([controlReady.promise, machineReady.promise]),
            runner.then(() => { throw new Error('Runner exited before registering handlers') }),
        ])
        control = ready[0]
        machine = ready[1]
    })

    afterEach(async () => {
        try {
            control?.requestShutdown()
            await vi.advanceTimersByTimeAsync(100)
            await expect(runner).rejects.toBe(runnerExited)
        } finally {
            vi.useRealTimers()
            vi.restoreAllMocks()
            vi.unstubAllEnvs()
            Object.defineProperty(configuration, 'runnerStateFile', { value: originalRunnerStateFile })
            rmSync(directory, { recursive: true, force: true })
        }
    })

    function spawnOptions(agent: SpawnSessionOptions['agent'] = 'claude'): SpawnSessionOptions {
        return { directory, agent }
    }

    function metadata(child: FakeChild, wrapperRuntime: 'rust' | 'ts'): Metadata {
        return {
            path: directory,
            host: 'localhost',
            machineId: MACHINE_ID,
            hostPid: child.pid,
            hostStartMarker: `start-${child.pid}`,
            startedBy: 'runner',
            wrapperRuntime,
        }
    }

    async function startRust(agent?: SpawnSessionOptions['agent']) {
        const started = Promise.withResolvers<void>()
        mocks.spawnRust.mockImplementationOnce(() => {
            started.resolve()
            return rust
        })
        const result = control.spawnSession(spawnOptions(agent))
        const settled = vi.fn<(result: SpawnSessionResult) => void>()
        void result.then(settled)
        await Promise.race([started.promise, result.then(value => { throw new Error(JSON.stringify(value)) })])
        return { result, settled }
    }

    function queueTsChild() {
        const started = Promise.withResolvers<void>()
        mocks.spawnTs.mockImplementationOnce(() => {
            started.resolve()
            return ts
        })
        return started.promise
    }

    function blockOwnershipLookup() {
        const requested = Promise.withResolvers<string>()
        const response = Promise.withResolvers<SessionLookup>()
        mocks.getSession.mockImplementationOnce(id => {
            requested.resolve(id)
            return response.promise
        })
        return { requested: requested.promise, respond: response.resolve }
    }

    it.skipIf(process.platform === 'win32').each([
        { group: 'survives', errorMessage: /cannot verify rust process tree/i },
        { group: 'exits at the deadline', errorMessage: /no startup time left/i },
    ])('refuses fallback by the original deadline when the Rust group $group', async ({ group, errorMessage }) => {
        const { settled } = await startRust()
        const remainingMs = 73
        const deadline = Date.now() + STARTUP_TIMEOUT_MS
        await vi.advanceTimersByTimeAsync(STARTUP_TIMEOUT_MS - remainingMs)
        vi.mocked(process.kill).mockImplementation((pid) => {
            if (pid === -rust.pid && (group === 'survives' || Date.now() < deadline)) return true
            throw noSuchProcess
        })
        rust.exit(FALLBACK_EXIT)

        await vi.advanceTimersByTimeAsync(remainingMs - 1)
        expect(settled).not.toHaveBeenCalled()
        await vi.advanceTimersByTimeAsync(1)

        expect(process.kill).toHaveBeenCalledWith(-rust.pid, 'SIGKILL')
        expect(settled).toHaveBeenCalledWith({ type: 'error', errorMessage: expect.stringMatching(errorMessage) })
        expect(mocks.spawnTs).not.toHaveBeenCalled()
    })

    it('refuses fallback by the original deadline when Rust stderr stays open', async () => {
        const { settled } = await startRust()
        await vi.advanceTimersByTimeAsync(900)
        rust.exit(FALLBACK_EXIT)

        await vi.advanceTimersByTimeAsync(99)
        expect(settled).not.toHaveBeenCalled()
        await vi.advanceTimersByTimeAsync(1)

        expect(settled).toHaveBeenCalledWith({ type: 'error', errorMessage: expect.stringMatching(/stderr did not close/i) })
        expect(mocks.spawnTs).not.toHaveBeenCalled()
    })

    it('refuses fallback by the original deadline when the hub does not verify ownership in time', async () => {
        const lookup = blockOwnershipLookup()
        const { settled } = await startRust()
        await vi.advanceTimersByTimeAsync(900)
        rust.reportSessionId(ALIAS_ID)
        rust.exit(FALLBACK_EXIT)
        rust.closeStderr()
        expect(await lookup.requested).toBe(ALIAS_ID)

        await vi.advanceTimersByTimeAsync(99)
        expect(settled).not.toHaveBeenCalled()
        await vi.advanceTimersByTimeAsync(1)

        expect(settled).toHaveBeenCalledWith({ type: 'error', errorMessage: expect.stringMatching(/lookup.*deadline/i) })
        lookup.respond({ id: CANONICAL_ID, metadata: metadata(rust, 'rust') })
        await vi.advanceTimersByTimeAsync(0)
        expect(mocks.spawnTs).not.toHaveBeenCalled()
    })

    it('times out the TypeScript retry at the original Rust startup deadline', async () => {
        const tsStarted = queueTsChild()
        const { result, settled } = await startRust()
        await vi.advanceTimersByTimeAsync(900)
        rust.exit(FALLBACK_EXIT)
        rust.closeStderr()
        await Promise.race([tsStarted, result.then(value => { throw new Error(JSON.stringify(value)) })])

        await vi.advanceTimersByTimeAsync(99)
        expect(settled).not.toHaveBeenCalled()
        await vi.advanceTimersByTimeAsync(1)

        expect(settled).toHaveBeenCalledWith({ type: 'error', errorMessage: expect.stringMatching(/webhook timeout/i) })
        expect(mocks.spawnTs).toHaveBeenCalledTimes(1)
    })

    it.each(['already_gone', 'still_alive'])('cancels the retry when a Rust PID stop returns %s during fallback', async status => {
        const lookup = blockOwnershipLookup()
        const { result } = await startRust()
        rust.reportSessionId(ALIAS_ID)
        rust.exit(FALLBACK_EXIT)
        rust.closeStderr()
        expect(await lookup.requested).toBe(ALIAS_ID)

        if (status === 'still_alive') mocks.readRuntimes.mockRejectedValueOnce(new Error('Runtime registry unavailable'))
        await expect(machine.stopSession(`PID-${rust.pid}`, { processStartMarker: `start-${rust.pid}` })).resolves.toBe(status)
        lookup.respond({ id: CANONICAL_ID, metadata: metadata(rust, 'rust') })

        await expect(result).resolves.toMatchObject({ type: 'error', errorMessage: expect.stringMatching(/cancelled/i) })
        expect(mocks.spawnTs).not.toHaveBeenCalled()
        expect(mocks.reportChildExit).toHaveBeenCalledExactlyOnceWith({
            sid: CANONICAL_ID,
            pid: rust.pid,
            processStartMarker: `start-${rust.pid}`,
        })
    })

    it('reports the verified canonical row as exited when fresh Codex cannot fall back', async () => {
        mocks.getSession.mockResolvedValueOnce({ id: CANONICAL_ID, metadata: metadata(rust, 'rust') })
        const { result } = await startRust('codex')
        rust.reportSessionId(ALIAS_ID)
        rust.exit(FALLBACK_EXIT)
        rust.closeStderr()

        await expect(result).resolves.toMatchObject({ type: 'error', errorMessage: expect.stringMatching(/cannot hand/i) })
        expect(mocks.spawnTs).not.toHaveBeenCalled()
        expect(mocks.reportChildExit).toHaveBeenCalledExactlyOnceWith({
            sid: CANONICAL_ID,
            pid: rust.pid,
            processStartMarker: `start-${rust.pid}`,
        })
    })

    it('leaves the hub row alone when fallback cannot verify its ownership', async () => {
        mocks.getSession.mockResolvedValueOnce({
            id: CANONICAL_ID,
            metadata: { ...metadata(rust, 'rust'), hostStartMarker: 'previous-generation' },
        })
        const { result } = await startRust()
        rust.reportSessionId(ALIAS_ID)
        rust.exit(FALLBACK_EXIT)
        rust.closeStderr()

        await expect(result).resolves.toMatchObject({ type: 'error', errorMessage: expect.stringMatching(/cannot verify.*ownership/i) })
        expect(mocks.spawnTs).not.toHaveBeenCalled()
        expect(mocks.reportChildExit).not.toHaveBeenCalled()
    })

    it('shares the TypeScript retry with a concurrent resume of the canonical hub session', async () => {
        mocks.getSession.mockResolvedValueOnce({ id: CANONICAL_ID, metadata: metadata(rust, 'rust') })
        const tsStarted = queueTsChild()
        const { result } = await startRust()
        rust.reportSessionId(ALIAS_ID)
        rust.exit(FALLBACK_EXIT)
        rust.closeStderr()
        await Promise.race([tsStarted, result.then(value => { throw new Error(JSON.stringify(value)) })])

        const resume = machine.spawnSession({ ...spawnOptions(), existingSessionId: CANONICAL_ID })
        control.onHappySessionWebhook(CANONICAL_ID, metadata(ts, 'ts'))

        const success = { type: 'success', sessionId: CANONICAL_ID }
        await expect(Promise.all([result, resume])).resolves.toEqual([success, success])
        expect(mocks.getSession).toHaveBeenCalledWith(ALIAS_ID)
        expect(mocks.spawnRust).toHaveBeenCalledTimes(1)
        expect(mocks.spawnTs).toHaveBeenCalledTimes(1)
        expect(mocks.reportChildExit).not.toHaveBeenCalled()
        const args = mocks.spawnTs.mock.calls[0]![0]
        const existingSessionIndex = args.indexOf('--existing-session-id')
        expect(existingSessionIndex).toBeGreaterThanOrEqual(0)
        expect(args[existingSessionIndex + 1]).toBe(CANONICAL_ID)
    })
})

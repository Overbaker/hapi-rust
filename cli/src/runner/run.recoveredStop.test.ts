import { mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { startRunnerControlServer } from './controlServer'

const mocks = vi.hoisted(() => ({
    controlServer: vi.fn(),
    readRuntimes: vi.fn(),
    findRuntime: vi.fn(),
    reapOrphans: vi.fn<typeof import('./orphanReap')['reapRunnerSpawnedOrphans']>(),
    kill: vi.fn(),
    sweep: vi.fn(async () => true),
    prepare: vi.fn(),
    alive: vi.fn((_pid?: number) => true),
    marker: vi.fn(() => 'generation'),
    listProcesses: vi.fn(async (): Promise<Array<{ pid: number; ppid?: number; cmd: string; startMarker: string | null }>> => []),
}))

vi.mock('./controlServer', () => ({ startRunnerControlServer: mocks.controlServer }))
vi.mock('./controlClient', () => ({
    isRunnerRunningCurrentlyInstalledHappyVersion: async () => false,
    stopRunner: async () => {},
}))
vi.mock('@/api/api', () => ({
    ApiClient: { create: async () => ({ prepareOpenCodeSessionStop: mocks.prepare }) },
}))
vi.mock('@/persistence', async importOriginal => ({
    ...await importOriginal<typeof import('@/persistence')>(),
    acquireRunnerLock: async () => ({}),
}))
vi.mock('@/ui/auth', () => ({ authAndSetupMachineIfNeeded: async () => ({ machineId: 'machine' }) }))
vi.mock('@/ui/logger', () => ({ logger: { debug: vi.fn(), debugLargeJson: vi.fn(), warn: vi.fn() } }))
vi.mock('@/ui/doctor', () => ({ getEnvironmentInfo: () => ({}) }))
vi.mock('@/codex/shared/registry', () => ({
    readRuntimes: mocks.readRuntimes,
    findRuntime: mocks.findRuntime,
    runtimeAuthHash: () => 'test-auth',
    runtimeMayBeAlive: () => true,
}))
vi.mock('@/runner/orphanReap', async importOriginal => {
    const actual = await importOriginal<typeof import('./orphanReap')>()
    return {
        reapRunnerSpawnedOrphans: mocks.reapOrphans,
        findStopSessionOrphanTargets: vi.fn(),
        extractHapiSessionIdFromCommand: actual.extractHapiSessionIdFromCommand,
        RUNNER_STARTED_BY_PATTERN: actual.RUNNER_STARTED_BY_PATTERN,
        listProcessesForOrphanScan: mocks.listProcesses,
    }
})
vi.mock('@/utils/process', async importOriginal => ({
    ...await importOriginal<typeof import('@/utils/process')>(),
    isWindows: () => false,
    isProcessAlive: (pid: number) => mocks.alive(pid),
    getProcessStartMarker: () => mocks.marker(),
    getProcessThreadGroupId: (pid: number) => pid,
    killProcess: mocks.kill,
    killProcessTreeByPid: mocks.kill,
    killProcessByChildProcess: mocks.kill,
    sweepProcessGroup: mocks.sweep,
}))

import { configuration } from '@/configuration'
import { logger } from '@/ui/logger'
import { startRunner } from './run'

describe('runner stop of recovered persisted sessions while the Hub is unavailable', () => {
    let stopSession: Parameters<typeof startRunnerControlServer>[0]['stopSession']
    let home: string
    const resumePids = (): number[] => JSON.parse(readFileSync(join(home, 'runner.json.resume-processes.json'), 'utf8'))
        .map((record: { pid: number }) => record.pid)
    const scanFinished = () => vi.waitFor(() => expect(vi.mocked(logger.debug).mock.calls
        .some(([message]) => message === '[WRAPPER INVENTORY] Scan result')).toBe(true))

    const startWith = async (records: unknown[], handoffPid?: number) => {
        home = mkdtempSync(join(tmpdir(), 'hapi-stop-recovered-'))
        const originalStateFile = configuration.runnerStateFile
        const captured = new Error('control handlers captured before runner networking')
        mocks.controlServer.mockImplementation(async handlers => {
            stopSession = handlers.stopSession
            throw captured
        })
        vi.spyOn(process, 'on').mockReturnValue(process)
        vi.spyOn(process, 'exit').mockImplementation(() => { throw captured })
        vi.stubEnv('HAPI_RUNNER_HANDOFF_FROM_PID', String(handoffPid ?? ''))
        const stateFile = join(home, 'runner.json')
        if (handoffPid) writeFileSync(stateFile, JSON.stringify({ pid: handoffPid }))
        writeFileSync(`${stateFile}.resume-processes.json`, JSON.stringify(records))
        Object.defineProperty(configuration, 'runnerStateFile', { value: stateFile, configurable: true })
        try {
            await expect(startRunner()).rejects.toBe(captured)
        } finally {
            Object.defineProperty(configuration, 'runnerStateFile', { value: originalStateFile, configurable: true })
            vi.restoreAllMocks()
            vi.unstubAllEnvs()
        }
    }

    beforeEach(() => {
        vi.clearAllMocks()
        mocks.alive.mockReturnValue(true)
        mocks.marker.mockReturnValue('generation')
        mocks.readRuntimes.mockResolvedValue([])
        mocks.findRuntime.mockResolvedValue(undefined)
        mocks.reapOrphans.mockResolvedValue(null)
        mocks.prepare.mockRejectedValue(new Error('hub unavailable'))
        mocks.kill.mockResolvedValue(true)
        mocks.sweep.mockResolvedValue(true)
        mocks.listProcesses.mockResolvedValue([])
    })

    afterEach(() => rmSync(home, { recursive: true, force: true }))

    it('still kills a recovered non-OpenCode process tree without Hub preparation', async () => {
        await startWith([{
            requestedSessionId: 'claude-session', confirmedSessionId: 'claude-session',
            pid: 4242, processStartMarker: 'generation', flavor: 'claude',
        }])
        await expect(stopSession('claude-session')).resolves.toBe('stopped')
        expect(mocks.prepare).not.toHaveBeenCalled()
        expect(mocks.kill).toHaveBeenCalled()
    })

    it('sweeps the process group after the tree kill succeeds', async () => {
        await startWith([{
            requestedSessionId: 'claude-session', confirmedSessionId: 'claude-session',
            pid: 4242, processStartMarker: 'generation', flavor: 'claude',
        }])
        await expect(stopSession('claude-session')).resolves.toBe('stopped')
        expect(mocks.sweep).toHaveBeenCalledWith(4242, expect.any(Object))
        expect(mocks.sweep.mock.invocationCallOrder[0]).toBeGreaterThan(mocks.kill.mock.invocationCallOrder[0]!)
    })

    it('reports still_alive when a required process-group cleanup fails after the tree kill', async () => {
        await startWith([{
            requestedSessionId: 'claude-session', confirmedSessionId: 'claude-session',
            pid: 4242, processStartMarker: 'generation', flavor: 'claude',
        }])
        mocks.sweep.mockResolvedValue(false)
        await expect(stopSession('claude-session', { requireProcessGroupCleanup: true })).resolves.toBe('still_alive')
    })

    it('does not sweep when the tree kill fails', async () => {
        await startWith([{
            requestedSessionId: 'claude-session', confirmedSessionId: 'claude-session',
            pid: 4242, processStartMarker: 'generation', flavor: 'claude',
        }])
        mocks.kill.mockResolvedValue(false)
        await expect(stopSession('claude-session')).resolves.toBe('still_alive')
        expect(mocks.sweep).not.toHaveBeenCalled()
    })

    it('never sweeps a fallback PID whose generation this stop did not verify (archive unchanged)', async () => {
        await startWith([{
            requestedSessionId: 'claude-session', confirmedSessionId: 'claude-session',
            pid: 4242, processStartMarker: 'generation', flavor: 'claude',
        }])
        mocks.alive.mockReturnValue(false)
        await expect(stopSession('claude-session')).resolves.toBe('already_gone')
        expect(mocks.kill).not.toHaveBeenCalled()
        expect(mocks.sweep).not.toHaveBeenCalled()
    })

    it('reports unknown for an unverified dead root when process-group cleanup is required', async () => {
        await startWith([{
            requestedSessionId: 'claude-session', confirmedSessionId: 'claude-session',
            pid: 4242, processStartMarker: 'generation', flavor: 'claude',
        }])
        mocks.alive.mockReturnValue(false)
        await expect(stopSession('claude-session', { requireProcessGroupCleanup: true })).resolves.toBe('unknown')
        expect(mocks.sweep).not.toHaveBeenCalled()
    })

    it('blocks a required cleanup when a verified group outlives the tree kill, but not archive', async () => {
        await startWith([{
            requestedSessionId: 'claude-session', confirmedSessionId: 'claude-session',
            pid: 4242, processStartMarker: 'generation', flavor: 'claude',
        }])
        mocks.sweep.mockResolvedValue(false)
        await expect(stopSession('claude-session', { requireProcessGroupCleanup: true })).resolves.toBe('still_alive')
        await expect(stopSession('claude-session')).resolves.toBe('stopped')
    })

    it('keeps the mandatory Hub preparation for a recovered OpenCode process', async () => {
        await startWith([{
            requestedSessionId: 'opencode-session', confirmedSessionId: 'opencode-session',
            pid: 4242, processStartMarker: 'generation', flavor: 'opencode',
        }])
        await expect(stopSession('opencode-session')).resolves.toBe('still_alive')
        expect(mocks.kill).not.toHaveBeenCalled()
    })

    it('keeps the mandatory Hub preparation when the recovered flavor is unknown', async () => {
        await startWith([{
            requestedSessionId: 'legacy-session', confirmedSessionId: 'legacy-session',
            pid: 4242, processStartMarker: 'generation',
        }])
        await expect(stopSession('legacy-session')).resolves.toBe('still_alive')
        expect(mocks.kill).not.toHaveBeenCalled()
    })

    it('reports already_gone when a recovered OpenCode process exits during Hub preparation', async () => {
        await startWith([{
            requestedSessionId: 'opencode-session', confirmedSessionId: 'opencode-session',
            pid: 4242, processStartMarker: 'generation', flavor: 'opencode',
        }])
        mocks.prepare.mockImplementation(async () => {
            // The process dies inside the preparation round-trip window; the
            // Hub grants no lease (undefined expiry).
            mocks.alive.mockReturnValue(false)
            return undefined as unknown as boolean
        })
        await expect(stopSession('opencode-session')).resolves.toBe('already_gone')
        expect(mocks.kill).not.toHaveBeenCalled()
        expect(mocks.sweep).toHaveBeenCalledWith(4242, expect.objectContaining({ expectedStartMarker: 'generation' }))
    })

    it('refuses to signal a recycled PID after Hub preparation', async () => {
        await startWith([{
            requestedSessionId: 'opencode-session', confirmedSessionId: 'opencode-session',
            pid: 4242, processStartMarker: 'generation', flavor: 'opencode',
        }])
        mocks.prepare.mockImplementation(async () => {
            // The Hub grants no lease; the OS recycles the PID right after
            // preparation's own verification, before the kill signal.
            mocks.marker.mockReturnValueOnce('generation').mockReturnValue('recycled-generation')
            return undefined as unknown as boolean
        })
        await expect(stopSession('opencode-session')).resolves.toBe('unknown')
        expect(mocks.kill).not.toHaveBeenCalled()
    })

    it.each([['hapi', 'ts'], ['hapi-session-wrapper', 'rust']])('adopts and records a %s wrapper after restart', async (binary, runtime) => {
        mocks.listProcesses.mockResolvedValue([
            { pid: 5151, cmd: `/usr/local/bin/${binary} claude --started-by runner --hapi-session-id orphan-session`, startMarker: 'generation' },
        ])
        await startWith([])
        await vi.waitFor(() => expect(resumePids()).toContain(5151))
        expect(JSON.parse(readFileSync(join(home, 'runner.json.resume-processes.json'), 'utf8'))).toContainEqual(expect.objectContaining({ pid: 5151, wrapperRuntime: runtime }))
        await expect(stopSession('orphan-session')).resolves.toBe('stopped')
        expect(mocks.prepare).not.toHaveBeenCalled()
        expect(mocks.kill.mock.calls[0]?.[0]).toBe(5151)
    })

    it('adopts the live handoff parent’s wrapper while protecting another runner’s wrapper', async () => {
        mocks.listProcesses.mockResolvedValue([
            { pid: 800, cmd: 'hapi runner start-sync', startMarker: 'generation' },
            { pid: 801, cmd: 'hapi runner start-sync', startMarker: 'generation' },
            { pid: 810, ppid: 800, cmd: '/opt/agent tools/hapi-session-wrapper claude --started-by runner --hapi-session-id ours', startMarker: 'generation' },
            { pid: 811, ppid: 801, cmd: 'hapi claude --started-by runner --hapi-session-id theirs', startMarker: 'generation' },
        ])
        await startWith([], 800)
        await scanFinished()
        expect(resumePids()).toEqual([810])
    })

    it('skips adoption while the orphan sweep is stopping that session', async () => {
        const wrapper = { pid: 5151, cmd: 'hapi claude --started-by runner --hapi-session-id busy', startMarker: 'generation' }
        let finishListing!: (rows: typeof wrapper[]) => void
        mocks.listProcesses.mockImplementationOnce(() => new Promise(resolve => { finishListing = resolve }))
        await startWith([])
        mocks.reapOrphans.mockImplementationOnce(async () => {
            finishListing([wrapper])
            await scanFinished()
            return null
        })
        await stopSession('busy')
        expect(mocks.reapOrphans).toHaveBeenCalled()
        expect(resumePids()).not.toContain(wrapper.pid)
    })

    it('only reports a wrapper without a start marker and never kills it', async () => {
        mocks.listProcesses.mockResolvedValue([
            { pid: 5252, cmd: '/usr/local/bin/hapi claude --started-by runner --hapi-session-id unmarked', startMarker: null },
        ])
        await startWith([])
        await scanFinished()
        await stopSession('unmarked')
        expect(resumePids()).not.toContain(5252)
        expect(mocks.kill).not.toHaveBeenCalled()
    })

    it('does not adopt a wrapper whose live start marker no longer matches the snapshot', async () => {
        mocks.listProcesses.mockResolvedValue([
            { pid: 5353, cmd: '/usr/local/bin/hapi claude --started-by runner --hapi-session-id recycled', startMarker: 'stale-generation' },
        ])
        await startWith([])
        await scanFinished()
        expect(resumePids()).not.toContain(5353)
    })

    it('clears a verified-exit tombstone when it adopts a live wrapper for that session', async () => {
        mocks.alive.mockImplementation((pid?: number) => pid !== 4242)
        mocks.listProcesses.mockResolvedValue([
            { pid: 5454, cmd: '/usr/local/bin/hapi claude --started-by runner --hapi-session-id came-back', startMarker: 'generation' },
        ])
        await startWith([{
            requestedSessionId: 'came-back', confirmedSessionId: 'came-back',
            pid: 4242, processStartMarker: 'generation', flavor: 'claude',
        }])
        const tombstones = (): string[] => JSON.parse(readFileSync(join(home, 'runner.json.verified-exits.json'), 'utf8'))
        await vi.waitFor(() => expect(resumePids()).toContain(5454))
        expect(tombstones()).not.toContain('came-back')
        await expect(stopSession('came-back')).resolves.toBe('stopped')
        expect(mocks.kill.mock.calls[0]?.[0]).toBe(5454)
    })
})

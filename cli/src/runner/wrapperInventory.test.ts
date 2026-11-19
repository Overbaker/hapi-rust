import { describe, expect, it, vi } from 'vitest'
import type { ProcessSnapshot } from './orphanReap'
import {
    classifyWrappers,
    detectBinaryDrift,
    enumerateRunnerWrappers,
    hapiDriverEntryIndex,
    runWrapperInventory,
    splitCommandLine,
    startWrapperInventory,
    type WrapperCandidate,
    type WrapperDrift,
} from './wrapperInventory'

vi.mock('@/ui/logger', () => ({ logger: { debug: vi.fn() } }))

function mulberry32(seed: number): () => number {
    let a = seed >>> 0
    return () => {
        a = (a + 0x6D2B79F5) >>> 0
        let t = a
        t = Math.imul(t ^ (t >>> 15), t | 1)
        t ^= t + Math.imul(t ^ (t >>> 7), t | 61)
        return ((t ^ (t >>> 14)) >>> 0) / 4294967296
    }
}

const PBT_ITERATIONS = 500
const PBT_BASE_SEED = Number.parseInt(process.env.HAPI_PBT_SEED ?? '', 10) || (Date.now() >>> 0)

async function forAll(name: string, property: (rand: () => number) => Promise<void> | void): Promise<void> {
    console.log(`[pbt] ${name} base seed=${PBT_BASE_SEED}`)
    for (let i = 0; i < PBT_ITERATIONS; i += 1) {
        const seed = (PBT_BASE_SEED + i) >>> 0
        try {
            await property(mulberry32(seed))
        } catch (error) {
            const message = error instanceof Error ? error.message : String(error)
            throw new Error(`[pbt] ${name} failed with seed=${seed} (HAPI_PBT_SEED=${seed} to replay): ${message}`)
        }
    }
}

function pick<T>(rand: () => number, items: readonly T[]): T {
    return items[Math.floor(rand() * items.length)]!
}

const SELF_PID = 1

function randomProcess(rand: () => number, pid: number): ProcessSnapshot {
    const binary = pick(rand, ['/usr/local/bin/hapi', 'hapi-session-wrapper', 'hapi', 'bun /src/hapi/cli/src/index.ts', '/usr/bin/python3', 'node server.js'])
    const parts = [binary, pick(rand, ['claude', 'codex', 'opencode', 'pi', 'mcp --url http://127.0.0.1:1'])]
    if (rand() < 0.8) parts.push(pick(rand, ['--started-by runner', '--started-by=runner', '--started-by terminal']))
    const id = `s-${Math.floor(rand() * 20)}`
    const idForm = Math.floor(rand() * 4)
    if (idForm === 1) parts.push(`--hapi-session-id ${id}`)
    if (idForm === 2) parts.push(`--existing-session-id=${id}`)
    if (rand() < 0.3) parts.push(`--resume ${id}-native`)
    const ppid = pick(rand, [undefined, 1, SELF_PID, pid - 1] as const)
    return {
        pid,
        ...(ppid !== undefined ? { ppid } : {}),
        cmd: parts.join(' '),
        name: '',
        startMarker: rand() < 0.75 ? `Mon Jan  1 00:00:${String(pid % 60).padStart(2, '0')} 2026` : null,
    }
}

describe('enumerateRunnerWrappers', () => {
    it('keeps runner-started HAPI drivers and excludes MCP bridges, foreign commands and itself', () => {
        const wrappers = enumerateRunnerWrappers([
            { pid: 10, cmd: 'hapi claude --started-by runner --hapi-session-id a', startMarker: 'm' },
            { pid: 11, cmd: 'hapi mcp --url http://x --started-by runner --hapi-session-id a', startMarker: 'm' },
            { pid: 12, cmd: 'python worker --started-by runner --session a', startMarker: 'm' },
            { pid: 13, cmd: 'hapi claude --started-by terminal --hapi-session-id a', startMarker: 'm' },
            { pid: 15, cmd: 'hapi-session-wrapper hook-forwarder --started-by runner' },
            { pid: 99, cmd: 'hapi runner start-sync --started-by runner', startMarker: 'm' },
            { pid: 14, cmd: 'hapi codex --started-by=runner --resume native', startMarker: null },
        ], 99, null)
        expect(wrappers).toEqual([
            { pid: 10, cmd: 'hapi claude --started-by runner --hapi-session-id a', sessionId: 'a', startMarker: 'm', flavor: 'claude', wrapperRuntime: 'ts', foreignParent: false },
            { pid: 14, cmd: 'hapi codex --started-by=runner --resume native', sessionId: null, startMarker: null, flavor: 'codex', wrapperRuntime: 'ts', foreignParent: false },
        ])
    })

    it('anchors driver detection to argv[0] and skips the npm launcher and foreign uids', () => {
        const wrappers = enumerateRunnerWrappers([
            { pid: 20, cmd: '/usr/lib/node_modules/@twsxtd/hapi-linux-x64/bin/hapi claude --started-by runner --hapi-session-id a', startMarker: 'm', uid: 1000 },
            { pid: 21, cmd: 'node /usr/lib/node_modules/@twsxtd/hapi/bin/hapi.cjs claude --started-by runner --hapi-session-id a', startMarker: 'm', uid: 1000 },
            { pid: 22, cmd: 'python /opt/hapi/tool.py --started-by runner --hapi-session-id a', startMarker: 'm', uid: 1000 },
            { pid: 23, cmd: '/usr/local/bin/hapi codex --started-by runner --hapi-session-id b', startMarker: 'm', uid: 0 },
            { pid: 24, cmd: '/usr/bin/bun --cwd /src/cli /src/cli/src/index.ts opencode --started-by runner --hapi-session-id c', startMarker: 'm', uid: 1000 },
            { pid: 25, cmd: 'C:\\hapi\\hapi.exe pi --started-by runner --hapi-session-id d', startMarker: 'm' },
        ], 1, 1000)
        expect(wrappers.map((w) => [w.pid, w.flavor])).toEqual([[20, 'claude'], [24, 'opencode'], [25, 'pi']])
    })
})

describe('hapiDriverEntryIndex', () => {
    it('enumerates a compiled driver under an unquoted path with spaces without a process name', () => {
        const cmd = '/opt/agent tools/bin/hapi codex --started-by runner'
        expect(enumerateRunnerWrappers([{ pid: 42, cmd }])).toMatchObject([{ pid: 42, flavor: 'codex' }])
        expect(hapiDriverEntryIndex(splitCommandLine('editor --file /opt/hapi'))).toBeNull()
    })

    it.each(['hapi', ''])('uses the last executable fragment before flags with name %j', name => {
        const argv = splitCommandLine('/opt/hapi tools/hapi installs/bin/hapi codex --file /other/hapi')
        expect(hapiDriverEntryIndex(argv, name)).toBe(2)
    })

    it('only accepts hapi as argv[0] or a bun/node src/index.ts entry', () => {
        expect(hapiDriverEntryIndex(['/x/bin/hapi', 'claude'])).toBe(0)
        expect(hapiDriverEntryIndex(['bun', '--cwd', '/c', '/c/src/index.ts', 'claude'])).toBe(3)
        expect(hapiDriverEntryIndex(['node', '/x/bin/hapi.cjs', 'claude'])).toBeNull()
        expect(hapiDriverEntryIndex(['/usr/bin/vim', 'hapi'])).toBeNull()
    })
})

describe('detectBinaryDrift', () => {
    const wrapper = { pid: 7, cmd: '/usr/local/bin/hapi claude --started-by runner' }
    const base = {
        platform: 'linux' as const,
        compiled: true,
        readExeLink: () => '/usr/local/bin/hapi',
        statExe: () => ({ dev: 1, ino: 100 }),
        currentExecutable: () => ({ dev: 1, ino: 100 }),
    }

    it('compares Rust wrappers to the Rust executable even under a dev runner', async () => {
        await forAll('rust-wrapper-drift', rand => {
            const current = { ino: Math.floor(rand() * 8), dev: Math.floor(rand() * 2) }
            const identity = rand() < 0.5 ? current : { ...current, dev: current.dev + 1 }
            const deleted = rand() < 0.25
            const resolved = rand() < 0.75
            expect(detectBinaryDrift(wrapper, {
                ...base, compiled: false,
                readExeLink: () => '/bin/hapi-session-wrapper' + (deleted ? ' (deleted)' : ''),
                currentRustExecutable: () => resolved ? current : null,
                statExe: () => identity,
            })).toBe(!resolved ? null : deleted ? 'deleted' : identity === current ? null : 'mismatch')
        })
    })

    it('reports a deleted executable', () => {
        expect(detectBinaryDrift(wrapper, { ...base, readExeLink: () => '/usr/local/bin/hapi (deleted)' })).toBe('deleted')
    })

    it('compares dev+ino against the binary spawnHappyCLI runs', () => {
        expect(detectBinaryDrift(wrapper, { ...base, statExe: () => ({ dev: 1, ino: 101 }) })).toBe('mismatch')
        expect(detectBinaryDrift(wrapper, { ...base, statExe: () => ({ dev: 2, ino: 100 }) })).toBe('mismatch')
        expect(detectBinaryDrift(wrapper, { ...base, readExeLink: () => '/other/path/to/same/inode' })).toBeNull()
    })

    it('never reports drift when the current executable cannot be resolved', () => {
        expect(detectBinaryDrift(wrapper, { ...base, currentExecutable: () => null, readExeLink: () => '/x (deleted)' })).toBeNull()
    })

    it('skips dev mode, non-Linux hosts and unreadable links', () => {
        expect(detectBinaryDrift(wrapper, { ...base, compiled: false, readExeLink: () => '/opt/old/hapi (deleted)' })).toBeNull()
        expect(detectBinaryDrift({ pid: 7, cmd: 'bun src/index.ts claude' }, { ...base, readExeLink: () => '/x (deleted)' })).toBeNull()
        expect(detectBinaryDrift(wrapper, { ...base, platform: 'darwin', readExeLink: () => '/x (deleted)' })).toBeNull()
        expect(detectBinaryDrift(wrapper, { ...base, readExeLink: () => null })).toBeNull()
        expect(detectBinaryDrift(wrapper, { ...base, statExe: () => null })).toBeNull()
    })
})

describe('classifyWrappers', () => {
    const wrapper = (pid: number, sessionId: string | null, startMarker: string | null): WrapperCandidate => ({
        pid, cmd: 'hapi', sessionId, startMarker, flavor: 'claude', wrapperRuntime: 'ts', foreignParent: false,
    })

    it('splits tracked, adopted and reported reasons', () => {
        const result = classifyWrappers([
            wrapper(1, 'a', 'm'),
            wrapper(2, 'b', 'm'),
            wrapper(3, null, 'm'),
            wrapper(4, 'd', null),
            wrapper(5, 'e', 'm'),
        ], { isTracked: (pid) => pid === 1, detectDrift: (w) => w.pid === 5 ? 'deleted' : null })
        expect(result.tracked.map((w) => w.pid)).toEqual([1])
        expect(result.adopted.map((w) => w.pid)).toEqual([2])
        expect(result.reported).toEqual([
            { pid: 3, reason: 'no_session_id' },
            { pid: 4, sessionId: 'd', reason: 'no_start_marker' },
            { pid: 5, sessionId: 'e', drift: 'deleted', reason: 'drift' },
        ])
        expect(result.adopt.map((w) => w.pid)).toEqual([2, 5])
    })

    it('PBT: tracked + adopted + reported == enumerated and adopted implies marker and id', async () => {
        await forAll('wrapper-inventory-partition', (rand) => {
            const count = Math.floor(rand() * 30)
            const processes = Array.from({ length: count }, (_, i) => randomProcess(rand, i + 1))
            const trackedPids = new Set(processes.filter(() => rand() < 0.3).map((p) => p.pid))
            const drifts = new Map<number, WrapperDrift>(processes.filter(() => rand() < 0.2)
                .map((p) => [p.pid, pick(rand, ['deleted', 'mismatch'] as const)]))
            const wrappers = enumerateRunnerWrappers(processes, SELF_PID, null)
            const result = classifyWrappers(wrappers, {
                isTracked: (pid) => trackedPids.has(pid),
                detectDrift: (w) => drifts.get(w.pid) ?? null,
            })
            expect(result.tracked.length + result.adopted.length + result.reported.length).toBe(wrappers.length)
            expect(wrappers.some((w) => w.pid === SELF_PID)).toBe(false)
            for (const w of wrappers) expect(/(?:^|\s)mcp(?:\s|$)/.test(w.cmd)).toBe(false)
            for (const w of [...result.adopted, ...result.adopt]) {
                expect(w.startMarker).not.toBeNull()
                expect(w.sessionId).not.toBeNull()
                expect(trackedPids.has(w.pid)).toBe(false)
                expect(w.foreignParent).toBe(false)
            }
            const seen = [...result.tracked.map((w) => w.pid), ...result.adopted.map((w) => w.pid), ...result.reported.map((r) => r.pid)]
            expect(new Set(seen).size).toBe(seen.length)
        })
    })
})

describe('runWrapperInventory', () => {
    it('adopts eligible wrappers, publishes counts and never kills', async () => {
        const adopt = vi.fn(() => true)
        const publish = vi.fn()
        const inventory = await runWrapperInventory({
            listProcesses: async () => [
                { pid: 20, cmd: 'hapi claude --started-by runner --hapi-session-id tracked', startMarker: 'm1' },
                { pid: 21, cmd: 'hapi claude --started-by runner --hapi-session-id orphan', startMarker: 'm2' },
                { pid: 22, cmd: 'hapi claude --started-by runner', startMarker: 'm3' },
            ],
            selfPid: SELF_PID,
            selfUid: null,
            isTracked: (pid) => pid === 20,
            canSignal: () => true,
            adopt,
            detectDrift: () => null,
            publish,
            now: () => 123,
        })
        expect(adopt).toHaveBeenCalledTimes(1)
        expect(adopt).toHaveBeenCalledWith(expect.objectContaining({ pid: 21, sessionId: 'orphan', startMarker: 'm2', flavor: 'claude' }))
        expect(inventory).toEqual({
            scannedAt: 123, enumerated: 3, tracked: 1, adopted: 1, reportedTotal: 1,
            reported: [{ pid: 22, reason: 'no_session_id' }],
        })
        expect(publish).toHaveBeenCalledWith(inventory)
    })

    it('reports duplicate_session, not_signalable and adopt_failed instead of counting them adopted', async () => {
        const adopt = vi.fn((wrapper: { pid: number }) => {
            if (wrapper.pid === 33) throw new Error('boom')
            return wrapper.pid !== 34
        })
        const inventory = await runWrapperInventory({
            listProcesses: async () => [
                { pid: 30, cmd: 'hapi claude --started-by runner --hapi-session-id dup', startMarker: 'm' },
                { pid: 31, cmd: 'hapi claude --started-by runner --hapi-session-id gone', startMarker: 'm' },
                { pid: 32, cmd: 'hapi claude --started-by runner --hapi-session-id ok', startMarker: 'm' },
                { pid: 33, cmd: 'hapi claude --started-by runner --hapi-session-id throws', startMarker: 'm' },
                { pid: 34, cmd: 'hapi claude --started-by runner --hapi-session-id refused', startMarker: 'm' },
            ],
            selfPid: SELF_PID,
            selfUid: null,
            isTracked: () => false,
            hasOtherLiveOwner: (sessionId) => sessionId === 'dup',
            canSignal: (pid) => pid !== 31,
            adopt,
            detectDrift: () => null,
        })
        expect(inventory!.adopted).toBe(1)
        expect(inventory!.reported).toEqual([
            { pid: 30, sessionId: 'dup', reason: 'duplicate_session' },
            { pid: 31, sessionId: 'gone', reason: 'not_signalable' },
            { pid: 33, sessionId: 'throws', reason: 'adopt_failed' },
            { pid: 34, sessionId: 'refused', reason: 'adopt_failed' },
        ])
        expect(adopt.mock.calls.map(([w]) => w.pid)).toEqual([32, 33, 34])
    })

    it('marks a second untracked wrapper of an already adopted session as duplicate', async () => {
        const owners = new Map<string, number>()
        const inventory = await runWrapperInventory({
            listProcesses: async () => [
                { pid: 40, cmd: 'hapi claude --started-by runner --hapi-session-id same', startMarker: 'm' },
                { pid: 41, cmd: 'hapi claude --started-by runner --hapi-session-id same', startMarker: 'm' },
            ],
            selfPid: SELF_PID,
            selfUid: null,
            isTracked: () => false,
            hasOtherLiveOwner: (sessionId, pid) => owners.has(sessionId) && owners.get(sessionId) !== pid,
            canSignal: () => true,
            adopt: (wrapper) => { owners.set(wrapper.sessionId, wrapper.pid); return true },
            detectDrift: () => null,
        })
        expect(inventory!.adopted).toBe(1)
        expect(inventory!.reported).toEqual([{ pid: 41, sessionId: 'same', reason: 'duplicate_session' }])
    })

    it('PBT: tracked + adopted + reportedTotal === enumerated', async () => {
        await forAll('run-wrapper-inventory-partition', async (rand) => {
            const count = Math.floor(rand() * 30)
            const processes = Array.from({ length: count }, (_, i) => randomProcess(rand, i + 1))
            const trackedPids = new Set(processes.filter(() => rand() < 0.3).map((p) => p.pid))
            const drifts = new Map<number, WrapperDrift>(processes.filter(() => rand() < 0.2)
                .map((p) => [p.pid, pick(rand, ['deleted', 'mismatch'] as const)]))
            const outcomes = new Map(processes.map((p) => [p.pid, pick(rand, ['ok', 'ok', 'false', 'throw', 'dead', 'dup'] as const)]))
            let adoptCalls = 0
            const inventory = await runWrapperInventory({
                listProcesses: async () => processes,
                selfPid: SELF_PID,
                selfUid: null,
                isTracked: (pid) => trackedPids.has(pid),
                hasOtherLiveOwner: (_id, pid) => outcomes.get(pid) === 'dup',
                canSignal: (pid) => outcomes.get(pid) !== 'dead',
                adopt: (wrapper) => {
                    const outcome = outcomes.get(wrapper.pid)
                    if (outcome === 'throw') throw new Error('adopt failed')
                    if (outcome === 'ok' && !drifts.has(wrapper.pid)) adoptCalls += 1
                    return outcome === 'ok'
                },
                detectDrift: (w) => drifts.get(w.pid) ?? null,
            })
            expect(inventory).not.toBeNull()
            const { enumerated, tracked, adopted, reportedTotal, reported } = inventory!
            expect(tracked + adopted + reportedTotal!).toBe(enumerated)
            expect(enumerated).toBe(enumerateRunnerWrappers(processes, SELF_PID, null).length)
            expect(adopted).toBe(adoptCalls)
            expect(reported.length).toBe(Math.min(reportedTotal!, 200))
        })
    })

    it('returns null and publishes nothing when the scan fails', async () => {
        const publish = vi.fn()
        const inventory = await runWrapperInventory({
            listProcesses: async () => { throw new Error('ps failed') },
            isTracked: () => false,
            adopt: vi.fn(() => true),
            publish,
        })
        expect(inventory).toBeNull()
        expect(publish).not.toHaveBeenCalled()
    })

    it('caps the reported list at 200 entries, drift first', async () => {
        const processes: ProcessSnapshot[] = Array.from({ length: 250 }, (_, i) => ({
            pid: 100 + i, cmd: 'hapi claude --started-by runner', startMarker: 'm',
        }))
        const inventory = await runWrapperInventory({
            listProcesses: async () => processes,
            selfPid: SELF_PID,
            selfUid: null,
            isTracked: () => false,
            adopt: vi.fn(() => true),
            detectDrift: (w) => w.pid === 349 ? 'deleted' : null,
        })
        expect(inventory!.reported).toHaveLength(200)
        expect(inventory!.reportedTotal).toBe(250)
        expect(inventory!.reported[0]).toEqual({ pid: 349, drift: 'deleted', reason: 'drift', adoptOutcome: 'no_session_id' })
    })
})

describe('startWrapperInventory', () => {
    it('runs once immediately and then on an unref interval', async () => {
        vi.useFakeTimers()
        const unref = vi.fn()
        const realSetInterval = globalThis.setInterval
        const setIntervalSpy = vi.spyOn(globalThis, 'setInterval').mockImplementation(((handler: () => void, ms: number) => {
            const timer = realSetInterval(handler, ms)
            const original = timer.unref?.bind(timer)
            timer.unref = () => { unref(); return original ? original() : timer }
            return timer
        }) as unknown as typeof setInterval)
        try {
            const listProcesses = vi.fn(async () => [] as ProcessSnapshot[])
            const handle = startWrapperInventory({ listProcesses, isTracked: () => false, adopt: vi.fn(() => true) }, 600_000)
            expect(unref).toHaveBeenCalledTimes(1)
            await handle.firstRun
            expect(listProcesses).toHaveBeenCalledTimes(1)
            await vi.advanceTimersByTimeAsync(600_000)
            expect(listProcesses).toHaveBeenCalledTimes(2)
            handle.stop()
            await vi.advanceTimersByTimeAsync(600_000)
            expect(listProcesses).toHaveBeenCalledTimes(2)
        } finally {
            setIntervalSpy.mockRestore()
            vi.useRealTimers()
        }
    })

    it('skips overlapping runs while a scan is in flight', async () => {
        let release: (value: ProcessSnapshot[]) => void = () => {}
        const listProcesses = vi.fn(() => new Promise<ProcessSnapshot[]>((resolve) => { release = resolve }))
        const handle = startWrapperInventory({ listProcesses, isTracked: () => false, adopt: vi.fn(() => true) }, 600_000)
        try {
            const second = handle.runNow()
            expect(second).toBe(handle.firstRun)
            expect(listProcesses).toHaveBeenCalledTimes(1)
            release([])
            await handle.firstRun
        } finally {
            handle.stop()
        }
    })

    it('neither adopts nor publishes when stopped before the listing resolves', async () => {
        let release: (value: ProcessSnapshot[]) => void = () => {}
        const adopt = vi.fn(() => true)
        const publish = vi.fn()
        const handle = startWrapperInventory({
            listProcesses: () => new Promise<ProcessSnapshot[]>((resolve) => { release = resolve }),
            selfPid: SELF_PID,
            selfUid: null,
            isTracked: () => false,
            canSignal: () => true,
            detectDrift: () => null,
            adopt,
            publish,
        }, 600_000)
        handle.stop()
        release([{ pid: 50, cmd: 'hapi claude --started-by runner --hapi-session-id late', startMarker: 'm' }])
        await expect(handle.firstRun).resolves.toBeNull()
        expect(adopt).not.toHaveBeenCalled()
        expect(publish).not.toHaveBeenCalled()
    })
})

describe('round-2: argv parsing, foreign runners and drift adopt outcomes', () => {
    it('keeps a quoted executable path with spaces as argv[0] and reads the flavor after it', () => {
        expect(splitCommandLine('"C:\\Program Files\\hapi\\hapi.exe" claude --x')).toEqual(['C:\\Program Files\\hapi\\hapi.exe', 'claude', '--x'])
        const wrappers = enumerateRunnerWrappers([
            { pid: 60, cmd: '"C:\\Program Files\\hapi\\hapi.exe" codex --started-by runner --hapi-session-id q', startMarker: 'm' },
            { pid: 61, cmd: "'/opt/my apps/hapi' pi --started-by runner --hapi-session-id r", startMarker: 'm' },
        ], 1, null)
        expect(wrappers.map((w) => [w.pid, w.flavor, w.sessionId])).toEqual([[60, 'codex', 'q'], [61, 'pi', 'r']])
    })

    it('trusts a hapi process name when an unquoted path with spaces splits argv[0]', () => {
        const wrappers = enumerateRunnerWrappers([
            { pid: 62, name: 'hapi', cmd: '/opt/my apps/hapi claude --started-by runner --hapi-session-id s', startMarker: 'm' },
            { pid: 63, name: 'hapi.exe', cmd: 'C:\\Program Files\\hapi\\hapi.exe opencode --started-by runner --hapi-session-id t', startMarker: 'm' },
            { pid: 64, name: 'vim', cmd: '/opt/my apps/hapi claude --started-by runner --hapi-session-id u', startMarker: 'm' },
        ], 1, null)
        expect(wrappers.map((w) => [w.pid, w.flavor])).toEqual([[62, 'claude'], [63, 'opencode']])
        expect(hapiDriverEntryIndex(['/opt/my', 'apps/hapi', 'claude'], 'hapi')).toBe(1)
    })

    it('marks wrappers whose live parent is another HAPI runner as foreign', () => {
        const runnerPid = 500
        const wrappers = enumerateRunnerWrappers([
            { pid: 700, ppid: 900, cmd: '/usr/local/bin/hapi runner start-sync', startMarker: 'm' },
            { pid: 701, ppid: 901, cmd: 'bun /dev/cli/src/index.ts runner start-sync', startMarker: 'm' },
            { pid: 902, ppid: 1, cmd: '/usr/lib/systemd/systemd --user', startMarker: 'm' },
            { pid: 71, ppid: runnerPid, cmd: 'hapi claude --started-by runner --hapi-session-id own', startMarker: 'm' },
            { pid: 72, ppid: 1, cmd: 'hapi claude --started-by runner --hapi-session-id orphaned', startMarker: 'm' },
            { pid: 73, ppid: 902, cmd: 'hapi claude --started-by runner --hapi-session-id subreaped', startMarker: 'm' },
            { pid: 74, ppid: 4444, cmd: 'hapi claude --started-by runner --hapi-session-id parent-gone', startMarker: 'm' },
            { pid: 75, ppid: 700, cmd: 'hapi claude --started-by runner --hapi-session-id other-runner', startMarker: 'm' },
            { pid: 76, ppid: 701, cmd: 'hapi claude --started-by runner --hapi-session-id dev-runner', startMarker: 'm' },
        ], runnerPid, null)
        expect(wrappers.map((w) => [w.pid, w.foreignParent])).toEqual([
            [71, false], [72, false], [73, false], [74, false], [75, true], [76, true],
        ])
    })

    it('reports foreign_runner wrappers and never adopts them', async () => {
        const adopt = vi.fn(() => true)
        const inventory = await runWrapperInventory({
            listProcesses: async () => [
                { pid: 800, ppid: 1, cmd: '/usr/local/bin/hapi runner start-sync', startMarker: 'm' },
                { pid: 81, ppid: 800, cmd: 'hapi claude --started-by runner --hapi-session-id theirs', startMarker: 'm' },
                { pid: 82, ppid: 800, cmd: 'hapi claude --started-by runner --hapi-session-id theirs-drifted', startMarker: 'm' },
            ],
            selfPid: SELF_PID,
            selfUid: null,
            isTracked: () => false,
            canSignal: () => true,
            adopt,
            detectDrift: (w) => w.pid === 82 ? 'mismatch' : null,
        })
        expect(adopt).not.toHaveBeenCalled()
        expect(inventory!.adopted).toBe(0)
        expect(inventory!.reported).toEqual([
            { pid: 82, sessionId: 'theirs-drifted', drift: 'mismatch', reason: 'drift', adoptOutcome: 'foreign_runner' },
            { pid: 81, sessionId: 'theirs', reason: 'foreign_runner' },
        ])
    })

    it('records why a drifted untracked wrapper was not adopted', async () => {
        const inventory = await runWrapperInventory({
            listProcesses: async () => [
                { pid: 90, cmd: 'hapi claude --started-by runner --hapi-session-id ok', startMarker: 'm' },
                { pid: 91, cmd: 'hapi claude --started-by runner --hapi-session-id dup', startMarker: 'm' },
                { pid: 92, cmd: 'hapi claude --started-by runner --hapi-session-id dead', startMarker: 'm' },
                { pid: 93, cmd: 'hapi claude --started-by runner --hapi-session-id fails', startMarker: 'm' },
                { pid: 94, cmd: 'hapi claude --started-by runner', startMarker: 'm' },
                { pid: 95, cmd: 'hapi claude --started-by runner --hapi-session-id nomarker', startMarker: null },
                { pid: 96, cmd: 'hapi claude --started-by runner --hapi-session-id tracked', startMarker: 'm' },
            ],
            selfPid: SELF_PID,
            selfUid: null,
            isTracked: (pid) => pid === 96,
            hasOtherLiveOwner: (sessionId) => sessionId === 'dup',
            canSignal: (pid) => pid !== 92,
            adopt: (wrapper) => wrapper.pid !== 93,
            detectDrift: () => 'deleted',
        })
        expect(inventory!.adopted).toBe(0)
        expect(inventory!.reported).toEqual([
            { pid: 90, sessionId: 'ok', drift: 'deleted', reason: 'drift' },
            { pid: 91, sessionId: 'dup', drift: 'deleted', reason: 'drift', adoptOutcome: 'duplicate_session' },
            { pid: 92, sessionId: 'dead', drift: 'deleted', reason: 'drift', adoptOutcome: 'not_signalable' },
            { pid: 93, sessionId: 'fails', drift: 'deleted', reason: 'drift', adoptOutcome: 'adopt_failed' },
            { pid: 94, drift: 'deleted', reason: 'drift', adoptOutcome: 'no_session_id' },
            { pid: 95, sessionId: 'nomarker', drift: 'deleted', reason: 'drift', adoptOutcome: 'no_start_marker' },
            { pid: 96, sessionId: 'tracked', drift: 'deleted', reason: 'drift' },
        ])
    })
})

it('recognises a Rust wrapper with a truncated Linux comm and spaces in the executable path', () => {
    const [wrapper] = enumerateRunnerWrappers([{
        pid: 42, cmd: '/opt/agent tools/hapi-session-wrapper codex --started-by runner', name: 'hapi-session-wr',
    }], 1, null)
    expect(wrapper).toMatchObject({ pid: 42, flavor: 'codex', wrapperRuntime: 'rust' })
    const identity = { dev: 3, ino: 7 }
    expect(detectBinaryDrift(wrapper!, {
        platform: 'linux', compiled: true, readExeLink: () => '/opt/releases/hapi-session-wrapper-1.0',
        statExe: () => identity, currentRustExecutable: () => identity, currentExecutable: () => ({ dev: 1, ino: 2 }),
    })).toBeNull()
})

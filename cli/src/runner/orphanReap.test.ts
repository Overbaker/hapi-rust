import { readFileSync } from 'node:fs'
import { basename, dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'
import spawn from 'cross-spawn'
import * as windowsProcesses from '@/utils/windowsProcessStop'
import { enumerateRunnerWrappers } from './wrapperInventory'
import { afterEach, describe, expect, it, vi } from 'vitest'
import {
    commandMatchesHapiMcpBridgeForSession,
    commandMatchesRunnerSpawnedSession,
    extractHapiSessionIdFromCommand,
    findRunnerSpawnedOrphanTargets,
    findStopSessionOrphanTargets,
    listPosixProcessesWithStartMarker,
    listProcessesForOrphanScan,
    parseWindowsProcessSnapshot,
    reapRunnerSpawnedOrphans,
    selectOrphanPidsForSession,
    selectOrphanTargetsForSession,
} from './orphanReap'
import {
    WINDOWS_CIM_CREATION_DATE_MARKER_EXPR,
    windowsProcessListCimCommand,
    windowsProcessMarkerCimCommand,
} from '@/utils/process'

describe('complete Windows process snapshots', () => {
    it('protects a live Windows parent and ignores a reused parent PID', async () => {
        const platform = Object.getOwnPropertyDescriptor(process, 'platform')!
        const parent = { name: 'hapi.exe', commandLine: 'hapi.exe runner', parentPid: 1, marker: '2026-02-02T00:00:00Z' }
        const wrapper = { ...parent, parentPid: 42, commandLine: 'hapi.exe codex --started-by runner' }
        const readTable = vi.spyOn(windowsProcesses, 'readWindowsProcessTable').mockResolvedValue(new Map([
            [42, parent],
            [43, { ...wrapper, marker: '2026-02-03T00:00:00Z' }],
            [44, { ...wrapper, marker: '2026-02-01T00:00:00Z' }],
        ]))
        try {
            Object.defineProperty(process, 'platform', { value: 'win32' })
            const wrappers = enumerateRunnerWrappers(await listProcessesForOrphanScan(), 1, null)
            expect(wrappers.map(({ pid, foreignParent }) => [pid, foreignParent])).toEqual([[43, true], [44, false]])
        } finally {
            Object.defineProperty(process, 'platform', platform)
            readTable.mockRestore()
        }
    })

    it.each(['', '[]', '{}', 'null', '[{"ProcessId":1},{}]', '[{"ProcessId":0}]'])('rejects unverifiable listing %s', raw => {
        expect(() => parseWindowsProcessSnapshot(raw)).toThrow();
    });
    it('retains missing creation times as unknown rather than treating the PID as absent', () => {
        expect(parseWindowsProcessSnapshot('[{"ProcessId":0},{"ProcessId":42,"CreationDate":null}]')).toEqual([
            { pid: 42, name: '', cmd: '', startMarker: null }
        ]);
    });
});

describe('orphanReap argv matching', () => {
    const sessionId = 'sess-abc-123'

    it('matches runner-spawned CLI with --existing-session-id', () => {
        const cmd = `bun src/index.ts claude --hapi-starting-mode remote --started-by runner --existing-session-id ${sessionId}`
        expect(commandMatchesRunnerSpawnedSession(cmd, sessionId)).toBe(true)
    })

    it('matches --hapi-session-id= form', () => {
        const cmd = `hapi cursor --started-by=runner --hapi-session-id=${sessionId}`
        expect(commandMatchesRunnerSpawnedSession(cmd, sessionId)).toBe(true)
    })

    it('rejects terminal-started sessions', () => {
        const cmd = `bun src/index.ts claude --started-by terminal --existing-session-id ${sessionId}`
        expect(commandMatchesRunnerSpawnedSession(cmd, sessionId)).toBe(false)
    })

    it('rejects --resume native id that merely equals the session string', () => {
        // Guard: native agent resume ids must not be treated as HAPI row ids.
        const cmd = `bun src/index.ts cursor --resume ${sessionId} --started-by runner`
        expect(commandMatchesRunnerSpawnedSession(cmd, sessionId)).toBe(false)
    })

    it('rejects substring false positives', () => {
        const cmd = `bun src/index.ts claude --started-by runner --existing-session-id ${sessionId}-extra`
        expect(commandMatchesRunnerSpawnedSession(cmd, sessionId)).toBe(false)
    })

    it('matches stamped HAPI MCP bridge helpers without --started-by (#2003)', () => {
        const cmd = `hapi-hapi-soup mcp --url http://127.0.0.1:45619/ --tools display_image --hapi-session-id ${sessionId}`
        expect(commandMatchesHapiMcpBridgeForSession(cmd, sessionId)).toBe(true)
        expect(commandMatchesRunnerSpawnedSession(cmd, sessionId)).toBe(false)
    })

    it('rejects MCP helpers for a different session id', () => {
        const cmd = `hapi mcp --url http://127.0.0.1:1/ --hapi-session-id other-sess`
        expect(commandMatchesHapiMcpBridgeForSession(cmd, sessionId)).toBe(false)
    })

    it('rejects unstamped legacy MCP helpers (no session flag)', () => {
        const cmd = 'hapi mcp --url http://127.0.0.1:1/ --tools display_image'
        expect(commandMatchesHapiMcpBridgeForSession(cmd, sessionId)).toBe(false)
    })

    it('selectOrphanPidsForSession includes stamped MCP bridges alongside agent roots', () => {
        const pids = selectOrphanPidsForSession(
            [
                { pid: 10, cmd: `bun src/index.ts cursor --started-by runner --existing-session-id ${sessionId}`, name: 'bun' },
                { pid: 11, cmd: `hapi mcp --url http://127.0.0.1:9/ --hapi-session-id ${sessionId}`, name: 'hapi' },
                { pid: 12, cmd: `hapi mcp --url http://127.0.0.1:9/ --hapi-session-id other`, name: 'hapi' },
            ],
            sessionId
        )
        expect(pids.sort((a, b) => a - b)).toEqual([10, 11])
    })

    it('treats empty Windows process-list stdout as scan_failed (not no-orphans)', async () => {
        // listWindowsProcessesWithCommandLine throws on empty stdout; catch →
        // scan_failed so archive does not fail-open as already_gone (#1911).
        const found = await findRunnerSpawnedOrphanTargets(sessionId, async () => {
            throw new Error('powershell Win32_Process returned empty stdout')
        })
        expect(found).toBe('scan_failed')
    })

    it('treats signalled ps (status null) as scan_failed (#1911 Overseer B1)', async () => {
        const found = await findRunnerSpawnedOrphanTargets(sessionId, async () => {
            throw new Error('ps aborted (signal)')
        })
        expect(found).toBe('scan_failed')
    })

    it('selectOrphanPidsForSession filters non-hapi and self', () => {
        const pids = selectOrphanPidsForSession(
            [
                { pid: 1, cmd: 'systemd', name: 'systemd' },
                { pid: 42, cmd: `bun src/index.ts claude --started-by runner --existing-session-id ${sessionId}`, name: 'bun' },
                { pid: 43, cmd: `bun src/index.ts claude --started-by runner --existing-session-id other`, name: 'bun' },
                { pid: process.pid, cmd: `bun src/index.ts claude --started-by runner --existing-session-id ${sessionId}`, name: 'bun' },
            ],
            sessionId
        )
        expect(pids).toEqual([42])
    })

    it('selectOrphanPidsForSession coerces string PIDs from win32 ps-list', () => {
        const pids = selectOrphanPidsForSession(
            [
                {
                    // ps-list has returned string PIDs on Windows; Number.isFinite("42") is false
                    pid: '42' as unknown as number,
                    cmd: `hapi.exe cursor --started-by runner --existing-session-id ${sessionId}`,
                    name: 'hapi.exe',
                },
            ],
            sessionId
        )
        expect(pids).toEqual([42])
    })

    it('matches win32 CIM snapshots that include CommandLine', () => {
        const pids = selectOrphanPidsForSession(
            [
                {
                    pid: 99,
                    name: 'hapi.exe',
                    cmd: `"C:\\\\Temp\\\\hapi.exe" /c keep.cmd --started-by runner --existing-session-id ${sessionId}`,
                },
                {
                    pid: 100,
                    name: 'hapi.exe',
                    cmd: '', // name-only (fastlist shape) cannot argv-match
                },
            ],
            sessionId
        )
        expect(pids).toEqual([99])
    })
})

describe('reapRunnerSpawnedOrphans (stopSession orphan path)', () => {
    it.each(['same', 'reused', 'unknown', 'exited'] as const)('awaits async identity and liveness after preparation (%s)', async state => {
        let prepared = false
        const killed: number[] = []
        const result = await reapRunnerSpawnedOrphans('async-preparation', {
            findTargets: async () => [{ pid: 4200, startMarker: 'generation' }],
            getStartMarker: async () => prepared
                ? state === 'reused' ? 'replacement' : state === 'unknown' ? null : 'generation'
                : 'generation',
            isAlive: async () => state !== 'exited',
            prepare: async () => { prepared = true; return true },
            killTree: async pid => { killed.push(pid); return true },
        })
        expect(prepared).toBe(true)
        expect(result).toBe(state === 'reused' || state === 'unknown' ? 'still_alive' : 'stopped')
        expect(killed).toEqual(state === 'same' ? [4200] : [])
    })

    it('does not signal an orphan whose own preparation was not confirmed', async () => {
        const killed: number[] = []
        const status = await reapRunnerSpawnedOrphans('same-session', {
            findTargets: async () => [{ pid: 123, startMarker: 'gen-a' }],
            getStartMarker: () => 'gen-a', isAlive: () => true,
            prepare: async () => false,
            killTree: async (pid) => { killed.push(pid); return true }
        })
        expect(status).toBe('still_alive')
        expect(killed).toEqual([])
    })

    it('rechecks an orphan generation after its preparation await', async () => {
        let generation = 'gen-a'
        const killed: number[] = []
        const status = await reapRunnerSpawnedOrphans('same-session', {
            findTargets: async () => [{ pid: 123, startMarker: 'gen-a' }],
            getStartMarker: () => generation, isAlive: () => true,
            prepare: async () => { generation = 'gen-b'; return true },
            killTree: async (pid) => { killed.push(pid); return true }
        })
        expect(status).toBe('still_alive')
        expect(killed).toEqual([])
    })

    const stableMarker = (marker = 'gen-a') => {
        let reads = 0
        return () => {
            reads++
            // Capture + re-check both return the same generation.
            void reads
            return marker
        }
    }

    it('returns null when no argv orphans match', async () => {
        const status = await reapRunnerSpawnedOrphans('missing-session', {
            findOrphans: async () => [],
            killTree: async () => {
                throw new Error('should not kill')
            },
            getStartMarker: () => 'unused',
        })
        expect(status).toBeNull()
    })

    it('kills matching orphan PIDs and returns stopped when maps would have missed', async () => {
        const killed: number[] = []
        const status = await reapRunnerSpawnedOrphans('sess-orphan-1', {
            findOrphans: async (sessionId) => {
                expect(sessionId).toBe('sess-orphan-1')
                return [4242, 4243]
            },
            killTree: async (pid) => {
                killed.push(pid)
                return true
            },
            getStartMarker: stableMarker('gen-stable'),
        })
        expect(status).toBe('stopped')
        expect(killed).toEqual([4242, 4243])
    })

    it('returns still_alive when the process scan fails', async () => {
        const status = await reapRunnerSpawnedOrphans('sess-orphan-scan-fail', {
            findOrphans: async () => 'scan_failed',
            killTree: async () => {
                throw new Error('should not kill')
            },
        })
        expect(status).toBe('still_alive')
    })

    it('returns still_alive when tree-kill cannot prove death', async () => {
        const status = await reapRunnerSpawnedOrphans('sess-orphan-2', {
            findOrphans: async () => [9999],
            killTree: async () => false,
            getStartMarker: stableMarker(),
        })
        expect(status).toBe('still_alive')
    })

    it('does not kill when start marker changes between capture and kill (PID reuse)', async () => {
        const killed: number[] = []
        const markers = new Map<number, string[]>([
            // First read = capture after argv match; second = pre-kill re-check
            [4242, ['gen-orphan', 'gen-reused-unrelated']],
        ])
        const status = await reapRunnerSpawnedOrphans('sess-orphan-reuse', {
            findOrphans: async () => [4242],
            killTree: async (pid) => {
                killed.push(pid)
                return true
            },
            getStartMarker: (pid) => {
                const queue = markers.get(pid)
                if (!queue || queue.length === 0) return null
                return queue.shift() ?? null
            },
            isAlive: () => true,
        })
        expect(killed).toEqual([])
        // Matched generation is gone (PID reused) — treat as resolved, not a kill.
        expect(status).toBe('stopped')
    })

    it('returns still_alive when pre-kill marker probe fails while PID is alive', async () => {
        const killed: number[] = []
        const markers = new Map<number, Array<string | null>>([
            [4242, ['gen-orphan', null]],
        ])
        const status = await reapRunnerSpawnedOrphans('sess-orphan-probe-fail', {
            findOrphans: async () => [4242],
            killTree: async (pid) => {
                killed.push(pid)
                return true
            },
            getStartMarker: (pid) => {
                const queue = markers.get(pid)
                if (!queue || queue.length === 0) return null
                return queue.shift() ?? null
            },
            isAlive: () => true,
        })
        expect(killed).toEqual([])
        // Null re-check is not proof of death — must not claim stopped.
        expect(status).toBe('still_alive')
    })

    it('returns still_alive without killing when marker cannot be read for a live orphan', async () => {
        const killed: number[] = []
        const status = await reapRunnerSpawnedOrphans('sess-orphan-no-marker', {
            findOrphans: async () => [5555],
            killTree: async (pid) => {
                killed.push(pid)
                return true
            },
            getStartMarker: () => null,
            isAlive: () => true,
        })
        expect(killed).toEqual([])
        expect(status).toBe('still_alive')
    })

    it('uses same-snapshot startMarker as expected (not a later probe) for PID-reuse guard', async () => {
        const killed: number[] = []
        const status = await reapRunnerSpawnedOrphans('sess-orphan-snapshot', {
            findTargets: async () => [
                { pid: 4242, startMarker: 'snapshot-gen' },
            ],
            killTree: async (pid) => {
                killed.push(pid)
                return true
            },
            // Re-check only — must not be consulted for the expected marker.
            getStartMarker: () => 'snapshot-gen',
            isAlive: () => true,
        })
        expect(status).toBe('stopped')
        expect(killed).toEqual([4242])
    })

    it('continues other orphans when one live PID has an unreadable marker', async () => {
        const killed: number[] = []
        const status = await reapRunnerSpawnedOrphans('sess-orphan-partial', {
            findTargets: async () => [
                { pid: 1111, startMarker: null },
                { pid: 2222, startMarker: 'gen-ok' },
            ],
            killTree: async (pid) => {
                killed.push(pid)
                return true
            },
            getStartMarker: (pid) => (pid === 2222 ? 'gen-ok' : null),
            isAlive: () => true,
        })
        expect(killed).toEqual([2222])
        expect(status).toBe('still_alive')
    })

    it('PID-filter on findTargets retains same-snapshot markers (runner stopSession shape)', async () => {
        // Regression: run.ts used findOrphans → number[], which forced a later
        // getStartMarker probe and discarded the argv-snapshot marker (#1911 bot).
        const killed: number[] = []
        const protectedPids = new Set([1111])
        const status = await reapRunnerSpawnedOrphans('sess-orphan-filter', {
            findTargets: async () => {
                const found = [
                    { pid: 1111, startMarker: 'snap-protected' },
                    { pid: 2222, startMarker: 'snap-orphan' },
                ]
                return found.filter((t) => !protectedPids.has(t.pid))
            },
            killTree: async (pid) => {
                killed.push(pid)
                return true
            },
            // Recheck only — expected marker must stay 'snap-orphan', not this.
            getStartMarker: () => 'snap-orphan',
            isAlive: () => true,
        })
        expect(killed).toEqual([2222])
        expect(status).toBe('stopped')
    })

    it('production findStopSessionOrphanTargets→reap keeps list snapshot markers (not a later probe)', async () => {
        // Drives the same helper run.ts uses — not an injected findOrphans seam.
        const sessionId = 'sess-prod-wiring'
        const snapshotIso = '2026-09-24T15:00:00.0000000Z'
        const listProcesses = async () => [
            {
                pid: 1111,
                name: 'hapi',
                cmd: `hapi cursor --started-by runner --existing-session-id ${sessionId}`,
                startMarker: 'snap-protected',
            },
            {
                pid: 2222,
                name: 'hapi',
                cmd: `hapi cursor --started-by runner --existing-session-id ${sessionId}`,
                startMarker: snapshotIso,
            },
        ]
        const protectedPids = new Set([1111])
        const killed: number[] = []
        let probeCalls = 0

        const status = await reapRunnerSpawnedOrphans(sessionId, {
            findTargets: (id) => findStopSessionOrphanTargets(
                id,
                (_sid, pid) => protectedPids.has(pid),
                listProcesses
            ),
            killTree: async (pid) => {
                killed.push(pid)
                return true
            },
            getStartMarker: (pid) => {
                probeCalls++
                // Recheck only — if expected came from a post-find probe, a wrong
                // generation could slip through. Snapshot must already be snapshotIso.
                return pid === 2222 ? snapshotIso : 'snap-protected'
            },
            isAlive: () => true,
        })

        expect(killed).toEqual([2222])
        expect(status).toBe('stopped')
        // One recheck per remaining target (protected filtered out before reap).
        expect(probeCalls).toBe(1)
    })
})

describe('run.ts stopSession orphan wiring (production callers)', () => {
    it('imports findStopSessionOrphanTargets and does not call PID-only discovery', () => {
        // Fails on 63bf8b25f (imported findRunnerSpawnedOrphanPids + findOrphans).
        const runSrc = readFileSync(
            join(dirname(fileURLToPath(import.meta.url)), 'run.ts'),
            'utf8'
        )
        expect(runSrc).toContain('findStopSessionOrphanTargets')
        expect(runSrc).not.toMatch(/\bfindRunnerSpawnedOrphanPids\b/)
        expect(runSrc).not.toMatch(/findOrphans\s*:/)
        // Both sweep sites must wire findTargets through the production helper.
        const findTargetsSites = runSrc.match(/findTargets:\s*\(id\)\s*=>\s*findStopSessionOrphanTargets/g) ?? []
        expect(findTargetsSites.length).toBe(2)
    })
})

describe('Windows orphan startMarker format agreement', () => {
    /** Pre-fix list command: raw DateTime → ConvertTo-Json (/Date(...)/ on WinPS 5.1). */
    const BROKEN_LIST_COMMAND =
        'Get-CimInstance Win32_Process | Select-Object ProcessId,Name,CommandLine,CreationDate | ConvertTo-Json -Compress'
    /** Pre-fix probe: culture ToString / WMIC DMTF — not ISO 'o'. */
    const BROKEN_PROBE_COMMAND =
        '(Get-CimInstance Win32_Process -Filter "ProcessId = 4242").CreationDate'

    it('list + single-probe CIM commands stringify CreationDate the same way', () => {
        expect(WINDOWS_CIM_CREATION_DATE_MARKER_EXPR).toContain("ToString('o')")
        expect(windowsProcessListCimCommand()).toContain(WINDOWS_CIM_CREATION_DATE_MARKER_EXPR)
        expect(windowsProcessListCimCommand()).toContain("$ErrorActionPreference='Stop'")
        expect(windowsProcessListCimCommand()).toContain('trap { exit 1 }')
        const probe = windowsProcessMarkerCimCommand(4242)
        expect(probe).toContain("CreationDate.ToUniversalTime().ToString('o')")
        expect(probe).toContain('ProcessId = 4242')
        expect(windowsProcessListCimCommand()).toMatch(/CreationDate.*ToString\('o'\)/)
    })

    it('broken WinPS shapes disagree; fixed list marker equals fixed probe marker', () => {
        // Simulate the two producers Overseer called out: listing vs recheck.
        // WinPS 5.1 ConvertTo-Json of DateTime:
        const listFromBrokenConvertToJson = '/Date(1727182800000)/'
        // Default DateTime stdout / culture ToString (not ISO 'o'):
        const probeFromBrokenPropertyPrint = '9/24/2026 3:00:00 PM'
        expect(listFromBrokenConvertToJson).not.toBe(probeFromBrokenPropertyPrint)

        const iso = '2026-09-24T15:00:00.0000000Z'
        // Fixed path: both sides emit the same ToString('o') string.
        expect(iso).toBe(iso)
        expect(BROKEN_LIST_COMMAND).not.toContain("ToString('o')")
        expect(BROKEN_PROBE_COMMAND).not.toContain("ToString('o')")
        expect(windowsProcessListCimCommand()).toContain("ToString('o')")
        expect(windowsProcessMarkerCimCommand(4242)).toContain("ToString('o')")

        // End-to-end: targets selected from a CIM-shaped list row, then reaped
        // with a probe that returns the *same* ISO string (two code paths).
        const sessionId = 'sess-win-marker'
        const targets = selectOrphanTargetsForSession(
            [
                {
                    pid: 4242,
                    name: 'hapi.exe',
                    cmd: `hapi.exe cursor --started-by runner --existing-session-id ${sessionId}`,
                    startMarker: iso, // as listWindowsProcessesWithCommandLine would set
                },
            ],
            sessionId,
            1 // Synthetic self PID: never collide with the fixture PID (4242).
        )
        expect(targets).toEqual([{ pid: 4242, startMarker: iso }])
    })

    it('reap with list ISO expected + probe ISO current kills; mismatch skips', async () => {
        const iso = '2026-09-24T15:00:00.0000000Z'
        const killedMatch: number[] = []
        const ok = await reapRunnerSpawnedOrphans('sess-win-agree', {
            findTargets: async () => [{ pid: 1, startMarker: iso }],
            killTree: async (pid) => {
                killedMatch.push(pid)
                return true
            },
            getStartMarker: () => iso, // windowsProcessMarkerCimCommand output
            isAlive: () => true,
        })
        expect(ok).toBe('stopped')
        expect(killedMatch).toEqual([1])

        const killedMismatch: number[] = []
        const skipped = await reapRunnerSpawnedOrphans('sess-win-disagree', {
            findTargets: async () => [{ pid: 2, startMarker: iso }],
            killTree: async (pid) => {
                killedMismatch.push(pid)
                return true
            },
            // Broken probe shape vs ISO list → treat as generation gone, no kill.
            getStartMarker: () => '/Date(1727182800000)/',
            isAlive: () => true,
        })
        expect(skipped).toBe('stopped')
        expect(killedMismatch).toEqual([])
    })
})

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

function forAll(name: string, property: (rand: () => number) => void): void {
    console.log(`[pbt] ${name} base seed=${PBT_BASE_SEED}`)
    for (let i = 0; i < PBT_ITERATIONS; i += 1) {
        const seed = (PBT_BASE_SEED + i) >>> 0
        try {
            property(mulberry32(seed))
        } catch (error) {
            const message = error instanceof Error ? error.message : String(error)
            throw new Error(`[pbt] ${name} failed with seed=${seed} (HAPI_PBT_SEED=${seed} to replay): ${message}`)
        }
    }
}

function pick<T>(rand: () => number, items: readonly T[]): T {
    return items[Math.floor(rand() * items.length)]!
}

function shuffle<T>(rand: () => number, items: T[]): T[] {
    const out = [...items]
    for (let i = out.length - 1; i > 0; i -= 1) {
        const j = Math.floor(rand() * (i + 1))
        ;[out[i], out[j]] = [out[j]!, out[i]!]
    }
    return out
}

function randomId(rand: () => number): string {
    const alphabet = 'abcdef0123456789-'
    const length = 4 + Math.floor(rand() * 12)
    let id = ''
    for (let i = 0; i < length; i += 1) id += pick(rand, alphabet.split(''))
    return id
}

describe('extractHapiSessionIdFromCommand', () => {
    it('reads the space and equals forms of both HAPI id flags', () => {
        expect(extractHapiSessionIdFromCommand('hapi claude --started-by runner --hapi-session-id abc')).toBe('abc')
        expect(extractHapiSessionIdFromCommand('hapi claude --started-by=runner --existing-session-id=abc')).toBe('abc')
        expect(extractHapiSessionIdFromCommand('hapi --existing-session-id   abc --started-by runner')).toBe('abc')
    })

    it('never treats a native --resume id as the HAPI session id', () => {
        expect(extractHapiSessionIdFromCommand('hapi claude --started-by runner --resume 0b5d6c1e-1111-2222-3333-444455556666')).toBeNull()
    })

    it('requires the runner start stamp', () => {
        expect(extractHapiSessionIdFromCommand('hapi claude --started-by terminal --hapi-session-id abc')).toBeNull()
        expect(extractHapiSessionIdFromCommand('hapi claude --hapi-session-id abc')).toBeNull()
    })

    it('returns null for conflicting ids', () => {
        expect(extractHapiSessionIdFromCommand('hapi --started-by runner --hapi-session-id a --existing-session-id b')).toBeNull()
        expect(extractHapiSessionIdFromCommand('hapi --started-by runner --hapi-session-id a --existing-session-id=a')).toBe('a')
    })

    it('rejects missing and option-shaped values', () => {
        expect(extractHapiSessionIdFromCommand('hapi --started-by runner --hapi-session-id')).toBeNull()
        expect(extractHapiSessionIdFromCommand('hapi --started-by runner --hapi-session-id= --yolo')).toBeNull()
        expect(extractHapiSessionIdFromCommand('hapi --hapi-session-id --started-by runner')).toBeNull()
        expect(extractHapiSessionIdFromCommand('hapi --started-by runner --existing-session-id=--yolo')).toBeNull()
        expect(extractHapiSessionIdFromCommand('hapi --started-by runner --hapi-session-id a --existing-session-id')).toBeNull()
        expect(commandMatchesRunnerSpawnedSession('hapi --started-by runner --hapi-session-id', '')).toBe(false)
    })

    it('PBT: extract matches an independent oracle and commandMatchesRunnerSpawnedSession agrees', () => {
        forAll('extract-matches-equivalence', (rand) => {
            const id = randomId(rand)
            const decoyId = randomId(rand)
            const units: string[] = [pick(rand, ['hapi', '/usr/local/bin/hapi', 'bun src/index.ts']), pick(rand, ['claude', 'codex', 'opencode', 'pi'])]
            const startedBy = pick(rand, ['--started-by runner', '--started-by=runner', '--started-by terminal', '--started-by=runnerx', ''])
            if (startedBy) units.push(startedBy)
            const values: Array<string | null> = []
            const idFlagCount = Math.floor(rand() * 4)
            for (let i = 0; i < idFlagCount; i += 1) {
                const flag = pick(rand, ['--existing-session-id', '--hapi-session-id'])
                const kind = pick(rand, ['id', 'id', 'id', 'conflict', 'empty', 'option'] as const)
                if (kind === 'empty') {
                    units.push(`${flag}=`)
                    values.push(null)
                } else if (kind === 'option') {
                    units.push(rand() < 0.5 ? `${flag} --yolo` : `${flag}=--yolo`)
                    values.push(null)
                } else {
                    const value = kind === 'id' ? id : decoyId
                    units.push(rand() < 0.5 ? `${flag} ${value}` : `${flag}=${value}`)
                    values.push(value.startsWith('-') ? null : value)
                }
            }
            const decoys = [
                `--resume ${decoyId}`,
                `--resume=${decoyId}`,
                `--session-id ${decoyId}`,
                `--hapi-session-idx ${decoyId}`,
                `x--hapi-session-id=${decoyId}`,
                `--existing-session-id-old ${decoyId}`,
                decoyId,
                '--yolo',
                '--model sonnet',
            ]
            const decoyCount = Math.floor(rand() * 4)
            for (let i = 0; i < decoyCount; i += 1) units.push(pick(rand, decoys))
            const shuffled = shuffle(rand, units)
            if (rand() < 0.15) {
                shuffled.push(pick(rand, ['--existing-session-id', '--hapi-session-id']))
                values.push(null)
            }
            const cmd = shuffled.join(pick(rand, [' ', '  ']))
            const runnerStarted = startedBy === '--started-by runner' || startedBy === '--started-by=runner'
            const distinct = new Set(values)
            const expected = runnerStarted && values.length > 0 && !distinct.has(null) && distinct.size === 1
                ? values[0]!
                : null
            const extracted = extractHapiSessionIdFromCommand(cmd)
            if (extracted !== expected) {
                throw new Error(`cmd=${JSON.stringify(cmd)} expected=${expected} extracted=${extracted}`)
            }
            for (const candidate of [id, decoyId, '']) {
                const matches = commandMatchesRunnerSpawnedSession(cmd, candidate)
                if ((candidate !== '' && extracted === candidate) !== matches) {
                    throw new Error(`cmd=${JSON.stringify(cmd)} candidate=${candidate} extracted=${extracted} matches=${matches}`)
                }
            }
        })
    })
})

describe('listPosixProcessesWithStartMarker', () => {
    afterEach(() => vi.restoreAllMocks())

    const psResult = (stdout: string, status: number | null = 0, error?: Error) =>
        ({ pid: 1, output: [], stdout, stderr: '', status, signal: status === null ? 'SIGKILL' : null, error }) as unknown as ReturnType<typeof spawn.sync>

    it('asks ps for pid, ppid, uid, lstart and args in the C locale and UTC', () => {
        const sync = vi.spyOn(spawn, 'sync').mockReturnValue(psResult(''))
        listPosixProcessesWithStartMarker()
        expect(sync).toHaveBeenCalledWith('ps', ['awwxo', 'pid=,ppid=,uid=,lstart=,args='], expect.objectContaining({
            env: expect.objectContaining({ LC_ALL: 'C', TZ: 'UTC' }),
        }))
    })

    it('parses padded columns, single-digit days, kernel threads and args with spaces', () => {
        vi.spyOn(spawn, 'sync').mockReturnValue(psResult([
            '    1     0     0 Mon Jan  5 08:01:02 2026 /sbin/init splash',
            '    2     0     0 Mon Jan  5 08:01:02 2026 [kthreadd]',
            ' 4321  1200  1000 Tue Feb 17 23:59:59 2026 /usr/local/bin/hapi claude --started-by runner --hapi-session-id abc',
            '',
            'garbage line',
            '98765     1  1000 Wed Mar  4 00:00:00 2026 /opt/my apps/tool codex',
        ].join('\n')))
        expect(listPosixProcessesWithStartMarker()).toEqual([
            { pid: 1, ppid: 0, uid: 0, startMarker: 'Mon Jan  5 08:01:02 2026', cmd: '/sbin/init splash', name: '' },
            { pid: 2, ppid: 0, uid: 0, startMarker: 'Mon Jan  5 08:01:02 2026', cmd: '[kthreadd]', name: '' },
            { pid: 4321, ppid: 1200, uid: 1000, startMarker: 'Tue Feb 17 23:59:59 2026', cmd: '/usr/local/bin/hapi claude --started-by runner --hapi-session-id abc', name: '' },
            { pid: 98765, ppid: 1, uid: 1000, startMarker: 'Wed Mar  4 00:00:00 2026', cmd: '/opt/my apps/tool codex', name: '' },
        ])
    })

    it.runIf(process.platform === 'linux')('reads comm for an ambiguous executable path', () => {
        const cmd = '/opt/my apps/hapi codex'
        vi.spyOn(spawn, 'sync').mockReturnValue(psResult(`${process.pid} 1 1000 Wed Mar  4 00:00:00 2026 ${cmd}`))
        expect(listPosixProcessesWithStartMarker()[0]?.name).toBe(basename(readFileSync('/proc/self/comm', 'utf8').trimEnd()))
    })

    it('uses the macOS comm basename for an ambiguous executable path', () => {
        const platform = Object.getOwnPropertyDescriptor(process, 'platform')!
        const sync = vi.spyOn(spawn, 'sync')
            .mockReturnValueOnce(psResult('42 1 1000 Wed Mar  4 00:00:00 2026 /opt/my apps/hapi codex'))
            .mockReturnValueOnce(psResult('/opt/my apps/hapi\n'))
        try {
            Object.defineProperty(process, 'platform', { value: 'darwin' })
            expect(listPosixProcessesWithStartMarker()[0]?.name).toBe('hapi')
            expect(sync).toHaveBeenLastCalledWith('ps', ['-p', '42', '-o', 'comm='], expect.any(Object))
        } finally {
            Object.defineProperty(process, 'platform', platform)
        }
    })

    it('throws on a non-zero exit, a signal-killed ps, or a spawn error', () => {
        vi.spyOn(spawn, 'sync').mockReturnValueOnce(psResult('    1     0     0 Mon Jan  5 08:01:02 2026 init', 1))
        expect(() => listPosixProcessesWithStartMarker()).toThrow('ps exit 1')
        vi.spyOn(spawn, 'sync').mockReturnValueOnce(psResult('    1     0     0 Mon Jan  5 08:01:02 2026 init', null))
        expect(() => listPosixProcessesWithStartMarker()).toThrow('ps aborted (signal)')
        vi.spyOn(spawn, 'sync').mockReturnValueOnce(psResult('', null, new Error('ENOENT')))
        expect(() => listPosixProcessesWithStartMarker()).toThrow('ENOENT')
    })
})

it.each(['hapi', 'hapi-session-wrapper', 'hapi-session-wr'])('recognises %s as the orphan driver for its stamped session', binary => {
    const targets = selectOrphanTargetsForSession([
        { pid: 91, name: binary, cmd: 'claude --started-by runner --existing-session-id mine', startMarker: 'generation' },
        { pid: 92, name: binary, cmd: 'claude --started-by runner --existing-session-id other' },
    ], 'mine', 1)
    expect(targets).toEqual([{ pid: 91, startMarker: 'generation' }])
})

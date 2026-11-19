import { afterEach, beforeAll, describe, expect, it } from 'vitest'
import { spawn, execFile, execFileSync } from 'node:child_process'
import { promisify } from 'node:util'
import {
    rmSync,
    realpathSync,
    copyFileSync,
    existsSync,
    mkdirSync,
    mkdtempSync,
    readFileSync,
    readdirSync,
    statSync,
    symlinkSync,
    writeFileSync,
} from 'node:fs'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { createHash, randomUUID } from 'node:crypto'
import { io, type Socket } from 'socket.io-client'
import { configuration } from '@/configuration'
import { readRunnerState } from '@/persistence'
import { stopRunnerHttp, stopRunnerSession } from './controlClient'
import { buildTestChildEnv, testOwnedMarker } from '@/test/integrationEnv'
import { cleanupAllRegisteredProcesses, trackChildProcess, trackSession } from '@/test/processRegistry'
import { reapTestOwnedProcesses } from '@/test/auditTestProcesses'
import { isProcessAlive } from '@/utils/process'
import { enumerateRunnerWrappers } from './wrapperInventory'
import { listProcessesForOrphanScan } from './orphanReap'

const repo = fileURLToPath(new URL('../../../', import.meta.url))
const cli = join(repo, 'cli')
const binary = resolve(process.env.HAPI_RUST_WRAPPER_BIN || join(repo, 'target/release/hapi-session-wrapper'))
const compiled = resolve(process.env.HAPI_COMPILED_CLI || join(cli, 'dist-exe/bun-linux-x64/hapi'))
const expectedBinary =
    (!!process.env.CI && process.env.CI !== 'false') ||
    process.env.HAPI_EXPECT_RUST_WRAPPER === '1' ||
    !!process.env.HAPI_WRAPPER_BENCH_OUT ||
    !!process.env.HAPI_RUST_WRAPPER_BIN ||
    (process.env.HAPI_RUST_WRAPPER_FLAVORS ?? '')
        .toLowerCase()
        .split(',')
        .some((flavor) => flavor.trim() === 'claude')
const skipTierB = process.platform !== 'linux' || (!existsSync(binary) && !expectedBinary)
const profile = process.env.HAPI_WRAPPER_BENCH_PROFILE
type Artifact = { path: string; dev: number; ino: number; sha256: string; bytes: number; version: string }
let artifacts: Record<Runtime, Artifact>
let revision: string

function identify(path: string): Artifact {
    if (!existsSync(path)) throw Error('Expected freshly built executable at ' + path)
    const initial = statSync(path)
    const version = execFileSync(path, ['--version'], { encoding: 'utf8' }).trim()
    const bytes = readFileSync(path)
    expect(bytes.subarray(0, 4).toString()).toBe('\x7fELF')
    const { dev, ino, size, mtimeMs } = statSync(path)
    expect([dev, ino, size, mtimeMs], 'Executable changed during identity check').toEqual([
        initial.dev,
        initial.ino,
        initial.size,
        initial.mtimeMs,
    ])
    return {
        path: realpathSync(path),
        dev,
        ino,
        bytes: bytes.length,
        version,
        sha256: createHash('sha256').update(bytes).digest('hex'),
    }
}
const fake = join(cli, 'src/test/fixtures/tier-b-claude.py')
const home = configuration.happyHomeDir
const bench = process.env.HAPI_WRAPPER_BENCH_OUT
const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms))
type Runtime = 'rust' | 'ts'
type Session = {
    id: string
    active: boolean
    activeAt: number
    metadata: {
        path: string
        wrapperRuntime: Runtime
        hostPid: number
        lifecycleState?: string
        claudeSessionId?: string
    }
}
type Message = {
    id: string
    content: {
        role: string
        content: {
            type: string
            data?: {
                type?: string
                message?: { content?: Array<{ text?: string; type?: string; tool_use_id?: string; content?: string }> }
            }
        }
    }
}

async function until<T>(label: string, read: () => Promise<T | undefined>, timeout = 30_000): Promise<T> {
    const end = Date.now() + timeout
    do {
        const value = await read()
        if (value !== undefined) return value
        await sleep(100)
    } while (Date.now() < end)
    throw Error('Timed out waiting for ' + label)
}

let jwt: string
async function api<T>(path: string, body?: object) {
    const res = await fetch(configuration.apiUrl + '/api' + path, {
        method: body ? 'POST' : 'GET',
        headers: { authorization: 'Bearer ' + jwt, 'content-type': 'application/json' },
        body: body && JSON.stringify(body),
        signal: AbortSignal.timeout(90_000),
    })
    if (!res.ok) throw Error(path + ': HTTP ' + res.status + ' ' + (await res.text()).slice(0, 500))
    return (await res.json()) as T
}
async function session(id: string): Promise<Session> {
    return (await api<{ session: Session }>('/sessions/' + id)).session
}
async function messages(id: string): Promise<Message[]> {
    return (await api<{ messages: Message[] }>('/sessions/' + id + '/messages?limit=100')).messages
}
async function sessions(): Promise<Session[]> {
    return (await api<{ sessions: Session[] }>('/sessions')).sessions
}
async function machine(id: string) {
    const r = await fetch(configuration.apiUrl + '/cli/machines/' + id, {
        headers: { authorization: 'Bearer ' + configuration.cliApiToken },
    })
    type Inventory = { scannedAt: number; tracked: number; adopted: number; reported: { pid: number }[] }
    type Machine = { active: boolean; runnerState?: { pid: number; status: string; wrapperInventory?: Inventory } }
    return r.ok ? ((await r.json()) as { machine: Machine }).machine : undefined
}
async function start(runtime: Runtime, bin = binary, executable = compiled, fixture: NodeJS.ProcessEnv = {}) {
    const child = spawn(executable, ['runner', 'start-sync'], {
        stdio: 'ignore',
        env: {
            ...buildTestChildEnv(),
            HAPI_RUST_WRAPPER_BIN: bin,
            HAPI_RUST_WRAPPER_FLAVORS: runtime === 'rust' ? 'claude' : 'none',
            HAPI_CLAUDE_PATH: fake,
            CLAUDE_CONFIG_DIR: join(home, 'claude'),
            HAPI_CLI_EXECUTABLE: executable,
            HAPI_TERMINAL_IDLE_TIMEOUT_MS: '3600000',
            ...fixture,
        },
    })
    trackChildProcess(child, 'tier-b-runner')
    return until('runner registration', async () => {
        if (child.exitCode !== null) throw Error('Runner exited ' + child.exitCode)
        const state = await readRunnerState()
        if (!state?.startedWithMachineId || state.pid !== child.pid) return
        const m = await machine(state.startedWithMachineId)
        if (m?.active && m.runnerState && m.runnerState.pid === child.pid && m.runnerState.status === 'running')
            return { id: state.startedWithMachineId, pid: state.pid, log: state.runnerLogPath! }
        return undefined
    })
}
async function stopRunner() {
    const state = await readRunnerState()
    if (state && isProcessAlive(state.pid)) {
        await stopRunnerHttp()
        await until('runner exit', async () => (isProcessAlive(state.pid) ? undefined : true))
    }
}
async function launch(machineId: string, directory: string, runtime: Runtime, expected = artifacts[runtime]) {
    const response = await api<{ type: string; sessionId: string }>('/machines/' + machineId + '/spawn', {
        directory,
        agent: 'claude',
        startingMode: 'remote',
        sessionType: 'simple',
    })
    if (response.type !== 'success') throw Error('spawn: ' + JSON.stringify(response))
    const id = response.sessionId
    trackSession(id, 'tier-b-session')
    const active = await until('active ' + runtime + ' wrapper', async () => {
        const row = await session(id)
        if (row.active && row.metadata?.wrapperRuntime === runtime && row.metadata.hostPid) return row
        return undefined
    })
    const running = statSync('/proc/' + active.metadata.hostPid + '/exe')
    expect([running.dev, running.ino]).toEqual([expected.dev, expected.ino])
    if (bench) {
        const env = readFileSync('/proc/' + active.metadata.hostPid + '/environ', 'utf8').split('\0')
        expect(env.includes('HAPI_API_URL=' + configuration.apiUrl)).toBe(true)
        if (configuration.apiUrl.startsWith('https://'))
            expect(env.includes('NODE_EXTRA_CA_CERTS=' + process.env.NODE_EXTRA_CA_CERTS)).toBe(true)
    }
    return active
}
async function turn(id: string, number: number, loaded = false) {
    const text = 'request-' + number
    await api('/sessions/' + id + '/messages', { text })
    return until('turn ' + number + ' including ready', async () => {
        const rows = await messages(id)
        const answers = rows
            .flatMap((r) => r.content.content.data?.message?.content ?? [])
            .filter((b) => b.text === 'fixture turn ' + number + ': ' + text)
        if (
            loaded &&
            !rows.some(
                (r) =>
                    Array.isArray(r.content.content.data?.message?.content) &&
                    r.content.content.data.message.content.some(
                        (b) =>
                            b.type === 'tool_result' &&
                            b.tool_use_id === 'read-' + number &&
                            b.content?.length === 34500,
                    ),
            )
        )
            return
        if (answers.length !== 1 || rows.at(-1)?.content.content.data?.type !== 'ready') return
        return rows
    })
}
type Task = { pid: number; parent: number; group: number; birth: string }
function processes(): Task[] {
    return readdirSync('/proc')
        .filter((v) => /^\d+$/.test(v))
        .flatMap((name) => {
            try {
                const line = readFileSync('/proc/' + name + '/stat', 'utf8')
                const fields = line.slice(line.lastIndexOf(') ') + 2).split(' ')
                return fields[0] === 'Z'
                    ? []
                    : [{ pid: Number(name), parent: Number(fields[1]), group: Number(fields[2]), birth: fields[19] }]
            } catch (error) {
                if (!['ENOENT', 'ESRCH'].includes((error as NodeJS.ErrnoException).code ?? '')) throw error
                return []
            }
        })
}
function owned(root: number) {
    const table = processes()
    const ids = new Set(table.filter((p) => p.group === root).map((p) => p.pid))
    ids.add(root)
    for (let size = -1; size !== ids.size; ) {
        size = ids.size
        for (const p of table) if (ids.has(p.parent)) ids.add(p.pid)
    }
    return table.filter((p) => ids.has(p.pid))
}
function stopped(root: number, before: Task[]) {
    const table = processes()
    expect(
        table.filter((p) => p.group === root || before.some((old) => old.pid === p.pid && old.birth === p.birth)),
    ).toEqual([])
}

describe.skipIf(skipTierB)('Rust Claude tier B', { timeout: 120_000 }, () => {
    beforeAll(async () => {
        revision = execFileSync('git', ['rev-parse', '--short=12', 'HEAD'], { cwd: repo, encoding: 'utf8' }).trim()
        expect(revision).toMatch(/^[a-f0-9]{12}$/)
        artifacts = { rust: identify(binary), ts: identify(compiled) }
        const receipt = JSON.parse(readFileSync(compiled + '.build.json', 'utf8'))
        expect(receipt, 'Compiled CLI build receipt must match the checkout and executable').toMatchObject({
            revision,
            sha256: artifacts.ts.sha256,
        })
        const stamp = artifacts.rust.version.match(/\(rust ([a-f0-9]{12,40})\)$/)
        expect(stamp?.[1].slice(0, 12), 'Rust wrapper must be built from the checkout HEAD').toBe(revision)
        const packageVersion = JSON.parse(readFileSync(join(cli, 'package.json'), 'utf8')).version
        expect(artifacts.ts.version).toBe('hapi version: ' + packageVersion)

        jwt = (await api<{ token: string }>('/auth', { accessToken: configuration.cliApiToken })).token
    })
    afterEach(async () => {
        rmSync(join(home, 'test-idle-session'), { force: true })
        await cleanupAllRegisteredProcesses()
        await stopRunner()
        expect(await reapTestOwnedProcesses(testOwnedMarker())).toEqual([])
    })
    it.each(['tracked', 'recovered', 'adopted'])(
        'stops the %s wrapper, its group and its setsid descendant',
        async (path) => {
            const directory = mkdtempSync(join(home, 'inventory-'))
            const marker = join(directory, 'descendants.json')
            const runner = await start('rust', binary, compiled, { TIER_B_DESCENDANTS: marker, TIER_B_LOADED: '1' })
            const row = await launch(runner.id, directory, 'rust')
            const pid = row.metadata.hostPid
            await turn(row.id, 1, true)
            if (path !== 'tracked') {
                await stopRunner()
                const recordsFile = configuration.runnerStateFile + '.resume-processes.json'
                const records = JSON.parse(readFileSync(recordsFile, 'utf8')) as {
                    pid: number
                    confirmedSessionId: string
                }[]
                expect(records).toContainEqual(expect.objectContaining({ pid, confirmedSessionId: row.id }))
                if (path === 'adopted') writeFileSync(recordsFile, JSON.stringify(records.filter((r) => r.pid !== pid)))
                const restartedAt = Date.now()
                const next = await start('rust')
                const inventory = await until('fresh inventory', async () => {
                    const inv = (await machine(next.id))?.runnerState?.wrapperInventory
                    return inv && inv.scannedAt >= restartedAt ? inv : undefined
                })
                expect(inventory).toMatchObject(
                    path === 'adopted' ? { adopted: 1, tracked: 0 } : { adopted: 0, tracked: 1 },
                )
                expect(inventory.reported.filter((r) => r.pid === pid)).toEqual([])
                if (path === 'adopted')
                    expect(readFileSync(next.log, 'utf8')).toContain(
                        'Adopted untracked wrapper PID ' + pid + ' for session ' + row.id,
                    )
                expect(
                    enumerateRunnerWrappers(await listProcessesForOrphanScan(), next.pid).find((w) => w.pid === pid),
                ).toMatchObject({ sessionId: row.id, wrapperRuntime: 'rust' })
            }
            const [inside, outside] = JSON.parse(readFileSync(marker, 'utf8'))
            const terminal = await openTerminal(row.id)
            try {
                const before = owned(pid)
                expect(before.find((p) => p.pid === inside)).toMatchObject({ group: pid })
                expect(before.find((p) => p.pid === outside)).toMatchObject({ group: outside })
                expect(await stopRunnerSession(row.id)).toBe('stopped')
                stopped(pid, before)
            } finally {
                terminal.socket.disconnect()
            }
        },
    )
    it.each([false, true])('hibernates and wakes TS → Rust → TS (native rotation: %s)', async (rotate) => {
        const runner = await start('ts')
        const directory = mkdtempSync(join(home, 'hibernate-'))
        const row = await launch(runner.id, directory, 'ts')
        let prior = await turn(row.id, 1)
        let native = await until('native resume id', async () => (await session(row.id)).metadata.claudeSessionId)
        let nativeFile = join(home, 'claude', 'projects', directory.replace(/[^a-zA-Z0-9]/g, '-'), native + '.jsonl')
        let previousNative = readFileSync(nativeFile)
        let n = 1
        for (const runtime of ['rust', 'ts'] as const) {
            const old = (await session(row.id)).metadata.hostPid
            const before = owned(old)
            writeFileSync(join(home, 'test-idle-session'), row.id)
            await until(
                'hub hibernation ' + runtime,
                async () => {
                    const current = await session(row.id)
                    if (!current.active && current.metadata.lifecycleState === 'hibernated') return true
                    return undefined
                },
                60_000,
            )
            stopped(old, before)
            await stopRunner()
            await start(runtime, binary, compiled, rotate ? { TIER_B_ROTATE: directory } : {})
            const turning = turn(row.id, ++n)
            void turning.catch(() => {})
            if (rotate) {
                const previous = native
                native = await until('rotated SessionStart hook metadata before SDK init', async () => {
                    const id = (await session(row.id)).metadata.claudeSessionId
                    return id && id !== previous ? id : undefined
                })
                writeFileSync(join(directory, native + '.continue'), '')
                nativeFile = join(dirname(nativeFile), native + '.jsonl')
            }
            const next = await turning
            expect(next.slice(0, prior.length)).toEqual(prior)
            const current = await session(row.id)
            expect(current.metadata.claudeSessionId).toBe(native)
            expect(current.active).toBe(true)
            const history = readFileSync(nativeFile)
            expect(history.subarray(0, previousNative.length).equals(previousNative)).toBe(true)
            expect(history.length).toBeGreaterThan(previousNative.length)
            previousNative = history
            const query =
                'import {Database} from "bun:sqlite"; const [path,id]=process.argv.slice(1); const db=new Database(path,{readonly:true}); console.log(db.query("SELECT COUNT(*) AS n FROM session_redirects WHERE old_id=? OR new_id=?").get(id,id).n)'
            const redirects = await promisify(execFile)(process.env.HAPI_BUN_EXEC!, [
                '-e',
                query,
                join(home, 'hapi.db'),
                row.id,
            ])
            expect(redirects.stdout.trim()).toBe('0')
            expect(current.metadata.wrapperRuntime).toBe(runtime)
            expect((await sessions()).filter((s) => s.metadata?.path === directory).map((s) => s.id)).toEqual([row.id])
            prior = next
        }
    })
    it.each(['missing', 'exit-86'])('falls back after %s with one hub row', async (reason) => {
        const dir = mkdtempSync(join(home, 'fallback-'))
        const bin = join(dir, 'hapi-session-wrapper')
        if (reason === 'exit-86') {
            // Fail Rust hook setup after REST adoption. The TS retry uses the original HAPI_HOME.
            symlinkSync(join(home, 'settings.json'), join(dir, 'settings.json'))
            writeFileSync(join(dir, 'tmp'), 'not a directory')
            const quote = (arg: string) => "'" + arg.replaceAll("'", "'\"'\"'") + "'"
            writeFileSync(bin, '#!/bin/sh\nexport HAPI_HOME=' + quote(dir) + '\nexec ' + quote(binary) + ' "$@"\n', {
                mode: 0o755,
            })
        }
        const executable = reason === 'missing' ? join(dir, 'hapi') : compiled
        if (reason === 'missing') copyFileSync(compiled, executable)
        const expected = reason === 'missing' ? identify(executable) : artifacts.ts
        expect(expected.sha256).toBe(artifacts.ts.sha256)
        const runner = await start('rust', bin, executable)
        const row = await launch(runner.id, dir, 'ts', expected)
        await turn(row.id, 1)
        expect((await sessions()).filter((s) => s.metadata?.path === dir).map((s) => s.id)).toEqual([row.id])
        const log = readFileSync(runner.log, 'utf8')
        expect(log).toContain(reason === 'missing' ? 'binary unavailable' : 'Retrying with TS')
        if (reason === 'exit-86') {
            expect(log).toContain('"exitCode":86')
            expect(log).toContain('--existing-session-id ' + row.id)
        }
    })
    it.skipIf(!bench)('measures wrapper cohorts', { timeout: 2_100_000 }, async () => {
        if (!['http-ts', 'http-rust', 'https-light', 'https-loaded'].includes(profile ?? ''))
            throw Error('Invalid HAPI_WRAPPER_BENCH_PROFILE')
        const secure = profile!.startsWith('https')
        const loaded = profile === 'https-loaded'
        expect(new URL(configuration.apiUrl).protocol).toBe(secure ? 'https:' : 'http:')
        if (secure) expect(existsSync(process.env.NODE_EXTRA_CA_CERTS!)).toBe(true)
        const runtimes: Runtime[] = profile === 'http-ts' ? ['ts'] : ['ts', 'rust']
        type Cohort = { pids: number[]; ids: string[]; latencyMs: number[]; transcriptBytes: number[] }
        const cohorts: Partial<Record<Runtime, Cohort>> = {}
        const runs: unknown[] = []
        const terminals: { socket: Socket; id: string }[] = []
        async function measure(label: string, warmup: number) {
            const script = join(repo, 'benchmarks/wrapper-compare/sample.py')
            await promisify(execFile)(
                'python3',
                [script, JSON.stringify(cohorts), resolve(bench!) + '.sample', String(warmup)],
                { timeout: (warmup + 120) * 1000, env: { ...process.env, HAPI_BENCH_WEB_TOKEN: jwt } },
            )
            for (const terminal of terminals) await checkTerminal(terminal.socket, terminal.id)
            const sample = JSON.parse(readFileSync(resolve(bench!) + '.sample', 'utf8'))
            runs.push({
                ...sample,
                label,
                turns: loaded ? 30 : 1,
                openTerminals: terminals.length,
                ...(secure
                    ? {
                          tlsLeafSha256: createHash('sha256')
                              .update(readFileSync(process.env.HAPI_BENCH_TLS_CERT!))
                              .digest('hex'),
                          extraCaSha256: createHash('sha256')
                              .update(readFileSync(process.env.NODE_EXTRA_CA_CERTS!))
                              .digest('hex'),
                      }
                    : {}),
                transport: secure ? 'HTTPS/WSS' : 'HTTP/WS',
            })
            writeFileSync(resolve(bench!), JSON.stringify({ revision, artifacts, runs }, null, 2) + '\n')
            rmSync(resolve(bench!) + '.sample')
        }
        try {
            for (const runtime of runtimes) {
                const runner = await start(
                    runtime,
                    binary,
                    compiled,
                    loaded ? { TIER_B_LOADED: '1', SHELL: '/bin/sh', ENV: '/dev/null', HOME: home } : {},
                )
                const group: Cohort = { pids: [], ids: [], latencyMs: [], transcriptBytes: [] }
                cohorts[runtime] = group
                for (let i = 0; i < 20; i++) {
                    const row = await launch(runner.id, mkdtempSync(join(home, 'measure-')), runtime)
                    group.latencyMs.push(startupLatency(runner.log, row.id))
                    if (i < 10) {
                        expect(await stopRunnerSession(row.id)).toBe('stopped')
                        continue
                    }
                    for (let n = 1; n <= (loaded ? 30 : 1); n++) await turn(row.id, n, loaded)
                    if (loaded) terminals.push(await openTerminal(row.id))
                    const native = await until(
                        'native session id',
                        async () => (await session(row.id)).metadata.claudeSessionId,
                    )
                    const file = join(
                        home,
                        'claude',
                        'projects',
                        row.metadata.path.replace(/[^a-zA-Z0-9]/g, '-'),
                        native + '.jsonl',
                    )
                    const bytes = statSync(file).size
                    if (loaded) {
                        expect(bytes).toBeGreaterThanOrEqual(1_000_000)
                        expect(bytes).toBeLessThan(1_200_000)
                    }
                    group.transcriptBytes.push(bytes)
                    group.ids.push(row.id)
                    group.pids.push(row.metadata.hostPid)
                }
                if (runtimes.length === 2 && runtime === 'ts') await stopRunner()
            }
            await measure(profile + '-ten', 600)
            if (!secure) {
                const runtime = profile === 'http-ts' ? 'ts' : 'rust'
                if (runtime === 'rust') {
                    for (const id of cohorts.ts!.ids) expect(await stopRunnerSession(id)).toBe('stopped')
                    delete cohorts.ts
                }
                const cohort = cohorts[runtime]!
                for (const id of cohort.ids.slice(1)) expect(await stopRunnerSession(id)).toBe('stopped')
                await stopRunner()
                cohorts[runtime] = {
                    ids: cohort.ids.slice(0, 1),
                    pids: cohort.pids.slice(0, 1),
                    latencyMs: [],
                    transcriptBytes: cohort.transcriptBytes.slice(0, 1),
                }
                const only = cohort.pids[0]
                const residents = processes()
                    .filter((p) => {
                        try {
                            const exe = statSync('/proc/' + p.pid + '/exe')
                            return exe.dev === artifacts[runtime].dev && exe.ino === artifacts[runtime].ino
                        } catch (e) {
                            if (!['ENOENT', 'ESRCH', 'EACCES'].includes((e as NodeJS.ErrnoException).code ?? ''))
                                throw e
                            return false
                        }
                    })
                    .map((p) => p.pid)
                expect(residents).toEqual([only])
                await measure(profile + '-single', 0)
                await start(runtime)
                expect(await stopRunnerSession(cohort.ids[0])).toBe('stopped')
            }
        } finally {
            for (const { socket } of terminals) socket.disconnect()
        }
    })
})

function startupLatency(logPath: string, id: string) {
    const log = readFileSync(logPath, 'utf8').split('\n')
    const end = log.findIndex((line) => line.includes('[CONTROL SERVER] Session started: ' + id))
    const begin = log
        .slice(0, end)
        .reverse()
        .find((line) => line.includes('[RUNNER RUN] Session wrapper selection'))
    if (!begin || end < 0) throw Error('Missing runner timestamps for ' + id)
    function ms(line: string) {
        const m = /^\[(\d+):(\d+):(\d+)\.(\d{3})\]/.exec(line)
        if (!m) throw Error('Missing log timestamp')
        return (((Number(m[1]) % 24) * 60 + Number(m[2])) * 60 + Number(m[3])) * 1000 + Number(m[4])
    }
    return (ms(log[end]) - ms(begin) + 86400000) % 86400000
}

async function openTerminal(sessionId: string) {
    const socket = io(configuration.apiUrl + '/terminal', {
        auth: { token: jwt },
        transports: ['websocket'],
        reconnection: false,
        forceNew: true,
    })
    const id = randomUUID()
    try {
        await new Promise<void>((resolve, reject) => {
            const timer = setTimeout(() => reject(Error('PTY open timed out')), 10_000)
            socket.once('connect_error', reject)
            socket.once('terminal:error', (e) => {
                clearTimeout(timer)
                reject(Error(JSON.stringify(e)))
            })
            socket.once('terminal:ready', () => {
                clearTimeout(timer)
                resolve()
            })
            socket.emit('terminal:create', { sessionId, terminalId: id, cols: 80, rows: 24 })
        })
        await checkTerminal(socket, id)
        return { socket, id }
    } catch (e) {
        socket.disconnect()
        throw e
    }
}
async function checkTerminal(socket: Socket, id: string) {
    const nonce = randomUUID()
    expect(socket.connected).toBe(true)
    await new Promise<void>((resolve, reject) => {
        let output = ''
        const onOutput = (event: { terminalId: string; data: string }) => {
            if (event.terminalId !== id) return
            output += event.data
            if (output.includes('alive-' + nonce)) {
                clearTimeout(timer)
                socket.off('terminal:output', onOutput)
                resolve()
            }
        }
        const timer = setTimeout(() => {
            socket.off('terminal:output', onOutput)
            reject(Error('PTY failed liveness check'))
        }, 5000)
        socket.on('terminal:output', onOutput)
        socket.emit('terminal:write', { terminalId: id, data: `printf '%s%s\\n' alive- ${nonce}\n` })
    })
}

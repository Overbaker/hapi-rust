import { afterEach, beforeAll, describe, expect, it } from 'vitest'
import { spawn, execFile } from 'node:child_process'
import { promisify } from 'node:util'
import {
    cpSync,
    existsSync,
    mkdirSync,
    mkdtempSync,
    readFileSync,
    readdirSync,
    statSync,
    symlinkSync,
    writeFileSync,
} from 'node:fs'
import { join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
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
const fake = join(cli, 'src/test/fixtures/tier-b-claude.py')
const home = configuration.happyHomeDir
const bench = process.env.HAPI_WRAPPER_BENCH_OUT
const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms))
type Runtime = 'rust' | 'ts'
type Session = {
    id: string
    active: boolean
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
        content: { type: string; data?: { type?: string; message?: { content?: Array<{ text?: string }> } } }
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
async function start(runtime: Runtime, bin = binary, root = cli) {
    const child = spawn(
        process.env.HAPI_BUN_EXEC!,
        ['--cwd', root, join(root, 'src/index.ts'), 'runner', 'start-sync'],
        {
            stdio: 'ignore',
            env: {
                ...buildTestChildEnv(),
                HAPI_RUST_WRAPPER_BIN: bin,
                HAPI_RUST_WRAPPER_FLAVORS: runtime === 'rust' ? 'claude' : 'none',
                HAPI_CLAUDE_PATH: fake,
                CLAUDE_CONFIG_DIR: join(home, 'claude'),
                HAPI_CLI_EXECUTABLE: join(root, 'bin/hapi.cjs'),
            },
        },
    )
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
async function launch(machineId: string, directory: string, runtime: Runtime) {
    const response = await api<{ type: string; sessionId: string }>('/machines/' + machineId + '/spawn', {
        directory,
        agent: 'claude',
        startingMode: 'remote',
        sessionType: 'simple',
    })
    if (response.type !== 'success') throw Error('spawn: ' + JSON.stringify(response))
    const id = response.sessionId
    trackSession(id, 'tier-b-session')
    return until('active ' + runtime + ' wrapper', async () => {
        const row = await session(id)
        if (row.active && row.metadata?.wrapperRuntime === runtime && row.metadata.hostPid) return row
        return undefined
    })
}
async function turn(id: string, number: number) {
    const text = 'request-' + number
    await api('/sessions/' + id + '/messages', { text })
    return until('turn ' + number + ' including ready', async () => {
        const rows = await messages(id)
        const answers = rows
            .flatMap((r) => r.content.content.data?.message?.content ?? [])
            .filter((b) => b.text === 'fixture turn ' + number + ': ' + text)
        if (answers.length !== 1 || rows.at(-1)?.content.content.data?.type !== 'ready') return
        return rows
    })
}
function group(pgid: number): number[] {
    return readdirSync('/proc')
        .filter((v) => /^\d+$/.test(v))
        .map(Number)
        .filter((pid) => {
            try {
                const stat = readFileSync('/proc/' + pid + '/stat', 'utf8')
                const [state, , group] = stat.slice(stat.lastIndexOf(') ') + 2).split(' ')
                return state !== 'Z' && Number(group) === pgid
            } catch (e) {
                if ((e as NodeJS.ErrnoException).code !== 'ENOENT') throw e
                return false
            }
        })
}
function isolatedCli() {
    const checkout = mkdtempSync(join(home, 'without-rust-'))
    const root = join(checkout, 'cli')
    mkdirSync(root)
    cpSync(join(cli, 'src'), join(root, 'src'), { recursive: true })
    for (const file of ['package.json', 'tsconfig.json', 'bunfig.toml']) cpSync(join(cli, file), join(root, file))
    mkdirSync(join(root, 'bin'))
    cpSync(join(cli, 'bin/hapi.cjs'), join(root, 'bin/hapi.cjs'))
    for (const file of ['tools', 'node_modules']) symlinkSync(join(cli, file), join(root, file))
    for (const file of ['node_modules', 'shared', 'tsconfig.base.json'])
        symlinkSync(join(repo, file), join(checkout, file))
    return root
}

describe.skipIf(process.platform !== 'linux' || !existsSync(binary))('Rust Claude tier B', { timeout: 120_000 }, () => {
    beforeAll(async () => {
        jwt = (await api<{ token: string }>('/auth', { accessToken: configuration.cliApiToken })).token
    })
    afterEach(async () => {
        await cleanupAllRegisteredProcesses()
        await stopRunner()
        expect(await reapTestOwnedProcesses(testOwnedMarker())).toEqual([])
    })
    it('tracks the active Rust generation across a runner restart and stops the entire group', async () => {
        const runner = await start('rust')
        const row = await launch(runner.id, mkdtempSync(join(home, 'inventory-')), 'rust')
        const pid = row.metadata.hostPid
        expect(statSync('/proc/' + pid + '/exe').ino).toBe(statSync(binary).ino)
        await turn(row.id, 1)
        expect(group(pid).length).toBeGreaterThan(1)
        await stopRunner()
        expect(isProcessAlive(pid)).toBe(true)
        const restartedAt = Date.now()
        const next = await start('rust')
        const inventory = await until('fresh wrapper inventory', async () => {
            const inv = (await machine(next.id))?.runnerState?.wrapperInventory
            if (inv && inv.scannedAt >= restartedAt) return inv
            return undefined
        })
        const live = enumerateRunnerWrappers(await listProcessesForOrphanScan(), next.pid)
        expect(live.find((wrapper) => wrapper.pid === pid)).toMatchObject({ sessionId: row.id, wrapperRuntime: 'rust' })
        expect(inventory.tracked + inventory.adopted).toBeGreaterThan(0)
        expect(inventory.reported.filter((r: { pid: number }) => r.pid === pid)).toEqual([])
        const records = JSON.parse(readFileSync(configuration.runnerStateFile + '.resume-processes.json', 'utf8'))
        expect(records).toContainEqual(
            expect.objectContaining({ pid, confirmedSessionId: row.id, wrapperRuntime: 'rust' }),
        )
        expect(await stopRunnerSession(row.id)).toBe('stopped')
        expect(group(pid)).toEqual([])
    })
    it('hibernates and wakes TS → Rust → TS on the original native and hub transcript', async () => {
        const runner = await start('ts')
        const directory = mkdtempSync(join(home, 'hibernate-'))
        const row = await launch(runner.id, directory, 'ts')
        let prior = await turn(row.id, 1)
        const native = await until('native resume id', async () => (await session(row.id)).metadata.claudeSessionId)
        const nativeFile = join(home, 'claude', 'projects', directory.replace(/[^a-zA-Z0-9]/g, '-'), native + '.jsonl')
        let previousNative = readFileSync(nativeFile)
        let n = 1
        for (const runtime of ['rust', 'ts'] as const) {
            const old = (await session(row.id)).metadata.hostPid
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
            expect(group(old)).toEqual([])
            await stopRunner()
            await start(runtime)
            const next = await turn(row.id, ++n)
            expect(next.slice(0, prior.length)).toEqual(prior)
            const current = await session(row.id)
            expect(current.id).toBe(row.id)
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
            expect(current.metadata).toMatchObject({ wrapperRuntime: runtime, claudeSessionId: native })
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
        const runner = await start('rust', bin, reason === 'missing' ? isolatedCli() : cli)
        const row = await launch(runner.id, dir, 'ts')
        await turn(row.id, 1)
        expect((await sessions()).filter((s) => s.metadata?.path === dir).map((s) => s.id)).toEqual([row.id])
        const log = readFileSync(runner.log, 'utf8')
        expect(log).toContain(reason === 'missing' ? 'binary unavailable' : 'Retrying with TS')
        if (reason === 'exit-86') {
            expect(log).toContain('"exitCode":86')
            expect(log).toContain('--existing-session-id ' + row.id)
        }
    })
    it.skipIf(!bench)('measures wrapper cohorts', { timeout: 1_500_000 }, async () => {
        const cohorts: Record<string, { pids: number[]; latencyMs: number[] }> = {}
        for (const runtime of ['ts', 'rust'] as const) {
            const runner = await start(runtime)
            const pids: number[] = [],
                latencyMs: number[] = []
            for (let i = 0; i < 20; i++) {
                const row = await launch(runner.id, mkdtempSync(join(home, 'measure-')), runtime)
                const log = readFileSync(runner.log, 'utf8').split('\n')
                const end = log.findIndex((line) => line.includes('[CONTROL SERVER] Session started: ' + row.id))
                const begin = log
                    .slice(0, end)
                    .reverse()
                    .find((line) => line.includes('[RUNNER RUN] Session wrapper selection'))
                if (!begin || end < 0) throw Error('Missing runner timestamps for ' + row.id)
                const ms = (line: string) => {
                    const m = /^\[(\d+):(\d+):(\d+)\.(\d{3})\]/.exec(line)
                    if (!m) throw Error('Missing log timestamp')
                    return (((Number(m[1]) % 24) * 60 + Number(m[2])) * 60 + Number(m[3])) * 1000 + Number(m[4])
                }
                latencyMs.push((ms(log[end]) - ms(begin) + 86400000) % 86400000)
                if (i < 10) {
                    expect(await stopRunnerSession(row.id)).toBe('stopped')
                    continue
                }
                await turn(row.id, 1)
                pids.push(row.metadata.hostPid)
            }
            cohorts[runtime] = { pids, latencyMs }
            if (runtime === 'ts') await stopRunner()
        }
        const script = join(repo, 'benchmarks/wrapper-compare/sample.py')
        await promisify(execFile)('python3', [script, JSON.stringify(cohorts), resolve(bench!)], { timeout: 720_000 })
    })
})

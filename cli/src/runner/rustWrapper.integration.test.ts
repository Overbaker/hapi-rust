/**
 * Runner integration tests for Rust session wrapper selection.
 *
 * The first suite uses a fake executable named `hapi-session-wrapper` to
 * exercise the runner switch. The second suite drives the real Rust binary
 * (HAPI_RUST_WRAPPER_BIN or target/release) through a remote Claude turn,
 * a terminal, a stop and the exit-86 fallback. Set HAPI_EXPECT_RUST_WRAPPER=1
 * to fail instead of skipping when the binary is missing.
 * Run through the serial integration project (vitest.integration.config.ts).
 */
import { afterAll, afterEach, beforeEach, describe, expect, it } from 'vitest'
import { spawn } from 'node:child_process'
import { randomUUID } from 'node:crypto'
import { existsSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, statSync, symlinkSync, writeFileSync } from 'node:fs'
import { join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { io, type Socket } from 'socket.io-client'
import { configuration } from '@/configuration'
import { readRunnerState } from '@/persistence'
import { listRunnerSessions, spawnRunnerSession, stopRunner, stopRunnerSession } from '@/runner/controlClient'
import { getHappyCliCommand } from '@/utils/spawnHappyCLI'
import { isProcessAlive } from '@/utils/process'
import { buildTestChildEnv, testOwnedMarker } from '@/test/integrationEnv'
import { cleanupAllRegisteredProcesses, trackChildProcess, trackRunnerPid, trackSession } from '@/test/processRegistry'
import { reapTestOwnedProcesses } from '@/test/auditTestProcesses'

const cli = fileURLToPath(new URL('../../', import.meta.url))
const releaseBinary = resolve(cli, '..', 'target', 'release', 'hapi-session-wrapper')

async function waitFor(condition: () => Promise<boolean>, timeout = 10_000, interval = 100): Promise<void> {
    const start = Date.now()
    while (Date.now() - start < timeout) {
        if (await condition()) return
        await new Promise(r => setTimeout(r, interval))
    }
    throw new Error('Timeout waiting for condition')
}

async function isServerHealthy(): Promise<boolean> {
    if (!configuration.cliApiToken) return false
    try {
        const response = await fetch(`${configuration.apiUrl}/cli/machines/__healthcheck__`, {
            headers: { Authorization: `Bearer ${configuration.cliApiToken}` },
            signal: AbortSignal.timeout(1000),
        })
        return response.status !== 401 && response.status !== 503
    } catch {
        return false
    }
}

type Mode = 'exit-86' | 'exit-1' | 'webhook-then-86' | 'exit-86-on-stop'

describe.skipIf(process.platform !== 'linux' || !await isServerHealthy())('Rust wrapper runner switch', { timeout: 60_000 }, () => {
    let dir: string
    let modeFile: string
    let countFile: string
    let grandchildFile: string
    let wrapper: string
    let runnerLog: string

    function writeFixtures(): void {
        dir = join(configuration.happyHomeDir, `rust-wrapper-${randomUUID()}`)
        mkdirSync(dir)
        modeFile = join(dir, 'mode')
        countFile = join(dir, 'count')
        grandchildFile = join(dir, 'grandchild')
        wrapper = join(dir, 'hapi-session-wrapper')
        const webhook = join(dir, 'webhook.mjs')
        writeFileSync(webhook, `
import { readFileSync } from 'node:fs'
const state = JSON.parse(readFileSync(${JSON.stringify(configuration.runnerStateFile)}, 'utf8'))
const sessionId = process.argv[2]
const metadata = { path: process.cwd(), host: 'fake', homeDir: '/tmp', happyHomeDir: '/tmp', happyLibDir: '/tmp',
    happyToolsDir: '/tmp', hostPid: process.pid, startedBy: 'runner', wrapperRuntime: 'rust' }
await fetch('http://127.0.0.1:' + state.httpPort + '/session-started', {
    method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ sessionId, metadata }) })
process.exit(86)
`)
        const q = (value: string) => `'${value.replace(/'/g, `'\\''`)}'`
        writeFileSync(wrapper, `#!/bin/sh
echo "$$" >> ${q(countFile)}
case "$(cat ${q(modeFile)})" in
  exit-86) sleep 30 & echo "$!" > ${q(grandchildFile)}; exit 86 ;;
  exit-1) exit 1 ;;
  exit-86-on-stop) trap 'exit 86' TERM; while :; do sleep 0.1; done ;;
  webhook-then-86) exec ${q(process.execPath)} ${q(webhook)} ${q(randomUUID())} ;;
esac
exit 2
`, { mode: 0o755 })
    }

    async function startRunner(env: NodeJS.ProcessEnv): Promise<void> {
        const { command, args } = getHappyCliCommand(['runner', 'start-sync'])
        const child = spawn(command, args, {
            detached: true,
            stdio: 'ignore',
            env: {
                ...buildTestChildEnv(),
                HAPI_DISABLE_VERSION_HANDOFF: '1',
                HAPI_RUST_WRAPPER_FLAVORS: 'claude',
                ...env,
            },
        })
        trackChildProcess(child, 'rust-wrapper-runner')
        child.unref()
        await waitFor(async () => {
            const state = await readRunnerState()
            return state !== null && state.pid === child.pid && isProcessAlive(state.pid)
        })
        const state = (await readRunnerState())!
        trackRunnerPid(state.pid, 'rust-wrapper-runner')
        runnerLog = state.runnerLogPath!
        const machineId = state.startedWithMachineId
        await waitFor(async () => {
            const response = await fetch(`${configuration.apiUrl}/cli/machines/${machineId}`, {
                headers: { Authorization: `Bearer ${configuration.cliApiToken}` },
                signal: AbortSignal.timeout(1000),
            })
            if (!response.ok) return false
            const { machine } = await response.json() as { machine?: { active: boolean; runnerState?: { status: string; pid?: number } | null } }
            return machine?.active === true && machine.runnerState?.status === 'running' && machine.runnerState.pid === state.pid
        })
    }

    async function spawnWith(mode: Mode, env: NodeJS.ProcessEnv = { HAPI_RUST_WRAPPER_BIN: wrapper }, sessionId?: string) {
        writeFileSync(modeFile, mode)
        await startRunner(env)
        const response = await spawnRunnerSession('/tmp', sessionId)
        if (response?.sessionId) trackSession(response.sessionId, `rust-wrapper-session:${response.sessionId}`)
        return response
    }

    const rustSpawns = () => existsSync(countFile) ? readFileSync(countFile, 'utf8').trim().split('\n').filter(Boolean).length : 0
    const runnerLogText = () => readFileSync(runnerLog, 'utf8')
    const tsSpawns = () => runnerLogText().split('\n').filter(line => line.includes('[SPAWN HAPI CLI] Spawning: hapi claude')).length

    beforeEach(async () => {
        await stopRunner()
        writeFixtures()
    })

    afterEach(async () => {
        await cleanupAllRegisteredProcesses()
        await Promise.race([stopRunner(), new Promise(r => setTimeout(r, 10_000))])
        expect(await reapTestOwnedProcesses(testOwnedMarker()), 'test-owned processes survived').toEqual([])
    })

    afterAll(async () => {
        await cleanupAllRegisteredProcesses()
        await Promise.race([stopRunner(), new Promise(r => setTimeout(r, 10_000))])
        await reapTestOwnedProcesses(testOwnedMarker())
    })

    it('retries once with TypeScript after exit 86 before the webhook, once the Rust tree is gone', async () => {
        const response = await spawnWith('exit-86')
        expect(response).toHaveProperty('success', true)
        expect(rustSpawns()).toBe(1)
        expect(tsSpawns()).toBe(1)
        const grandchild = Number(readFileSync(grandchildFile, 'utf8').trim())
        expect(isProcessAlive(grandchild)).toBe(false)
        const log = runnerLogText()
        expect(log.indexOf('Retrying with TS')).toBeGreaterThan(-1)
        expect(log.indexOf('Retrying with TS')).toBeLessThan(log.indexOf('[SPAWN HAPI CLI] Spawning: hapi claude'))
        const tracked = (await listRunnerSessions()).find((s: { happySessionId: string }) => s.happySessionId === response.sessionId)
        expect(tracked?.startedBy).toBe('runner')
        expect(await stopRunnerSession(response.sessionId)).toBe('stopped')
    })

    it('does not retry after any other exit code', async () => {
        const response = await spawnWith('exit-1')
        expect(response).not.toHaveProperty('success', true)
        expect(runnerLogText()).toMatch(/Child PID \d+ exited with code 1,/)
        expect(rustSpawns()).toBe(1)
        expect(tsSpawns()).toBe(0)
        expect(runnerLogText()).not.toContain('Retrying with TS')
    })

    it('does not retry an exit 86 that comes after the session-started webhook', async () => {
        const response = await spawnWith('webhook-then-86')
        expect(response).toHaveProperty('success', true)
        await waitFor(async () => !(await listRunnerSessions()).some((s: { happySessionId: string }) => s.happySessionId === response.sessionId))
        expect(rustSpawns()).toBe(1)
        expect(tsSpawns()).toBe(0)
        expect(runnerLogText()).not.toContain('Retrying with TS')
    })

    it('does not retry when the session is stopped during Rust startup', async () => {
        const sessionId = randomUUID()
        writeFileSync(modeFile, 'exit-86-on-stop')
        await startRunner({ HAPI_RUST_WRAPPER_BIN: wrapper })
        const pending = spawnRunnerSession('/tmp', sessionId)
        await waitFor(async () => rustSpawns() === 1)
        expect(await stopRunnerSession(sessionId)).toBe('stopped')
        expect(await pending).not.toHaveProperty('success', true)
        expect(rustSpawns()).toBe(1)
        expect(tsSpawns()).toBe(0)
        expect(runnerLogText()).not.toContain('Retrying with TS')
    })

    it.skipIf(existsSync(releaseBinary))('starts TypeScript directly when the Rust binary is missing', async () => {
        const response = await spawnWith('exit-86', { HAPI_RUST_WRAPPER_BIN: join(dir, 'missing', 'hapi-session-wrapper') })
        expect(response).toHaveProperty('success', true)
        expect(rustSpawns()).toBe(0)
        expect(tsSpawns()).toBe(1)
        expect(runnerLogText()).toContain('binary unavailable')
        const records = JSON.parse(readFileSync(`${configuration.runnerStateFile}.resume-processes.json`, 'utf8')) as { confirmedSessionId?: string; wrapperRuntime?: string }[]
        expect(records.find(record => record.confirmedSessionId === response.sessionId)?.wrapperRuntime).toBe('ts')
        expect(await stopRunnerSession(response.sessionId)).toBe('stopped')
    })
})

const realBinary = resolve(process.env.HAPI_RUST_WRAPPER_BIN || releaseBinary)
const expectRealBinary = process.env.HAPI_EXPECT_RUST_WRAPPER === '1'
const fakeClaude = join(cli, 'src/test/fixtures/tier-b-claude.py')
const sleep = (ms: number) => new Promise(r => setTimeout(r, ms))

type Task = { pid: number; parent: number; group: number; birth: string }
function processes(): Task[] {
    return readdirSync('/proc').filter(v => /^\d+$/.test(v)).flatMap(name => {
        try {
            const line = readFileSync('/proc/' + name + '/stat', 'utf8')
            const fields = line.slice(line.lastIndexOf(') ') + 2).split(' ')
            return fields[0] === 'Z' ? [] : [{ pid: Number(name), parent: Number(fields[1]), group: Number(fields[2]), birth: fields[19] }]
        } catch {
            return []
        }
    })
}
function owned(root: number): Task[] {
    const table = processes()
    const ids = new Set(table.filter(p => p.group === root).map(p => p.pid))
    ids.add(root)
    for (let size = -1; size !== ids.size;) {
        size = ids.size
        for (const p of table) if (ids.has(p.parent)) ids.add(p.pid)
    }
    return table.filter(p => ids.has(p.pid))
}

describe.skipIf(process.platform !== 'linux' || (!existsSync(realBinary) && !expectRealBinary) || !await isServerHealthy())(
    'Rust Claude wrapper (real binary)', { timeout: 120_000 }, () => {
    let jwt = ''
    async function until<T>(label: string, read: () => Promise<T | undefined>, timeout = 30_000): Promise<T> {
        const end = Date.now() + timeout
        do {
            const value = await read()
            if (value !== undefined) return value
            await sleep(100)
        } while (Date.now() < end)
        throw new Error('Timed out waiting for ' + label)
    }
    async function api<T>(path: string, body?: object): Promise<T> {
        const res = await fetch(configuration.apiUrl + '/api' + path, {
            method: body ? 'POST' : 'GET',
            headers: { authorization: 'Bearer ' + jwt, 'content-type': 'application/json' },
            body: body && JSON.stringify(body),
            signal: AbortSignal.timeout(30_000),
        })
        if (!res.ok) throw new Error(path + ': HTTP ' + res.status + ' ' + (await res.text()).slice(0, 500))
        return await res.json() as T
    }
    type Row = { id: string; active: boolean; metadata: { path: string; wrapperRuntime?: string; hostPid?: number } }
    type Message = { content: { content: { data?: { type?: string; message?: { content?: { text?: string }[] } } } } }
    const session = async (id: string) => (await api<{ session: Row }>('/sessions/' + id)).session
    const messages = async (id: string) => (await api<{ messages: Message[] }>('/sessions/' + id + '/messages?limit=100')).messages
    const sessions = async () => (await api<{ sessions: Row[] }>('/sessions')).sessions

    async function startRunner(bin: string, env: NodeJS.ProcessEnv = {}): Promise<string> {
        const { command, args } = getHappyCliCommand(['runner', 'start-sync'])
        const child = spawn(command, args, {
            detached: true,
            stdio: 'ignore',
            env: {
                ...buildTestChildEnv(),
                HAPI_DISABLE_VERSION_HANDOFF: '1',
                HAPI_RUST_WRAPPER_BIN: bin,
                HAPI_CLAUDE_PATH: fakeClaude,
                CLAUDE_CONFIG_DIR: join(configuration.happyHomeDir, 'claude'),
                ...env,
            },
        })
        trackChildProcess(child, 'rust-real-runner')
        child.unref()
        const state = await until('runner registration', async () => {
            const state = await readRunnerState()
            if (!state || state.pid !== child.pid || !isProcessAlive(state.pid)) return undefined
            const response = await fetch(`${configuration.apiUrl}/cli/machines/${state.startedWithMachineId}`, {
                headers: { Authorization: `Bearer ${configuration.cliApiToken}` },
                signal: AbortSignal.timeout(1000),
            })
            if (!response.ok) return undefined
            const { machine } = await response.json() as { machine?: { active: boolean; runnerState?: { status: string; pid?: number } | null } }
            return machine?.active && machine.runnerState?.status === 'running' && machine.runnerState.pid === state.pid ? state : undefined
        })
        trackRunnerPid(state.pid, 'rust-real-runner')
        return state.runnerLogPath!
    }
    async function launch(directory: string, runtime: 'rust' | 'ts'): Promise<Row> {
        const response = await spawnRunnerSession(directory)
        expect(response).toHaveProperty('success', true)
        trackSession(response.sessionId, 'rust-real-session')
        return until('active ' + runtime + ' wrapper', async () => {
            const row = await session(response.sessionId)
            return row.active && (row.metadata?.wrapperRuntime ?? 'ts') === runtime && row.metadata.hostPid ? row : undefined
        })
    }
    async function turn(id: string, n: number) {
        const text = 'request-' + n
        await api('/sessions/' + id + '/messages', { text })
        return until('turn ' + n, async () => {
            const rows = await messages(id)
            const answers = rows.flatMap(r => r.content.content.data?.message?.content ?? [])
                .filter(b => b.text === 'fixture turn ' + n + ': ' + text)
            return answers.length === 1 && rows.at(-1)?.content.content.data?.type === 'ready' ? rows : undefined
        })
    }
    async function checkTerminal(sessionId: string): Promise<Socket> {
        const socket = io(configuration.apiUrl + '/terminal', { auth: { token: jwt }, transports: ['websocket'], reconnection: false, forceNew: true })
        const id = randomUUID()
        await new Promise<void>((resolve, reject) => {
            const write = () => socket.emit('terminal:write', { terminalId: id, data: "printf '%s%s\\n' alive- ok\n" })
            const writer = setInterval(write, 500)
            const timer = setTimeout(() => {
                clearInterval(writer)
                reject(new Error('PTY failed liveness check'))
            }, 10_000)
            let output = ''
            socket.on('terminal:output', (event: { terminalId: string; data: string }) => {
                if (event.terminalId !== id) return
                output += event.data
                if (output.includes('alive-ok')) {
                    clearTimeout(timer)
                    clearInterval(writer)
                    resolve()
                }
            })
            socket.on('connect', () => socket.emit('terminal:create', { sessionId, terminalId: id, cols: 80, rows: 24 }))
            socket.on('connect_error', reject)
        })
        return socket
    }

    beforeEach(async () => {
        await stopRunner()
        if (!jwt) jwt = (await api<{ token: string }>('/auth', { accessToken: configuration.cliApiToken })).token
    })
    afterEach(async () => {
        await cleanupAllRegisteredProcesses()
        await Promise.race([stopRunner(), new Promise(r => setTimeout(r, 10_000))])
        expect(await reapTestOwnedProcesses(testOwnedMarker()), 'test-owned processes survived').toEqual([])
    })

    it('runs a remote turn and a terminal on the Rust wrapper, then stops its whole tree', async () => {
        expect(existsSync(realBinary), 'Rust wrapper binary ' + realBinary).toBe(true)
        const directory = mkdtempSync(join(configuration.happyHomeDir, 'rust-real-'))
        const marker = join(directory, 'descendants.json')
        await startRunner(realBinary, { TIER_B_DESCENDANTS: marker })
        const row = await launch(directory, 'rust')
        const pid = row.metadata.hostPid!
        const running = statSync('/proc/' + pid + '/exe')
        const expected = statSync(realBinary)
        expect([running.dev, running.ino]).toEqual([expected.dev, expected.ino])
        await turn(row.id, 1)
        const [inside, outside] = JSON.parse(readFileSync(marker, 'utf8')) as number[]
        const terminal = await checkTerminal(row.id)
        try {
            const before = owned(pid)
            expect(before.some(p => p.pid === inside)).toBe(true)
            expect(before.some(p => p.pid === outside)).toBe(true)
            expect(await stopRunnerSession(row.id)).toBe('stopped')
            await until('wrapper tree exit', async () => owned(pid).length === 0
                && !before.some(old => processes().some(p => p.pid === old.pid && p.birth === old.birth)) ? true : undefined, 15_000)
        } finally {
            terminal.disconnect()
        }
    })

    it('falls back to TypeScript after exit 86 with one hub row', async () => {
        expect(existsSync(realBinary), 'Rust wrapper binary ' + realBinary).toBe(true)
        const directory = mkdtempSync(join(configuration.happyHomeDir, 'rust-fallback-'))
        const bin = join(directory, 'hapi-session-wrapper')
        // Fail Rust hook setup after REST adoption; the TS retry keeps the original HAPI_HOME.
        symlinkSync(join(configuration.happyHomeDir, 'settings.json'), join(directory, 'settings.json'))
        writeFileSync(join(directory, 'tmp'), 'not a directory')
        const quote = (arg: string) => "'" + arg.replaceAll("'", "'\"'\"'") + "'"
        writeFileSync(bin, '#!/bin/sh\nexport HAPI_HOME=' + quote(directory) + '\nexec ' + quote(realBinary) + ' "$@"\n', { mode: 0o755 })
        const log = await startRunner(bin)
        const row = await launch(directory, 'ts').catch(error => {
            throw new Error(String(error) + '\n' + readFileSync(log, 'utf8').slice(-6000))
        })
        await turn(row.id, 1)
        expect((await sessions()).filter(s => s.metadata?.path === directory).map(s => s.id)).toEqual([row.id])
        const text = readFileSync(log, 'utf8')
        expect(text).toContain('Retrying with TS')
        expect(text).toContain('--existing-session-id ' + row.id)
        expect(await stopRunnerSession(row.id)).toBe('stopped')
    })
})

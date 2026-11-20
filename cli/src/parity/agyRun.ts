import { spawn, execFileSync, type ChildProcess } from 'node:child_process'
import { watch, symlinkSync, readdirSync, mkdirSync, mkdtempSync, existsSync, writeFileSync, readFileSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { createInterface } from 'node:readline'
import { EventEmitter } from 'node:events'
import { createHash } from 'node:crypto'
import { reached, type Fence } from './fences'
import { readLaunches } from './launchLog'
import assert from 'node:assert/strict'
import { fakeHub, type ScriptStep } from './fakeHub'
import { normalize } from './normalize'
import pkg from '../../package.json'

const parity = dirname(fileURLToPath(import.meta.url))
const cli = resolve(parity, '../..')
export const repo = resolve(cli, '..')
export const testTimeout = 150_000
export const agyCorpus = join(parity, 'corpus/agy')
export const agyNames = existsSync(agyCorpus)
    ? readdirSync(agyCorpus, { withFileTypes: true }).filter(entry => entry.isDirectory()).map(entry => entry.name).sort()
    : []
// agy round summaries carry wall-clock spans; their presence is compared, not their size.
function scrubDurations(value: unknown): void {
    if (Array.isArray(value)) { value.forEach(scrubDurations); return }
    if (!value || typeof value !== 'object') return
    for (const [key, entry] of Object.entries(value)) {
        if ((key === 'duration_ms' || key === 'generation_ms') && typeof entry === 'number') (value as Record<string, unknown>)[key] = '<ms>'
        else scrubDurations(entry)
    }
}
// Request ids (`<agent call>#<uuid>`) are object keys in agent state; the
// normalizer only rewrites values, so keys get their own ordinal tokens.
const uuidPattern = /[a-f\d]{8}(?:-[a-f\d]{4}){3}-[a-f\d]{12}/gi
function stableKeys<T>(value: T, ids = new Map<string, string>()): T {
    if (Array.isArray(value)) return value.map(entry => stableKeys(entry, ids)) as T
    if (!value || typeof value !== 'object') return value
    return Object.fromEntries(Object.entries(value).map(([key, entry]) => [key.replace(uuidPattern, id => {
        const lower = id.toLowerCase()
        if (!ids.has(lower)) ids.set(lower, '<key:' + (ids.size + 1) + '>')
        return ids.get(lower)!
    }), stableKeys(entry, ids)])) as T
}
function waitForClose(child: ChildProcess) {
    return new Promise<void>(resolve => child.once('close', () => resolve()))
}

export async function runAgy(name: string, binary?: string, options: { rust?: boolean } = {}) {
    const startedAt = Date.now()
    const directory = join(agyCorpus, name)
    const scenario: { argv: string[]; env: Record<string,string>; agentScript: string; timeoutMs: number
        hubScript: (ScriptStep & { until?: Fence | Fence[]; release?: { after: Fence; marker: string }[]
            settledAlive?: Record<string, unknown>; expectAck?: unknown })[] } =
        JSON.parse(readFileSync(join(directory, 'scenario.json'), 'utf8'))
    const out = mkdtempSync(join(tmpdir(), 'hapi-parity-'))
    const home = join(out, 'hapi')
    mkdirSync(home)
    const cwd = join(out, 'project')
    mkdirSync(cwd)
    const changes = new EventEmitter()
    const changed = () => { changes.emit('change') }
    const hub = await fakeHub(out, changed)
    const watcher = watch(out, (_, file) => { if (file === 'agent-error') changed() })
    const apiUrl = 'http://127.0.0.1:' + hub.port
    const machineId = 'parity-machine'
    const token = 'parity-token'
    const bun = process.env.HAPI_BUN_EXEC || 'bun'
    const env = {
        ...scenario.env, PATH: process.env.PATH, HOME: out, TMPDIR: out, HAPI_API_URL: apiUrl, HAPI_HOME: home,
        HAPI_RUNNER_HTTP_TIMEOUT: String(scenario.timeoutMs),
        CLI_API_TOKEN: token, HAPI_HOSTNAME: 'parity', HAPI_CLAUDE_PATH: join(parity, 'fakes/fake-claude.ts'),
        PARITY_AGENT_SCRIPT: join(directory, scenario.agentScript), PARITY_OUT: out, TZ: 'UTC', LC_ALL: 'C',
        HAPI_CLI_EXECUTABLE: join(cli, 'bin/hapi.cjs'),
        ...(options.rust ? { HAPI_WRAPPER_CLI_VERSION: pkg.version, HAPI_WRAPPER_RUNTIME_PATH: cli } : {})
    }
    const bin = join(out, 'bin')
    mkdirSync(bin)
    symlinkSync(join(parity, 'fakes/agy.ts'), join(bin, 'agy'))
    env.PATH = bin + ':' + process.env.PATH
    writeFileSync(join(home, 'settings.json'), JSON.stringify({ machineId }))
    const runner = spawn(bun, [join(parity, 'fakes/runner.ts'), 'runner', 'start-sync'], { env, cwd, stdio: ['pipe', 'pipe', 'inherit'] })
    const runnerClosed = waitForClose(runner)
    let port = 0
    createInterface({ input: runner.stdout }).on('line', line => {
        const data = JSON.parse(line)
        if (data.port) { port = data.port; changed() }
        else hub.record('POST /session-started', [data.webhook], false, 0)
    })
    let child: ReturnType<typeof spawn> | undefined
    let childClosed: Promise<void> | undefined
    function killChildren() {
        if (child?.pid) { try { process.kill(-child.pid, 'SIGKILL') } catch {} }
        runner.kill('SIGKILL')
    }
    function removeOutput() {
        if (!process.env.PARITY_KEEP_OUTPUT) rmSync(out, { recursive: true, force: true })
    }
    function cleanupOnExit() {
        killChildren()
        removeOutput()
    }
    process.once('exit', cleanupOnExit)
    let output = ''
    let runnerFailure: string | undefined
    runner.on('error', error => { runnerFailure = error.message; changed() })
    runner.on('exit', (code, signal) => { runnerFailure = 'runner exited ' + (signal ?? code); changed() })
    let exited = false
    const deadline = Date.now() + scenario.timeoutMs
    function wait(label: string, predicate: () => boolean) {
        return new Promise<void>((resolve, reject) => {
            const timer = setTimeout(() => {
                changes.off('change', check)
                reject(Error('Timed out waiting for ' + label + ' in ' + out + '\n' + output))
            }, Math.max(0, deadline - Date.now()))
            changes.on('change', check)
            check()
            function check() {
                try {
                    if (existsSync(join(out, 'agent-error'))) throw Error(readFileSync(join(out, 'agent-error'), 'utf8'))
                    if (runnerFailure) throw Error(runnerFailure)
                    if (!predicate()) {
                        if (!exited) return
                        throw Error('Wrapper exited ' + (child?.signalCode ?? child?.exitCode) + ' waiting for ' + label + ' in ' + out + '\n' + output)
                    }
                    resolve()
                } catch (error) {
                    reject(error)
                }
                clearTimeout(timer)
                changes.off('change', check)
            }
        })
    }
    try {
        await wait('runner', () => port > 0)
        writeFileSync(join(home, 'runner.state.json'), JSON.stringify({
            pid: runner.pid, httpPort: port, startedWithCliVersion: pkg.version, startedWithApiUrl: apiUrl,
            startedWithMachineId: machineId, startedWithCliApiTokenHash: createHash('sha256').update(token).digest('hex')
        }))
        child = spawn(binary ?? bun, [...(binary ? [] : [join(cli, 'src/bootstrap.ts')]), ...scenario.argv],
            { cwd, env, detached: true, stdio: ['ignore', 'pipe', 'pipe'] })
        childClosed = waitForClose(child)
        child.stdout!.on('data', part => output += part)
        child.stderr!.on('data', part => output += part)
        child.on('error', error => { output += error.message; exited = true; changed() })
        child.on('exit', () => { exited = true; changed() })
        if (!child.pid) throw Error('Could not spawn ' + (binary ?? bun))
        const pid = child.pid
        const marker = execFileSync('ps', ['-p', String(pid), '-o', 'lstart='], { env, encoding: 'utf8' }).trim()
        await wait('socket heartbeat and runner webhook', () => hub.rows.some(r => r.event === 'session-alive') &&
            hub.rows.some(r => r.event === 'POST /session-started'))
        runner.stdin.end('connected\n')
        await wait('startup registration and native session acknowledgement', () => reached(hub.rows, [
            { event: 'rpc-register', contains: ':steer-queued-message' },
            // A resume writes the native id early and again once agy resumed.
            { event: 'update-metadata', contains: '"agySessionId"', result: 'success', count: scenario.argv.includes('--resume') ? 3 : 1 },
            { event: 'update-state', contains: '"steeringActive":false', result: 'success' },
            { event: 'session-ready' }
        ]))
        for (const action of scenario.hubScript) {
            if (action.settledAlive) {
                const expected = Object.entries(action.settledAlive)
                await wait('settled heartbeat ' + JSON.stringify(action.settledAlive),
                    () => expected.every(([key, value]) => hub.latestAlive?.[key] === value))
            }
            const start = hub.rows.length
            await hub.execute(action, () => child!.kill('SIGTERM'), { pid, marker })
            for (const gate of action.release ?? []) {
                await wait(JSON.stringify(gate.after), () => reached(hub.rows.slice(start), gate.after))
                writeFileSync(join(out, gate.marker), '')
            }
            const fences = action.until
            if (fences) await wait(JSON.stringify(fences), () => reached(hub.rows.slice(start), fences))
            if (action.expectAck !== undefined) {
                const row = hub.rows.slice(start).find(r => ['rpc-ack','update-ack'].includes(r.event))
                assert.ok(row, 'Missing acknowledgement')
                const response = row.event === 'rpc-ack' ? JSON.parse(String(row.args[0])) : row.args[0]
                for (const [key, value] of Object.entries(action.expectAck as object)) {
                    assert.deepEqual(response[key], value)
                }
            }
        }
        await wait('clean exit', () => exited)
        assert.equal(child.exitCode, 0, output)
        scrubDurations(hub.rows)
        for (const row of hub.rows) {
            const ready = row.event === 'session-ready' ? row.args[0] as { time?: unknown } | undefined : undefined
            if (typeof ready?.time === 'number') ready.time = '<ts>'
        }
        const decoded = JSON.parse(execFileSync(bun, [join(parity, 'decode.ts')], {
            input: JSON.stringify(hub.rows), encoding: 'utf8', cwd
        }))
        const logs = readLaunches(out)
        const compiledRuntime = join(home, 'runtime', pkg.version)
        const runtimeDir = binary && (hub.session.metadata as { happyLibDir?: string } | null)?.happyLibDir === compiledRuntime
            ? compiledRuntime : cli
        return stableKeys(normalize(hub.rows, { pid, marker, cli, runtimeDir, out, apiUrl, startedAt, mcpPort: 0, endedAt: Date.now(), leaseExpiry: hub.leaseExpiry }, logs, decoded))
    } finally {
        watcher.close()
        try {
            killChildren()
            await Promise.all([childClosed, runnerClosed])
            await hub.close()
        } finally {
            process.off('exit', cleanupOnExit)
            removeOutput()
        }
    }
}

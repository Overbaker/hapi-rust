import { spawn, execFileSync } from 'node:child_process'
import { accessSync, constants, readdirSync, mkdirSync, mkdtempSync, existsSync, writeFileSync, readFileSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { createInterface } from 'node:readline'
import { connect } from 'node:net'
import { once } from 'node:events'
import { createHash } from 'node:crypto'
import { setTimeout as sleep } from 'node:timers/promises'
import assert from 'node:assert/strict'
import { fakeHub, type ScriptStep } from './fakeHub'
import { normalize } from './normalize'
import pkg from '../../package.json'

const parity = dirname(fileURLToPath(import.meta.url))
const cli = resolve(parity, '../..')
export const corpus = join(parity, 'corpus/claude')
export const names = readdirSync(corpus, { withFileTypes: true }).filter(entry => entry.isDirectory()).map(entry => entry.name).sort()
export async function run(name: string, binary?: string) {
    const startedAt = Date.now()
    const executable = binary ? resolve(cli, '..', binary) : undefined
    if (executable) accessSync(executable, constants.X_OK)
    const directory = join(corpus, name)
    const scenario: { argv: string[]; env: Record<string,string>; agentScript: string; timeoutMs: number
        hubScript: (ScriptStep & { until?: { event: string; contains?: string; count?: number }; expectAck?: unknown })[] } =
        JSON.parse(readFileSync(join(directory, 'scenario.json'), 'utf8'))
    const out = mkdtempSync(join(tmpdir(), 'hapi-parity-'))
    const home = join(out, 'hapi')
    mkdirSync(home)
    const cwd = join(out, 'project')
    mkdirSync(cwd)
    const hub = await fakeHub(out)
    const apiUrl = 'http://127.0.0.1:' + hub.port
    const machineId = 'parity-machine'
    const token = 'parity-token'
    const bun = process.env.HAPI_BUN_EXEC || 'bun'
    const env = {
        ...scenario.env, PATH: process.env.PATH, HOME: out, TMPDIR: out, HAPI_API_URL: apiUrl, HAPI_HOME: home,
        CLI_API_TOKEN: token, HAPI_HOSTNAME: 'parity', HAPI_CLAUDE_PATH: join(parity, 'fakes/fake-claude.ts'),
        PARITY_AGENT_SCRIPT: join(directory, scenario.agentScript), PARITY_OUT: out, TZ: 'UTC', LC_ALL: 'C',
        HAPI_WRAPPER_CLI_VERSION: pkg.version, HAPI_WRAPPER_RUNTIME_PATH: cli, HAPI_CLI_EXECUTABLE: join(cli, 'bin/hapi.cjs')
    }
    writeFileSync(join(home, 'settings.json'), JSON.stringify({ machineId }))
    const runner = spawn(bun, [join(parity, 'fakes/runner.ts'), 'runner', 'start-sync'], { env, cwd, stdio: ['pipe', 'pipe', 'inherit'] })
    let port = 0
    createInterface({ input: runner.stdout }).on('line', line => {
        const data = JSON.parse(line)
        if (data.port) port = data.port
        else hub.record('POST /session-started', [data.webhook], false, 0)
    })
    let child: ReturnType<typeof spawn> | undefined
    let output = ''
    let exited = false
    const deadline = Date.now() + scenario.timeoutMs
    async function wait(label: string, predicate: () => boolean) {
        while (true) {
            if (existsSync(join(out, 'agent-error'))) throw Error(readFileSync(join(out, 'agent-error'), 'utf8'))
            if (predicate()) return
            if (exited || Date.now() > deadline) throw Error('Waiting for ' + label + ' in ' + out + '\n' + output)
            await sleep(10)
        }
    }
    try {
        await wait('runner', () => port > 0)
        writeFileSync(join(home, 'runner.state.json'), JSON.stringify({
            pid: runner.pid, httpPort: port, startedWithCliVersion: pkg.version, startedWithApiUrl: apiUrl,
            startedWithMachineId: machineId, startedWithCliApiTokenHash: createHash('sha256').update(token).digest('hex')
        }))
        child = spawn(executable ?? bun, [...(binary ? [] : [join(cli, 'src/index.ts')]), ...scenario.argv],
            { cwd, env, detached: true, stdio: ['ignore', 'pipe', 'pipe'] })
        child.stdout!.on('data', part => output += part)
        child.stderr!.on('data', part => output += part)
        child.on('error', error => { output += error.message; exited = true })
        child.on('exit', () => exited = true)
        if (!child.pid) throw Error('Could not spawn ' + (executable ?? bun))
        const pid = child.pid
        const marker = execFileSync('ps', ['-p', String(pid), '-o', 'lstart='], { env, encoding: 'utf8' }).trim()
        await wait('socket heartbeat and runner webhook', () => hub.rows.some(r => r.event === 'session-alive') &&
            hub.rows.some(r => r.event === 'POST /session-started'))
        runner.stdin.end('connected\n')
        await wait('startup', () => hub.rows.some(r => r.event === 'rpc-register' && JSON.stringify(r.args).includes(':steer-queued-message'))
            && hub.rows.some(r => r.event === 'update-metadata' && JSON.stringify(r.args).includes('"startingMode":"remote"')))
        writeFileSync(join(out, 'catalog-ready'), '')
        await wait('catalog', () => hub.rows.some(r => r.event === 'update-metadata' && JSON.stringify(r.args).includes('"slashCommands"')))
        const endpoint = new URL((hub.session.metadata as unknown as { hapiMcpUrl: string }).hapiMcpUrl)
        assert.equal(endpoint.hostname, '127.0.0.1')
        const mcpPort = Number(endpoint.port)
        const probe = connect(mcpPort, '127.0.0.1')
        try { await once(probe, 'connect') } finally { probe.destroy() }
        for (const action of scenario.hubScript) {
            if (action === scenario.hubScript.at(-1) || 'dropConnection' in action) {
                const previous = hub.rows.length
                await wait('pre-shutdown or disconnect heartbeat', () => hub.rows.slice(previous).some(r => r.event === 'session-alive'))
            }
            const start = hub.rows.length
            await hub.execute(action, () => child!.kill('SIGTERM'), { pid, marker })
            const fence = action.until
            if (fence) await wait(JSON.stringify(fence), () => hub.rows.slice(start).filter(r => r.event === fence.event &&
                (!fence.contains || JSON.stringify(r.args).includes(fence.contains))).length >= (fence.count ?? 1))
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
        const decoded = JSON.parse(execFileSync(bun, [join(parity, 'decode.ts')], {
            input: JSON.stringify(hub.rows), encoding: 'utf8', cwd
        }))
        const logs = ['probe.log.json', 'agent.log.json'].flatMap(file =>
            existsSync(join(out, file)) ? JSON.parse(readFileSync(join(out, file), 'utf8')) : [])
        return normalize(hub.rows, { pid, marker, cli, out, apiUrl, startedAt, mcpPort, endedAt: Date.now(), leaseExpiry: hub.leaseExpiry }, logs, decoded)
    } finally {
        if (child?.pid) { try { process.kill(-child.pid, 'SIGKILL') } catch {} }
        runner.kill()
        await hub.close()
        if (!process.env.PARITY_KEEP_OUTPUT) rmSync(out, { recursive: true, force: true })
    }
}

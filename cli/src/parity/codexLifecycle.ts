import assert from 'node:assert/strict'
import { spawn, type ChildProcess } from 'node:child_process'
import {
    existsSync,
    readFileSync,
    writeFileSync,
    symlinkSync,
    mkdirSync,
    readdirSync,
    statSync,
    renameSync,
    rmSync
} from 'node:fs'
import { join, resolve } from 'node:path'
import { randomUUID } from 'node:crypto'
import { startTestHub, teardown, TEST_CONFIG_FILE } from '../test/globalSetup'

const [binary, scenario] = process.argv.slice(2)
const cli = resolve(import.meta.dir, '../..')
type Session = {
    id: string
    active: boolean
    thinking: boolean
    metadata: {
        path: string
        hostPid: number
        wrapperRuntime: string
        codexSessionId: string
        lifecycleState: string
        hostStartMarker: string
    }
}
type Message = {
    id: string
    localId: string | null
    content: {
        role: string
        content: { type: string; text?: string; data?: { type?: string; message?: string } }
    }
}
const read = <T = any>(path: string): T => JSON.parse(readFileSync(path, 'utf8'))
const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms))
async function until<T>(name: string, fn: () => Promise<T | undefined>, limit = 25_000): Promise<T> {
    const end = Date.now() + limit
    do {
        const result = await fn()
        if (result !== undefined) return result
        await sleep(70)
    } while (Date.now() < end)
    throw Error('Timed out waiting for ' + name)
}
const alive = (pid: number) => {
    try {
        process.kill(pid, 0)
        return true
    } catch {
        return false
    }
}
let runner: ChildProcess | undefined,
    runnerOutput = ''
try {
    await startTestHub(join(import.meta.dir, 'fakes/runner-stop.mjs'))
    const cfg = read(TEST_CONFIG_FILE)
    const { tmpHome: home, token, bunExec, apiUrl } = cfg
    const native = join(home, 'native')
    const bin = join(home, 'bin')
    symlinkSync(join(import.meta.dir, 'fakes/codex.ts'), join(bin, 'codex'))
    const script = join(home, 'agent.json')
    writeFileSync(script, JSON.stringify({ mode: scenario }))
    const machineId = randomUUID()
    const settings = join(home, 'settings.json')
    writeFileSync(settings, JSON.stringify({ ...(existsSync(settings) ? read(settings) : {}), machineId }))
    const jwt = (
        (await (
            await fetch(apiUrl + '/api/auth', {
                method: 'POST',
                headers: { 'content-type': 'application/json' },
                body: JSON.stringify({ accessToken: token })
            })
        ).json()) as { token: string }
    ).token
    assert(jwt)
    async function api<T>(path: string, body?: unknown): Promise<T> {
        const r = await fetch(apiUrl + '/api' + path, {
            method: body ? 'POST' : 'GET',
            headers: { authorization: 'Bearer ' + jwt, 'content-type': 'application/json' },
            body: body === undefined ? undefined : JSON.stringify(body),
            signal: AbortSignal.timeout(60_000)
        })
        if (!r.ok) throw Error(path + ': ' + r.status + ' ' + (await r.text()))
        return (await r.json()) as T
    }
    const stateFile = join(home, 'runner.state.json')
    let port = 0
    async function runnerPost(path: string, body = {}) {
        const r = await fetch('http://127.0.0.1:' + port + path, {
            method: 'POST',
            headers: { 'content-type': 'application/json' },
            body: JSON.stringify(body),
            signal: AbortSignal.timeout(30_000)
        })
        assert.equal(r.status, 200)
        return (await r.json()) as any
    }
    async function start(mode: 'ts' | 'rust') {
        runner = spawn(bunExec, [join(cli, 'src/index.ts'), 'runner', 'start-sync'], {
            cwd: cli,
            env: {
                PATH: bin + ':' + process.env.PATH,
                HOME: home,
                TMPDIR: process.env.TMPDIR,
                CLI_API_TOKEN: token,
                HAPI_HOME: home,
                HAPI_API_URL: apiUrl,
                HAPI_RUST_WRAPPER_BIN: binary,
                HAPI_RUST_WRAPPER_FLAVORS: mode === 'rust' ? 'codex' : 'none',
                CODEX_HOME: native,
                PARITY_OUT: home,
                PARITY_AGENT_SCRIPT: script,
                HAPI_TEST_MARKER: home,
                HAPI_DISABLE_PEER_TOOLS: '1',
                HAPI_RUNNER_HEARTBEAT_INTERVAL: '1000',
                HAPI_RUNNER_HTTP_TIMEOUT: '30000'
            },
            stdio: ['ignore', 'pipe', 'pipe']
        })
        const child = runner
        for (const stream of [child.stdout!, child.stderr!])
            stream.on('data', (d) => {
                runnerOutput = (runnerOutput + String(d)).slice(-4000)
            })
        await until('runner registration', async () => {
            if (child.exitCode !== null) throw Error('runner exit: ' + child.exitCode + ' ' + runnerOutput)
            const st = existsSync(stateFile) ? read(stateFile) : null
            if (st?.pid !== child.pid) return
            const r = await fetch(apiUrl + '/cli/machines/' + machineId, {
                headers: { authorization: 'Bearer ' + token }
            })
            if (!r.ok) return
            const { machine } = (await r.json()) as any
            if (
                machine?.active &&
                machine.runnerState?.pid === child.pid &&
                machine.runnerState.status === 'running'
            ) {
                port = st.httpPort
                return true
            }
            return undefined
        })
    }
    async function stop() {
        if (runner?.pid && alive(runner.pid)) {
            const child = runner
            await runnerPost('/stop')
            await until('runner exit', async () =>
                child.exitCode !== null || child.signalCode ? true : undefined
            )
        }
        runner = undefined
    }
    const get = async (id: string) => (await api<{ session: Session }>('/sessions/' + id)).session
    const sessions = async () =>
        Promise.all((await api<{ sessions: { id: string }[] }>('/sessions')).sessions.map((s) => get(s.id)))
    const rows = async (id: string) =>
        (await api<{ messages: Message[] }>('/sessions/' + id + '/messages?limit=200')).messages
    async function send(id: string, text: string) {
        await api('/sessions/' + id + '/messages', { text, localId: text })
    }
    async function turn(id: string, n: number) {
        const text = 'round-' + n
        await send(id, text)
        return until(text, async () => {
            const history = await rows(id)
            const answer = history.filter(
                (m) =>
                    m.content.content.data?.type === 'message' &&
                    m.content.content.data.message === 'ack ' + text
            )
            if (!answer.length) return
            assert.equal(answer.length, 1)
            assert.equal(
                history.filter((m) => m.content.role === 'user' && m.content.content.text === text).length,
                1
            )
            if (!history.some((m) => m.localId?.endsWith(':duration:turn-' + n))) return
            return history
        })
    }
    function registry(id: string) {
        const dir = join(home, 'codex-runtimes')
        const record = readdirSync(dir)
            .filter((f) => f.endsWith('.json'))
            .map((f) => read(join(dir, f)))
            .reverse()
            .find((r) => r.sessions[id]?.active)
        assert(record, 'no active registry binding for ' + id)
        assert.deepEqual(record, read(join(native, 'hapi-runtime-owners', record.id + '.json')))
        return record
    }
    async function launch(mode: 'ts' | 'rust') {
        const cwd = join(home, 'project-' + mode)
        mkdirSync(cwd)
        const result = await api<any>('/machines/' + machineId + '/spawn', {
            directory: cwd,
            agent: 'codex',
            startingMode: 'remote',
            sessionType: 'simple'
        })
        assert.equal(result.type, 'success', JSON.stringify(result))
        return until(mode + ' wrapper', async () => {
            const s = await get(result.sessionId)
            if (!s.active || !s.metadata.codexSessionId) return
            verifyRuntime(s, mode)
            return s
        })
    }
    function verifyRuntime(s: Session, mode: 'ts' | 'rust') {
        assert.equal(s.metadata.wrapperRuntime, mode)
        const actual = statSync('/proc/' + s.metadata.hostPid + '/exe')
        const expected = statSync(mode === 'rust' ? binary : bunExec)
        assert.deepEqual([actual.dev, actual.ino], [expected.dev, expected.ino])
    }
    if (scenario === 'mixed-stop') {
        for (const mode of ['ts', 'rust'] as const) {
            await start(mode)
            const first = await launch(mode)
            await send(first.id, '/new')
            const sibling = await until('second live root', async () =>
                (await sessions()).find(
                    (s) =>
                        s.id !== first.id &&
                        s.active &&
                        s.metadata?.hostPid === first.metadata.hostPid &&
                        s.metadata.codexSessionId
                )
            )
            for (const gate of ['sibling-working', 'release-sibling'])
                rmSync(join(home, gate), { force: true })
            await send(sibling.id, 'in-flight')
            await until('sibling in a native turn', async () =>
                existsSync(join(home, 'sibling-working')) && (await get(sibling.id)).thinking
                    ? true
                    : undefined
            )
            const before = registry(first.id)
            assert.equal(Object.values(before.sessions).filter((v: any) => v.active).length, 2)
            const raw = join(home, 'test-raw-stop')
            rmSync(raw + '.result', { force: true })
            writeFileSync(raw + '.tmp', first.id)
            renameSync(raw + '.tmp', raw)
            await until('generation-verified raw PID decision', async () =>
                existsSync(raw + '.result') ? true : undefined
            )
            assert.deepEqual(read(raw + '.result'), { pid: first.metadata.hostPid, status: 'stopped' })
            assert(alive(before.pid) && alive(before.serverPid))
            assert.deepEqual(registry(first.id), before)
            assert.equal((await runnerPost('/stop-session', { sessionId: first.id })).status, 'stopped')
            await until('first root archived', async () => (!(await get(first.id)).active ? true : undefined))
            const after = registry(sibling.id)
            assert.equal(after.pid, before.pid)
            assert.equal(after.marker, before.marker)
            assert.equal(after.sessions[first.id].active, false)
            assert.deepEqual(after.sessions[sibling.id], before.sessions[sibling.id])
            assert(alive(after.serverPid))
            assert.equal((await get(sibling.id)).thinking, true)
            assert.match(
                readFileSync(read(stateFile).runnerLogPath, 'utf8'),
                new RegExp('PID ' + before.pid + ' still hosts active shared roots; not tree-killing')
            )
            writeFileSync(join(home, 'release-sibling'), '')
            await until('in-flight sibling turn finishes', async () =>
                (await rows(sibling.id)).some((m) => m.content.content.data?.message === 'ack in-flight')
                    ? true
                    : undefined
            )
            await turn(sibling.id, 2)
            assert.equal((await runnerPost('/stop-session', { sessionId: sibling.id })).status, 'stopped')
            await until('last root exits', async () =>
                !alive(before.pid) && !alive(before.serverPid) ? true : undefined
            )
            await stop()
        }
    } else if (scenario === 'hibernation') {
        await start('ts')
        const initial = await launch('ts')
        let prior = await turn(initial.id, 1),
            n = 1
        for (const mode of ['rust', 'ts'] as const) {
            const record = registry(initial.id)
            assert.equal(Object.values(record.sessions).filter((b: any) => b.active).length, 1)
            writeFileSync(join(home, 'test-idle-session'), initial.id)
            await until(
                'hibernated',
                async () => {
                    const s = await get(initial.id)
                    if (
                        !s.active &&
                        s.metadata.lifecycleState === 'hibernated' &&
                        !alive(record.pid) &&
                        !alive(record.serverPid)
                    )
                        return true
                    return undefined
                },
                60000
            )
            await stop()
            await start(mode)
            const next = await turn(initial.id, ++n)
            const s = await get(initial.id)
            verifyRuntime(s, mode)
            assert.equal(s.metadata.codexSessionId, initial.metadata.codexSessionId)
            assert.equal(s.metadata.path, initial.metadata.path)
            assert.deepEqual(
                (await sessions()).filter((r) => r.metadata?.path === initial.metadata.path).map((r) => r.id),
                [initial.id]
            )
            assert.equal(s.id, initial.id)
            assert.deepEqual(next.slice(0, prior.length), prior)
            const expectedIds = Array.from({ length: n }, (_, i) => [
                'round-' + (i + 1),
                'codex:' +
                    initial.metadata.codexSessionId +
                    ':turn-' +
                    (i + 1) +
                    ':a-turn-' +
                    (i + 1) +
                    ':agent_message',
                'codex:' + initial.metadata.codexSessionId + ':duration:turn-' + (i + 1)
            ]).flat()
            assert.deepEqual(
                next.filter((m) => m.localId).map((m) => m.localId),
                expectedIds
            )
            assert.equal(next.filter((m) => m.content.content.data?.type === 'ready').length, n)
            assert.deepEqual(
                next
                    .filter((m) => m.content.content.data?.type === 'message')
                    .map((m) => m.content.content.data!.message),
                Array.from({ length: n }, (_, i) => 'ack round-' + (i + 1))
            )
            assert.deepEqual(
                next.filter((m) => m.content.role === 'user').map((m) => m.content.content.text),
                Array.from({ length: n }, (_, i) => 'round-' + (i + 1))
            )
            prior = next
        }
        const last = registry(initial.id)
        assert.equal((await runnerPost('/stop-session', { sessionId: initial.id })).status, 'stopped')
        await until('final wrapper exit', async () =>
            !alive(last.pid) && !alive(last.serverPid) ? true : undefined
        )
        await stop()
    } else throw Error('unknown lifecycle case')
} catch (e) {
    console.error(runnerOutput)
    if (existsSync(TEST_CONFIG_FILE)) {
        const cfg = read(TEST_CONFIG_FILE)
        const logs = join(cfg.tmpHome, 'logs')
        if (existsSync(logs)) {
            for (const name of readdirSync(logs).slice(-4))
                console.error(name, readFileSync(join(logs, name), 'utf8').slice(-1700))
        }
    }
    throw e
} finally {
    if (runner?.pid && alive(runner.pid)) {
        const child = runner
        child.kill()
        await until(
            'cleanup runner',
            async () => (child.exitCode !== null || child.signalCode ? true : undefined),
            5000
        ).catch(() => child.kill('SIGKILL'))
    }
    await teardown()
}

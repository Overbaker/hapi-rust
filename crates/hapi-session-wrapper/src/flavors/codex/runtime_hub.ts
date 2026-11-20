import assert from 'node:assert/strict'
import { createServer } from 'node:http'
import { createRequire } from 'node:module'
import {
    existsSync,
    renameSync,
    mkdtempSync,
    mkdirSync,
    readdirSync,
    readFileSync,
    rmSync,
    statSync,
    symlinkSync,
    writeFileSync
} from 'node:fs'
import { spawn, execFileSync } from 'node:child_process'
import { once } from 'node:events'
import { delimiter, dirname, join, resolve } from 'node:path'
import { tmpdir } from 'node:os'
import { createHash, randomUUID } from 'node:crypto'
const root = resolve(import.meta.dir, '../../../../..')
const { Server } = createRequire(join(root, 'hub/package.json'))('socket.io')
const WebSocket = createRequire(join(root, 'cli/package.json'))('ws')
const [bin, scenario] = process.argv.slice(2)
const dir = mkdtempSync(join(tmpdir(), 'rsw-shared-'))
const project = join(dir, 'project')
mkdirSync(project)
const native = join(dir, 'native')
if (scenario === 'shared') {
    mkdirSync(native)
    writeFileSync(join(native, 'config.toml'), 'model_catalog_json = ""')
}
const sid = randomUUID()
const rows = new Map()
const sockets = new Map()
const messages = []
const reports = []
const webhookTimes: number[] = []
const heartbeats = new Map<string, number>()
const row = (id, m = { path: project, host: 'test' }) => ({
    id,
    namespace: 'default',
    metadata: m,
    metadataVersion: 1,
    agentState: {},
    agentStateVersion: 1,
    thinking: false,
    thinkingAt: 0,
    seq: 0,
    active: false,
    activeAt: 0,
    createdAt: 1,
    updatedAt: 1
})
const cold = [
    'cold',
    'attached',
    'legacy-active',
    'missing-binding',
    'other-rejection',
    'owner-conflict',
    'prepare-error',
    'transport-loss'
].includes(scenario)
if (cold) rows.set(sid, row(sid, { path: project, host: 'test', codexSessionId: 'prior-thread' }))
if (scenario === 'attached') rows.get(sid).metadata.codexRuntime = { kind: 'attached' }
if (scenario === 'legacy-active') rows.get(sid).active = true
if (scenario === 'missing-binding') delete rows.get(sid).metadata.codexSessionId
if (scenario === 'owner-conflict') {
    const owner = join(native, 'hapi-runtime-owners')
    mkdirSync(owner, { recursive: true })
    const marker = execFileSync('ps', ['-p', String(process.pid), '-o', 'lstart='], {
        encoding: 'utf8',
        env: { ...process.env, LC_ALL: 'C', TZ: 'UTC' }
    }).trim()
    writeFileSync(
        join(owner, 'previous.json'),
        JSON.stringify({
            id: 'previous',
            pid: process.pid,
            marker,
            endpoint: 'unix:///previous',
            command: 'codex',
            args: [],
            codexHome: native,
            hub: 'other',
            authHash: 'other',
            sessions: { old: { threadId: 'prior-thread', namespace: 'other', active: true } }
        })
    )
}
let machines = 0
let serial = 0
let stderr = ''
const log = join(dir, 'engine.jsonl')
writeFileSync(log, '')
const records = () =>
    readFileSync(log, 'utf8')
        .trim()
        .split('\n')
        .filter(Boolean)
        .map((x) => JSON.parse(x))
const http = createServer(async (req, res) => {
    try {
        const path = req.url!.split('?')[0]
        const chunks = []
        for await (const c of req) chunks.push(c)
        const body = chunks.length ? JSON.parse(Buffer.concat(chunks).toString()) : null
        const send = (v) => {
            res.setHeader('content-type', 'application/json')
            res.end(JSON.stringify(v))
        }
        if (path === '/session-started') {
            webhookTimes.push(performance.now())
            reports.push(body)
            if (scenario === 'webhook-error') {
                res.writeHead(503).end()
                return
            }
            send({})
            return
        }
        assert.equal(req.headers.authorization, 'Bearer fixture')
        if (path === '/cli/machines') {
            machines++
            if (scenario === 'signal-bootstrap') return
            send({ machine: { ...row(body.id, body.metadata), runnerState: null, runnerStateVersion: 1 } })
            return
        }
        if (path.endsWith('/messages')) {
            send({ messages: [] })
            return
        }
        if (path === '/cli/sessions' && body) {
            const id = body.id ?? randomUUID()
            assert(!rows.has(id))
            const r = { ...row(id, body.metadata), agentState: body.agentState }
            rows.set(id, r)
            if (scenario.startsWith('rollback-') && rows.size === 2) {
                const path = join(dir, 'codex-runtimes')
                renameSync(path, path + '.saved')
                writeFileSync(path, 'disk failure')
            }
            send({ session: r })
            return
        }
        const id = path.split('/').at(-1)
        assert(rows.has(id))
        send({ session: rows.get(id) })
    } catch (e) {
        console.error(e)
        res.writeHead(500).end()
    }
})
const io = new Server(http, { transports: ['websocket'] })
io.of('/cli').on('connection', (socket) => {
    const id = socket.handshake.auth.sessionId
    assert(rows.has(id))
    assert.equal(socket.handshake.auth.token, 'fixture')
    sockets.set(id, socket)
    socket.on('session-alive', () => heartbeats.set(id, performance.now()))
    socket.on('ping', (ack) => ack())
    socket.on('messages-steer-state', (_, ack) => ack({ ok: true }))
    for (const [event, key] of [
        ['update-metadata', 'metadata'],
        ['update-state', 'agentState']
    ]) {
        socket.on(event, (p, ack) => {
            const r = rows.get(id),
                v = key + 'Version'
            const match = p.expectedVersion === r[v]
            if (match) {
                r[key] = p[key]
                r[v]++
            }
            ack({ result: match ? 'success' : 'version-mismatch', version: r[v], [key]: r[key] })
        })
    }
    socket.on('message', (p) => messages.push({ id, message: p.message }))
})
await new Promise((r) => http.listen(0, '127.0.0.1', r))
writeFileSync(join(dir, 'settings.json'), JSON.stringify({ machineId: 'machine' }))
writeFileSync(
    join(dir, 'runner.state.json'),
    JSON.stringify({
        pid: process.pid,
        httpPort: http.address().port,
        startedWithCliVersion: JSON.parse(readFileSync(join(root, 'cli/package.json'), 'utf8')).version,
        startedWithMachineId: 'machine',
        startedWithApiUrl: 'http://127.0.0.1:' + http.address().port,
        startedWithCliApiTokenHash: createHash('sha256').update('fixture').digest('hex'),
        sharedCodexRuntime: scenario !== 'old-runner'
    })
)
if (scenario === 'signal-marker') {
    writeFileSync(join(dir, 'ps'), '#!/bin/sh\nif [ "$2" = "$PPID" ]; then touch "$ENGINE_LOG.marker"; exec sleep 30; fi\nexec /usr/bin/ps "$@"\n', { mode: 0o755 })
}
if (process.platform === 'win32') {
    writeFileSync(join(dir, 'codex.cmd'), `@echo off\r\n"${process.execPath}" "${join(import.meta.dir, 'fake_engine.ts')}" %*\r\n`)
} else {
    symlinkSync(join(import.meta.dir, 'fake_engine.ts'), join(dir, 'codex'))
}
const start = performance.now()
const child = spawn(
    bin,
    [
        'codex',
        '--started-by',
        'runner',
        ...(['unreserved-rejection', 'fresh-prepare-error'].includes(scenario)
            ? []
            : [cold ? '--existing-session-id' : '--hapi-session-id', sid])
    ],
    {
        cwd: project,
        detached: scenario === 'measure',
        env: {
            ...process.env,
            ...(scenario === 'measure' ? { HOME: dir, HAPI_CLI_EXECUTABLE: bin } : {}),
            PATH: dir + delimiter + process.env.PATH,
            HAPI_HOME: dir,
            HAPI_API_URL: 'http://127.0.0.1:' + http.address().port,
            CLI_API_TOKEN: 'fixture',
            HAPI_WRAPPER_CLI_VERSION: '1.test',
            HAPI_WRAPPER_RUNTIME_PATH: dir,
            HAPI_INVOKED_CWD: project,
            CODEX_HOME: scenario === 'shared' ? '../native' : native,
            ENGINE_LOG: log,
            ENGINE_SCENARIO: scenario,
            COLD_THREAD: cold ? 'prior-thread' : '',
            HAPI_DISABLE_PEER_TOOLS: '0'
        },
        stdio: ['ignore', 'pipe', 'pipe']
    }
)
const exit = once(child, 'exit')
child.stderr.on('data', (d) => {
    stderr += String(d)
})
let stdout = ''
child.stdout.on('data', (d) => {
    stdout += String(d)
})
async function wait(label, pred) {
    const until = Date.now() + 12000
    while (Date.now() < until) {
        if (pred()) return
        if (child.exitCode !== null || child.signalCode) break
        await Bun.sleep(15)
    }
    throw Error(label + ' ' + stderr + ' ' + JSON.stringify(records().slice(-4)))
}
const send = (id, text) => {
    const localId = randomUUID()
    sockets.get(id).emit('update', {
        body: {
            t: 'new-message',
            message: {
                id: localId,
                localId,
                seq: ++serial,
                createdAt: serial,
                content: { role: 'user', content: { type: 'text', text }, meta: { sentFrom: 'web' } }
            }
        }
    })
}
const rpc = async (id, m, p) => {
    const v = await sockets
        .get(id)
        .timeout(5000)
        .emitWithAck('rpc-request', { method: id + ':' + m, params: JSON.stringify(p) })
    return v ? JSON.parse(v) : null
}
const registry = () =>
    JSON.parse(
        readFileSync(
            join(
                dir,
                'codex-runtimes',
                readdirSync(join(dir, 'codex-runtimes')).find((p) => p.endsWith('.json'))
            ),
            'utf8'
        )
    )
const ack = (id, text) =>
    messages.some((v) => v.id === id && v.message.content.data?.message === 'ack ' + text)
const boundedExit = async () => {
    let timer
    try {
        return await Promise.race([
            exit,
            new Promise((_, reject) => {
                timer = setTimeout(() => reject(Error('wrapper did not exit ' + stderr)), 12000)
            })
        ])
    } finally {
        clearTimeout(timer)
    }
}
try {
    const rejected = {
        'prepare-error': 'prepare refused',
        'fresh-prepare-error': 'prepare refused',
        'reserved-prepare-error': 'prepare refused',
        'init-exit': '37',
        'start-rejection': 'start refused',
        'unreserved-rejection': 'start refused',
        'start-loss': /Connection reset|Shared Codex connection closed/i,
        attached: 'Use hapi attach',
        'legacy-active': 'Existing session is active',
        'missing-binding': 'no Codex thread binding',
        'old-runner': 'predates shared Codex sessions',
        'owner-conflict': 'Thread belongs to another Codex runtime',
        'other-rejection': 'permission denied',
        'transport-loss': /Connection reset|Shared Codex connection closed/i
    }[scenario]
    if (rejected) {
        if (scenario === 'init-exit') {
            await wait(
                'initialize registry',
                () =>
                    existsSync(join(dir, 'codex-runtimes')) &&
                    readdirSync(join(dir, 'codex-runtimes')).some((n) => n.endsWith('.json'))
            )
            writeFileSync(log + '.release', '')
        }
        assert.deepEqual(await boundedExit(), [
            ['prepare-error', 'fresh-prepare-error', 'init-exit'].includes(scenario) ? 86 : 1,
            null
        ])
        assert(typeof rejected === 'string' ? stderr.includes(rejected) : rejected.test(stderr), stderr)
        assert.equal(reports.length, 0)
        if (['other-rejection', 'transport-loss'].includes(scenario)) {
            assert(!stderr.includes('HAPI_RUST_WRAPPER_SESSION_ID='), stderr)
            assert.equal(records().filter((r) => r.method === 'thread/resume').length, 1)
            assert(!records().some((r) => r.method === 'thread/unarchive'))
            assert.equal(registry().sessions[sid].active, false)
        } else if (['fresh-prepare-error', 'reserved-prepare-error'].includes(scenario)) {
            assert(!stderr.includes('HAPI_RUST_WRAPPER_SESSION_ID='))
            assert(!records().some((r) => r.method === 'thread/start'))
            assert.equal(rows.size, 1)
            assert.equal([...rows.values()][0].metadata.lifecycleState, 'archived')
        } else if (scenario === 'prepare-error') {
            assert(stderr.includes('HAPI_RUST_WRAPPER_SESSION_ID=' + sid))
            assert(!records().some((r) => r.method === 'thread/resume'))
        } else if (['start-rejection', 'unreserved-rejection', 'start-loss'].includes(scenario)) {
            assert(!stderr.includes('HAPI_RUST_WRAPPER_SESSION_ID='))
            assert.equal(rows.size, 1)
            assert.equal(records().filter((r) => r.method === 'thread/start').length, 1)
        } else {
            assert.equal(machines, 0)
            assert(!records().some((r) => r.method === 'thread/start' || r.method === 'thread/resume'))
        }
    } else if (scenario === 'signal-marker') {
        await wait('blocked process marker', () => existsSync(log + '.marker'))
        const start = performance.now()
        child.kill('SIGTERM')
        assert.deepEqual(await boundedExit(), [0, null])
        assert(performance.now() - start < 2000)
        assert(!records().length && !reports.length)
    } else if (scenario === 'signal-start' || scenario === 'signal-bootstrap') {
        await wait('blocked startup', () =>
            scenario === 'signal-bootstrap'
                ? machines === 1
                : records().some((r) => r.method === 'thread/start')
        )
        const start = performance.now()
        child.kill('SIGTERM')
        assert.deepEqual(await boundedExit(), [0, null])
        assert(performance.now() - start < 2000)
        assert.equal(reports.length, 0)
        assert.equal(
            records().filter((r) => r.method === 'thread/start').length,
            scenario === 'signal-bootstrap' ? 0 : 1
        )
    } else if (scenario === 'measure') {
        await wait('initial webhook', () => reports.length === 1)
        const result = { spawnMs: webhookTimes[0] - start, newRootMs: [] as number[], fiveMs: 0, idle: {} }
        const image = statSync(bin)
        const checkImage = (pid: number) => {
            const running = statSync('/proc/' + pid + '/exe')
            assert.equal(running.ino, image.ino)
            assert.equal(running.dev, image.dev)
        }
        checkImage(child.pid)
        const warm = Number(process.env.CODEX_BENCH_WARM)
        const roots = Number(process.env.CODEX_BENCH_ROOTS ?? 5)
        assert([1, 5].includes(roots))
        for (let n = 1; n <= roots; n++) {
            if (n > 1) {
                const requestAt = performance.now()
                send(sid, '/new')
                await wait('webhook ' + n, () => reports.length === n)
                result.newRootMs.push(webhookTimes[n - 1] - requestAt)
            }
            const id = reports[n - 1].sessionId
            assert.equal(reports[n - 1].metadata.hostPid, child.pid)
            const bridges = records().filter((v) => v.bridge)
            assert.equal(bridges.length, n)
            bridges.forEach((v) => checkImage(v.bridge))
            if (n === 5) result.fiveMs = webhookTimes[4] - start
            if (Number.isFinite(warm)) {
                send(id, 'benchmark')
                await wait('completed turn', () => ack(id, 'benchmark'))
                if (n === roots) {
                    console.error('Idle warmup', bin, n, 'roots', warm, 'seconds')
                    await Bun.sleep(warm * 1000)
                    const begin = performance.now()
                    const sample = Bun.spawn(
                        [
                            'python3',
                            join(root, 'benchmarks/wrapper-compare/codex_sample.py'),
                            String(child.pid),
                            String(registry().serverPid),
                            ...records()
                                .filter((v) => v.bridge)
                                .map((v) => String(v.bridge))
                        ],
                        { stderr: 'inherit', stdout: 'pipe' }
                    )
                    const out = await new Response(sample.stdout).text()
                    assert.equal(await sample.exited, 0)
                    assert(reports.every((r) => heartbeats.get(r.sessionId) > begin))
                    result.idle[n] = JSON.parse(out)
                }
            }
        }
        child.kill('SIGTERM')
        assert.deepEqual(await boundedExit(), [0, null])
        console.log(JSON.stringify(result))
    } else {
        await wait('initial webhook', () => reports.length === 1)
        assert.equal(reports[0].sessionId, sid)
        assert.equal(reports[0].metadata.wrapperRuntime, 'rust')
        assert.equal(reports[0].metadata.hostPid, child.pid)
        assert.ok(reports[0].metadata.hostStartMarker)
        assert.equal(reports[0].metadata.capabilities.concurrentClients, true)
        if (scenario === 'windows') {
            const record = registry()
            assert.match(record.endpoint, /^ws:\/\/127\.0\.0\.1:\d+$/)
            assert.equal(record.codexHome, native)
            assert.ok(record.token)
            await assert.rejects(once(new WebSocket(record.endpoint), 'open'), /401/)
            const gateway = new WebSocket(record.endpoint, { headers: { Authorization: 'Bearer ' + record.token } })
            await once(gateway, 'open')
            const initialized = once(gateway, 'message')
            gateway.send(JSON.stringify({ id: 1, method: 'initialize', params: { clientInfo: { name: 'test', version: '1' } } }))
            assert.ok(JSON.parse(String((await initialized)[0])).result)
            gateway.close()
            send(sid, 'hello')
            await wait('Windows turn', () => ack(sid, 'hello'))
            await rpc(sid, 'killSession', {}).catch(() => {})
            assert.deepEqual(await boundedExit(), [0, null])
        } else if (scenario === 'live-exit') {
            process.kill(registry().serverPid, 'SIGKILL')
            assert.deepEqual(await boundedExit(), [1, null])
            assert.match(stderr, /app-server exited.*9/)
        } else if (scenario === 'last-rejection' || scenario.startsWith('rollback-')) {
            const ws = new WebSocket('ws+unix://' + registry().endpoint.slice(7) + ':/')
            await once(ws, 'open')
            const reply = once(ws, 'message')
            ws.send(
                JSON.stringify({
                    method: scenario === 'rollback-cold' ? 'thread/resume' : 'thread/start',
                    id: 1,
                    params: scenario === 'rollback-cold' ? { threadId: 'external' } : {}
                })
            )
            if (scenario === 'last-rejection') {
                await wait('reserved', () => registry().pendingCreations?.length === 1)
                await rpc(sid, 'killSession', {})
                await wait('root retired', () =>
                    reports.some((r) => r.metadata.lifecycleState === 'archived')
                )
                writeFileSync(log + '.release', '')
                await boundedExit()
                assert.equal(child.exitCode, 0)
            } else {
                const [raw] = await reply
                assert.match(JSON.parse(String(raw)).error.message, /directory|File exists/i)
                const other = [...rows.keys()].find((id) => id !== sid)
                await wait('failed root disconnected', () => other && sockets.get(other)?.disconnected)
                assert.equal(rows.get(other).metadata.lifecycleState, 'archived')
                rmSync(join(dir, 'codex-runtimes'))
                renameSync(join(dir, 'codex-runtimes.saved'), join(dir, 'codex-runtimes'))
                send(sid, 'still running')
                await wait('sibling remains usable', () => ack(sid, 'still running'))
                child.kill('SIGTERM')
                assert.deepEqual(await boundedExit(), [0, null])
            }
            ws.close()
        } else if (cold) {
            assert.equal(rows.size, 1)
            assert.equal(registry().sessions[sid].threadId, 'prior-thread')
            assert.equal(records().filter((r) => r.method === 'thread/unarchive').length, 1)
            assert(!records().some((r) => r.method === 'thread/start'))
            child.kill('SIGHUP')
            assert.deepEqual(await boundedExit(), [0, null])
            assert.equal(registry().sessions[sid].active, false)
            assert.notEqual(rows.get(sid).metadata.lifecycleState, 'archived')
        } else if (scenario === 'gateway') {
            const ws = new WebSocket('ws+unix://' + registry().endpoint.slice(7) + ':/')
            await once(ws, 'open')
            const nativeRpc = async (method, params) => {
                const id = ++serial
                const response = new Promise((resolve, reject) => {
                    const listener = (raw) => {
                        const v = JSON.parse(String(raw))
                        if (v.id === id) {
                            ws.off('message', listener)
                            v.error ? reject(Error(v.error.message)) : resolve(v.result)
                        }
                    }
                    ws.on('message', listener)
                })
                ws.send(JSON.stringify({ method, id, params }))
                return response
            }
            await nativeRpc('initialize', {})
            assert.deepEqual(await nativeRpc('hapi/attach', { sessionId: sid }), { threadId: 'thread-1' })
            await assert.rejects(nativeRpc('thread/rollback', { threadId: 'thread-1' }), /In-place rewind/)
            await assert.rejects(nativeRpc('thread/resume', { path: '/bad' }), /native thread ID/)
            const response = await nativeRpc('thread/fork', { threadId: 'thread-1', cwd: project })
            assert.equal(rows.size, 2)
            assert.equal(reports.length, 2)
            const forked = reports[1].sessionId
            assert.equal(rows.get(forked).metadata.forkedFrom, sid)
            assert.equal(registry().sessions[forked].threadId, response.thread.id)
            assert.equal(records().find((r) => r.method === 'thread/fork').p.deferGoalContinuation, true)
            assert.deepEqual(registry().pendingCreations, [])
            ws.close()
            child.kill('SIGINT')
            assert.deepEqual(await boundedExit(), [0, null])
            assert(Object.values(registry().sessions).every((b) => b.active === false))
            assert(!records().some((v) => v.method === 'thread/archive'))
        } else if (scenario === 'terminate' || scenario === 'webhook-error') {
            if (scenario === 'webhook-error') {
                send(sid, 'webhook unavailable')
                await wait('root survives failed webhook', () => ack(sid, 'webhook unavailable'))
            }
            child.kill('SIGTERM')
            assert.deepEqual(await boundedExit(), [0, null])
            assert.equal(registry().sessions[sid].active, false)
        } else {
            send(sid, 'hello')
            await wait('plain turn', () => ack(sid, 'hello'))
            send(sid, 'approval')
            await wait('approval', () => Object.keys(rows.get(sid).agentState.requests ?? {}).length > 0)
            assert(!ack(sid, 'approval'))
            const key = Object.keys(rows.get(sid).agentState.requests)[0]
            await rpc(sid, 'permission', { id: key, approved: true })
            await wait('native approval', () => records().some((r) => r.reply?.result?.decision === 'accept'))
            await wait(
                'approved turn completed',
                () =>
                    ack(sid, 'approval') &&
                    rows.get(sid).agentState.completedRequests?.[key]?.status === 'resolved'
            )
            send(sid, '/new')
            await wait('second webhook', () => reports.length === 2)
            assert.equal(rows.size, 2)
            const sibling = reports[1].sessionId
            assert.notEqual(sid, sibling)
            assert.equal(reports[1].metadata.hostPid, child.pid)
            const configs = records()
                .filter((r) => r.method === 'thread/start')
                .map((r) => r.p.config)
            assert.equal(configs.length, 2)
            for (const [i, c] of configs.entries()) {
                const bridge = c['mcp_servers.hapi']
                assert.equal(bridge.command, bin)
                assert.equal(bridge.args[0], 'mcp')
                assert.equal(bridge.args.at(-1), reports[i].sessionId)
                assert.equal(c['shell_environment_policy.set.HAPI_SESSION_ID'], reports[i].sessionId)
                assert.equal(bridge.tools.change_title.approval_mode, 'approve')
                assert(!bridge.tools.spawn_peer)
            }
            const bridge = configs[0]['mcp_servers.hapi']
            const tool = spawn(bridge.command, bridge.args, { stdio: ['pipe', 'pipe', 'pipe'] })
            let output = ''
            tool.stdout.on('data', (s) => (output += String(s)))
            const finished = once(tool, 'close')
            for (const [id, method, params] of [
                [
                    1,
                    'initialize',
                    {
                        protocolVersion: '2025-11-25',
                        capabilities: {},
                        clientInfo: { name: 'test', version: '1' }
                    }
                ],
                [2, 'tools/list', {}],
                [3, 'tools/call', { name: 'change_title', arguments: { title: 'Projected only' } }]
            ])
                tool.stdin.write(JSON.stringify({ jsonrpc: '2.0', id, method, params }) + '\n')
            tool.stdin.end()
            assert.deepEqual(await finished, [0, null])
            const replies = output
                .trim()
                .split('\n')
                .map((s) => JSON.parse(s))
            assert.equal(replies.find((x) => x.id === 1).result.serverInfo.name, 'HAPI MCP Bridge')
            assert(replies.find((x) => x.id === 2).result.tools.some((t) => t.name === 'skill_lookup'))
            assert.match(
                replies.find((x) => x.id === 3).result.content[0].text,
                /Successfully changed chat title/
            )
            assert.notEqual(rows.get(sid).metadata.name, 'Projected only')
            assert.deepEqual(
                (await rpc(sibling, 'listCodexModels', {})).models.map((v) => v.id),
                ['mock', 'second']
            )
            const endpoint = registry().endpoint.slice(7)
            const ws = new WebSocket('ws+unix://' + endpoint + ':/')
            await once(ws, 'open')
            ws.send(JSON.stringify({ id: 99, method: 'hapi/stopSession', params: { sessionId: sid } }))
            const [raw] = await once(ws, 'message')
            assert.deepEqual(JSON.parse(String(raw)), { id: 99, result: { stopped: true } })
            ws.close()
            await wait('first archive', () =>
                reports.some((p) => p.sessionId === sid && p.metadata.lifecycleState === 'archived')
            )
            assert.equal(registry().sessions[sid].active, false)
            assert.equal(registry().sessions[sibling].active, true)
            send(sibling, 'still running')
            await wait('sibling reply', () => ack(sibling, 'still running'))
            await rpc(sibling, 'killSession', {})
            assert.deepEqual(await boundedExit(), [0, null])
            assert.equal(registry().sessions[sibling].active, false)
            assert.equal(reports.filter((p) => p.metadata.lifecycleState === 'archived').length, 2)
        }
    }
    if (
        existsSync(join(dir, 'codex-runtimes')) &&
        readdirSync(join(dir, 'codex-runtimes')).some((p) => p.endsWith('.json'))
    ) {
        const record = registry()
        assert.deepEqual(
            JSON.parse(readFileSync(join(native, 'hapi-runtime-owners', record.id + '.json'), 'utf8')),
            record
        )
        assert.throws(() => process.kill(record.serverPid, 0), { code: 'ESRCH' })
        assert.deepEqual(scenario === 'measure' ? record.pendingCreations ?? [] : record.pendingCreations, [])
        assert(Object.values(record.sessions).every((b) => !b.active))
        const path = records().find((r) => r.engineSocket)?.engineSocket
        if (path) assert(!existsSync(dirname(path)), path)
        assert(!stderr.includes('ownership retained for orphan recovery'), stderr)
    }
    if (scenario !== 'measure') assert.equal(stdout, '')
} finally {
    if (child.exitCode === null && !child.signalCode) {
        child.kill('SIGTERM')
        await boundedExit().catch(() => child.kill('SIGKILL'))
    }
    await io.close()
    rmSync(dir, { force: true, recursive: true })
}

import assert from 'node:assert/strict'
import { mkdtempSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { summary } from './stats'

const dir = mkdtempSync(join(tmpdir(), 'hapi-hook-baseline-'))
const token = crypto.randomUUID()
const input = JSON.stringify({ hook_event_name: 'SessionStart', session_id: 'baseline' })
let received = 0
const server = Bun.serve({ hostname: '127.0.0.1', port: 0, async fetch(req) {
    if (req.method !== 'POST' || new URL(req.url).pathname !== '/hook/session-start' ||
        req.headers.get('x-hapi-hook-token') !== token || await req.text() !== input) return new Response(null, { status: 400 })
    received++
    return new Response(null, { status: 204 })
} })
try {
    const env = { ...process.env, HAPI_HOME: dir, BENCH_TOKEN: token, BENCH_PORT: String(server.port) }
    await Bun.write(join(dir, 'stdin'), input)
    const ts = `bun cli/src/index.ts hook-forwarder --port ${server.port} --token ${token}`
    const shell = `sh -c 'exec curl -fsS --max-time 1 -H "content-type: application/json" -H "x-hapi-hook-token: $BENCH_TOKEN" --data-binary @- http://127.0.0.1:$BENCH_PORT/hook/session-start'`
    for (const [name, command] of [['ts', ts], ['shell', shell]]) {
        const before = received
        const json = join(dir, name + '.json')
        const child = Bun.spawn(['hyperfine', '--shell=none', '--runs=200', '--input', join(dir, 'stdin'),
            '--export-json', json, command], { env, stdout: 'ignore', stderr: 'inherit' })
        assert.equal(await child.exited, 0)
        const result = (await Bun.file(json).json()).results[0]
        assert.equal(result.times.length, 200)
        assert.equal(received - before, 200)
        console.log(JSON.stringify({ hook: name, meanMs: result.mean * 1000,
            seconds: result.times, ms: summary(result.times.map((s: number) => s * 1000)) }))
    }
} finally {
    await server.stop(true)
    rmSync(dir, { recursive: true, force: true })
}

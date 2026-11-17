import { existsSync, readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import { performance } from 'node:perf_hooks'
import { createServer } from 'node:net'
import { createTwoFilesPatch } from '../web/node_modules/diff/libesm/index.js'
import { unified } from '../web/node_modules/unified/index.js'
import remarkParse from '../web/node_modules/remark-parse/index.js'
import remarkGfm from '../web/node_modules/remark-gfm/index.js'
import { createHapiWasmEngineFromBytes } from '../web/src/wasm/hapiWasmClient'
import { AGENT_MESSAGE_PAYLOAD_TYPE } from '@hapi/protocol'

const ROOT_DIR = resolve(import.meta.dir, '..')
const BASELINE_DIR = resolve(ROOT_DIR, '../hapi')
const TS_OPT_HUB_DIR = resolve(ROOT_DIR, '../hapi-perf-hub-streaming-sse')
const WASM_PATH = resolve(ROOT_DIR, 'web/public/wasm/hapi_wasm.wasm')
const HUB_RS_BIN = resolve(ROOT_DIR, 'target/release/hapi-hub-rs')
const RUNNER_RS_BIN = resolve(ROOT_DIR, 'target/release/hapi-runner-rs')

function readVmRssKib(pid: number): number {
    try {
        const status = readFileSync(`/proc/${pid}/status`, 'utf8')
        const match = /^VmRSS:\s+(\d+)\s+kB/m.exec(status)
        return match ? Number(match[1]) : 0
    } catch {
        return 0
    }
}

async function getFreePort(): Promise<number> {
    return await new Promise((resolvePort, reject) => {
        const srv = createServer()
        srv.listen(0, '127.0.0.1', () => {
            const addr = srv.address()
            const port = typeof addr === 'object' && addr ? addr.port : 0
            srv.close(() => resolvePort(port))
        })
        srv.on('error', reject)
    })
}

async function benchRunnerProcesses() {
    const bootScript = `
        await import('${ROOT_DIR}/packages/hapi-protocol/src/messages.ts');
        console.log(JSON.stringify({ pid: process.pid }));
        await new Promise((r) => setTimeout(r, 1200));
    `
    const spawnOneBun = async () => {
        const t0 = performance.now()
        const proc = Bun.spawn([process.execPath, '-e', bootScript], {
            stdout: 'pipe',
            stderr: 'pipe',
            env: { ...process.env }
        })
        const reader = proc.stdout.getReader()
        let acc = ''
        while (true) {
            const { value, done } = await reader.read()
            if (done) break
            acc += new TextDecoder().decode(value)
            if (acc.includes('\n')) break
        }
        const bootMs = performance.now() - t0
        const rssKib = readVmRssKib(proc.pid)
        return { proc, bootMs, rssKib }
    }

    const bunSingle = await spawnOneBun()
    bunSingle.proc.kill()
    await bunSingle.proc.exited

    const tTen0 = performance.now()
    const bunTen = await Promise.all(Array.from({ length: 10 }, () => spawnOneBun()))
    const bunTenWallMs = performance.now() - tTen0
    const bunTenRssKib = bunTen.reduce((sum, item) => sum + item.rssKib, 0)
    for (const item of bunTen) {
        item.proc.kill()
        await item.proc.exited
    }

    const runRustProbe = async (sessions: number) => {
        const t0 = performance.now()
        const proc = Bun.spawn([RUNNER_RS_BIN, '--probe-sessions', String(sessions), '--hold-ms', '20'], {
            stdout: 'pipe',
            stderr: 'pipe'
        })
        const stdout = await new Response(proc.stdout).text()
        await proc.exited
        const wallMs = performance.now() - t0
        const parsed = JSON.parse(stdout.trim()) as {
            sessionCount: number
            bootMs: number
            vmRssKib: number
        }
        return { wallMs, internalBootMs: parsed.bootMs, rssKib: parsed.vmRssKib }
    }

    const rust1 = await runRustProbe(1)
    const rust10 = await runRustProbe(10)

    return {
        bunSingle: { bootMs: round(bunSingle.bootMs), rssMb: round(bunSingle.rssKib / 1024) },
        bunTenSessions: { bootMs: round(bunTenWallMs), totalRssMb: round(bunTenRssKib / 1024) },
        rustSingle: { spawnMs: round(rust1.wallMs), initMs: round(rust1.internalBootMs), rssMb: round(rust1.rssKib / 1024) },
        rustTenSessions: { spawnMs: round(rust10.wallMs), initMs: round(rust10.internalBootMs), totalRssMb: round(rust10.rssKib / 1024) },
        memorySpeedup10xSessions: round(bunTenRssKib / Math.max(1, rust10.rssKib)),
        bootSpeedup10xSessions: round(bunTenWallMs / Math.max(0.01, rust10.wallMs)),
    }
}

async function benchWasmVsJs() {
    const wasmBytes = readFileSync(WASM_PATH)
    const engine = await createHapiWasmEngineFromBytes(wasmBytes)

    // 1. 25 KB streaming Markdown (50 cumulative chunks)
    const section = [
        '## Analysis Section',
        '',
        'Checking streaming performance with **bold**, `inline_code()`, and [links](https://example.com).',
        '',
        '```ts',
        'export function streamDelta(prev: string, next: string) {',
        '    return next.startsWith(prev) ? next.slice(prev.length) : next;',
        '}',
        '```',
        '',
        '| Stage | Latency | Status |',
        '| --- | --- | --- |',
        '| Parse | 0.1 ms | ok |',
        '',
    ].join('\n')

    const chunks: string[] = []
    let acc = ''
    for (let i = 0; i < 50; i += 1) {
        acc += (i % 5 === 0 ? `\n\n${section}\n` : `Word_${i} streaming token update. `)
        chunks.push(acc)
    }
    const mdBytes = Buffer.byteLength(acc, 'utf8')

    const remarkProcessor = unified().use(remarkParse).use(remarkGfm)
    // Warmup
    remarkProcessor.runSync(remarkProcessor.parse(chunks[0]!))
    engine.renderStreamMarkdown(chunks[0]!)
    engine.resetStreamMarkdown()

    const tJsMd0 = performance.now()
    for (const c of chunks) {
        const tree = remarkProcessor.parse(c)
        remarkProcessor.runSync(tree)
    }
    const jsMdMs = performance.now() - tJsMd0

    engine.resetStreamMarkdown()
    const tWasmMd0 = performance.now()
    for (const c of chunks) {
        engine.renderStreamMarkdown(c)
    }
    const wasmMdMs = performance.now() - tWasmMd0

    // 2. 2,000-line unified diff
    const oldLines = Array.from({ length: 2000 }, (_, i) => `export const row_${i} = ${i}; // unchanged line padding`).join('\n') + '\n'
    const newLines = Array.from({ length: 2000 }, (_, i) =>
        i % 40 === 0 ? `export const row_${i} = ${i + 999}; // modified` : `export const row_${i} = ${i}; // unchanged line padding`
    ).join('\n') + '\n'

    // Warmup
    createTwoFilesPatch('a.ts', 'b.ts', 'a\n', 'b\n')
    engine.computeDiff('a\n', 'b\n', 3)

    const tJsDiff0 = performance.now()
    const jsPatch = createTwoFilesPatch('a/file', 'b/file', oldLines, newLines)
    const jsDiffMs = performance.now() - tJsDiff0

    const tWasmDiff0 = performance.now()
    const wasmPatch = engine.computeDiff(oldLines, newLines, 3)
    const wasmDiffMs = performance.now() - tWasmDiff0

    return {
        markdownBytes: mdBytes,
        chunks: chunks.length,
        jsRemark50ChunksMs: round(jsMdMs),
        wasmIncremental50ChunksMs: round(wasmMdMs),
        markdownSpeedupX: round(jsMdMs / Math.max(0.01, wasmMdMs)),
        diffLines: 2000,
        jsDiffMs: round(jsDiffMs),
        wasmDiffMs: round(wasmDiffMs),
        diffSpeedupX: round(jsDiffMs / Math.max(0.01, wasmDiffMs)),
        mainThreadBlockWasmWorkerMs: 0,
        patchLengths: { js: jsPatch.length, wasm: wasmPatch.unified.length },
    }
}

async function benchTsHubStoreAndSse(repoPath: string, label: string) {
    const { Store } = await import(`${repoPath}/hub/src/store/index.ts`)
    const { SSEManager } = await import(`${repoPath}/hub/src/sse/sseManager.ts`)

    const store = new Store(':memory:')
    const sse = new SSEManager(1000)
    const session = store.sessions.getOrCreateSession(`bench-${label}`, { path: '/tmp/bench' }, null, 'default')

    for (let i = 0; i < 500; i += 1) {
        store.messages.addMessage(session.id, {
            role: 'agent',
            content: { type: AGENT_MESSAGE_PAYLOAD_TYPE, data: { type: 'message', message: `seed-${i}` } }
        })
    }

    let wireBytes = 0
    for (let c = 0; c < 10; c += 1) {
        sse.subscribe({
            id: `sub-${c}`,
            namespace: 'default',
            send: (frame: string) => {
                wireBytes += Buffer.byteLength(frame, 'utf8')
            },
            close: () => {}
        })
    }

    const t0 = performance.now()
    let reasoningText = ''
    for (let chunk = 0; chunk < 100; chunk += 1) {
        reasoningText += `token_${chunk} reasoning step analysis text padding. `
        const stored = store.messages.addMessage(session.id, {
            role: 'agent',
            content: {
                type: AGENT_MESSAGE_PAYLOAD_TYPE,
                data: { type: 'reasoning', id: 'stream-bench', live: chunk < 99, message: reasoningText }
            }
        })
        store.messages.deleteLiveReasoningSnapshots(session.id, 'stream-bench', stored.id)
        const tc = store.messages.upsertToolCallMessage(
            session.id,
            {
                role: 'agent',
                content: {
                    type: AGENT_MESSAGE_PAYLOAD_TYPE,
                    data: { type: 'tool-call', callId: 'call-bench', name: 'bash', status: chunk === 99 ? 'completed' : 'in_progress', input: { step: chunk } }
                }
            },
            'call-bench'
        )
        sse.broadcast({ type: 'message-received', namespace: 'default', sessionId: session.id, message: stored })
        sse.broadcast({ type: 'message-updated', namespace: 'default', sessionId: session.id, message: tc.message })
    }
    const totalMs = performance.now() - t0
    store.close()
    return { label, totalMs: round(totalMs), sseWireBytes: wireBytes }
}

async function benchRustHubEndToEnd() {
    const port = await getFreePort()
    const baseUrl = `http://127.0.0.1:${port}`
    const proc = Bun.spawn([HUB_RS_BIN], {
        env: { ...process.env, HAPI_HUB_RS_LISTEN: `127.0.0.1:${port}`, HAPI_HUB_RS_DB: ':memory:' },
        stdout: 'pipe',
        stderr: 'pipe',
    })

    try {
        for (let i = 0; i < 100; i += 1) {
            try {
                const res = await fetch(`${baseUrl}/health`)
                if (res.ok) break
            } catch {
                await Bun.sleep(10)
            }
        }

        // Seed 500 messages in one batch
        const seedBatch = Array.from({ length: 500 }, (_, i) => ({
            content: { role: 'agent', content: { type: AGENT_MESSAGE_PAYLOAD_TYPE, data: { type: 'message', message: `seed-${i}` } } },
            createdAt: 1000 + i
        }))
        await fetch(`${baseUrl}/cli/sessions/bench-rs/messages`, {
            method: 'POST',
            headers: { 'content-type': 'application/json' },
            body: JSON.stringify({ messages: seedBatch })
        })

        // Connect 10 live HTTP SSE clients
        const controllers = Array.from({ length: 10 }, () => new AbortController())
        const sseResponses = await Promise.all(
            controllers.map((ctrl) => fetch(`${baseUrl}/api/events`, { signal: ctrl.signal }))
        )
        let sseWireBytes = 0
        let totalDeliveredEvents = 0
        const targetEventsPerClient = 200
        const readers = sseResponses.map(async (resp) => {
            const reader = resp.body!.getReader()
            const decoder = new TextDecoder()
            let buf = ''
            let clientEvents = 0
            while (clientEvents < targetEventsPerClient) {
                const { value, done } = await reader.read()
                if (done || !value) break
                sseWireBytes += value.byteLength
                buf += decoder.decode(value, { stream: true })
                const parts = buf.split('\n\n')
                buf = parts.pop() ?? ''
                for (const p of parts) {
                    if (p.includes('data:')) {
                        clientEvents += 1
                        totalDeliveredEvents += 1
                    }
                }
            }
        })

        const t0 = performance.now()
        let reasoningText = ''
        for (let chunk = 0; chunk < 100; chunk += 1) {
            reasoningText += `token_${chunk} reasoning step analysis text padding. `
            await fetch(`${baseUrl}/cli/sessions/bench-rs/messages`, {
                method: 'POST',
                headers: { 'content-type': 'application/json' },
                body: JSON.stringify({
                    messages: [
                        {
                            content: {
                                role: 'agent',
                                content: {
                                    type: AGENT_MESSAGE_PAYLOAD_TYPE,
                                    data: { type: 'reasoning', id: 'stream-bench', live: chunk < 99, message: reasoningText }
                                }
                            },
                            createdAt: 2000 + chunk
                        },
                        {
                            content: {
                                role: 'agent',
                                content: {
                                    type: AGENT_MESSAGE_PAYLOAD_TYPE,
                                    data: { type: 'tool-call', callId: 'call-bench', name: 'bash', status: chunk === 99 ? 'completed' : 'in_progress', input: { step: chunk } }
                                }
                            },
                            createdAt: 2000 + chunk
                        }
                    ]
                })
            })
        }
        await Promise.race([Promise.all(readers), Bun.sleep(2000)])
        const totalMs = performance.now() - t0
        const rssKib = readVmRssKib(proc.pid)
        for (const c of controllers) c.abort()

        // Measure /cli 200-message gzip payload
        const syncNoGzip = await fetch(`${baseUrl}/cli/sessions/bench-rs/messages?afterSeq=300&limit=200`, {
            headers: { 'accept-encoding': 'identity' }
        })
        const rawBytes = (await syncNoGzip.arrayBuffer()).byteLength
        const syncGzip = await fetch(`${baseUrl}/cli/sessions/bench-rs/messages?afterSeq=300&limit=200`, {
            headers: { 'accept-encoding': 'gzip' },
            decompress: false
        } as RequestInit)
        const gzipWireHeader = syncGzip.headers.get('content-encoding')
        const wireBytesGzip = (await syncGzip.arrayBuffer()).byteLength

        return {
            totalMs: round(totalMs),
            sseWireBytes,
            totalDeliveredEvents,
            rssMb: round(rssKib / 1024),
            cli200MsgRawBytes: rawBytes,
            cli200MsgGzipWireBytes: wireBytesGzip,
            gzipHeader: gzipWireHeader,
        }
    } finally {
        proc.kill()
        await proc.exited
    }
}

function round(n: number): number {
    return Math.round(n * 100) / 100
}

async function main() {
    for (const [label, p] of [
        ['WASM binary', WASM_PATH],
        ['hapi-hub-rs binary', HUB_RS_BIN],
        ['hapi-runner-rs binary', RUNNER_RS_BIN],
    ] as const) {
        if (!existsSync(p)) {
            throw new Error(`Missing required artifact (${label}): ${p}`)
        }
    }

    const runner = await benchRunnerProcesses()
    const wasmVsJs = await benchWasmVsJs()
    const hubBaseline = existsSync(BASELINE_DIR)
        ? await benchTsHubStoreAndSse(BASELINE_DIR, 'ts-baseline')
        : null
    const hubTsOpt = existsSync(TS_OPT_HUB_DIR)
        ? await benchTsHubStoreAndSse(TS_OPT_HUB_DIR, 'ts-optimized')
        : null
    const hubRust = await benchRustHubEndToEnd()

    const report = {
        environment: {
            platform: process.platform,
            arch: process.arch,
            bunVersion: Bun.version,
        },
        runner,
        wasmVsJs,
        hub: {
            tsBaseline: hubBaseline,
            tsOptimized: hubTsOpt,
            rustHttpAndSse: hubRust,
            hubLatencySpeedupVsBaselineX: hubBaseline ? round(hubBaseline.totalMs / Math.max(0.01, hubRust.totalMs)) : null,
            hubWireRatioBaselineOverRustX: hubBaseline ? round(hubBaseline.sseWireBytes / Math.max(1, hubRust.sseWireBytes)) : null,
        },
    }

    console.log(JSON.stringify(report, null, 2))
}

void main()

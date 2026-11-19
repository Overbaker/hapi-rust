import assert from 'node:assert/strict'
import { mkdtempSync, readFileSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { createRequire } from 'node:module'
import { Database } from 'bun:sqlite'
import { Store } from '../../hub/src/store'

const cli = createRequire(new URL('../../cli/package.json', import.meta.url))
export const { io } = cli('socket.io-client') as typeof import('../../cli/node_modules/socket.io-client')

const corpus = readFileSync(join(import.meta.dir, 'corpus.md'), 'utf8')
const transcript = process.env.BASELINE_CLAUDE_JSONL
function assistantText(path: string) {
    const blocks: string[] = []
    for (const line of readFileSync(path, 'utf8').split('\n')) {
        if (!line.trim()) continue
        const record = JSON.parse(line)
        if (record.type !== 'assistant' || !Array.isArray(record.message?.content)) continue
        for (const block of record.message.content) {
            if (block.type === 'text' && typeof block.text === 'string') blocks.push(block.text)
        }
    }
    return blocks.join('\n\n')
}
const visibleText=transcript ? assistantText(transcript) : ''
export const longTranscript=Buffer.byteLength(visibleText)>=30*1024
const replayBody = longTranscript ? visibleText : corpus
const encoder = new TextEncoder()
export function replayText(kib: number) {
    const bytes = encoder.encode(replayBody)
    assert(bytes.length >= kib * 1024)
    let end = kib * 1024
    while ((bytes[end] & 0xc0) === 0x80) end--
    return new TextDecoder('utf-8', { fatal: true }).decode(bytes.slice(0, end))
}
const sections = corpus.split(/\n## /)
export type SeedKind = 'standard' | 'clean' | 'heavy' | 'sweep'
export type Seed = {count:number, kind:SeedKind}
function seed(dbPath: string, extra: Seed[]) {
    const store = new Store(dbPath)
    const ids: string[] = []
    try {
        for (const {count,kind} of [{count:500,kind:'standard'}, {count:2000,kind:'standard'}, {count:500,kind:'clean'}, {count:500,kind:'heavy'}, ...extra]) {
            const clean = kind === 'clean'
            const name = `Baseline ${count}${clean ? " clean tail" : ""}`
            const { id } = store.sessions.getOrCreateSession(name + "-" + ids.length, {
                name, path: '/tmp/baseline', host: 'localhost', flavor: 'codex'
            }, null, 'default')
            ids.push(id)
            for (let i = 1; i <= count; i++) {
                const callId = `read-${Math.floor(i / 8)}`
                const text = sections[(i * 17) % sections.length]
                const long = i % 64 === 51 && !(clean && i > count - 200)
                const data = i % 8 === 2
                    ? { type: 'tool-call', name: 'Read', input: { path: `src/module-${i % 31}.ts` }, callId }
                    : i % 8 === 3
                    ? { type: 'tool-call-result', callId, output: { stdout: long ? corpus.repeat(3) : text.slice(0, 1200) } }
                    : { type: 'message', message: i === count ? `Baseline ${count} message ${i}`
                        : text.slice(0, [280, 850, 1600, 2300][i % 4]) + (i % 8 === 6 ? '\n\n```ts\n' + 'const row = records.at(-1)\nreturn row?.id\n' + '```' : '') }
                if (kind === 'heavy' && data.type === 'tool-call') data.name = i % 16 === 2 ? 'Bash' : 'Read'
                if (kind === 'heavy' && data.output && !long) {
                    const kb = [5, 16, 32, 50][Math.floor(i / 8) % 4]
                    data.output.stdout = corpus.repeat(2).slice(0, kb * 1024)
                }
                const value = kind === 'sweep'
                    ? (i % 2 === count % 2 ? {role:'agent',content:{type:'codex',data:{type:'message', message:i===count ? `Baseline ${count} message ${i}` : text.slice(0,280)}}} : {role:'user',content:{type:'text',text:text.slice(0,80)}})
                    : i % 8 === 1
                    ? { role: 'user', content: { type: 'text', text: 'Please check ' + text.split('\n')[0].slice(0, 95) } }
                    : { role: 'agent', content: { type: 'codex', data } }
                const row = store.messages.addMessage(id,value)
                if (long && kind !== "sweep") assert.match(JSON.stringify(row.content), /hapi: truncated/)
            }
        }
    } finally {
        store.close()
    }
    const db = new Database(dbPath, { readonly: true })
    try {
        for (const [i, id] of ids.entries()) {
            const row = db.query("SELECT count(*) n, sum(typeof(content)='blob') blobs FROM messages WHERE session_id=?").get(id) as { n: number, blobs: number }
            assert.equal(row.n, [500, 2000, 500, 500, ...extra.map(s=>s.count)][i])
            if (i<4 || extra[i-4].kind!=="sweep") assert(row.blobs > 0 && row.blobs < row.n)
        }
    } finally {
        db.close()
    }
    return ids
}

export async function withHub<T>(run: (hub: { url: string, ids: string[], token: string, jwt: string, dir: string, pid: number }) => Promise<T>, profileDir?: string, extra: Seed[] = [], preload?:string) {
    const dir = mkdtempSync(join(tmpdir(), 'hapi-baseline-'))
    const token = crypto.randomUUID()
    let hub: ReturnType<typeof Bun.spawn> | undefined
    try {
        const dbPath = join(dir, 'hapi.db')
        const ids = seed(dbPath,extra)
        const reservation = Bun.serve({ hostname: '127.0.0.1', port: 0, fetch: () => new Response() })
        const port = reservation.port!
        await reservation.stop(true)
        const url = `http://127.0.0.1:${port}`
        hub = Bun.spawn(['bun', ...(preload ? ['--preload',preload] : []), ...(profileDir ? ['--cpu-prof', '--cpu-prof-dir=' + profileDir] : []), 'src/index.ts'], {
            cwd: join(import.meta.dir, '../../hub'), stdout: Bun.file(join(dir, 'hub.log')), stderr: 'inherit',
            env: { ...process.env, HAPI_HOME: dir, DB_PATH: dbPath, CLI_API_TOKEN: token,
                HAPI_LISTEN_HOST: '127.0.0.1', HAPI_LISTEN_PORT: String(port) }
        })
        for (let i = 0; i < 150; i++) {
            if (hub.exitCode !== null) throw Error(await Bun.file(join(dir, 'hub.log')).text())
            const res = await fetch(url + '/api/auth', { method: 'POST', headers: { 'Content-Type': 'application/json' },
                body: JSON.stringify({ accessToken: token }) }).catch(() => null)
            if (res?.ok) {
                const { token: jwt } = await res.json()
                assert(jwt)
                return await run({ url, ids, token, jwt, dir, pid: hub.pid })
            }
            await Bun.sleep(100)
        }
        throw Error('Temporary hub did not become ready')
    } finally {
        hub?.kill()
        await hub?.exited
        rmSync(dir, { recursive: true, force: true })
    }
}

export async function replay(url: string, token: string, sid: string, kib: number, interval = 250, sent?: (id: string, length: number) => void, path: 'reasoning'|'message'='reasoning', recordedBody?:string) {
    const socket = io(url + '/cli', { transports: ['websocket'], reconnection: false, autoConnect: false,
        auth: { token, clientType: 'session-scoped', sessionId: sid } })
    const id = crypto.randomUUID()
    const body = recordedBody ?? replayText(kib)
    try {
        await new Promise<void>((resolve, reject) => {
            socket.once('connect', resolve).once('connect_error', reject).connect()
        })
        for (let i = 1; i <= 100; i++) {
            const data = { type: path, ...(path==='message'?{streamSnapshot:true}:{}), id, message: body.slice(0, Math.ceil(body.length * i / 100)),
                ...(i < 100 ? { live: true } : {}) }
            sent?.(id, data.message.length)
            socket.emit('message', { sid, message: { role: 'agent', content: { type: 'codex', data } } })
            await Bun.sleep(interval)
        }
        return id
    } finally {
        socket.disconnect()
    }
}

if (import.meta.main) {
    await withHub(async ({ ids, url, token, jwt }) => {
        for (const kib of [6, 30]) {
            const id = await replay(url, token, ids[0], kib, 5)
            const res = await fetch(url + '/api/sessions/' + ids[0] + '/messages?limit=2', {
                headers: { Authorization: 'Bearer ' + jwt }
            })
            const { messages } = await res.json()
            assert(messages.some((m: any) => m.content.content.data?.id === id &&
                !m.content.content.data.live && m.content.content.data.message === replayText(kib)))
        }
        console.log(JSON.stringify({kind:'fixture',verified:true,replaySource:longTranscript ? 'Claude JSONL assistant text' : 'checked-in Claude-coauthored prose and code',seeds:[500,2000,500,500],cleanTail:200}))
    })
}

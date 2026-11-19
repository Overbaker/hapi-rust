import assert from 'node:assert/strict'
import { mkdtempSync, readdirSync, readFileSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { execFileSync } from 'node:child_process'
import { compressSseResponse } from '../../hub/src/web/sseCompression'
import { withHub, replay, replayText, io, longTranscript } from './fixture'
import { summary } from './stats'

type Hub = Parameters<Parameters<typeof withHub>[0]>[0]

async function subscribe(hub: Hub, sid: string, onFrame: (frame: string, data: any) => void, encoding: 'gzip'|'identity'='identity') {
    const abort = new AbortController()
    const response = await fetch(hub.url + '/api/events?sessionId=' + sid, {
        headers: { Authorization: 'Bearer ' + hub.jwt, 'Accept-Encoding': encoding }, decompress:false, signal: abort.signal
    })
    assert.equal(response.status, 200)
    assert.equal(response.headers.get('content-encoding'),encoding==='gzip'?'gzip':null)
    const bytes=encoding==='gzip'? response.body!.pipeThrough(new DecompressionStream('gzip')) : response.body!
    const ready = Promise.withResolvers<void>()
    const finished = (async () => {
        let pending = ''
        for await (const text of bytes.pipeThrough(new TextDecoderStream())) {
            pending += text
            let end: number
            while ((end = pending.indexOf('\n\n')) >= 0) {
                const frame = pending.slice(0, end + 2)
                pending = pending.slice(end + 2)
                const data = JSON.parse(frame.match(/^data: ?(.+)$/m)![1])
                if (data.type === 'connection-changed') ready.resolve()
                if (data.type === 'message-received') onFrame(frame, data)
            }
        }
    })().catch(error => { if (!abort.signal.aborted) { ready.reject(error); throw error } })
    await Promise.race([ready.promise, finished, Bun.sleep(5000).then(() => { throw Error('SSE handshake timeout') })])
    return async () => { abort.abort(); await finished }
}

async function compressed(frames: string[]) {
    const body = new ReadableStream({ start(c) {
        for (const frame of frames) c.enqueue(new TextEncoder().encode(frame))
        c.close()
    } })
    return (await compressSseResponse(new Response(body), 'gzip').arrayBuffer()).byteLength
}

async function streamBytes(hub: Hub, kib: number) {
    const frames: string[] = []
    const deltas: string[] = []
    const deltaOnly: string[] = []
    let offset = 0
    const close = await subscribe(hub, hub.ids[0], (frame, event) => {
        const data = event.message.content.content.data
        const text = data.message.slice(offset)
        const delta = { type: 'message-delta', sessionId: event.sessionId, messageId: data.id,
            baseSeq: event.message.seq - 1, offset, text }
        const replacement = 'data: ' + JSON.stringify(data.live ? delta : { ...delta, settled: true })
        const projected = frame.replace(/^data: ?.+$/m, () => replacement)
        deltaOnly.push(projected)
        deltas.push(data.live ? projected : frame)
        frames.push(frame)
        offset = data.message.length
    })
    try {
        await replay(hub.url, hub.token, hub.ids[0], kib, 25)
        for (let i = 0; i < 100 && frames.length < 100; i++) await Bun.sleep(20)
        assert.equal(frames.length, 100)
        assert.equal(offset, replayText(kib).length)
        console.log(JSON.stringify({ kib, bytes: Buffer.byteLength(frames.join('')), gzip: await compressed(frames),
            deltaOnlyBytes: Buffer.byteLength(deltaOnly.join('')), deltaOnlyGzip: await compressed(deltaOnly),
            textBytes:Buffer.byteLength(replayText(kib)), corpus:longTranscript ? 'transcript' : 'repository', modeledDeltaBytes: Buffer.byteLength(deltas.join('')), modeledDeltaGzip: await compressed(deltas) }))
    } finally { await close() }
}

async function recorded(hub:Hub) {
    const path=process.env.BASELINE_CLAUDE_JSONL
    if (!path) return
    const blocks=readFileSync(path,'utf8').split('\n').filter(Boolean).map(line=>JSON.parse(line))
        .filter(r=>r.type==='assistant' && Array.isArray(r.message?.content))
        .map(r=>({id:r.message.id, text:r.message.content.filter((b:any)=>b.type==='text').map((b:any)=>b.text).join('')}))
        .filter(r=>r.text)
    if (!blocks.length) return
    const frames:string[]=[]
    const close=await subscribe(hub,hub.ids[0],frame=>frames.push(frame))
    const socket=io(hub.url+'/cli',{auth:{token:hub.token,clientType:'session-scoped',sessionId:hub.ids[0]},autoConnect:false,transports:['websocket']})
    try {
        await new Promise<void>((ok,fail)=>socket.once('connect',ok).once('connect_error',fail).connect())
        for (const b of blocks) {
            socket.emit('message',{sid:hub.ids[0],message:{role:'agent',content:{type:'output',data:{type:'assistant',uuid:crypto.randomUUID(),
                message:{id:b.id,content:[{type:'text',text:b.text}]}}}}})
            await Bun.sleep(25)
        }
        for (let i=0;i<100 && frames.length<blocks.length;i++) await Bun.sleep(20)
        assert.equal(frames.length,blocks.length)
        console.log(JSON.stringify({kind:'recordedTranscript',assistantRecords:blocks.length,textBytes:blocks.reduce((n,r)=>n+Buffer.byteLength(r.text),0),
            wireBytes:Buffer.byteLength(frames.join('')),gzip:await compressed(frames)}))
    } finally {socket.close();await close()}
}

function analyze(path: string, encoding:string) {
    const profile = JSON.parse(readFileSync(path, 'utf8'))
    type Frame = { id: number, children?: number[], callFrame: { functionName: string, url: string } }
    const nodes = new Map<number, Frame>(profile.nodes.map((n: Frame) => [n.id, n]))
    const parents = new Map<number, number>()
    for (const n of nodes.values()) for (const c of n.children ?? []) parents.set(c, n.id)
    const counts = { parse: 0, decode: 0, stringify: 0, compression:0, other: 0 }
    for (const sample of profile.samples) {
        const stack = []
        for (let n = nodes.get(sample); n; n = nodes.get(parents.get(n.id)!)) stack.push(n.callFrame)
        if (stack.some(f=>/CompressionStream|createGzip|deflate|compressStream|gzipCompress/i.test(f.functionName))) {counts.compression++;continue}
        if (!stack.some(f => /web\/routes\/messages\.ts/.test(f.url) || f.functionName === 'getMessagesPage')) continue
        if (stack.some(f => /^(safeParse|parse)$/.test(f.functionName) && /contentCodec|^$/.test(f.url))) counts.parse++
        else if (stack.some(f => /decodeMessageContent|zstdDecompress/.test(f.functionName))) counts.decode++
        else if (stack.some(f => /^(stringify|json)$/.test(f.functionName))) counts.stringify++
        else counts.other++
    }
    console.log(JSON.stringify({ encoding,handlerCpuSamples: counts }))
}

const clockTicks=Number(execFileSync('getconf',['CLK_TCK'],{encoding:'utf8'}))
function processCpuSeconds(pid:number) {
    const fields=readFileSync('/proc/'+pid+'/stat','utf8').split(') ')[1].split(' ')
    return (Number(fields[11])+Number(fields[12]))/clockTicks
}
async function pages(encoding:'gzip'|'identity') {
    const prof = mkdtempSync(join(tmpdir(), 'hapi-page-prof-'))
    try {
        await withHub(async hub => {
            for (const [i, id] of [hub.ids[0],hub.ids[1],hub.ids[3]].entries()) {
                const wall = [], ttfb = []
                let rows = 0, cpuStart = 0, wireBytes = 0
                const warmups = []
                for (let n = 0; n < 320; n++) {
                    if (n===20) cpuStart=processCpuSeconds(hub.pid)
                    const start = performance.now()
                    const res = await fetch(hub.url + '/api/sessions/' + id + '/messages?limit=200', {
                        headers: { Authorization: 'Bearer ' + hub.jwt, 'Accept-Encoding': encoding }, decompress:false
                    })
                    assert(res.ok)
                    const headers = performance.now()
                    assert.equal(res.headers.get('content-encoding'),encoding==='gzip'?'gzip':null)
                    const wire=await res.arrayBuffer()
                    const end=performance.now()
                    wireBytes=wire.byteLength
                    const body=encoding==='gzip'? (await import('node:zlib')).gunzipSync(Buffer.from(wire)).toString() : new TextDecoder().decode(wire)
                    rows = JSON.parse(body).messages.length
                    assert(rows > 0)
                    if (n >= 20) { wall.push(end - start); ttfb.push(headers - start) }
                    else warmups.push(end-start)
                }
                console.log(JSON.stringify({ encoding,wireBytes,hubCpuMsPerPage:1000*(processCpuSeconds(hub.pid)-cpuStart)/wall.length,seed:i===2?'heavy':'standard',session: [500, 2000,500][i], rows, warmups, wall, headersMs:ttfb, pageMs: summary(wall), ttfb: summary(ttfb) }))
            }
        }, prof)
        const file = readdirSync(prof).find(s => s.endsWith('.cpuprofile'))
        assert(file, 'No CPU profile emitted')
        analyze(join(prof, file),encoding)
    } finally { rmSync(prof, { recursive: true, force: true }) }
}

async function load(hub: Hub,encoding:'gzip'|'identity',kib:number) {
    const ids = [...hub.ids]
    while (ids.length < 10) {
        const res = await fetch(hub.url + '/cli/sessions', { method: 'POST', headers: {
            Authorization: 'Bearer ' + hub.token, 'Content-Type': 'application/json'
        }, body: JSON.stringify({ tag: 'load-' + ids.length, metadata: { path: '/tmp/baseline', host: 'localhost' } }) })
        assert(res.ok, await res.clone().text())
        ids.push((await res.json()).session.id)
    }
    const sent = new Map<string, number>()
    const delays: number[] = []
    const closes = []
    const cpu=()=>processCpuSeconds(hub.pid)
    try {
        for (let i = 0; i < 50; i++) closes.push(await subscribe(hub, ids[i % 10], (_, event) => {
            const d = event.message.content.content.data
            const start = sent.get(d.id + ':' + d.message.length)
            assert(start !== undefined)
            delays.push(performance.now() - start)
        },encoding))
        const start = performance.now(), cpuStart = cpu()
        await Promise.all(ids.map(id => replay(hub.url, hub.token, id, kib, 83,
            (stream, length) => sent.set(stream + ':' + length, performance.now()))))
        for (let i = 0; i < 100 && delays.length < 5000; i++) await Bun.sleep(20)
        assert.equal(delays.length, 5000)
        console.log(JSON.stringify({ encoding,kib,subscribers: 50, streams: 10, intervalMs: 83, samples:delays, deliveryMs: summary(delays),
            cpuPctOneCore: (cpu() - cpuStart) * 100000 / (performance.now() - start) }))
    } finally { await Promise.all(closes.map(close => close())) }
}

await withHub(async hub => {
    await recorded(hub)
    await streamBytes(hub, 6)
    await streamBytes(hub, 30)
    for (const kib of [6,30]) {
        await load(hub,'identity',kib)
        await load(hub,'gzip',kib)
    }
})
await pages('identity')
await pages('gzip')

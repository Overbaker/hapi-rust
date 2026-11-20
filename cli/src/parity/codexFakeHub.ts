import { createServer, type IncomingMessage, type Server as HttpServer } from 'node:http'
import { existsSync, writeFileSync } from 'node:fs'
import { join } from 'node:path'
import { setTimeout as sleep } from 'node:timers/promises'
import { Server, type ServerOptions, type Socket } from '../../../hub/node_modules/socket.io'
import type { UpdateMetadataAck, UpdateStateAck } from '@hapi/protocol'

import { monotonicTimeMs } from '../utils/time'

export const sid = 'a1111111-1111-4111-8111-111111111111'
export type Row = { connection: number; step: number; event: string; args: unknown[]; ackRequested: boolean; ack?: unknown[] }
export type ScriptStep =
    | { sendUpdate: Record<string, any> }
    | { rpc: string; params: unknown }
    | { dropConnection: number; backfill?: Record<string, any> }
    | { forceVersionMismatch: { event: 'update-state' | 'update-metadata'; count: number } }
    | { delayAck: number }
    | { signal: 'SIGTERM' }

export async function body(req: IncomingMessage) {
    const chunks = []
    for await (const chunk of req) chunks.push(chunk)
    return chunks.length ? JSON.parse(Buffer.concat(chunks).toString()) : null
}

export async function listen(server: HttpServer) {
    await new Promise<void>((resolve, reject) => server.once('error', reject).listen(0, '127.0.0.1', resolve))
    return (server.address() as { port: number }).port
}

export async function fakeHub(out: string, changed: () => void = () => {}, multiRoot = false) {
    const rows: Row[] = []
    let connection = 0
    let step = 0
    const mismatches = { 'update-state': 0, 'update-metadata': 0 }
    let latestAlive: Record<string, unknown> | undefined
    let delay = 0
    const pendingAcks = new Set<NodeJS.Timeout>()
    let closed = false
    let leaseExpiry: number | undefined
    let blockedUntil = 0
    let socket: Socket
    let sequence = 0
    const histories = new Map<string, Record<string, any>[]>()
    function history(id:string) {
        if (!histories.has(id)) histories.set(id, [])
        return histories.get(id)!
    }
    const now = Date.now()
    const base = { namespace: 'default', seq: 0, active: false, activeAt: now, createdAt: now, updatedAt: now, metadataVersion: 1 }
    const session = { ...base, id: sid, thinking: false, thinkingAt: 0, metadata: null, agentState: null, agentStateVersion: 1 }
    const siblingId = 'b2222222-2222-4222-8222-222222222222'
    const sessions = new Map([[sid,session]])
    const heldBindings=new Set<string>()
    const sockets = new Map<string,Socket>()
    function save() { writeFileSync(join(out, 'transcript.json'), JSON.stringify(rows, null, 2)) }
    function record(event: string, args: unknown[], ackRequested = false, ordinal = connection) {
        const row: Row = { connection: ordinal, step, event, args: structuredClone(args), ackRequested }
        rows.push(row)
        save()
        changed()
        return row
    }
    const server = createServer(async (req, res) => {
        const url = new URL(req.url!, 'http://localhost')
        const data = await body(req)
        const event = req.method + ' ' + url.pathname
        record(event, [{ body: data, query: Object.fromEntries(url.searchParams), authorization: req.headers.authorization }], false, 0)
        let response
        if (event === 'POST /cli/machines') {
            response = { machine: { ...base, ...data, runnerStateVersion: 1 } }
        } else if (event === 'POST /cli/sessions') {
            const target = multiRoot && sessions.get(sid)?.metadata && data.id !== sid ? siblingId : sid
            const row = sessions.get(target) ?? {...session,id:target,metadataVersion:1,agentStateVersion:1}
            sessions.set(target,row)
            Object.assign(row, { metadata: data.metadata, agentState: data.agentState ?? null,
                model: data.model ?? null, effort: data.effort ?? null, modelReasoningEffort: data.modelReasoningEffort ?? null })
            response = { session:row, sessionSummaryContract: false, autoBridgeTransientModelErrors: false }
        } else if (event === 'GET /cli/sessions/' + siblingId && sessions.has(siblingId)) {
            response = {session:sessions.get(siblingId),sessionSummaryContract:false}
        } else if (event === 'GET /cli/sessions/' + sid) {
            response = { session, sessionSummaryContract: false, autoBridgeTransientModelErrors: false }
        } else if (req.method === 'GET' && [...sessions.keys()].some(id=>url.pathname === '/cli/sessions/'+id+'/messages')) {
            const messages=history(url.pathname.split('/')[3])
            const afterSeq = Number(url.searchParams.get('afterSeq'))
            const limit = Number(url.searchParams.get('limit') ?? 200)
            if (!url.searchParams.has('afterSeq') || !Number.isInteger(afterSeq) || afterSeq < 0 ||
                !Number.isInteger(limit) || limit < 1 || limit > 200) {
                res.statusCode = 400
                response = { error: 'Invalid query' }
            } else response = { messages: messages.filter(m => m.seq > afterSeq).slice(0, limit) }
        } else {
            res.statusCode = 404
        }
        res.setHeader('Content-Type', 'application/json')
        res.end(JSON.stringify(response ?? {}))
    })
    const io = new Server(server, { transports: ['websocket'] } as Partial<ServerOptions>)
    io.of('/cli').use(async (_socket, next) => {
        if (blockedUntil > Date.now()) await sleep(blockedUntil - Date.now())
        if (!closed) next()
    })
    io.of('/cli').on('connection', client => {
        const id = multiRoot ? client.handshake.auth.sessionId as string : sid
        const session = sessions.get(id)!
        sockets.set(id,client)
        latestAlive = undefined
        socket = client
        const ordinal = ++connection
        record('auth', [client.handshake.auth])
        client.onAny((event: string, ...args) => {
            const cb = typeof args.at(-1) === 'function' ? args.pop() : undefined
            if (multiRoot && event==='session-alive')writeFileSync(join(out,'alive-'+id),'')
            if (event === 'session-alive' && ordinal === connection) latestAlive = args[0]
            const row = record(event, args, Boolean(cb), ordinal)
            if (!cb) return
            let ack: unknown[] = []
            if (event === 'update-metadata' || event === 'update-state') {
                const key = event === 'update-metadata' ? 'metadata' : 'agentState'
                const version = key === 'metadata' ? 'metadataVersion' : 'agentStateVersion'
                if (mismatches[event] > 0) {
                    session[version]++
                    mismatches[event]--
                }
                const success = args[0].expectedVersion === session[version]
                if (success) {
                    session[version]++
                    session[key] = args[0][key]
                }
                const common = success ? { result: 'success' as const, version: session[version] }
                    : { result: 'version-mismatch' as const, version: session[version] }
                const answer: UpdateMetadataAck | UpdateStateAck = key === 'metadata'
                    ? { ...common, metadata: session.metadata } : { ...common, agentState: session.agentState }
                ack = [answer]
            } else if (event === 'messages-steer-state') ack = [{ ok: true }]
            const reply = () => {
                cb(...ack)
                if(multiRoot && event==='update-metadata'){
                    const turns=args[0].metadata?.conversationHistoryTurns
                    for(const key of Object.keys(turns??{}))writeFileSync(join(out,'projected-'+Buffer.from(key).toString('hex')),'')
                }
                row.ack = structuredClone(ack)
                save()
                changed()
            }
            const bound = multiRoot && event==='update-metadata' && args[0].metadata?.codexSessionId
            if(bound && !heldBindings.has(id)){
                heldBindings.add(id)
                void (async()=>{while(!closed && !existsSync(join(out,'initial-list-'+bound)))await sleep(3); if(!closed)reply()})()
            } else if (delay) {
                const timer = setTimeout(() => {
                    pendingAcks.delete(timer)
                    if (!closed) reply()
                }, delay)
                pendingAcks.add(timer)
            } else reply()
        })
    })
    const port = await listen(server)
    return {
        rows, port, record, session, sessions, siblingId,
        get latestAlive() { return latestAlive },
        get leaseExpiry() { return leaseExpiry },
        async execute(action: ScriptStep & {target?:string}, kill: () => void, facts: { pid: number; marker: string }) {
            const target = action.target === 'sibling' ? siblingId : sid
            const client = sockets.get(target) ?? socket
            const row = sessions.get(target)!
            const messages=history(target)
            step++
            record('step', [action])
            if ('rpc' in action) {
                const params = JSON.stringify(action.params, (_, value) => {
                    if (value === '$permissionId') {
                        const keys=Object.keys((row.agentState as {requests?:object}|null)?.requests??{})
                        if(keys.length!==1)throw Error('Expected exactly one pending permission')
                        return keys[0]
                    }
                    if (value === '$pid') return facts.pid
                    if (value === '$marker') return facts.marker
                    if (value === '$leaseExpiry') {
                        leaseExpiry = monotonicTimeMs() + 15_000
                        return leaseExpiry
                    }
                    return value
                })
                const response = await client.timeout(30_000).emitWithAck('rpc-request', { method: target + ':' + action.rpc, params })
                record('rpc-ack', response === undefined ? [] : [response])
            } else if ('sendUpdate' in action) {
                const update: Record<string, any> = { ...action.sendUpdate, sid:target }
                for (const key of ['metadata', 'agentState'] as const) {
                    if (action.sendUpdate[key] == null) continue
                    const version = key === 'metadata' ? 'metadataVersion' : 'agentStateVersion'
                    const value = { ...row[key] as object | null, ...action.sendUpdate[key] }
                    Object.assign(row, { [key]: value, [version]: row[version] + 1 })
                    update[key] = { value, version: row[version] }
                }
                if (action.sendUpdate.message) messages.push(action.sendUpdate.message)
                const payload = { id: 'update-' + ++sequence, seq: sequence, createdAt: 1, body: update }
                if (update.t === 'cancel-queued-message' || update.t === 'retry-queued-message') {
                    record('update-ack', [await client.timeout(30_000).emitWithAck('update', payload)])
                } else client.emit('update', payload)
            } else if ('dropConnection' in action) {
                blockedUntil = Date.now() + action.dropConnection
                client.conn.close()
                if (action.backfill) messages.push(action.backfill)
                await sleep(action.dropConnection)
            } else if ('forceVersionMismatch' in action) mismatches[action.forceVersionMismatch.event] = action.forceVersionMismatch.count
            else if ('delayAck' in action) delay = action.delayAck
            else kill()
        },
        close: () => {
            closed = true
            for (const timer of pendingAcks) clearTimeout(timer)
            pendingAcks.clear()
            return io.close()
        }
    }
}

#!/usr/bin/env bun
import { createServer } from 'node:http'
import { createRequire } from 'node:module'
import { appendFileSync, existsSync } from 'node:fs'
import { resolve } from 'node:path'
const { WebSocketServer } = createRequire(resolve(import.meta.dir, '../../../../../cli/package.json'))('ws')
if (process.argv[2] === '--version') {
    console.log('codex-cli 0.154.0')
    process.exit(0)
}
const stdinMode = process.argv[2] === 'app-server' && !process.argv.includes('--listen')
const log = (data: object) => appendFileSync(process.env.ENGINE_LOG!, JSON.stringify(data) + '\n')
const tables = new Map<string, { id: string; cwd: string; turns: unknown[] }>()
let count = 0
if (process.env.COLD_THREAD)
    tables.set(process.env.COLD_THREAD, { id: process.env.COLD_THREAD, cwd: process.cwd(), turns: [] })
let archived = !!process.env.COLD_THREAD
let connections = 0
function connection(send: (v: object) => void, disconnect = () => {}) {
    const connectionId = ++connections
    const pending = new Map<string, () => void>()
    return async (req: any) => {
        const { id, method, params: p = {} } = req
        if (method === 'initialize') {
            if (process.env.ENGINE_SCENARIO?.endsWith('prepare-error') && connectionId === 2) {
                send({ id, error: { message: 'prepare refused' } })
                return
            }

            send({ id, result: {} })
            return
        }
        if (!method) {
            log({ reply: req })
            const resume = pending.get(String(id))
            if (resume && req.result?.decision === 'accept') {
                pending.delete(String(id))
                resume()
            }
            return
        }
        if (method === 'initialized') return
        if (p.threadId === '__hapi_capability_probe__') {
            send({ id, error: { code: -32602, message: 'invalid thread' } })
            return
        }
        log({ method, p })
        const answer = (r: any) => send({ id, result: r })
        switch (method) {
            case 'thread/start':
            case 'thread/fork': {
                if (['start-rejection', 'unreserved-rejection'].includes(process.env.ENGINE_SCENARIO!)) {
                    send({ id, error: { message: 'start refused' } })
                    return
                }
                if (process.env.ENGINE_SCENARIO === 'start-loss') {
                    disconnect()
                    return
                }
                if (process.env.ENGINE_SCENARIO === 'signal-start') return
                if (process.env.ENGINE_SCENARIO === 'last-rejection' && count > 0) {
                    while (!existsSync(process.env.ENGINE_LOG + '.release')) await Bun.sleep(10)
                    send({ id, error: { message: 'late rejection' } })
                    return
                }
                const t = { id: 'thread-' + ++count, cwd: p.cwd, turns: [] }
                tables.set(t.id, t)
                answer({ thread: t, model: 'mock' })
                return
            }
            case 'thread/unarchive':
                archived = false
                answer({})
                return
            case 'thread/resume':
                if (process.env.ENGINE_SCENARIO === 'transport-loss') {
                    disconnect()
                    return
                }
                if (process.env.ENGINE_SCENARIO === 'other-rejection') {
                    send({ id, error: { message: 'permission denied' } })
                    return
                }
                if (archived) {
                    send({ id, error: { message: `session ${p.threadId} is archived. unarchive it first` } })
                    return
                }
            case 'thread/read':
                answer({ thread: tables.get(p.threadId), model: 'mock' })
                return
            case 'thread/queue/list':
            case 'thread/list':
            case 'thread/loaded/list':
                answer({ data: [] })
                return
            case 'thread/metadata/update':
            case 'thread/archive':
                answer({})
                return
            case 'thread/settings/update':
                answer({})
                send({
                    method: 'thread/settings/updated',
                    params: { threadId: p.threadId, threadSettings: { model: 'mock', ...p } }
                })
                return
            case 'model/list':
                answer(
                    p.cursor
                        ? { data: [{ id: 'second' }] }
                        : {
                              data: [{ model: 'mock', defaultReasoningEffort: 'high' }],
                              nextCursor: stdinMode ? null : 'next'
                          }
                )
                return
            case 'thread/queue/add': {
                answer({
                    queuedSubmission: {
                        id: 'queued-' + id,
                        input: p.input,
                        clientUserMessageId: p.clientUserMessageId
                    }
                })
                const threadId = p.threadId
                const turnId = 'turn-' + id
                const item = {
                    type: 'userMessage',
                    id: 'user-' + id,
                    clientId: p.clientUserMessageId,
                    content: p.input
                }
                const agent = { type: 'agentMessage', id: 'agent-' + id, text: 'ack ' + p.input[0].text }
                const turn = { id: turnId, status: 'inProgress', items: [item] as unknown[] }
                tables.get(threadId)!.turns.push(turn)
                send({ method: 'turn/started', params: { threadId, turn: { id: turnId } } })
                send({ method: 'item/completed', params: { threadId, turnId, item } })
                const finish = () => {
                    turn.status = 'completed'
                    turn.items.push(agent)
                    send({ method: 'item/completed', params: { threadId, turnId, item: agent } })
                    send({
                        method: 'turn/completed',
                        params: { threadId, turn: { id: turnId, status: 'completed' } }
                    })
                }
                if (p.input[0].text === 'approval') {
                    const key = 'permission-' + id
                    pending.set(key, () => {
                        send({ method: 'serverRequest/resolved', params: { threadId, requestId: key } })
                        finish()
                    })
                    send({
                        id: key,
                        method: 'item/commandExecution/requestApproval',
                        params: { threadId, turnId, itemId: 'shell-item', command: 'echo ok' }
                    })
                } else {
                    finish()
                }
                return
            }
            default:
                send({ id, error: { code: -32601, message: 'unknown method: ' + method } })
        }
    }
}
if (stdinMode) {
    const receive = connection((v) => console.log(JSON.stringify(v)))
    for await (const line of (await import('node:readline')).createInterface({ input: process.stdin }))
        receive(JSON.parse(line))
} else {
    const http = createServer()
    const ws = new WebSocketServer({ server: http })
    ws.on('connection', (socket) => {
        const receive = connection(
            (v) => socket.send(JSON.stringify(v)),
            () => socket.terminate()
        )
        socket.on('message', (raw) => receive(JSON.parse(String(raw))))
    })
    const socket = process.argv[process.argv.indexOf('--listen') + 1].slice(7)
    log({ engineSocket: socket })
    if (process.env.ENGINE_SCENARIO === 'init-exit') {
        while (!existsSync(process.env.ENGINE_LOG + '.release')) await Bun.sleep(5)
        process.exit(37)
    }
    http.listen(socket)
}

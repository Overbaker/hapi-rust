#!/usr/bin/env bun
import assert from 'node:assert/strict'
import { createServer } from 'node:http'
import { existsSync, readFileSync, writeFileSync } from 'node:fs'
import { join } from 'node:path'
import { WebSocketServer } from 'ws'
import { claimLaunch } from '../launchLog'

if (process.argv[2] === '--version') {
    console.log('codex-cli 0.154.0')
    process.exit(0)
}
const argv = process.argv.slice(2)
assert.equal(argv[0], 'app-server')
const socket = argv[argv.indexOf('--listen') + 1]
assert.match(socket, /^unix:\/\/\/tmp\/hapi-cx-[a-zA-Z0-9]{6}\/engine\.sock$/)
const out = process.env.PARITY_OUT!
const store = join(process.env.CODEX_HOME!, 'fixture-native-history.json')
const script: { mode: string; members?: Record<string, string> } = JSON.parse(
    readFileSync(process.env.PARITY_AGENT_SCRIPT!, 'utf8')
)
const launch = claimLaunch(out, 'agent')
assert.equal(process.env.HAPI_SESSION_ID, undefined)
launch.save({
    argv: argv.map((v) => (v === socket ? 'unix://<private>/engine.sock' : v)),
    cwd: process.cwd(),
    codexHome: process.env.CODEX_HOME
})
type Item = Record<string, any>
type Turn = { id: string; status: string; items: Item[] }
type Thread = { id: string; cwd: string; turns: Turn[]; archived?: boolean }
const threads: Thread[] = existsSync(store) ? JSON.parse(readFileSync(store, 'utf8')) : []
const queues = new Map<string, Item[]>()
const save = () => writeFileSync(store, JSON.stringify(threads))
const replyFns = new Map<string, (value: Item) => void>()
let sequence = 0
const marker = (key: string) => writeFileSync(join(out, key), '')
const http = createServer()
const ws = new WebSocketServer({ server: http })
async function fence(key: string) {
    if (!process.env.PARITY_CLOCK) return
    while (!existsSync(join(out, key))) await Bun.sleep(3)
}
ws.on('connection', (client) => {
    const send = (v: Item) => client.send(JSON.stringify(v))
    const note = (method: string, params: Item) => send({ method, params })
    client.on('message', async (raw) => {
        try {
            const req = JSON.parse(String(raw))
            const { method, id, params: p = {} } = req
            const reply = (result: Item) => send({ id, result })
            if (!method) {
                const fn = replyFns.get(String(id))
                assert(fn)
                replyFns.delete(String(id))
                fn(req.result)
                return
            }
            if (method === 'initialize') {
                reply({})
                return
            }
            if (method === 'initialized') return
            if (p.threadId === '__hapi_capability_probe__') {
                send({ id, error: { code: -32602, message: 'unknown thread' } })
                return
            }
            const tid = String(p.threadId)
            const thread = threads.find((t) => t.id === tid)
            const queue = queues.get(tid) ?? []
            queues.set(tid, queue)
            switch (method) {
                case 'thread/start':
                case 'thread/fork': {
                    await fence('alive-' + p.config['shell_environment_policy.set.HAPI_SESSION_ID'])
                    const t = {
                        id: 'thread-' + (threads.length + 1),
                        cwd: p.cwd,
                        turns: method === 'thread/fork' ? structuredClone(thread!.turns) : []
                    }
                    threads.push(t)
                    save()
                    reply({ thread: t, model: 'mock' })
                    return
                }
                case 'thread/unarchive':
                    assert(thread)
                    thread.archived = false
                    save()
                    reply({})
                    return
                case 'thread/resume':
                    if (thread?.archived) {
                        send({ id, error: { message: 'session ' + tid + ' is archived. unarchive first' } })
                        return
                    }
                case 'thread/read':
                    assert(thread)
                    reply({ thread, model: 'mock' })
                    return
                case 'thread/list':
                case 'thread/loaded/list':
                    reply({ data: [] })
                    return
                case 'thread/archive':
                    assert(thread)
                    thread.archived = true
                    save()
                    reply({})
                    return
                case 'thread/metadata/update':
                    reply({})
                    return
                case 'thread/queue/list':
                    marker('initial-list-' + tid)
                    reply({ data: queue })
                    return
                case 'thread/queue/delete': {
                    const i = queue.findIndex((v) => v.id === p.queuedSubmissionId)
                    if (i >= 0) queue.splice(i, 1)
                    reply({ deleted: i >= 0 })
                    return
                }
                case 'thread/queue/add': {
                    assert(thread)
                    const sub = {
                        id: 'queued-' + ++sequence,
                        input: p.input,
                        clientUserMessageId: p.clientUserMessageId
                    }
                    reply({ queuedSubmission: sub })
                    const msg = p.input[0].text as string
                    if (msg === 'queued') {
                        queue.push(sub)
                        marker('native-queued')
                        return
                    }
                    const turn: Turn = {
                        id: 'turn-' + (thread.turns.length + 1),
                        status: 'inProgress',
                        items: []
                    }
                    thread.turns.push(turn)
                    const context = { threadId: tid, turnId: turn.id }
                    note('turn/started', { threadId: tid, turn: { id: turn.id } })
                    const item = {
                        type: 'userMessage',
                        id: 'u-' + turn.id,
                        clientId: p.clientUserMessageId,
                        content: p.input
                    }
                    turn.items.push(item)
                    note('item/completed', { ...context, item })
                    await fence(
                        'projected-' + Buffer.from(script.members?.[msg] ?? 'local-1').toString('hex')
                    )
                    const finish = (answer: string) => {
                        const item = { type: 'agentMessage', id: 'a-' + turn.id, text: answer }
                        turn.items.push(item)
                        note('item/completed', { ...context, item })
                        turn.status = 'completed'
                        save()
                        note('turn/completed', { threadId: tid, turn })
                    }
                    if (script.mode.startsWith('approval-')) {
                        send({
                            method: 'item/commandExecution/requestApproval',
                            id: 'approval',
                            params: { ...context, itemId: 'tool', command: 'echo ok' }
                        })
                        replyFns.set('approval', (result) => {
                            assert.equal(
                                result.decision,
                                script.mode.endsWith('allow') ? 'accept' : 'decline'
                            )
                            note('serverRequest/resolved', { threadId: tid, requestId: 'approval' })
                            finish(result.decision)
                        })
                    } else if (script.mode === 'async-question') {
                        const q = {
                            type: 'agentMessage',
                            phase: 'final_answer',
                            delivery: 'async',
                            id: 'question',
                            questions: [{ title: 'Which?', options: ['Blue'] }]
                        }
                        turn.items.push(q)
                        note('item/completed', { ...context, item: q })
                        save()
                    } else if (script.mode === 'steer' || script.mode === 'queue-cancel') {
                        save()
                    } else if (script.mode === 'mixed-stop' && msg === 'in-flight') {
                        save()
                        marker('sibling-working')
                        while (!existsSync(join(out, 'release-sibling'))) await Bun.sleep(10)
                        finish('ack ' + msg)
                    } else finish('ack ' + msg)
                    return
                }
                case 'turn/steer': {
                    assert(thread)
                    const turn = thread.turns.at(-1)!
                    assert.equal(p.expectedTurnId, turn.id)
                    reply({})
                    const item = {
                        type: 'userMessage',
                        id: 'followup',
                        clientId: p.clientUserMessageId,
                        content: p.input
                    }
                    turn.items.push(item)
                    note('item/completed', { threadId: tid, turnId: turn.id, item })
                    await fence('projected-' + Buffer.from(p.clientUserMessageId).toString('hex'))
                    turn.status = 'completed'
                    save()
                    note('turn/completed', { threadId: tid, turn })
                    return
                }
                default:
                    throw Error('Unexpected native method ' + method)
            }
        } catch (err) {
            writeFileSync(join(out, 'agent-error'), String(err))
            process.exit(1)
        }
    })
})
http.listen(socket.slice(7))

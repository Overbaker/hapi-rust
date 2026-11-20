#!/usr/bin/env bun
/**
 * Scripted `agy` for the parity corpus. `--version` answers 1.5.3; `acp`
 * speaks ndjson JSON-RPC driven by PARITY_AGENT_SCRIPT (one step per line):
 *   {"await": method, "match"?: params subset, "reply"?: result, "error"?: {code, message}}
 *       takes the next request (or notification) with that method
 *   {"reply": result, "to"?: name}        answers the last (or the `as`-named) request
 *   {"notify": update}                     session/update for the session
 *   {"request": method, "params": {...}}   agent request; waits for its answer
 *   {"awaitMarker": name} / {"sleep": ms} / {"exit": code}
 *   {"transcript": step}                   appends a brain transcript step
 * Methods the script never awaits are answered automatically.
 */
import assert from 'node:assert/strict'
import { appendFileSync, existsSync, mkdirSync, readFileSync, watch, writeFileSync } from 'node:fs'
import { homedir } from 'node:os'
import { join } from 'node:path'
import { createInterface } from 'node:readline'
import { claimLaunch } from '../launchLog'

type Frame = Record<string, any>
type Step = Record<string, any>

const out = process.env.PARITY_OUT!
const argv = process.argv.slice(2)
const kind = argv[0] === '--version' ? 'version' : argv[0] === 'acp' ? 'agent' : 'probe'
const launch = claimLaunch(out, kind)
const envKeys = ['GEMINI_FORCE_FILE_STORAGE', 'SSH_CLIENT', 'SSH_CONNECTION', 'SSH_TTY', 'HAPI_SESSION_ID']
const log = {
    argv,
    env: Object.fromEntries(envKeys.filter(key => process.env[key] !== undefined).map(key => [key, process.env[key]])),
    stdin: [] as Frame[],
    completed: 0
}
const save = () => launch.save(log)
save()
if (kind === 'version') {
    console.log('1.5.3')
    process.exit(0)
}
if (kind === 'probe') process.exit(1)

function matches(actual: any, expected: any): boolean {
    if (expected === null || typeof expected !== 'object') return actual === expected
    return !!actual && Object.entries(expected).every(([k, v]) => matches(actual[k], v))
}
async function waitMarker(name: string) {
    await new Promise<void>((resolve, reject) => {
        const check = () => {
            if (existsSync(join(out, name))) {
                watcher.close()
                resolve()
            }
        }
        const watcher = watch(out, check).on('error', reject)
        check()
    })
}

const lines = readFileSync(process.env.PARITY_AGENT_SCRIPT!, 'utf8').split('\n').filter(Boolean).map(line => JSON.parse(line))
const header: Step = lines[0]?.config ? lines.shift().config : {}
const script: Step[] = lines.filter(step => !step.invocation || step.invocation === launch.index)
const sessionId: string = header.sessionId ?? 'c3333333-3333-4333-8333-333333333333'
const scripted = new Set(script.filter(step => step.await).map(step => step.await as string))
const send = (frame: Frame) => process.stdout.write(JSON.stringify({ jsonrpc: '2.0', ...frame }) + '\n')
let currentSession = sessionId

function automatic(frame: Frame) {
    const { method, id, params = {} } = frame
    if (id === undefined) return
    switch (method) {
        case 'initialize':
            return send({ id, result: header.initialize ?? {
                protocolVersion: 1,
                agentCapabilities: { loadSession: false, promptCapabilities: { image: false }, sessionCapabilities: { resume: {}, list: {} } },
                authMethods: []
            } })
        case 'session/new':
            currentSession = sessionId
            return send({ id, result: { sessionId, ...(header.session ?? {}) } })
        case 'session/resume':
            currentSession = params.sessionId
            return send({ id, result: { sessionId: params.sessionId, ...(header.session ?? {}) } })
        case 'session/list':
            return send({ id, result: { sessions: header.sessions ?? [] } })
        case 'session/set_mode':
        case 'session/set_model':
        case 'session/set_config_option':
            return send({ id, result: {} })
        default:
            return send({ id, error: { code: -32601, message: 'Method not found: ' + method } })
    }
}

const inbox: Frame[] = []
const answers = new Map<number | string, (frame: Frame) => void>()
let wake: (() => void) | null = null
const input = createInterface({ input: process.stdin })
input.on('line', line => {
    if (!line.trim()) return
    const frame = JSON.parse(line)
    log.stdin.push(frame)
    save()
    if (frame.method === undefined) {
        const answer = answers.get(frame.id)
        answers.delete(frame.id)
        answer?.(frame)
        return
    }
    if (!scripted.has(frame.method)) {
        automatic(frame)
        return
    }
    inbox.push(frame)
    wake?.()
})
input.on('close', () => process.exit(0))

async function take(method: string, match: unknown): Promise<Frame> {
    for (;;) {
        const index = inbox.findIndex(frame => frame.method === method && matches(frame.params, match ?? {}))
        if (index >= 0) return inbox.splice(index, 1)[0]!
        await new Promise<void>(resolve => { wake = resolve })
        wake = null
    }
}

let nextId = 1
let last: Frame | undefined
const named = new Map<string, Frame>()
const target = (step: Step) => step.to ? named.get(step.to) : last
function transcriptFile() {
    const dir = join(homedir(), '.gemini', 'antigravity-cli', 'brain', currentSession, '.system_generated', 'logs')
    mkdirSync(dir, { recursive: true })
    return join(dir, 'transcript.jsonl')
}
try {
    for (const step of script) {
        if (step.await) {
            last = await take(step.await, step.match)
            if (step.as) named.set(step.as, last)
            if (step.reply !== undefined) send({ id: last.id, result: step.reply })
            if (step.error !== undefined) send({ id: last.id, error: step.error })
        } else if (step.reply !== undefined) {
            assert.ok(target(step)?.id !== undefined, 'reply without an awaited request')
            send({ id: target(step)!.id, result: step.reply })
        } else if (step.error !== undefined) {
            send({ id: target(step)!.id, error: step.error })
        } else if (step.notify) {
            send({ method: 'session/update', params: { sessionId: currentSession, update: step.notify } })
        } else if (step.request) {
            const id = 'agent-' + nextId++
            const answered = new Promise<Frame>(resolve => answers.set(id, resolve))
            send({ id, method: step.request, params: { sessionId: currentSession, ...step.params } })
            const answer = await answered
            if (step.expect !== undefined) assert.ok(matches(answer.result, step.expect), JSON.stringify({ expected: step.expect, received: answer }))
        } else if (step.awaitMarker) {
            await waitMarker(step.awaitMarker)
        } else if (step.sleep) {
            await Bun.sleep(step.sleep)
        } else if (step.transcript) {
            appendFileSync(transcriptFile(), JSON.stringify(step.transcript) + '\n')
        } else if (step.exit !== undefined) {
            process.exit(step.exit)
        }
        log.completed++
        save()
    }
} catch (error) {
    writeFileSync(join(out, 'agent-error'), String(error))
    process.exit(1)
}

#!/usr/bin/env node
import { randomUUID } from 'node:crypto'
import { mkdirSync, writeFileSync } from 'node:fs'
import { join } from 'node:path'
import { createInterface } from 'node:readline'

const claude = process.env.IDLE_FLAVOR === 'claude'
if (process.argv.includes('--version')) {
    console.log(claude ? '2.1.0' : 'agy version 2.0.0')
    process.exit(0)
}
const sessionId = randomUUID()
const send = message => process.stdout.write(JSON.stringify(message) + '\n')
const answered = () => writeFileSync(process.env.IDLE_ANSWERED, String(process.pid))
for await (const line of createInterface({ input: process.stdin })) {
    if (!line.trim()) continue
    const msg = JSON.parse(line)
    if (claude) {
        if (msg.type === 'control_request') {
            send({ type: 'control_response', response: { subtype: 'success', request_id: msg.request_id, response: {} } })
        } else if (msg.type === 'user') {
            send({ type: 'system', subtype: 'init', session_id: sessionId, tools: [], slash_commands: [], mcp_servers: [], model: 'fixture' })
            send({ ...msg, session_id: sessionId })
            send({ type: 'assistant', uuid: randomUUID(), session_id: sessionId, message: { id: randomUUID(), type: 'message', role: 'assistant', model: 'fixture', content: [{ type: 'text', text: 'idle-benchmark-answer' }], usage: { input_tokens: 1, output_tokens: 1 } } })
            send({ type: 'result', subtype: 'success', session_id: sessionId, is_error: false, duration_ms: 1, duration_api_ms: 1, num_turns: 1, result: 'idle-benchmark-answer', usage: { input_tokens: 1, output_tokens: 1 }, total_cost_usd: 0 })
            if (JSON.stringify(msg.message).includes('idle-benchmark-prompt')) answered()
        }
    } else if (msg.id !== undefined) {
        let result = {}
        if (msg.method === 'initialize') result = { protocolVersion: 1, agentCapabilities: { loadSession: true } }
        if (msg.method === 'session/new') {
            const logs = join(process.env.HOME, '.gemini/antigravity-cli/brain', sessionId, '.system_generated/logs')
            mkdirSync(logs, { recursive: true })
            writeFileSync(join(logs, 'transcript.jsonl'), '')
            result = { sessionId }
        }
        if (msg.method === 'session/prompt') {
            send({ jsonrpc: '2.0', method: 'session/update', params: { sessionId, update: { sessionUpdate: 'agent_message_chunk', content: { type: 'text', text: 'idle-benchmark-answer' } } } })
            result = { stopReason: 'end_turn' }
        }
        send({ jsonrpc: '2.0', id: msg.id, result })
        if (msg.method === 'session/prompt') answered()
    }
}

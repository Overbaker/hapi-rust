import { mkdtempSync, writeFileSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
const directory = mkdtempSync(join(tmpdir(), 'codex-id-vectors-'))
const image = join(directory, 'result.png')
writeFileSync(image, Buffer.from('89504e470d0a1a0a', 'hex'))
const rollout = join(directory, 'history.jsonl')
writeFileSync(
    rollout,
    [
        { type: 'session_meta', payload: { id: '🧪' } },
        { type: 'event_msg', payload: { type: 'task_started', turn_id: 'T' } },
        {
            type: 'compacted',
            payload: { message: 'native summary', resume_metadata: { last_started_turn_id: 'T' } }
        },
        {
            type: 'event_msg',
            payload: {
                type: 'item_completed',
                thread_id: '🧪',
                turn_id: 'T',
                item: { type: 'ContextCompaction', id: 'compact' }
            }
        }
    ]
        .map((x) => JSON.stringify(x) + '\n')
        .join('')
)
import { SharedCodexProjection } from '../../../../../cli/src/codex/shared/projection'
const rows = []
for (const parent of [undefined, 'parent']) {
    const events: unknown[] = []
    let metadata: Record<string, unknown> = {}
    const session = {
        getMetadata: () => metadata,
        updateMetadata: (fn) => {
            metadata = fn(metadata)
        },
        sendAgentMessage: (body, id) => events.push({ body, id }),
        sendUserMessage: (message, _, id) => events.push({ message, id }),
        sendSessionEvent: (body) => events.push({ body })
    }
    const projection = new SharedCodexProjection(
        session as never,
        '🧪',
        async (id) =>
            id === 'batch' ? [{ id: 'orig', input: [{ type: 'text', text: 'accepted' }] }] : undefined,
        parent
    )
    const item = (id, type, fields) => ({ id, type, ...fields })
    const thread = {
        id: '🧪',
        path: rollout,
        turns: [
            {
                id: 'T',
                status: 'completed',
                items: [
                    item('cmd', 'commandExecution', { command: 'ls', exitCode: 0 }),
                    item('patch', 'fileChange', { changes: { x: {} } }),
                    item('tool', 'mcpToolCall', {
                        server: 's',
                        tool: 't',
                        result: { Ok: null, Err: 'denied' }
                    }),
                    item('null', 'mcpToolCall', { server: 's', tool: 't', result: { Ok: null } }),
                    item('collab', 'collabAgentToolCall', {
                        tool: 'spawn_agent',
                        agentsStates: { child: { status: 'completed', '2': 1, '1': 2 } }
                    }),
                    item('compact', 'contextCompaction', {}),
                    item('img', 'imageGeneration', { savedPath: image }),
                    item('ask', 'agentMessage', { delivery: 'async', questions: [{ title: 'Choose' }] }),
                    item('row', 'userMessage', {
                        clientId: 'batch',
                        content: [{ type: 'text', text: 'batch' }]
                    }),
                    item('P', 'plan', { text: 'do it' }),
                    item('U', 'userMessage', { content: [{ type: 'mention', path: 'a' }] }),
                    item('A', 'agentMessage', { text: 'answer' }),
                    item('Q', 'mcpToolCall', {
                        server: 'hapi',
                        tool: 'change_title',
                        arguments: { title: '  hello   world ' },
                        result: { Ok: {} },
                        status: 'completed'
                    })
                ]
            },
            { id: 'live', status: 'inProgress', items: [item('partial', 'agentMessage', { text: 'par' })] }
        ]
    }
    const operations = [
        ['history', thread],
        ['reset'],
        ['history', thread],
        [
            'item/completed',
            { threadId: '🧪', turnId: 'live', item: item('partial', 'agentMessage', { text: 'part two' }) }
        ],
        ...(parent
            ? [
                  ['turn/started', { threadId: '🧪', turn: { id: 'T' } }],
                  ['turn/completed', { threadId: '🧪', turn: { id: 'T', status: 'completed' } }]
              ]
            : []),
        ['turn/plan/updated', { turnId: 'T', plan: [{ step: 'x', status: 'running' }] }],
        [
            'thread/tokenUsage/updated',
            { threadId: '🧪', turnId: 'T', tokenUsage: { '10': 1, '2': 2, cost: 1e-7 } }
        ],
        ['thread/goal/updated', { threadId: '🧪', goal: { objective: 'hello' } }],
        ['error', { message: 'bad' }]
    ]
    for (const [op, p] of operations) {
        if (op === 'history') await projection.history(p)
        else if (op === 'reset') projection.reset()
        else await projection.notification(op as string, p)
    }
    rows.push({ parent, operations, events, metadata })
}
console.log(JSON.stringify(rows))

rmSync(directory, { recursive: true, force: true })

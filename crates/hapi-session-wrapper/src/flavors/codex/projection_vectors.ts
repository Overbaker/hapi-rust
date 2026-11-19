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
    const projection = new SharedCodexProjection(session as never, '🧪', async () => {}, parent)
    const item = (id, type, fields) => ({ id, type, ...fields })
    const thread = {
        id: '🧪',
        turns: [
            {
                id: 'T',
                status: 'completed',
                items: [
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

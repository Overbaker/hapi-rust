import { otherModelListers, rustKnownGaps } from './rustKnownGaps'

type JsonObject = Record<string, any>
type Transcript = {
    methods: Record<string, string[]>
    channels: Record<string, JsonObject[]>
    agent: { argv: string[]; stdin: JsonObject[] }[]
    decoded: JsonObject[]
}

export function expectedRustTranscript(name: string, golden: Transcript): Transcript {
    const expected = structuredClone(golden)
    const allowed = new Set(rustKnownGaps(name).map(gap => gap.path))
    if (!allowed.size) return expected

    for (const [connection, methods] of Object.entries(expected.methods)) {
        expected.methods[connection] = methods.filter(method => !otherModelListers.some(lister =>
            allowed.has('methods[*].' + lister) && method === '<uuid:1>:' + lister))
    }
    if (allowed.has('agent[*].argv[--allowedTools].mcp__hapi__link_pr')) {
        for (const { argv } of expected.agent) {
            const index = argv.indexOf('--allowedTools')
            if (index !== -1) argv[index + 1] = argv[index + 1].split(',').filter(tool => tool !== 'mcp__hapi__link_pr').join(',')
        }
    }
    const stamp = (metadata: JsonObject) => {
        if (allowed.has('session-metadata.wrapperRuntime')) metadata.wrapperRuntime = 'rust'
    }
    for (const [channel, rows] of Object.entries(expected.channels)) {
        if (channel.endsWith('/http')) {
            for (const row of rows) {
                if (row.event === 'POST /cli/sessions') stamp(row.args[0].body.metadata)
                if (row.event === 'POST /session-started') stamp(row.args[0].metadata)
            }
        }
        if (!channel.endsWith('/update-metadata')) continue
        for (const row of rows) {
            for (const metadata of [row.args[0].metadata, row.ack[0].metadata]) {
                stamp(metadata)
                const history = metadata.capabilities?.conversationHistory
                if (history && allowed.has('update-metadata.metadata.capabilities.conversationHistory.rewindToMessage')) {
                    delete history.rewindToMessage
                }
            }
        }
    }
    return expected
}

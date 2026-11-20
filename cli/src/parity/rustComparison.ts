import assert from 'node:assert/strict'
import { otherModelListers, projectedUuid, rustKnownGaps } from './rustKnownGaps'

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
    if (allowed.has('agent[0].argv=--version')) {
        assert.deepEqual(expected.agent[0].argv, ['--version'])
        expected.agent.shift()
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
    let previous: JsonObject | undefined
    let removed = 0
    for (const [channel, rows] of Object.entries(expected.channels)) {
        if (channel.endsWith('/http')) {
            for (const row of rows) {
                if (row.event === 'POST /cli/sessions') stamp(row.args[0].body.metadata)
                if (row.event === 'POST /session-started') stamp(row.args[0].metadata)
            }
        }
        if (!channel.endsWith('/update-metadata')) continue
        expected.channels[channel] = rows.filter((row, index) => {
            const path = `channels[${channel}][${index}]`
            const arg = row.args[0], ack = row.ack[0]
            for (const metadata of [arg.metadata, ack.metadata]) {
                stamp(metadata)
                for (const key of ['forkAtMessage', 'rewindToMessage']) {
                    const history = metadata.capabilities?.conversationHistory
                    if (history && allowed.has('update-metadata.metadata.capabilities.conversationHistory.' + key)) delete history[key]
                }
                for (const key of ['conversationHistoryPoints', 'conversationHistoryEntryIds']) {
                    if (allowed.has('update-metadata.metadata.' + key)) delete metadata[key]
                }
            }
            arg.expectedVersion -= removed
            ack.version -= removed
            if (allowed.has(path)) {
                assert.deepEqual(arg.metadata, previous, path + ' must change history only')
                assert.deepEqual(ack, { result: 'success', metadata: previous, version: arg.expectedVersion + 1 })
                removed++
                return false
            }
            if (allowed.has(path + '.args[0].metadata.claudeSessionId')) {
                assert.equal(ack.result, 'version-mismatch')
                arg.metadata.claudeSessionId = rows[index + 2].args[0].metadata.claudeSessionId
            }
            previous = ack.metadata
            return true
        })
        if (!expected.channels[channel].length) delete expected.channels[channel]
    }
    function renumber(object: JsonObject | undefined, key: string) {
        if (object && Object.hasOwn(object, key)) object[key] = projectedUuid(name, object[key])
    }
    if (allowed.has('channels[*/chat].message.content.{id,data.uuid,data.parentUuid}')) {
        for (const [key, rows] of Object.entries(expected.channels)) {
            if (!key.endsWith('/chat')) continue
            for (const row of rows) {
                const content = row.args[0]?.message?.content
                renumber(content, 'id')
                renumber(content?.data, 'uuid')
                renumber(content?.data, 'parentUuid')
            }
        }
    }
    if (allowed.has('agent[*].stdin[*].uuid')) {
        for (const launch of expected.agent) for (const frame of launch.stdin) renumber(frame, 'uuid')
    }
    if (allowed.has('decoded[*].content[*].{uuid,parentUUID}')) {
        for (const message of expected.decoded) {
            if (!Array.isArray(message.content)) continue
            for (const block of message.content) {
                renumber(block, 'uuid')
                renumber(block, 'parentUUID')
            }
        }
    }
    return expected
}

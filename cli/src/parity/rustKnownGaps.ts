const historyNote = './README.md#claude-conversation-history'

export const otherModelListers = [
    'listAgyModels', 'listCodexModels', 'listCopilotModelsForCwd', 'listCursorModels',
    'listGrokModelsForCwd', 'listKimiModelsForCwd', 'listOpencodeModelVariants',
    'listOpencodeModelsForCwd', 'listPiModelsForMachine',
]

export function rustKnownGaps(scenario: string) {
    const entry = (path: string, reason: string, note = historyNote) => ({ scenario, path, reason, note })
    return [
        entry('session-metadata.wrapperRuntime', 'The Rust runner stamps its runtime.', './README.md#runtime-identity'),
        ...otherModelListers.map(method => entry('methods[*].' + method, 'Claude registers its own model lister.', './README.md#runtime-identity')),
        entry('agent[0].argv=--version', 'The TS version probe negotiates native rewind support.'),
        ...['forkAtMessage', 'rewindToMessage'].map(key => entry('update-metadata.metadata.capabilities.conversationHistory.' + key, 'Rust supports fork-current only.')),
        ...(!['hub-archived', 'kill-session', 'sigterm'].includes(scenario) ? [
            entry('agent[*].argv[--allowedTools].mcp__hapi__link_pr', 'The Rust MCP server lacks the PR-link tool.', './README.md#pull-request-links'),
        ] : []),
    ]
}

const codexScenarios = new Set([
    'approval-allow', 'approval-deny', 'async-question', 'disconnect-reconnect',
    'fork', 'kill-one-of-two', 'new', 'plain-turn', 'queue-cancel', 'sigterm', 'steer',
])

export function codexRustKnownGaps(scenario: string) {
    if (!codexScenarios.has(scenario)) return []
    const entry = (path: string, reason: string) => ({ scenario: 'codex/' + scenario, path, reason })
    return [
        entry('session-metadata.wrapperRuntime', 'Rust identifies its wrapper in session metadata.'),
        entry('session-metadata.capabilities.conversationHistory.forkPreview', 'Rust lacks the fork preview RPC.'),
        entry('methods[*].preview-fork-boundary', 'Rust implements forks but lacks boundary previews.'),
        ...otherModelListers.filter(method => method !== 'listCodexModels').map(method =>
            entry('methods[*].' + method, 'The shared Rust workspace registers Claude and Codex model listers.')),
    ]
}

export const piRustKnownGaps = [
    { path: 'metadata.wrapperRuntime', reason: 'The Rust session advertises its runtime to the runner.' },
    ...otherModelListers.map(method => ({
        path: 'methods[*].' + method,
        reason: 'The Rust workspace handlers omit this machine or other-agent model catalog. Pi registers listPiModels for its own session.',
    })),
]

export function expectedPiRust(golden: Record<string, any>) {
    const expected = structuredClone(golden)
    for (const gap of piRustKnownGaps) {
        if (gap.path === 'metadata.wrapperRuntime') {
            expected.metadata.wrapperRuntime = 'rust'
        } else {
            const method = gap.path.slice('methods[*].'.length)
            expected.methods['1'] = expected.methods['1'].filter((m: string) => m !== '<uuid:1>:' + method)
        }
    }
    return expected
}

const agyScenarios = new Set([
    'config-switch', 'interrupt', 'kill-session', 'plain-turn', 'question',
    'live-wakeup', 'live-wakeup-stop', 'resume', 'sigterm-mid-turn', 'steer', 'tool-approve', 'tool-deny',
])
const agyNote = './README.md#linux-agy'

/** Named Rust differences for the agy corpus; each entry carries its reason. */
export function agyRustKnownGaps(scenario: string) {
    if (!agyScenarios.has(scenario)) return []
    const entry = (path: string, reason: string) => ({ scenario: 'agy/' + scenario, path, reason, note: agyNote })
    return [
        entry('session-metadata.wrapperRuntime', 'Rust identifies its wrapper in session metadata.'),
        ...otherModelListers.filter(method => method !== 'listAgyModels').map(method =>
            entry('methods[*].' + method, 'The Rust agy wrapper registers only the Claude and agy model listers.')),
        entry('methods[*].killSession/generation', 'The shared Rust shutdown handler registers the process-generation kill method.'),
        ...(scenario === 'kill-session' ? [
            entry('killSession.ack.machineId', 'The shared Rust shutdown handler names the machine in its kill acknowledgement.'),
            entry('killSession.metadata.archiveReason', 'The shared Rust shutdown handler archives a killed session as User terminated.'),
        ] : []),
    ]
}

type AgyTranscript = { methods: Record<string, string[]>; channels: Record<string, Record<string, any>[]> }

export function expectedAgyRustTranscript<T extends AgyTranscript>(name: string, golden: T): T {
    const expected = structuredClone(golden)
    const allowed = new Set(agyRustKnownGaps(name).map(gap => gap.path))
    for (const [connection, methods] of Object.entries(expected.methods)) {
        const kept = methods.filter(method => {
            const match = /^<uuid:\d+>:(\w[\w-]*)$/.exec(method)
            return !match || !allowed.has('methods[*].' + match[1])
        })
        if (allowed.has('methods[*].killSession/generation')) kept.push('<uuid:1>:killSession/generation/parity-machine/<pid>')
        expected.methods[connection] = kept.sort()
    }
    const stamp = (metadata: Record<string, any> | undefined) => {
        if (metadata && allowed.has('session-metadata.wrapperRuntime')) metadata.wrapperRuntime = 'rust'
    }
    for (const rows of Object.values(expected.channels)) {
        for (const row of rows) {
            if (row.event === 'POST /cli/sessions') stamp(row.args[0].body.metadata)
            if (row.event === 'POST /session-started') stamp(row.args[0].metadata)
            if (row.event === 'update-metadata') {
                for (const metadata of [row.args[0].metadata, row.ack?.[0]?.metadata]) {
                    stamp(metadata)
                    if (metadata?.archiveReason === 'Hub restart' && allowed.has('killSession.metadata.archiveReason')) metadata.archiveReason = 'User terminated'
                }
            }
            if (row.event === 'rpc-ack' && row.args[0]?.success === true && row.args[0]?.message === 'Killing hapi CLI process'
                && allowed.has('killSession.ack.machineId')) row.args[0].machineId = 'parity-machine'
        }
    }
    return expected
}

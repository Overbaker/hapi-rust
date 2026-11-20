import { createHash } from 'node:crypto'
import { describe, expect, it, spyOn, afterAll } from 'bun:test'
import { readFileSync, writeFileSync } from 'node:fs'
import { AppServerEventConverter } from '../../../../../cli/src/codex/utils/appServerEventConverter'
import { logger } from '../../../../../cli/src/ui/logger'

if (process.env.CODEX_EVENT_VECTORS) {
    const vectors: unknown[][] = []
    class RecordingConverter extends AppServerEventConverter {
        private readonly sequence: unknown[] = []
        private depth = 0
        constructor() {
            super()
            vectors.push(this.sequence)
        }
        override handleNotification(method: string, params: unknown) {
            this.depth++
            try {
                const result = super.handleNotification(method, params)
                if (this.depth === 1)
                    this.sequence.push({
                        method,
                        params,
                        result,
                        hashes: result.map((event) =>
                            createHash('sha256').update(JSON.stringify(event)).digest('hex')
                        )
                    })
                return result
            } finally {
                this.depth--
            }
        }
        override reset() {
            this.sequence.push({ reset: true })
            super.reset()
        }
    }
    const path = '../../cli/src/codex/utils/appServerEventConverter.test.ts'
    const suite = readFileSync(path, 'utf8')
    const body = new Bun.Transpiler({ loader: 'ts' }).transformSync(
        suite.slice(suite.indexOf("describe('AppServerEventConverter'"))
    )
    new Function('describe', 'it', 'expect', 'vi', 'logger', 'AppServerEventConverter', body)(
        describe,
        it,
        expect,
        { spyOn },
        logger,
        RecordingConverter
    )
    it('records null fallbacks and typed optional fields', () => {
        const c = new RecordingConverter()
        for (const [method, p] of [
            [
                'thread/status/changed',
                { status: null, thread: { id: 't', status: { type: 'systemError', message: 'boom' } } }
            ],
            [
                'turn/completed',
                {
                    status: 'failed',
                    error: null,
                    turn: { id: 'T', error: { message: 'limit', codexErrorInfo: 'usageLimitExceeded' } }
                }
            ],
            ['thread/tokenUsage/updated', { tokenUsage: null, token_usage: { total: { outputTokens: 42 } } }],
            ['thread/goal/updated', { threadId: 't', turnId: '', goal: { objective: 'ship' } }],
            ['item/started', { item: { id: 'p', type: 'fileChange', diff: 'diff', autoApproved: 7 } }],
            ['item/started', { item: { id: 'p2', type: 'fileChange', changes: [{ path: '/a' }] } }],
            [
                'item/started',
                {
                    item: {
                        id: 'c',
                        type: 'commandExecution',
                        commandActions: 42,
                        command_actions: [],
                        source: false,
                        startedAtMs: '0'
                    }
                }
            ],
            [
                'item/completed',
                { item: { id: 'c', type: 'commandExecution', stderr: '', error: '', exitCode: '0' } }
            ],
            ...['', false, 0, null].map((error) => [
                'item/completed',
                { item: { id: 'm', type: 'mcpToolCall', error, result: { Ok: {} } } }
            ])
        ] as const)
            c.handleNotification(method as string, p)
    })
    afterAll(() => writeFileSync(process.env.CODEX_EVENT_VECTORS!, JSON.stringify(vectors)))
}

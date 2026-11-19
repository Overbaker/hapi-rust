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
    afterAll(() => writeFileSync(process.env.CODEX_EVENT_VECTORS!, JSON.stringify(vectors)))
}

import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import { describe, expect, it } from 'vitest'
import {
    createHapiWasmEngineFromBytes,
    fallbackComputeDiff,
    HapiWasmWorkerClient,
} from './hapiWasmClient'

describe('hapi-wasm WebAssembly engine', () => {
    const wasmPath = resolve(process.cwd(), 'public/wasm/hapi_wasm.wasm')

    it('renders incremental Markdown, computes unified diff, and folds message windows in WASM', async () => {
        const bytes = readFileSync(wasmPath)
        const engine = await createHapiWasmEngineFromBytes(bytes)

        engine.resetStreamMarkdown()
        const chunk1 = engine.renderStreamMarkdown('# Hello\n\nFirst block.\n\nPartial')
        expect(chunk1.committed_prefix_bytes).toBeGreaterThan(0)
        expect(chunk1.html).toContain('<h1>Hello</h1>')

        const chunk2 = engine.renderStreamMarkdown('# Hello\n\nFirst block.\n\nPartial completed')
        expect(chunk2.cache_reused).toBe(true)
        expect(chunk2.html).toContain('Partial completed')

        const diff = engine.computeDiff('line1\nline2\nline3\n', 'line1\nline2-edit\nline3\nline4\n', 2)
        expect(diff.addedLines).toBe(2)
        expect(diff.removedLines).toBe(1)
        expect(diff.unified).toContain('+line2-edit')

        const folded = engine.foldWindow(
            [
                {
                    id: 'r1',
                    seq: 1,
                    createdAt: 100,
                    content: { role: 'agent', content: { type: 'codex', data: { type: 'reasoning', id: 's1', message: 'a' } } },
                },
            ],
            [
                {
                    id: 'r2',
                    seq: 2,
                    createdAt: 200,
                    content: { role: 'agent', content: { type: 'codex', data: { type: 'reasoning', id: 's1', message: 'ab' } } },
                },
            ],
            50
        )
        expect(folded).toHaveLength(1)
        expect((folded[0] as { id: string }).id).toBe('r2')
    })

    it('falls back cleanly when Web Worker is unavailable', async () => {
        const client = new HapiWasmWorkerClient()
        const res = await client.computeDiff('a\nb\n', 'a\nc\n')
        expect(res.addedLines).toBe(1)
        expect(res.removedLines).toBe(1)
        expect(fallbackComputeDiff('x', 'x').hunkCount).toBe(0)
        client.dispose()
    })
})

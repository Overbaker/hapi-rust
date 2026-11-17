export interface IncrementalMarkdownResult {
    committed_prefix_bytes: number
    html: string
    cache_reused: boolean
}

export interface UnifiedDiffResult {
    unified: string
    addedLines: number
    removedLines: number
    hunkCount: number
}

interface HapiWasmExports extends WebAssembly.Exports {
    memory: WebAssembly.Memory
    hapi_alloc(len: number): number
    hapi_dealloc(ptr: number, len: number): void
    hapi_result_ptr(): number
    hapi_render_markdown(ptr: number, len: number): number
    hapi_reset_stream_markdown(): void
    hapi_render_stream_markdown(ptr: number, len: number): number
    hapi_compute_diff(
        oldPtr: number,
        oldLen: number,
        newPtr: number,
        newLen: number,
        contextLines: number
    ): number
    hapi_fold_window(
        existingPtr: number,
        existingLen: number,
        incomingPtr: number,
        incomingLen: number,
        maxWindow: number
    ): number
}

const textEncoder = new TextEncoder()
const textDecoder = new TextDecoder()

export interface HapiWasmEngine {
    renderMarkdown(markdown: string): string
    resetStreamMarkdown(): void
    renderStreamMarkdown(markdown: string): IncrementalMarkdownResult
    computeDiff(oldText: string, newText: string, contextLines?: number): UnifiedDiffResult
    foldWindow<T>(existing: T[], incoming: T[], maxWindow?: number): T[]
}

export async function createHapiWasmEngineFromBytes(
    wasmBytes: BufferSource
): Promise<HapiWasmEngine> {
    const instantiated = await WebAssembly.instantiate(wasmBytes, {})
    const exports = instantiated.instance.exports as unknown as HapiWasmExports

    const withInput = <R>(input: string, fn: (ptr: number, len: number) => R): R => {
        const bytes = textEncoder.encode(input)
        const ptr = exports.hapi_alloc(bytes.length)
        try {
            if (bytes.length > 0) {
                new Uint8Array(exports.memory.buffer, ptr, bytes.length).set(bytes)
            }
            return fn(ptr, bytes.length)
        } finally {
            exports.hapi_dealloc(ptr, bytes.length)
        }
    }

    const withTwoInputs = <R>(
        first: string,
        second: string,
        fn: (p1: number, l1: number, p2: number, l2: number) => R
    ): R => {
        const b1 = textEncoder.encode(first)
        const b2 = textEncoder.encode(second)
        const p1 = exports.hapi_alloc(b1.length)
        const p2 = exports.hapi_alloc(b2.length)
        try {
            if (b1.length > 0) new Uint8Array(exports.memory.buffer, p1, b1.length).set(b1)
            if (b2.length > 0) new Uint8Array(exports.memory.buffer, p2, b2.length).set(b2)
            return fn(p1, b1.length, p2, b2.length)
        } finally {
            exports.hapi_dealloc(p1, b1.length)
            exports.hapi_dealloc(p2, b2.length)
        }
    }

    const readResultString = (len: number): string => {
        if (len <= 0) return ''
        const ptr = exports.hapi_result_ptr()
        const view = new Uint8Array(exports.memory.buffer, ptr, len)
        return textDecoder.decode(view)
    }

    return {
        renderMarkdown(markdown: string): string {
            const outLen = withInput(markdown, (ptr, len) => exports.hapi_render_markdown(ptr, len))
            return readResultString(outLen)
        },
        resetStreamMarkdown(): void {
            exports.hapi_reset_stream_markdown()
        },
        renderStreamMarkdown(markdown: string): IncrementalMarkdownResult {
            const outLen = withInput(markdown, (ptr, len) => exports.hapi_render_stream_markdown(ptr, len))
            return JSON.parse(readResultString(outLen)) as IncrementalMarkdownResult
        },
        computeDiff(oldText: string, newText: string, contextLines = 3): UnifiedDiffResult {
            const outLen = withTwoInputs(oldText, newText, (p1, l1, p2, l2) =>
                exports.hapi_compute_diff(p1, l1, p2, l2, contextLines)
            )
            return JSON.parse(readResultString(outLen)) as UnifiedDiffResult
        },
        foldWindow<T>(existing: T[], incoming: T[], maxWindow = 200): T[] {
            const outLen = withTwoInputs(
                JSON.stringify(existing),
                JSON.stringify(incoming),
                (p1, l1, p2, l2) => exports.hapi_fold_window(p1, l1, p2, l2, maxWindow)
            )
            if (outLen === 0) return existing
            return JSON.parse(readResultString(outLen)) as T[]
        },
    }
}

export function fallbackComputeDiff(oldText: string, newText: string): UnifiedDiffResult {
    const oldLines = oldText.split('\n')
    const newLines = newText.split('\n')
    let added = 0
    let removed = 0
    const lines: string[] = []
    const maxLen = Math.max(oldLines.length, newLines.length)
    for (let i = 0; i < maxLen; i += 1) {
        const a = oldLines[i]
        const b = newLines[i]
        if (a === b) {
            if (a !== undefined) lines.push(` ${a}`)
        } else {
            if (a !== undefined) {
                removed += 1
                lines.push(`-${a}`)
            }
            if (b !== undefined) {
                added += 1
                lines.push(`+${b}`)
            }
        }
    }
    return {
        unified: `--- a/file\n+++ b/file\n@@ -1,${oldLines.length} +1,${newLines.length} @@\n${lines.join('\n')}\n`,
        addedLines: added,
        removedLines: removed,
        hunkCount: added + removed > 0 ? 1 : 0,
    }
}

export class HapiWasmWorkerClient {
    private worker: Worker | null = null
    private nextId = 1
    private pending = new Map<number, { resolve: (value: unknown) => void; reject: (err: unknown) => void }>()

    private ensureWorker(): Worker | null {
        if (typeof Worker === 'undefined') return null
        if (this.worker) return this.worker
        const worker = new Worker(new URL('./hapiWasm.worker.ts', import.meta.url), { type: 'module' })
        worker.onmessage = (event: MessageEvent<{ id: number; ok: boolean; result?: unknown; error?: string }>) => {
            const entry = this.pending.get(event.data.id)
            if (!entry) return
            this.pending.delete(event.data.id)
            if (event.data.ok) {
                entry.resolve(event.data.result)
            } else {
                entry.reject(new Error(event.data.error ?? 'WASM worker error'))
            }
        }
        worker.onerror = (error) => {
            for (const entry of this.pending.values()) {
                entry.reject(error)
            }
            this.pending.clear()
        }
        this.worker = worker
        return worker
    }

    private callWorker<T>(method: string, args: unknown[]): Promise<T> {
        const worker = this.ensureWorker()
        if (!worker) {
            return Promise.reject(new Error('Web Worker unavailable'))
        }
        const id = this.nextId++
        return new Promise<T>((resolve, reject) => {
            this.pending.set(id, {
                resolve: resolve as (value: unknown) => void,
                reject,
            })
            worker.postMessage({ id, method, args })
        })
    }

    async computeDiff(oldText: string, newText: string, contextLines = 3): Promise<UnifiedDiffResult> {
        try {
            return await this.callWorker<UnifiedDiffResult>('computeDiff', [oldText, newText, contextLines])
        } catch {
            return fallbackComputeDiff(oldText, newText)
        }
    }

    async renderStreamMarkdown(markdown: string): Promise<IncrementalMarkdownResult | null> {
        try {
            return await this.callWorker<IncrementalMarkdownResult>('renderStreamMarkdown', [markdown])
        } catch {
            return null
        }
    }

    dispose(): void {
        this.worker?.terminate()
        this.worker = null
        this.pending.clear()
    }
}

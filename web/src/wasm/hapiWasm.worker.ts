import { createHapiWasmEngineFromBytes, type HapiWasmEngine } from './hapiWasmClient'

let enginePromise: Promise<HapiWasmEngine> | null = null

async function getEngine(): Promise<HapiWasmEngine> {
    if (!enginePromise) {
        enginePromise = fetch('/wasm/hapi_wasm.wasm')
            .then((res) => {
                if (!res.ok) throw new Error(`Failed to fetch hapi_wasm.wasm: ${res.status}`)
                return res.arrayBuffer()
            })
            .then((bytes) => createHapiWasmEngineFromBytes(bytes))
    }
    return enginePromise
}

self.onmessage = async (event: MessageEvent<{ id: number; method: string; args: unknown[] }>) => {
    const { id, method, args } = event.data
    try {
        const engine = await getEngine()
        let result: unknown
        switch (method) {
            case 'renderMarkdown':
                result = engine.renderMarkdown(String(args[0] ?? ''))
                break
            case 'renderStreamMarkdown':
                result = engine.renderStreamMarkdown(String(args[0] ?? ''))
                break
            case 'resetStreamMarkdown':
                engine.resetStreamMarkdown()
                result = true
                break
            case 'computeDiff':
                result = engine.computeDiff(
                    String(args[0] ?? ''),
                    String(args[1] ?? ''),
                    typeof args[2] === 'number' ? args[2] : 3
                )
                break
            case 'foldWindow':
                result = engine.foldWindow(
                    Array.isArray(args[0]) ? args[0] : [],
                    Array.isArray(args[1]) ? args[1] : [],
                    typeof args[2] === 'number' ? args[2] : 200
                )
                break
            default:
                throw new Error(`Unknown WASM method: ${method}`)
        }
        self.postMessage({ id, ok: true, result })
    } catch (error) {
        self.postMessage({
            id,
            ok: false,
            error: error instanceof Error ? error.message : String(error),
        })
    }
}

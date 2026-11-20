import { accessSync, constants, statSync } from 'node:fs'
import { resolve } from 'node:path'

export type ParityRuntime = { ts: boolean; rust?: string }

/**
 * PARITY_RUNTIME selects the legs: `ts` (default without a Rust binary), `rust`
 * or `both` (default when HAPI_PARITY_RUST_BIN is set). Relative binary paths
 * resolve from the repository root.
 */
export function parityRuntime(env: NodeJS.ProcessEnv, repo: string): ParityRuntime {
    const mode = env.PARITY_RUNTIME || (env.HAPI_PARITY_RUST_BIN ? 'both' : 'ts')
    if (!['ts', 'rust', 'both'].includes(mode)) throw Error('Invalid PARITY_RUNTIME: ' + mode)
    if (mode === 'ts') return { ts: true }
    if (!env.HAPI_PARITY_RUST_BIN) throw Error('Rust parity requires HAPI_PARITY_RUST_BIN')
    const rust = resolve(repo, env.HAPI_PARITY_RUST_BIN)
    accessSync(rust, constants.X_OK)
    if (!statSync(rust).isFile()) throw Error('Expected a Rust wrapper executable at ' + rust)
    return { ts: mode === 'both', rust }
}

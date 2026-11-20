import { accessSync, constants, statSync } from 'node:fs'
import { resolve } from 'node:path'

export function parityRuntime(env: NodeJS.ProcessEnv, repo: string) {
    const mode = env.PARITY_RUNTIME ?? (env.HAPI_PARITY_RUST_BIN ? 'both' : 'ts')
    if (!['ts', 'rust', 'both'].includes(mode)) throw Error('Invalid PARITY_RUNTIME')
    let binary: string | undefined
    if (mode !== 'ts') {
        if (!env.HAPI_PARITY_RUST_BIN) throw Error('Rust parity requires HAPI_PARITY_RUST_BIN')
        binary = resolve(repo, env.HAPI_PARITY_RUST_BIN)
        accessSync(binary, constants.X_OK)
        if (!statSync(binary).isFile()) throw Error('Expected a Rust wrapper executable at ' + binary)
    }
    return {
        ts: mode !== 'rust', binary,
        rustLeg(pending: boolean) {
            if (!binary) return !env.PARITY_RUNTIME && pending ? 'pending' : 'off'
            return pending && env.PARITY_RUN_PENDING !== '1' ? 'pending' : 'run'
        }
    }
}

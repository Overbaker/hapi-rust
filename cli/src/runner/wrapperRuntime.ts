import { accessSync, constants, realpathSync, statSync } from 'node:fs'
import { dirname, isAbsolute, join } from 'node:path'
import { isBunCompiled, projectPath } from '@/projectPath'
import { resolveHappyCliExecutable } from '@/utils/spawnHappyCLI'

export type WrapperRuntime = { runtime: 'rust'; bin: string } | { runtime: 'ts'; reason: string }

const binaryName = 'hapi-session-wrapper'

function isExecutable(path: string): boolean {
    try {
        accessSync(path, constants.X_OK)
        return statSync(path).isFile()
    } catch {
        return false
    }
}

export function resolveRustExecutable(env = process.env): string | null {
    const override = env.HAPI_RUST_WRAPPER_BIN?.trim()
    if (override && isAbsolute(override) && isExecutable(override)) return override
    try {
        const sibling = join(dirname(realpathSync(resolveHappyCliExecutable())), binaryName)
        if (isExecutable(sibling)) return sibling
    } catch {}
    if (!isBunCompiled()) {
        const release = join(projectPath(), '..', 'target', 'release', binaryName)
        if (isExecutable(release)) return release
    }
    return null
}

export function resolveWrapperRuntime(
    agent: string, env = process.env, platform = process.platform, startingMode?: string,
): WrapperRuntime {
    const flavors = env.HAPI_RUST_WRAPPER_FLAVORS?.toLowerCase().split(',').map(value => value.trim())
    if (!flavors?.includes(agent) || flavors.includes('none')) return { runtime: 'ts', reason: 'flavor disabled' }
    if (platform !== 'linux') return { runtime: 'ts', reason: 'unsupported platform' }
    if (startingMode === 'local') return { runtime: 'ts', reason: 'local mode' }
    if (!['claude', 'codex', 'agy'].includes(agent)) return { runtime: 'ts', reason: 'unsupported flavor' }
    const bin = resolveRustExecutable(env)
    return bin ? { runtime: 'rust', bin } : { runtime: 'ts', reason: 'binary unavailable' }
}

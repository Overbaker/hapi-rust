import { accessSync, constants, realpathSync, statSync } from 'node:fs'
import { basename, dirname, isAbsolute, join } from 'node:path'
import { isBunCompiled, projectPath } from '@/projectPath'
import { resolveHappyCliExecutable } from '@/utils/spawnHappyCLI'

export type WrapperRuntime = { runtime: 'rust'; bin: string } | { runtime: 'ts'; reason: string }

/**
 * Agent flavors whose Rust session wrapper exists in this tree. They use the
 * Rust wrapper by default; HAPI_RUST_WRAPPER_FLAVORS narrows or disables it.
 */
export const SUPPORTED_RUST_WRAPPER_FLAVORS: readonly string[] = ['claude', 'codex']

export const SUPPORTED_RUST_WRAPPER_FLAVORS_WIN32: readonly string[] = ['codex']

function isExecutable(path: string): boolean {
    try {
        accessSync(path, constants.X_OK)
        return statSync(path).isFile()
    } catch {
        return false
    }
}

export function resolveRustExecutable(env: NodeJS.ProcessEnv = process.env, platform: NodeJS.Platform = process.platform): string | null {
    const binaryName = platform === 'win32' ? 'hapi-session-wrapper.exe' : 'hapi-session-wrapper'
    const override = env.HAPI_RUST_WRAPPER_BIN?.trim()
    if (override && isAbsolute(override) && (platform === 'win32' ? basename(override).toLowerCase() : basename(override)) === binaryName && isExecutable(override)) {
        return override
    }
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
    agent: string,
    env: NodeJS.ProcessEnv = process.env,
    platform: NodeJS.Platform = process.platform,
    startingMode?: string,
    supportedFlavors: readonly string[] = SUPPORTED_RUST_WRAPPER_FLAVORS,
): WrapperRuntime {
    const normalizedAgent = agent.trim().toLowerCase()
    const flavors = env.HAPI_RUST_WRAPPER_FLAVORS
        ?.toLowerCase()
        .split(',')
        .map(value => value.trim())
        .filter(Boolean)
    if (!normalizedAgent || (flavors?.length && (flavors.includes('none') || !flavors.includes(normalizedAgent)))) {
        return { runtime: 'ts', reason: 'flavor disabled' }
    }
    if (platform !== 'linux' && !(platform === 'win32' && SUPPORTED_RUST_WRAPPER_FLAVORS_WIN32.includes(normalizedAgent))) return { runtime: 'ts', reason: 'unsupported platform' }
    if (startingMode === 'local') return { runtime: 'ts', reason: 'local mode' }
    if (!supportedFlavors.includes(normalizedAgent)) return { runtime: 'ts', reason: 'unsupported flavor' }
    const bin = resolveRustExecutable(env, platform)
    return bin ? { runtime: 'rust', bin } : { runtime: 'ts', reason: 'binary unavailable' }
}

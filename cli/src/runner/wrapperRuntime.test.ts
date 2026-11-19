import { chmodSync, mkdirSync, mkdtempSync, rmSync, symlinkSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { afterEach, beforeEach, expect, it, vi } from 'vitest'
import { resolveWrapperRuntime } from './wrapperRuntime'

let root: string
vi.mock('@/projectPath', () => ({ projectPath: () => join(root, 'cli'), isBunCompiled: () => false }))
vi.mock('@/utils/spawnHappyCLI', () => ({ resolveHappyCliExecutable: () => join(root, 'link') }))
beforeEach(() => { root = mkdtempSync(join(tmpdir(), 'wrapper-runtime-')) })
afterEach(() => { rmSync(root, { recursive: true, force: true }) })

it('checks configuration on each spawn', () => {
    const env = { HAPI_RUST_WRAPPER_BIN: join(root, 'hapi-session-wrapper'), HAPI_RUST_WRAPPER_FLAVORS: ' Claude , CODEX, agy,pi ' }
    symlinkSync(process.execPath, env.HAPI_RUST_WRAPPER_BIN)
    for (const flavor of ['claude', 'codex', 'agy']) {
        expect(resolveWrapperRuntime(flavor, env, 'linux')).toEqual({ runtime: 'rust', bin: env.HAPI_RUST_WRAPPER_BIN })
    }
    expect(resolveWrapperRuntime('pi', env, 'linux').runtime).toBe('ts')
    expect(resolveWrapperRuntime('codex', env, 'darwin').runtime).toBe('ts')
    expect(resolveWrapperRuntime('codex', env, 'linux', 'local').runtime).toBe('ts')
    for (const flavors of ['', 'none', 'agy']) {
        env.HAPI_RUST_WRAPPER_FLAVORS = flavors
        expect(resolveWrapperRuntime('claude', env, 'linux').runtime).toBe('ts')
    }
    expect(resolveWrapperRuntime('claude', {}, 'linux').runtime).toBe('ts')
})

it('resolves executable files in override, real CLI sibling, then dev release order', () => {
    const bin = 'hapi-session-wrapper'
    const release = join(root, 'target', 'release', bin)
    const sibling = join(root, 'bin', bin)
    mkdirSync(join(root, 'target', 'release'), { recursive: true })
    mkdirSync(join(root, 'bin'))
    writeFileSync(join(root, 'bin', 'hapi'), '')
    symlinkSync(join(root, 'bin', 'hapi'), join(root, 'link'))
    const env = { HAPI_RUST_WRAPPER_FLAVORS: 'claude', HAPI_RUST_WRAPPER_BIN: root }
    const select = () => resolveWrapperRuntime('claude', env, 'linux')
    expect(select().runtime).toBe('ts')
    writeFileSync(release, '', { mode: 0o755 })
    writeFileSync(sibling, '', { mode: 0o644 })
    expect(select()).toEqual({ runtime: 'rust', bin: release })
    chmodSync(sibling, 0o755)
    expect(select()).toEqual({ runtime: 'rust', bin: sibling })
    env.HAPI_RUST_WRAPPER_BIN = process.execPath
    expect(select()).toEqual({ runtime: 'rust', bin: sibling })
    env.HAPI_RUST_WRAPPER_BIN = join(root, bin)
    symlinkSync(process.execPath, env.HAPI_RUST_WRAPPER_BIN)
    expect(select()).toEqual({ runtime: 'rust', bin: env.HAPI_RUST_WRAPPER_BIN })
})

/**
 * Runner integration tests for Rust session wrapper selection.
 *
 * A fake executable named `hapi-session-wrapper` stands in for the Rust
 * wrapper. The runner is started with a Bun preload that adds `claude` to
 * SUPPORTED_RUST_WRAPPER_FLAVORS, which is empty on this tree.
 * Run through the serial integration project (vitest.integration.config.ts).
 */
import { afterAll, afterEach, beforeEach, describe, expect, it } from 'vitest'
import { spawn } from 'node:child_process'
import { randomUUID } from 'node:crypto'
import { existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs'
import { join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { configuration } from '@/configuration'
import { readRunnerState } from '@/persistence'
import { listRunnerSessions, spawnRunnerSession, stopRunner, stopRunnerSession } from '@/runner/controlClient'
import { getHappyCliCommand } from '@/utils/spawnHappyCLI'
import { isProcessAlive } from '@/utils/process'
import { buildTestChildEnv, testOwnedMarker } from '@/test/integrationEnv'
import { cleanupAllRegisteredProcesses, trackChildProcess, trackRunnerPid, trackSession } from '@/test/processRegistry'
import { reapTestOwnedProcesses } from '@/test/auditTestProcesses'

const cli = fileURLToPath(new URL('../../', import.meta.url))
const releaseBinary = resolve(cli, '..', 'target', 'release', 'hapi-session-wrapper')

async function waitFor(condition: () => Promise<boolean>, timeout = 10_000, interval = 100): Promise<void> {
    const start = Date.now()
    while (Date.now() - start < timeout) {
        if (await condition()) return
        await new Promise(r => setTimeout(r, interval))
    }
    throw new Error('Timeout waiting for condition')
}

async function isServerHealthy(): Promise<boolean> {
    if (!configuration.cliApiToken) return false
    try {
        const response = await fetch(`${configuration.apiUrl}/cli/machines/__healthcheck__`, {
            headers: { Authorization: `Bearer ${configuration.cliApiToken}` },
            signal: AbortSignal.timeout(1000),
        })
        return response.status !== 401 && response.status !== 503
    } catch {
        return false
    }
}

type Mode = 'exit-86' | 'exit-1' | 'webhook-then-86' | 'exit-86-on-stop'

describe.skipIf(process.platform !== 'linux' || !await isServerHealthy())('Rust wrapper runner switch', { timeout: 60_000 }, () => {
    let dir: string
    let modeFile: string
    let countFile: string
    let grandchildFile: string
    let wrapper: string
    let runnerLog: string

    function writeFixtures(): void {
        dir = join(configuration.happyHomeDir, `rust-wrapper-${randomUUID()}`)
        mkdirSync(dir)
        modeFile = join(dir, 'mode')
        countFile = join(dir, 'count')
        grandchildFile = join(dir, 'grandchild')
        wrapper = join(dir, 'hapi-session-wrapper')
        const webhook = join(dir, 'webhook.mjs')
        writeFileSync(join(dir, 'preload.ts'), [
            `import { SUPPORTED_RUST_WRAPPER_FLAVORS } from ${JSON.stringify(join(cli, 'src/runner/wrapperRuntime.ts'))}`,
            `;(SUPPORTED_RUST_WRAPPER_FLAVORS as string[]).push('claude')`,
            '',
        ].join('\n'))
        writeFileSync(webhook, `
import { readFileSync } from 'node:fs'
const state = JSON.parse(readFileSync(${JSON.stringify(configuration.runnerStateFile)}, 'utf8'))
const sessionId = process.argv[2]
const metadata = { path: process.cwd(), host: 'fake', homeDir: '/tmp', happyHomeDir: '/tmp', happyLibDir: '/tmp',
    happyToolsDir: '/tmp', hostPid: process.pid, startedBy: 'runner', wrapperRuntime: 'rust' }
await fetch('http://127.0.0.1:' + state.httpPort + '/session-started', {
    method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ sessionId, metadata }) })
process.exit(86)
`)
        const q = (value: string) => `'${value.replace(/'/g, `'\\''`)}'`
        writeFileSync(wrapper, `#!/bin/sh
echo "$$" >> ${q(countFile)}
case "$(cat ${q(modeFile)})" in
  exit-86) sleep 30 & echo "$!" > ${q(grandchildFile)}; exit 86 ;;
  exit-1) exit 1 ;;
  exit-86-on-stop) trap 'exit 86' TERM; while :; do sleep 0.1; done ;;
  webhook-then-86) exec ${q(process.execPath)} ${q(webhook)} ${q(randomUUID())} ;;
esac
exit 2
`, { mode: 0o755 })
    }

    async function startRunner(env: NodeJS.ProcessEnv): Promise<void> {
        const { command, args } = getHappyCliCommand(['runner', 'start-sync'])
        const child = spawn(command, ['--preload', join(dir, 'preload.ts'), ...args], {
            detached: true,
            stdio: 'ignore',
            env: {
                ...buildTestChildEnv(),
                HAPI_DISABLE_VERSION_HANDOFF: '1',
                HAPI_RUST_WRAPPER_FLAVORS: 'claude',
                ...env,
            },
        })
        trackChildProcess(child, 'rust-wrapper-runner')
        child.unref()
        await waitFor(async () => {
            const state = await readRunnerState()
            return state !== null && state.pid === child.pid && isProcessAlive(state.pid)
        })
        const state = (await readRunnerState())!
        trackRunnerPid(state.pid, 'rust-wrapper-runner')
        runnerLog = state.runnerLogPath!
        const machineId = state.startedWithMachineId
        await waitFor(async () => {
            const response = await fetch(`${configuration.apiUrl}/cli/machines/${machineId}`, {
                headers: { Authorization: `Bearer ${configuration.cliApiToken}` },
                signal: AbortSignal.timeout(1000),
            })
            if (!response.ok) return false
            const { machine } = await response.json() as { machine?: { active: boolean; runnerState?: { status: string; pid?: number } | null } }
            return machine?.active === true && machine.runnerState?.status === 'running' && machine.runnerState.pid === state.pid
        })
    }

    async function spawnWith(mode: Mode, env: NodeJS.ProcessEnv = { HAPI_RUST_WRAPPER_BIN: wrapper }, sessionId?: string) {
        writeFileSync(modeFile, mode)
        await startRunner(env)
        const response = await spawnRunnerSession('/tmp', sessionId)
        if (response?.sessionId) trackSession(response.sessionId, `rust-wrapper-session:${response.sessionId}`)
        return response
    }

    const rustSpawns = () => existsSync(countFile) ? readFileSync(countFile, 'utf8').trim().split('\n').filter(Boolean).length : 0
    const runnerLogText = () => readFileSync(runnerLog, 'utf8')
    const tsSpawns = () => runnerLogText().split('\n').filter(line => line.includes('[SPAWN HAPI CLI] Spawning: hapi claude')).length

    beforeEach(async () => {
        await stopRunner()
        writeFixtures()
    })

    afterEach(async () => {
        await cleanupAllRegisteredProcesses()
        await Promise.race([stopRunner(), new Promise(r => setTimeout(r, 10_000))])
        expect(await reapTestOwnedProcesses(testOwnedMarker()), 'test-owned processes survived').toEqual([])
    })

    afterAll(async () => {
        await cleanupAllRegisteredProcesses()
        await Promise.race([stopRunner(), new Promise(r => setTimeout(r, 10_000))])
        await reapTestOwnedProcesses(testOwnedMarker())
    })

    it('retries once with TypeScript after exit 86 before the webhook, once the Rust tree is gone', async () => {
        const response = await spawnWith('exit-86')
        expect(response).toHaveProperty('success', true)
        expect(rustSpawns()).toBe(1)
        expect(tsSpawns()).toBe(1)
        const grandchild = Number(readFileSync(grandchildFile, 'utf8').trim())
        expect(isProcessAlive(grandchild)).toBe(false)
        const log = runnerLogText()
        expect(log.indexOf('Retrying with TS')).toBeGreaterThan(-1)
        expect(log.indexOf('Retrying with TS')).toBeLessThan(log.indexOf('[SPAWN HAPI CLI] Spawning: hapi claude'))
        const tracked = (await listRunnerSessions()).find((s: { happySessionId: string }) => s.happySessionId === response.sessionId)
        expect(tracked?.startedBy).toBe('runner')
        expect(await stopRunnerSession(response.sessionId)).toBe('stopped')
    })

    it('does not retry after any other exit code', async () => {
        const response = await spawnWith('exit-1')
        expect(response).not.toHaveProperty('success', true)
        expect(runnerLogText()).toMatch(/Child PID \d+ exited with code 1,/)
        expect(rustSpawns()).toBe(1)
        expect(tsSpawns()).toBe(0)
        expect(runnerLogText()).not.toContain('Retrying with TS')
    })

    it('does not retry an exit 86 that comes after the session-started webhook', async () => {
        const response = await spawnWith('webhook-then-86')
        expect(response).toHaveProperty('success', true)
        await waitFor(async () => !(await listRunnerSessions()).some((s: { happySessionId: string }) => s.happySessionId === response.sessionId))
        expect(rustSpawns()).toBe(1)
        expect(tsSpawns()).toBe(0)
        expect(runnerLogText()).not.toContain('Retrying with TS')
    })

    it('does not retry when the session is stopped during Rust startup', async () => {
        const sessionId = randomUUID()
        writeFileSync(modeFile, 'exit-86-on-stop')
        await startRunner({ HAPI_RUST_WRAPPER_BIN: wrapper })
        const pending = spawnRunnerSession('/tmp', sessionId)
        await waitFor(async () => rustSpawns() === 1)
        expect(await stopRunnerSession(sessionId)).toBe('stopped')
        expect(await pending).not.toHaveProperty('success', true)
        expect(rustSpawns()).toBe(1)
        expect(tsSpawns()).toBe(0)
        expect(runnerLogText()).not.toContain('Retrying with TS')
    })

    it.skipIf(existsSync(releaseBinary))('starts TypeScript directly when the Rust binary is missing', async () => {
        const response = await spawnWith('exit-86', { HAPI_RUST_WRAPPER_BIN: join(dir, 'missing', 'hapi-session-wrapper') })
        expect(response).toHaveProperty('success', true)
        expect(rustSpawns()).toBe(0)
        expect(tsSpawns()).toBe(1)
        expect(runnerLogText()).toContain('binary unavailable')
        const records = JSON.parse(readFileSync(`${configuration.runnerStateFile}.resume-processes.json`, 'utf8')) as { confirmedSessionId?: string; wrapperRuntime?: string }[]
        expect(records.find(record => record.confirmedSessionId === response.sessionId)?.wrapperRuntime).toBe('ts')
        expect(await stopRunnerSession(response.sessionId)).toBe('stopped')
    })
})

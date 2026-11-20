import { spawn, execFileSync } from 'node:child_process'
import { mkdirSync, mkdtempSync, readFileSync, rmSync, symlinkSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import pkg from '../../package.json'
import { fakeHub } from './fakeHub'
import { describe, expect, it } from 'vitest'
import { agyCorpus, agyNames, repo, runAgy, testTimeout } from './agyRun'
import { expectedAgyRustTranscript } from './rustKnownGaps'
import { parityRuntime } from './runtime'

const runtime = parityRuntime(process.env, repo)

describe.each(agyNames)('agy/%s', name => {
    const golden = JSON.parse(readFileSync(join(agyCorpus, name, 'golden.ts.json'), 'utf8'))
    if (runtime.ts) it('TS wire transcript and web decoding match the golden', async () => {
        const actual = await runAgy(name)
        if (process.env.PARITY_DUMP_DIR) writeFileSync(join(process.env.PARITY_DUMP_DIR, name + '.ts.json'), JSON.stringify(actual, null, 2) + '\n')
        expect(actual).toEqual(golden)
    }, testTimeout)
    if (runtime.rust) it('Rust wire transcript and web decoding match the named contract', async () => {
        const actual = await runAgy(name, runtime.rust!, { rust: true })
        if (process.env.PARITY_DUMP_DIR) writeFileSync(join(process.env.PARITY_DUMP_DIR, name + '.rust.json'), JSON.stringify(actual, null, 2) + '\n')
        expect(actual).toEqual(process.env.HAPI_PARITY_RAW === '1' ? golden : expectedAgyRustTranscript(name, golden))
    }, testTimeout)
})

// Startup failures must exit 86 before the hub sees the session so the runner retries with TypeScript.
describe.runIf(!!runtime.rust)('agy/startup-fallback', () => {
    const bun = execFileSync('sh', ['-c', 'command -v "${HAPI_BUN_EXEC:-bun}"'], { encoding: 'utf8' }).trim()
    it.each([
        ['agy is missing', false, {}, /No such file|not found|os error 2/],
        ['agy predates agy acp', true, { PARITY_AGY_VERSION: '1.4.12' }, /agy 1\.4\.12 predates agy acp \(1\.5\.0\)/],
        ['agy acp fails initialize', true, { PARITY_AGY_INITIALIZE_ERROR: 'login required' }, /Failed to start agy acp.*login required/],
    ] as const)('exits 86 before the webhook when %s', async (_, installed, extra, stderr) => {
        const out = mkdtempSync(join(tmpdir(), 'hapi-parity-'))
        const hub = await fakeHub(out)
        try {
            const bin = join(out, 'bin')
            mkdirSync(bin)
            mkdirSync(join(out, 'hapi'))
            symlinkSync(bun, join(bin, 'bun'))
            if (installed) symlinkSync(join(repo, 'cli/src/parity/fakes/agy.ts'), join(bin, 'agy'))
            writeFileSync(join(out, 'agent.jsonl'), '')
            const child = spawn(runtime.rust!, ['agy', '--started-by', 'runner', '--hapi-session-id', 'a1111111-1111-4111-8111-111111111111'], {
                cwd: out,
                env: {
                    ...extra, PATH: bin, HOME: out, TMPDIR: out, HAPI_HOME: join(out, 'hapi'), HAPI_API_URL: 'http://127.0.0.1:' + hub.port,
                    CLI_API_TOKEN: 'parity-token', PARITY_OUT: out, PARITY_AGENT_SCRIPT: join(out, 'agent.jsonl'),
                    HAPI_WRAPPER_CLI_VERSION: pkg.version, HAPI_WRAPPER_RUNTIME_PATH: join(repo, 'cli'),
                },
                stdio: ['ignore', 'ignore', 'pipe'],
            })
            let output = ''
            child.stderr!.on('data', part => output += part)
            const code = await new Promise<number | null>(resolve => child.once('close', resolve))
            expect(code, output).toBe(86)
            expect(output).toMatch(stderr)
            expect(output).not.toContain('HAPI_RUST_WRAPPER_SESSION_ID=')
            expect(hub.rows.map(row => row.event)).toEqual([])
        } finally {
            await hub.close()
            rmSync(out, { recursive: true, force: true })
        }
    }, testTimeout)
})

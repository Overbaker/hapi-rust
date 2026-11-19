import { it } from 'vitest'
import { execFile } from 'node:child_process'
import { promisify } from 'node:util'
import { mkdtempSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { repo } from './run'
import { parityRuntime } from './runtime'

const runtime = parityRuntime(process.env, repo)
for (const scenario of ['mixed-stop', 'hibernation'] as const) {
    it.skipIf(!runtime.binary)(
        'real runner and hub: codex ' + scenario,
        async () => {
            const tmp = mkdtempSync(join(tmpdir(), 'codex-tier-b-'))
            try {
                await promisify(execFile)(
                    process.env.HAPI_BUN_EXEC || 'bun',
                    [join(repo, 'cli/src/parity/codexLifecycle.ts'), runtime.binary!, scenario],
                    {
                        cwd: repo,
                        env: { PATH: process.env.PATH, HOME: tmp, TMPDIR: tmp },
                        timeout: 180_000
                    }
                )
            } finally {
                rmSync(tmp, { recursive: true, force: true })
            }
        },
        190_000
    )
}

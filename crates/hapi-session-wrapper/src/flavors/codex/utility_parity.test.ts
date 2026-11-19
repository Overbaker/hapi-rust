import { afterEach, describe, expect, it } from 'bun:test'
import { readSync, writeSync, readFileSync, constants } from 'node:fs'
import { execFileSync } from 'node:child_process'
import { mkdtemp, writeFile, appendFile, rename, rm, open } from 'node:fs/promises'
import { join } from 'node:path'
import { tmpdir } from 'node:os'
if (process.env.HAPI_RUST_CODEX_UTILITIES === '1') {
    let next = 0
    function call(id, method, ...args) {
        writeSync(1, JSON.stringify([id, method, ...args]) + '\n')
        const chunks = []
        const buf = Buffer.alloc(8192)
        do {
            const n = readSync(0, buf)
            if (!n) throw Error('Rust test driver closed')
            chunks.push(Buffer.from(buf.subarray(0, n)))
        } while (chunks.at(-1).at(-1) !== 10)
        const reply = JSON.parse(Buffer.concat(chunks).toString())
        if (reply.error) throw Error(reply.error)
        return reply.value
    }
    class Clock {
        id = ++next
        start(...args) {
            call(this.id, 'clockStart', ...args)
        }
        finish(...args) {
            return call(this.id, 'finish', ...args) ?? undefined
        }
    }
    class Ledger {
        id = ++next
        noteTokenUsage(n) {
            call(this.id, 'usage', n)
        }
        beginUserWait(at) {
            const id = call(this.id, 'beginUserWait', at)
            return (end) => call(this.id, 'endUserWait', id, end)
        }
    }
    for (const method of ['startTurn', 'beginTool', 'endTool', 'settle', 'reset'])
        Ledger.prototype[method] = function (...args) {
            return call(this.id, method, ...args)
        }
    class Lookup {
        id = ++next
        constructor(thread) {
            call(this.id, 'lookup', thread)
        }
        async find(thread, turn, item, opts = {}) {
            for (let i = 0; i < (opts.attempts ?? 10); i++) {
                const r = call(this.id, 'probe', thread, turn, item)
                if (r.done) return r.result
                if (i + 1 < (opts.attempts ?? 10)) await opts.sleep?.()
            }
            return null
        }
    }
    const env = {
        afterEach,
        describe,
        it,
        expect,
        constants,
        execFileSync,
        mkdtemp,
        writeFile,
        appendFile,
        rename,
        rm,
        open,
        join,
        tmpdir,
        CodexGenerationLedger: Ledger,
        CodexRoundDuration: Clock,
        CodexCompactionSummaryLookup: Lookup,
        resolveCodexSlashCommand: (...a) => call(0, 'slash', ...a),
        settingsMatch: (...a) => call(0, 'matches', ...a),
        inheritedSandbox: (s) => call(0, 'sandbox', s)
    }
    for (const path of [
        'shared/settings',
        'utils/slashCommands',
        'utils/codexGenerationLedger',
        'utils/roundDuration',
        'utils/codexCompactionSummary'
    ]) {
        const source = readFileSync('../../cli/src/codex/' + path + '.test.ts', 'utf8').replace(
            /^import .*$/gm,
            ''
        )
        const body = new Bun.Transpiler({ loader: 'ts' }).transformSync(source)
        new Function(...Object.keys(env), body)(...Object.values(env))
    }
}

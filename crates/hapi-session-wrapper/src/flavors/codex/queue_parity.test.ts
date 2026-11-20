import { afterAll, afterEach, describe, expect, it, mock } from 'bun:test'
import { createInterface } from 'node:readline'
import { readFileSync, writeSync } from 'node:fs'
import { mkdtemp, rm, readFile, writeFile } from 'node:fs/promises'
import { join } from 'node:path'
import { tmpdir } from 'node:os'
if (process.env.HAPI_RUST_QUEUE_TEST === '1') {
    expect.extend({
        toHaveBeenCalledExactlyOnceWith(fn, ...args) {
            return {
                pass: this.equals(fn.mock.calls, [args]),
                message: () =>
                    `Expected exactly one call with ${JSON.stringify(args)}, received ${JSON.stringify(fn.mock.calls)}`
            }
        }
    })
    let next = 0
    const lost = Symbol()
    const calls = new Map()
    const queues = new Map()
    const send = (v) => writeSync(1, JSON.stringify(v) + '\n')
    const lines = createInterface({ input: process.stdin })
    lines.on('line', async (line) => {
        const msg = JSON.parse(line)
        if (msg.call !== undefined) {
            const { q, resolve, reject } = calls.get(msg.call)
            calls.delete(msg.call)
            q.states = msg.states ?? {}
            if (msg.error) reject(Error(msg.error))
            else resolve(msg.value)
        } else {
            const q = queues.get(msg.queue)
            try {
                const value = await q.events[msg.event]?.(...msg.args)
                if (msg.seq !== undefined) send({ seq: msg.seq, value })
            } catch (error) {
                send({ seq: msg.seq, error: error.message, lost: !!error[lost] })
            }
        }
    })
    afterAll(() => {
        lines.close()
        process.stdin.pause()
    })
    class Queue {
        states = {}
        queue = ++next
        work = new Map()
        constructor(
            client,
            private thread,
            private file,
            consumed,
            uncertain,
            mirror,
            requeued
        ) {
            this.events = {
                rpc: client.request.bind(client),
                consumed,
                uncertain,
                mirror,
                requeued,
                command: (id) => this.work.get(id)()
            }
            queues.set(this.queue, this)
        }
        call(method, ...args) {
            const call = ++next
            return new Promise((resolve, reject) => {
                calls.set(call, { resolve, reject, q: this })
                send({ queue: this.queue, call, method, args })
            })
        }
        load() {
            return this.call('load', this.thread, this.file)
        }
        owns(id) {
            return id in this.states
        }
        state(id) {
            return this.states[id]
        }
        async committed(...args) {
            return (await this.call('committed', ...args)) ?? undefined
        }
        command(id, work) {
            this.work.set(id, work)
            return this.call('command', id)
        }
    }
    for (const method of ['cancel', 'deleted', 'enqueue', 'nativeMutation', 'reconcile', 'steer', 'suspend'])
        Queue.prototype[method] = function (...args) {
            return this.call(method, ...args)
        }
    for (const file of ['queue', 'queue.batch']) {
        const suite = readFileSync('../../cli/src/codex/shared/' + file + '.test.ts', 'utf8').replace(
            /^import .*$/gm,
            ''
        )
        const body = new Bun.Transpiler({ loader: 'ts' }).transformSync(suite)
        new Function(
            'describe',
            'expect',
            'it',
            'afterEach',
            'vi',
            'SharedCodexQueue',
            'INDETERMINATE_SYMBOL',
            'mkdtemp',
            'rm',
            'readFile',
            'writeFile',
            'join',
            'tmpdir',
            body
        )(
            describe,
            expect,
            it,
            afterEach,
            { fn: mock },
            Queue,
            lost,
            mkdtemp,
            rm,
            readFile,
            writeFile,
            join,
            tmpdir
        )
    }
}

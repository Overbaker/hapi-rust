import { describe, it, expect, mock } from 'bun:test'
import { readFileSync, readSync, writeSync } from 'node:fs'
import { mkdtemp, writeFile, rm } from 'node:fs/promises'
import { join } from 'node:path'
import { tmpdir } from 'node:os'
import { codexPlanProposalId } from '../../../../../cli/src/codex/shared/plan'
import { extractNotifySummary } from '../../../../../shared/src/messages'

if (process.env.HAPI_RUST_CODEX_MODELS === '1') {
    let next = 0
    function call(id, op, ...args) {
        writeSync(1, JSON.stringify([id, op, args]) + '\n')
        const chunks = []
        const buf = Buffer.alloc(8192)
        do {
            const n = readSync(0, buf)
            if (!n) throw Error('Rust model driver closed')
            chunks.push(Buffer.from(buf.subarray(0, n)))
        } while (chunks.at(-1).at(-1) !== 10)
        return JSON.parse(Buffer.concat(chunks).toString())
    }
    const lost = Symbol()
    class Model {
        id = ++next
        waits = new Map()
        constructor(
            public session,
            thread = '',
            parent?,
            generation = ''
        ) {
            this.run(
                'new',
                thread,
                parent,
                session.getMetadata?.(),
                session.getMetadata?.()?.codexSessionId,
                generation
            )
        }
        run(op, ...args) {
            const { result, effects, error } = call(this.id, op, ...args)
            for (const [type, ...params] of effects) {
                if (type.startsWith('update')) this.session[type]((v) => this.run('apply', params[0], v))
                else if (type === 'waitStart') this.waits.set(params[1], this.wait?.(params[0]))
                else if (type === 'waitEnd') {
                    this.waits.get(params[1])?.(Date.now())
                    this.waits.delete(params[1])
                } else if (type === 'observe') this.observe?.(...params)
                else if (type === 'sendUserMessage')
                    this.session.sendUserMessage(params[0], undefined, params[2])
                else this.session[type]?.(...params)
            }
            if (error) throw Error(error)
            return result
        }
    }
    class Projection extends Model {
        constructor(
            session,
            public thread,
            public committed,
            public parent,
            public readSummary,
            public observe
        ) {
            super(session, thread, parent)
        }
        turnFor(id) {
            return this.run('turnFor', id) ?? undefined
        }
        reset() {
            this.run('reset')
        }
        resetAfterHubReconnect() {
            this.run('hubReset')
        }
        seedFreshUsage() {
            this.run('seed')
        }
        beginUserWait(at) {
            const n = this.run('beginWait', at)
            return (end) => this.run('endWait', n, end)
        }
        async alias(item) {
            if (!this.parent && item?.type === 'userMessage') {
                const id =
                    item.clientId ??
                    item.clientUserMessageId ??
                    (item.id ? `codex:${this.thread}:user:${item.id}` : null)
                if (id) return [await this.committed(id, item.content)]
            }
            return []
        }
        async notification(method, p, model, at = Date.now()) {
            const aliases = ['item/started', 'item/completed'].includes(method)
                ? await this.alias(p.item)
                : []
            const args = [method, p, model, at, aliases, this.session.getMetadata?.()]
            if (method === 'item/completed' && p.item?.type === 'contextCompaction' && this.readSummary)
                args.push(await this.readSummary())
            this.run('notify', ...args)
        }
        async history(thread) {
            const rev = this.run('titleRevision')
            const aliases = []
            for (const t of thread.turns ?? [])
                for (const i of t.items ?? []) {
                    if (i.type === 'userMessage') {
                        aliases.push(...(await this.alias(i)), ...(await this.alias(i)))
                    }
                }
            this.run('history', thread, rev, null, 0, aliases, this.session.getMetadata?.())
        }
    }
    class Permissions extends Model {
        constructor(
            session,
            public client,
            gen,
            asyncAnswer,
            public wait
        ) {
            super(session, '', undefined, gen)
            session.rpcHandlerManager.registerHandler('permission', async (reply) => {
                if (await asyncAnswer?.(reply)) return
                const r = this.run('permReply', reply)
                if (r.interrupt) await client.request('turn/interrupt', r.interrupt)
                else client.respond(r.id, r.response)
            })
        }
        receive(request) {
            const r = this.run('receive', request)
            if (r) this.client.respond(r.id, r.response)
        }
        resolved(...args) {
            this.run('resolved', ...args)
        }
        close() {
            this.run('permClose')
        }
    }
    class Questions extends Model {
        constructor(
            session,
            public submit
        ) {
            super(session)
        }
        history(...args) {
            this.run('qHistory', ...args)
        }
        observeItem(...args) {
            this.run('observe', ...args)
        }
        async answer(reply) {
            const r = this.run('qAnswer', reply)
            if (Array.isArray(r)) {
                try {
                    await this.submit(...r)
                } catch (e) {
                    this.run('submitFailed', reply.id, !!e[lost])
                    throw e
                }
                return true
            }
            return r
        }
        close() {
            this.run('qClose')
        }
    }
    const env = {
        describe,
        it,
        expect,
        vi: {
            fn: mock,
            waitFor: async (f) => {
                for (let i = 0; i < 100; i++) {
                    try {
                        return f()
                    } catch (e) {
                        if (i === 99) throw e
                        await Bun.sleep(5)
                    }
                }
            }
        },
        mkdtemp,
        writeFile,
        rm,
        join,
        tmpdir,
        SharedCodexProjection: Projection,
        SharedCodexPermissions: Permissions,
        SharedCodexAsyncQuestions: Questions,
        codexPlanProposalId,
        extractNotifySummary,
        inputText: (i) => call(0, 'inputText', i).result,
        asyncQuestionsForItem: (...args) => call(0, 'questions', ...args).result,
        INDETERMINATE_SYMBOL: lost
    }
    for (const name of ['projection', 'projection.title', 'permissions', 'asyncQuestions']) {
        const suite = readFileSync('../../cli/src/codex/shared/' + name + '.test.ts', 'utf8').replace(
            /^import .*$/gm,
            ''
        )
        const body = new Bun.Transpiler({ loader: 'ts' }).transformSync(suite)
        new Function(...Object.keys(env), body)(...Object.values(env))
    }
}

import {join} from 'node:path'
import {readdirSync} from 'node:fs'
import {run,repo} from './run'

export const piCorpus = join(repo,'cli/src/parity/corpus/pi')
export const piNames = readdirSync(piCorpus).sort()

export async function piRun(name: string, binary?: string) {
    const transcript = await run(name, binary, {flavor:'pi', rust:Boolean(binary)})
    const rows = Object.values(transcript.channels).flat() as {event:string; args: Record<string, any>[]; ack?: Record<string, any>[]}[]
    const terminal = rows.filter(r => r.event === 'update-metadata' && r.ack?.[0]?.result === 'success').at(-1)
    const lifecycle = rows.filter(r => ['session-ready','session-end'].includes(r.event)).map(r => ({event:r.event, sid:r.args[0]!.sid, reason:r.args[0]!.reason}))
    return JSON.parse(JSON.stringify({
        methods:transcript.methods,
        metadata:terminal?.ack?.[0]?.metadata,
        state:rows.filter(r => r.event === 'update-state').at(-1)?.ack?.[0]?.agentState,
        lifecycle,
        replies: rows.filter(r => r.event === 'rpc-ack').map(r => r.args),
        consumed:rows.filter(r => r.event === 'messages-consumed').map(r => r.args),
        chat:rows.filter(r => r.event === 'message').map(r => r.args[0]!.message),
        decoded:transcript.decoded,
        alive:transcript.alive,
        agent:transcript.agent
    }))
}

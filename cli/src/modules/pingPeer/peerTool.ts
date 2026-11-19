import { z } from 'zod'
import { CREATABLE_AGENT_FLAVORS } from '@hapi/protocol/modes'
import { PermissionModeSchema } from '@hapi/protocol/schemas'
import { SESSION_NAME_MAX_LENGTH } from '@hapi/protocol'
import { isPeerToolsDisabled } from '@/modules/common/peerTools'
import { spawnPeer } from '@/modules/spawnPeer/spawnPeer'
import { PeerDeliveryModeSchema } from './deliveryMode'
import { formatInspectPeerReport, formatPeerSessionsList, forwardPeerReply, startPeerReplyWatch, type PeerReplyWatch, inspectPeer, listPeerSessions, peerListFetchLimit, pingPeer, supportsPeerReply } from './pingPeer'

const nameString = z.string().trim().min(1)
const limit = z.number().int().min(1).max(100).optional()
const ping = z.object({ sessionIdPrefix: nameString, message: z.string().min(1),
    deliveryMode: PeerDeliveryModeSchema.optional(), wake: z.boolean().optional(), waitForReply: z.boolean().optional() })
const spawn = z.object({ directory: nameString, message: z.string().min(1), name: nameString.max(SESSION_NAME_MAX_LENGTH).optional(),
    machine: nameString.optional(), agent: z.enum(CREATABLE_AGENT_FLAVORS).optional(), model: nameString.optional(),
    effort: nameString.optional(), sessionType: z.enum(['simple', 'worktree']).optional(), permissionMode: PermissionModeSchema.optional(),
    waitForReply: z.boolean().optional() })

function replyNote(wait: boolean | undefined, task: {localId?: string, sessionId: string, flavor?: string | null}, caller: string, delegated?: (peer: string, task: string, state: PeerReplyWatch) => void) {
    if (!wait) return ''
    if (!task.localId) return '; the delivered message could not be identified, so check the reply with inspect_peer'
    if (!supportsPeerReply(task.flavor)) return `; ${task.flavor} peers do not report turn ends, so check the reply with inspect_peer`
    const options = { peerSessionId: task.sessionId, taskLocalId: task.localId, callerSessionId: caller }
    if (delegated) delegated(task.sessionId, task.localId, startPeerReplyWatch(options))
    else void forwardPeerReply(options)
        .catch(error => console.error('peer reply forwarding failed:', error))
    return '; its reply will be posted here'
}

export async function handlePeerTool(name: string, input: unknown, delegated?: Parameters<typeof replyNote>[3]) {
    const caller = process.env.HAPI_SESSION_ID ?? ''
    let text: string
    const failure = { list_peers: 'list peers', inspect_peer: 'inspect peer', ping_peer: 'ping peer', spawn_peer: 'spawn peer' }
    try {
        if (isPeerToolsDisabled()) throw new Error('Peer tools disabled by HAPI_DISABLE_PEER_TOOLS')
        switch (name) {
            case 'list_peers': {
                const args = z.object({ limit, includeInactive: z.boolean().optional() }).parse(input)
                const maxRows = args.limit ?? 30
                const includeInactive = args.includeInactive === true
                const sessions = await listPeerSessions({ limit: peerListFetchLimit(maxRows, { excludeCaller: true }), includeInactive })
                text = formatPeerSessionsList(sessions, { maxRows, hasMore: sessions.filter(s => s.id !== caller).length > maxRows,
                    excludeSessionId: caller, includeInactive })
                break
            }
            case 'inspect_peer': {
                const args = z.object({ sessionIdPrefix: nameString, messageLimit: limit }).parse(input)
                text = formatInspectPeerReport(await inspectPeer(args))
                break
            }
            case 'ping_peer': {
                const { waitForReply, ...args } = ping.parse(input)
                const r = await pingPeer({ ...args, wake: args.wake === true })
                const target = `${r.sessionId}${r.resumed ? ' (resumed)' : ''} (${r.name}) localId=${r.localId}`
                text = (r.delivery === 'accepted'
                    ? `Accepted for ${target} — delivery shows in the recipient chat when its runtime consumes it${r.steerNote ? ` (${r.steerNote})` : ''}`
                    : `${r.delivery} to ${target}`) + replyNote(waitForReply, r, caller, delegated)
                break
            }
            case 'spawn_peer': {
                const { waitForReply, ...args } = spawn.parse(input)
                const r = await spawnPeer({ ...args, requireParent: true, cwd: process.env.HAPI_PEER_TOOL_CWD,
                    parent: { sessionId: caller, name: process.env.HAPI_SESSION_NAME ?? null, agentSessionId: process.env.HAPI_AGENT_SESSION_ID ?? null } })
                text = `Spawned ${r.sessionId} (${r.name})${replyNote(waitForReply, r, caller, delegated)}`
                break
            }
            default: throw new Error(`Unknown peer tool: ${name}`)
        }
        return { content: [{ type: 'text', text }], isError: false }
    } catch (e) {
        const action = Object.hasOwn(failure, name) ? failure[name as keyof typeof failure] : 'call peer tool'
        return { content: [{ type: 'text', text: `Failed to ${action}: ${e instanceof Error ? e.message : String(e)}` }], isError: true }
    }
}

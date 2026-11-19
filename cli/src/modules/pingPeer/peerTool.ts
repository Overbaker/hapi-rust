import { z } from 'zod'
import {
    INSPECT_PEER_TOOL_DESCRIPTION,
    LIST_PEERS_INCLUDE_INACTIVE_PARAM_DESCRIPTION,
    LIST_PEERS_TOOL_DESCRIPTION,
    PEER_DELIVERY_MODE_DESCRIPTION,
    PEER_WAIT_FOR_REPLY_PARAM_DESCRIPTION,
    PEER_WAKE_PARAM_DESCRIPTION,
    PING_PEER_TOOL_DESCRIPTION,
    SESSION_ID_PREFIX_PARAM_DESCRIPTION,
    SPAWN_PEER_TOOL_DESCRIPTION,
} from '@hapi/protocol/sessionCitation'
import { CREATABLE_AGENT_FLAVORS } from '@hapi/protocol/modes'
import { PermissionModeSchema } from '@hapi/protocol/schemas'
import { SESSION_NAME_MAX_LENGTH } from '@hapi/protocol'
import { isPeerToolsDisabled } from '@/modules/common/peerTools'
import { spawnPeer } from '@/modules/spawnPeer/spawnPeer'
import { PeerDeliveryModeSchema } from './deliveryMode'
import { formatInspectPeerReport, formatPeerSessionsList, forwardPeerReply, startPeerReplyWatch, type PeerReplyWatch, inspectPeer, listPeerSessions, peerListFetchLimit, pingPeer, supportsPeerReply } from './pingPeer'

export const peerTools = {
    ping_peer: {
        title: 'Ping Peer Session',
        description: PING_PEER_TOOL_DESCRIPTION,
        inputSchema: z.object({
            sessionIdPrefix: z.string().trim().min(1).describe(SESSION_ID_PREFIX_PARAM_DESCRIPTION),
            message: z.string().min(1).describe('Message text to deliver to the target session'),
            deliveryMode: PeerDeliveryModeSchema.optional().describe(PEER_DELIVERY_MODE_DESCRIPTION),
            wake: z.boolean().optional().describe(PEER_WAKE_PARAM_DESCRIPTION),
            waitForReply: z.boolean().optional().describe(PEER_WAIT_FOR_REPLY_PARAM_DESCRIPTION),
        }),
    },
    spawn_peer: {
        title: 'Spawn Peer Session',
        description: SPAWN_PEER_TOOL_DESCRIPTION,
        inputSchema: z.object({
            directory: z.string().trim().min(1)
                .describe('Working directory for the new session on the target machine (must exist there)'),
            message: z.string().min(1).describe('Required first user message (the remit). Empty spawn is a failed spawn.'),
            name: z.string().trim().min(1).max(SESSION_NAME_MAX_LENGTH).optional().describe('Session display name'),
            machine: z.string().trim().min(1).optional()
                .describe(
                    'Optional target runner: hub machine UUID or hostname (metadata.host / displayName from GET /api/machines). '
                    + 'Omit to spawn on this host.'
                ),
            agent: z.enum(CREATABLE_AGENT_FLAVORS).optional()
                .describe('Agent flavor override. When omitted, uses hub peerSpawnDefaults then stock claude.'),
            model: z.string().trim().min(1).optional()
                .describe('Model override for the resolved agent flavor.'),
            effort: z.string().trim().min(1).optional()
                .describe('Effort override (flavor-dependent).'),
            sessionType: z.enum(['simple', 'worktree']).optional()
                .describe('simple or worktree. Default simple (use directory as cwd). worktree creates a new tree from directory, which must be a git repository.'),
            permissionMode: PermissionModeSchema.optional()
                .describe(
                    'Permission mode for the new session. Omit to use hub/stock default (yolo). '
                    + 'Pass only to tighten or when the operator names a mode — do not clone the parent session.'
                ),
            waitForReply: z.boolean().optional().describe(PEER_WAIT_FOR_REPLY_PARAM_DESCRIPTION),
        }),
    },
    inspect_peer: {
        title: 'Inspect Peer Session',
        description: INSPECT_PEER_TOOL_DESCRIPTION,
        inputSchema: z.object({
            sessionIdPrefix: z.string().trim().min(1).describe(SESSION_ID_PREFIX_PARAM_DESCRIPTION),
            messageLimit: z.number().int().min(1).max(100).optional().describe(
                'Recent message page size (default 30, max 100). Text snippets only.'
            ),
        }),
    },
    list_peers: {
        title: 'List Peer Sessions',
        description: LIST_PEERS_TOOL_DESCRIPTION,
        inputSchema: z.object({
            limit: z.number().int().min(1).max(100).optional().describe(
                'Max sessions to return (default 30, max 100). Newest updatedAt first.'
            ),
            includeInactive: z.boolean().optional().describe(LIST_PEERS_INCLUDE_INACTIVE_PARAM_DESCRIPTION),
        }),
    },
} as const

export type PeerToolContext = {
    callerSessionId: string
    name?: string | null
    agentSessionId?: string | null
    cwd?: string
    delegateReply?: (peer: string, task: string, state: PeerReplyWatch) => void
    initialize?: () => Promise<void>
}

function replyNote(wait: boolean | undefined, task: {localId?: string, sessionId: string, flavor?: string | null}, caller: string, delegated?: PeerToolContext['delegateReply']) {
    if (!wait) return ''
    if (!task.localId) return '; the delivered message could not be identified, so check the reply with inspect_peer'
    if (!supportsPeerReply(task.flavor)) return `; ${task.flavor} peers do not report turn ends, so check the reply with inspect_peer`
    const options = { peerSessionId: task.sessionId, taskLocalId: task.localId, callerSessionId: caller }
    if (delegated) delegated(task.sessionId, task.localId, startPeerReplyWatch(options))
    else void forwardPeerReply(options)
        .catch(error => console.error('peer reply forwarding failed:', error))
    return '; its reply will be posted here'
}

export async function handlePeerTool(name: string, input: unknown, ctx: PeerToolContext) {
    const caller = ctx.callerSessionId
    let text: string
    const failure = { list_peers: 'list peers', inspect_peer: 'inspect peer', ping_peer: 'ping peer', spawn_peer: 'spawn peer' }
    try {
        if (isPeerToolsDisabled()) throw new Error('Peer tools disabled by HAPI_DISABLE_PEER_TOOLS')
        switch (name) {
            case 'list_peers': {
                const args = peerTools.list_peers.inputSchema.parse(input)
                await ctx.initialize?.()
                const maxRows = args.limit ?? 30
                const includeInactive = args.includeInactive === true
                const sessions = await listPeerSessions({ limit: peerListFetchLimit(maxRows, { excludeCaller: true }), includeInactive })
                text = formatPeerSessionsList(sessions, { maxRows, hasMore: sessions.filter(s => s.id !== caller).length > maxRows,
                    excludeSessionId: caller, includeInactive })
                break
            }
            case 'inspect_peer': {
                const args = peerTools.inspect_peer.inputSchema.parse(input)
                await ctx.initialize?.()
                text = formatInspectPeerReport(await inspectPeer(args))
                break
            }
            case 'ping_peer': {
                const { waitForReply, ...args } = peerTools.ping_peer.inputSchema.parse(input)
                await ctx.initialize?.()
                const r = await pingPeer({ ...args, wake: args.wake === true })
                const target = `${r.sessionId}${r.resumed ? ' (resumed)' : ''} (${r.name}) localId=${r.localId}`
                text = (r.delivery === 'accepted'
                    ? `Accepted for ${target} — delivery shows in the recipient chat when its runtime consumes it${r.steerNote ? ` (${r.steerNote})` : ''}`
                    : `${r.delivery} to ${target}`) + replyNote(waitForReply, r, caller, ctx.delegateReply)
                break
            }
            case 'spawn_peer': {
                const { waitForReply, ...args } = peerTools.spawn_peer.inputSchema.parse(input)
                await ctx.initialize?.()
                const r = await spawnPeer({ ...args, requireParent: true, cwd: ctx.cwd,
                    parent: { sessionId: caller, name: ctx.name ?? null, agentSessionId: ctx.agentSessionId ?? null } })
                text = `Spawned ${r.sessionId} (${r.name})${replyNote(waitForReply, r, caller, ctx.delegateReply)}`
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

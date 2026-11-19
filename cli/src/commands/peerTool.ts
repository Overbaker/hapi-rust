import { initializeToken } from '@/ui/tokenInit'
import { advancePeerReplyWatch, type PeerReplyWatch } from '@/modules/pingPeer/pingPeer'
import { isPeerToolsDisabled } from '@/modules/common/peerTools'
import { handlePeerTool } from '@/modules/pingPeer/peerTool'
import type { CommandDefinition } from './types'

export const peerToolCommand: CommandDefinition = {
    name: 'peer-tool',
    requiresRuntimeAssets: false,
    run: async ({ commandArgs }) => {
        const chunks = []
        for await (const chunk of process.stdin) chunks.push(Buffer.from(chunk))
        try {
            const args: unknown = JSON.parse(Buffer.concat(chunks).toString('utf8'))
            if (isPeerToolsDisabled()) throw new Error('Peer tools disabled by HAPI_DISABLE_PEER_TOOLS')
            if (process.env.HAPI_PEER_TOOL_PHASE === 'poll') {
                await initializeToken()
                const { peerSessionId, taskLocalId, state } = args as { peerSessionId: string, taskLocalId: string, state: PeerReplyWatch }
                const next = await advancePeerReplyWatch({ peerSessionId, taskLocalId, callerSessionId: process.env.HAPI_SESSION_ID ?? '' }, state)
                process.stdout.write(JSON.stringify({ content: [], isError: false,
                    ...(next && { _meta: { peerSessionId, taskLocalId, state: next } }) }) + '\n')
                return
            }
            let watch: unknown
            const receipt = await handlePeerTool(commandArgs[0] ?? '', args, {
                callerSessionId: process.env.HAPI_SESSION_ID ?? '',
                name: process.env.HAPI_SESSION_NAME,
                agentSessionId: process.env.HAPI_AGENT_SESSION_ID,
                cwd: process.env.HAPI_PEER_TOOL_CWD,
                initialize: initializeToken,
                delegateReply: process.env.HAPI_PEER_TOOL_PHASE === 'call'
                    ? (peerSessionId, taskLocalId, state) => { watch = { peerSessionId, taskLocalId, state } }
                    : undefined,
            })
            const result = watch ? { ...receipt, _meta: watch } : receipt
            process.stdout.write(JSON.stringify(result) + '\n')
        } catch (e) {
            process.stdout.write(JSON.stringify({ content: [{ type: 'text', text: String(e) }], isError: true }) + '\n')
        }
    }
}

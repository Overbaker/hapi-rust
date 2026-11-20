import type { Suggestion } from '@/hooks/useActiveSuggestions'
import type { SessionSummary } from '@/types/api'
import { isHibernatedSession, isSessionActiveForList } from './sessionListOrder'
import { buildSessionReferenceText, matchSessionsForMention } from './sessionReference'
import { getSessionTitle } from './sessionTitle'

/** Build independent shortlists so active sessions cannot crowd out the inactive group. */
export function buildSessionMentionSuggestions(args: {
    sessions: readonly SessionSummary[]
    query: string
    excludeId?: string
    resolveMachineLabel: (machineId: string | null) => string
    inactiveCollapsed: boolean
    inactiveLabel: string
    hibernatedLabel?: string
    toggleInactive: () => void
}): Suggestion[] {
    const matches = matchSessionsForMention(args.sessions, args.query, {
        excludeId: args.excludeId,
        resolveMachineLabel: args.resolveMachineLabel,
        limit: Infinity,
    })
    const activeOrHibernated = matches.filter(isSessionActiveForList).slice(0, 20)
    const inactive = matches.filter(session => !isSessionActiveForList(session)).slice(0, 20)
    const toSuggestion = (session: SessionSummary): Suggestion => {
        const title = getSessionTitle(session)
        const idPrefix = session.id.slice(0, 8)
        const hibernated = isHibernatedSession(session)
        return {
            key: `session:${session.id}`,
            text: buildSessionReferenceText(title, session.id),
            label: `@${title || idPrefix}`,
            description: session.active
                ? `Session · ${idPrefix} · active`
                : hibernated
                    ? `Session · ${idPrefix} · ${args.hibernatedLabel ?? 'Hibernated'}`
                    : `Session · ${idPrefix}`,
            badge: hibernated ? { label: args.hibernatedLabel ?? 'Hibernated', tone: 'hibernated' } : undefined,
            sessionMention: { id: session.id, title: title || idPrefix },
        }
    }
    const result = activeOrHibernated.map(toSuggestion)
    if (inactive.length) {
        result.push({
            key: 'session:inactive-toggle',
            text: '',
            label: args.inactiveLabel,
            action: { expanded: !args.inactiveCollapsed, onSelect: args.toggleInactive },
        })
        if (!args.inactiveCollapsed) result.push(...inactive.map(toSuggestion))
    }
    return result
}

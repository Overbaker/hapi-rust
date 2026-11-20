import { describe, expect, it, vi } from 'vitest'
import type { SessionSummary } from '@/types/api'
import { buildSessionMentionSuggestions } from './sessionMentionSuggestions'

function session(id: string, active: boolean, name: string, updatedAt: number): SessionSummary {
    return { id, active, updatedAt, hasConversationContent: true,
        metadata: { name, path: '/work/project', lifecycleState: active ? 'running' : 'archived' } } as SessionSummary
}

describe('session mention suggestions', () => {
    const options = { query: '', resolveMachineLabel: () => '', inactiveCollapsed: false,
        inactiveLabel: 'Inactive sessions', toggleInactive: vi.fn() }

    it('groups active before inactive, including archived sessions on an empty query', () => {
        const results = buildSessionMentionSuggestions({ ...options, sessions: [
            session('old', false, 'Archived', 200), session('live', true, 'Active', 100),
        ] })
        expect(results.map(item => item.key)).toEqual(['session:live', 'session:inactive-toggle', 'session:old'])
        expect(results[1]?.action?.expanded).toBe(true)
        expect(results[2]?.sessionMention?.id).toBe('old')
    })

    it('keeps the toggle while collapsed and groups even when inactive has a stronger search match', () => {
        const toggleInactive = vi.fn()
        const sessions = [session('old', false, 'Plan', 200), session('live', true, 'Other plan', 100)]
        const expanded = buildSessionMentionSuggestions({ ...options, query: 'plan', sessions })
        expect(expanded.map(item => item.key)).toEqual(['session:live', 'session:inactive-toggle', 'session:old'])
        const collapsed = buildSessionMentionSuggestions({ ...options, query: 'plan', sessions, inactiveCollapsed: true, toggleInactive })
        expect(collapsed.map(item => item.key)).toEqual(['session:live', 'session:inactive-toggle'])
        expect(collapsed[1]?.action?.expanded).toBe(false)
        collapsed[1]?.action?.onSelect()
        expect(toggleInactive).toHaveBeenCalledOnce()
    })

    it('preserves access to inactive sessions even when more than 20 active matches exist', () => {
        const sessions = Array.from({ length: 25 }, (_, index) => session(`live-${index}`, true, 'Plan', index))
        sessions.push(session('old', false, 'Plan', 0))
        const results = buildSessionMentionSuggestions({ ...options, sessions, query: 'plan' })
        expect(results).toHaveLength(22)
        expect(results.at(-2)?.key).toBe('session:inactive-toggle')
        expect(results.at(-1)?.key).toBe('session:old')
    })

    it('keeps hibernated sessions outside the collapsed inactive group with a distinct badge and description', () => {
        const hibernated = {
            id: 'hib-12345678',
            active: false,
            updatedAt: 150,
            hasConversationContent: true,
            metadata: { name: 'Sleeper', path: '/work/project', lifecycleState: 'hibernated' },
        } as SessionSummary
        const archived = session('arch-12345678', false, 'Archived', 200)
        const active = session('live-12345678', true, 'Active', 100)

        const results = buildSessionMentionSuggestions({
            ...options,
            inactiveCollapsed: true,
            hibernatedLabel: 'Hibernated',
            sessions: [archived, hibernated, active],
        })
        expect(results.map(item => item.key)).toEqual([
            'session:live-12345678',
            'session:hib-12345678',
            'session:inactive-toggle',
        ])
        expect(results[1]?.badge).toEqual({ label: 'Hibernated', tone: 'hibernated' })
        expect(results[1]?.description).toBe('Session · hib-1234 · Hibernated')
        expect(results[0]?.badge).toBeUndefined()
    })
})
